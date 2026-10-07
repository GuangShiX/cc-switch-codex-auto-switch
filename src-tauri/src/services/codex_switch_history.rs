//! Bounded native failure history. This contains operation metadata and
//! sanitized failure reasons, never login data, account names or chat bodies.

use std::io::Read;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::{get_app_config_dir, write_json_file};

const FILE_NAME: &str = "codex-auto-switch-failures.json";
const MAX_RECORDS: usize = 100;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_REASON_CHARS: usize = 2048;
const MAX_CANDIDATE_REASON_CHARS: usize = 384;
const MAX_CANDIDATE_FAILURES: usize = 32;
const MAX_PROVIDER_ID_CHARS: usize = 128;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSwitchFailureRecord {
    pub at: u128,
    pub phase: String,
    pub reason: String,
    pub current_provider_id: Option<String>,
    pub target_provider_id: Option<String>,
    /// Optional fields keep old history files and renderers compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_failures: Vec<String>,
}

/// The caller must pass candidate reasons using internal provider IDs,
/// not display names. Capture stage before the terminal phase replaces it.
#[derive(Default)]
pub struct FailureContext<'a> {
    pub operation_id: Option<&'a str>,
    pub source: Option<&'a str>,
    pub stage: Option<&'a str>,
    pub candidate_failures: &'a [String],
}

fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn path() -> std::path::PathBuf {
    get_app_config_dir().join(FILE_NAME)
}

fn identifier(value: Option<&str>, maximum: usize) -> Option<String> {
    let value = value?.trim();
    // Reject unsafe IDs; stripping separators could turn an email/path into
    // a plausible ID while preserving personal information.
    (!value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')))
    .then(|| value.into())
}

fn provider_id(value: Option<&str>) -> Option<String> {
    identifier(value, MAX_PROVIDER_ID_CHARS)
}

fn operation_id(value: Option<&str>) -> Option<String> {
    value
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .map(|value| value.to_string())
}

fn redactions() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // Also redact short labelled secrets embedded in Chinese text.
            r#"(?i)(?:access[_-]?token|refresh[_-]?token|id[_-]?token|auth[_-]?token|api[_-]?key|authorization|cookie|password|client[_-]?secret|secret|token)["']?\s*[:=：]\s*(?:"[^"\r\n]*"|'[^'\r\n]*'|(?:(?:Bearer|Basic)\s+)?[A-Za-z0-9._~+/=:-]+)"#,
            // Raw chat response fields are not diagnostic metadata.
            r#"(?i)"(?:content|prompt|input|messages|access_token|refresh_token|id_token)"\s*:\s*(?:"(?:\\.|[^"\\])*"|\[[^\]\r\n]*\])"#,
            r"(?i)Bearer\s+[A-Za-z0-9._~+/=-]+",
            r#"(?i)https?://[^\s<>"'，；。\p{Han}]+"#,
            r"(?i)[A-Z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Z0-9-]+(?:\.[A-Z0-9-]+)+",
            // Quoted paths can contain spaces; unquoted paths stop before
            // trailing diagnostic words rather than hiding the whole cause.
            r#"["'](?:[A-Za-z]:[\\/]|\\\\)[^"'\r\n]+["']"#,
            r#"[A-Za-z]:[\\/][^\s"'，；。]+"#,
            r#"\\\\[^\s"'，；。]+"#,
            r#"/(?:Users|home|tmp|var|private|mnt|data)/[^\s"'，；。]+"#,
            // Unlabelled JWT/OAuth segments, including next to Chinese text.
            // Methods like thread/loaded/list and "重开/恢复" remain useful.
            r"[A-Za-z0-9._~+=-]{32,}",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("constant redaction expression"))
        .collect()
    })
}

fn sanitize_reason_with_limit(value: &str, maximum: usize) -> String {
    let mut result: String = value.chars().take(16_384).collect();
    for pattern in redactions() {
        result = pattern.replace_all(&result, "[已脱敏]").into_owned();
    }
    result = result.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = result.chars().count() > maximum || value.chars().count() > 16_384;
    result = result.chars().take(maximum).collect();
    if truncated {
        result.push_str(" [原因过长已截断]");
    }
    if result.is_empty() {
        result = "未提供失败原因".into();
    }
    result
}

fn sanitize_reason(value: &str) -> String {
    sanitize_reason_with_limit(value, MAX_REASON_CHARS)
}

fn sanitize_candidate_reason(value: &str) -> String {
    // Provider IDs are already a separate permitted metadata field. Preserve
    // this known prefix so a real UUID still maps to its account card; run the
    // secret redaction on the diagnostic cause rather than on that ID.
    if let Some((id, reason)) = value.split_once('：') {
        if let Some(id) = provider_id(Some(id)) {
            let remaining = MAX_CANDIDATE_REASON_CHARS.saturating_sub(id.chars().count() + 1);
            return format!("{id}：{}", sanitize_reason_with_limit(reason, remaining));
        }
    }
    sanitize_reason_with_limit(value, MAX_CANDIDATE_REASON_CHARS)
}

fn sanitized_record(mut record: CodexSwitchFailureRecord) -> CodexSwitchFailureRecord {
    record.phase = identifier(Some(&record.phase), 64).unwrap_or_else(|| "unknown".into());
    record.reason = sanitize_reason(&record.reason);
    record.current_provider_id = provider_id(record.current_provider_id.as_deref());
    record.target_provider_id = provider_id(record.target_provider_id.as_deref());
    record.operation_id = operation_id(record.operation_id.as_deref());
    record.source = identifier(record.source.as_deref(), 64);
    record.stage = identifier(record.stage.as_deref(), 64);
    record.candidate_failures = record
        .candidate_failures
        .into_iter()
        .take(MAX_CANDIDATE_FAILURES)
        .map(|reason| sanitize_candidate_reason(&reason))
        .collect();
    record
}

fn new_record(
    phase: &str,
    reason: &str,
    current_provider_id: Option<&str>,
    target_provider_id: Option<&str>,
    at: u128,
    context: FailureContext<'_>,
) -> CodexSwitchFailureRecord {
    sanitized_record(CodexSwitchFailureRecord {
        at,
        phase: phase.into(),
        reason: reason.into(),
        current_provider_id: current_provider_id.map(str::to_string),
        target_provider_id: target_provider_id.map(str::to_string),
        operation_id: context.operation_id.map(str::to_string),
        source: context.source.map(str::to_string),
        stage: context.stage.map(str::to_string),
        candidate_failures: context.candidate_failures.to_vec(),
    })
}

fn read_records_at(path: &Path) -> Result<Vec<CodexSwitchFailureRecord>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "换号失败历史无法读取（{:?}）；原记录未改动",
                error.kind()
            ))
        }
    };
    let mut bytes = Vec::new();
    // Bound the actual read, not just metadata which races a growing file.
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("换号失败历史读取失败（{:?}）；原记录未改动", error.kind()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("换号失败历史文件过大；原记录未改动，请先备份并核对".into());
    }
    let records: Vec<CodexSwitchFailureRecord> =
        serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "换号失败历史记录损坏（第 {} 行）；原记录未改动",
                error.line()
            )
        })?;
    Ok(records
        .into_iter()
        .rev()
        .take(MAX_RECORDS)
        .map(sanitized_record)
        .collect())
}

fn append_record_at(path: &Path, record: &CodexSwitchFailureRecord) -> Result<(), String> {
    // Never erase damaged/unreadable evidence. On storage failure the caller
    // writes the new sanitized record to native diagnostics instead.
    let mut records = read_records_at(path)?;
    records.reverse();
    records.push(record.clone());
    if records.len() > MAX_RECORDS {
        records.drain(..records.len() - MAX_RECORDS);
    }
    write_json_file(path, &records).map_err(|_| "换号失败历史保存失败；未清空原记录".into())
}

/// Diagnostics persistence cannot break switching; all values are sanitized
/// before writing either the history file or native log.
pub fn record_failure_with_context(
    phase: &str,
    reason: &str,
    current_provider_id: Option<&str>,
    target_provider_id: Option<&str>,
    at: u128,
    context: FailureContext<'_>,
) {
    let record = new_record(
        phase,
        reason,
        current_provider_id,
        target_provider_id,
        at,
        context,
    );
    let _guard = lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Err(error) = append_record_at(&path(), &record) {
        log::warn!("{error}");
        if let Ok(record) = serde_json::to_string(&record) {
            log::warn!("[CodexSwitchFailure] 未能持久化的脱敏失败记录：{record}");
        }
    }
}

#[allow(dead_code)] // Compatibility for existing native callers.
pub fn record_failure(
    phase: &str,
    reason: &str,
    current_provider_id: Option<&str>,
    target_provider_id: Option<&str>,
    at: u128,
) {
    record_failure_with_context(
        phase,
        reason,
        current_provider_id,
        target_provider_id,
        at,
        FailureContext::default(),
    );
}

pub fn record_failure_now_with_context(
    phase: &str,
    reason: &str,
    current_provider_id: Option<&str>,
    target_provider_id: Option<&str>,
    context: FailureContext<'_>,
) {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    record_failure_with_context(
        phase,
        reason,
        current_provider_id,
        target_provider_id,
        at,
        context,
    );
}

#[allow(dead_code)] // Compatibility for existing native callers.
pub fn record_failure_now(
    phase: &str,
    reason: &str,
    current_provider_id: Option<&str>,
    target_provider_id: Option<&str>,
) {
    record_failure_now_with_context(
        phase,
        reason,
        current_provider_id,
        target_provider_id,
        FailureContext::default(),
    );
}

/// Newest first; errors reach the panel instead of a misleading empty list.
/// Old five-field records remain readable after an upgrade.
pub fn records() -> Result<Vec<CodexSwitchFailureRecord>, String> {
    let _guard = lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    read_records_at(&path())
}

#[tauri::command]
pub fn get_codex_auto_switch_failure_history() -> Result<Vec<CodexSwitchFailureRecord>, String> {
    records()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_uuid_remains_correlatable_while_its_reason_is_redacted() {
        let id = "66666666-6666-4666-8666-666666666666";
        let candidate = sanitize_candidate_reason(&format!(
            "{id}：access_token=short-secret person@example.invalid thread/loaded/list 未就绪"
        ));
        assert!(candidate.starts_with(&format!("{id}：")));
        assert!(candidate.contains("thread/loaded/list"));
        assert!(!candidate.contains("short-secret"));
        assert!(!candidate.contains("person@example.invalid"));
        let unsafe_id = sanitize_candidate_reason("person@example.invalid：等待 5 小时重置");
        assert!(!unsafe_id.contains("person@example.invalid"));
    }

    fn example(at: u128) -> CodexSwitchFailureRecord {
        new_record(
            "blocked",
            "正常退出失败；桌面重开/恢复未完成",
            Some("provider-a"),
            Some("provider-b"),
            at,
            FailureContext {
                operation_id: Some("3218820b-65f0-446f-8a57-fae95342ccdd"),
                source: Some("manual"),
                stage: Some("closing"),
                candidate_failures: &[],
            },
        )
    }

    #[test]
    fn preserves_actual_methods_and_chinese_slash_failure_reasons() {
        let raw = "桌面重开/恢复失败：thread/loaded/list 没有此后台接口；account/read 失败";
        assert_eq!(sanitize_reason(raw), raw);
    }

    #[test]
    fn sanitizes_embedded_secrets_addresses_paths_and_json_chat_fields() {
        let reason = sanitize_reason(concat!(
            "关闭失败：access_token=secret-access；refreshToken:secret-refresh；",
            "authorization=Bearer short-secret；Authorization: Bearer another-secret ",
            "token=abc Authorization: Basic YWJj alice@example.com https://example.test/path?token=x ",
            "C:\\Users\\Alice\\auth.json；需重新核对。",
            "错误abcdefghijklmnopqrstuvwx0123456789结束。",
            "{\"content\":\"private chat body\",\"refresh_token\":\"raw-secret\"}"
        ));
        for private in [
            "secret-access",
            "secret-refresh",
            "short-secret",
            "another-secret",
            "abc",
            "YWJj",
            "alice@example.com",
            "example.test",
            "C:\\Users",
            "abcdefghijklmnopqrstuvwx0123456789",
            "private chat body",
            "raw-secret",
        ] {
            assert!(!reason.contains(private), "leaked {private}");
        }
        assert!(reason.contains("关闭失败"));
        assert!(reason.contains("需重新核对"));
        assert!(reason.contains("[已脱敏]"));
        assert_eq!(
            sanitize_reason(r#"C:\Users\Alice\auth.json open failed; normal exit unavailable"#),
            "[已脱敏] open failed; normal exit unavailable"
        );
        assert_eq!(
            sanitize_reason(r#"读取 "C:\Users\Alice\Private Project\auth.json" 失败"#),
            "读取 [已脱敏] 失败"
        );
    }

    #[test]
    fn unsafe_identifiers_are_rejected_instead_of_mangled() {
        assert_eq!(
            provider_id(Some(" provider-01:codex ")),
            Some("provider-01:codex".into())
        );
        assert_eq!(provider_id(Some("provider-01/a@example.com")), None);
        assert_eq!(provider_id(Some("   ")), None);
        assert_eq!(operation_id(Some("secret-not-a-uuid")), None);
    }

    #[test]
    fn older_records_are_compatible_and_operation_context_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        std::fs::write(&path, r#"[{"at":1,"phase":"blocked","reason":"old reason","currentProviderId":"old-provider","targetProviderId":null}]"#).unwrap();
        let old = read_records_at(&path).unwrap();
        assert_eq!(old[0].operation_id, None);
        assert_eq!(old[0].stage, None);
        append_record_at(&path, &example(2)).unwrap();
        let reopened = read_records_at(&path).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened[0].stage.as_deref(), Some("closing"));
        assert_eq!(reopened[0].phase, "blocked");
        assert_eq!(reopened[0].source.as_deref(), Some("manual"));
        assert_eq!(
            reopened[0].operation_id.as_deref(),
            Some("3218820b-65f0-446f-8a57-fae95342ccdd")
        );
        let encoded = std::fs::read_to_string(&path).unwrap();
        assert!(!encoded.contains("displayName"));
        assert!(!encoded.contains("chatBody"));
    }

    #[test]
    fn persistent_history_retains_latest_one_hundred_in_chronological_file_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        assert!(read_records_at(&path).unwrap().is_empty());
        for at in 1..=105 {
            append_record_at(&path, &example(at)).unwrap();
        }
        let newest = read_records_at(&path).unwrap();
        assert_eq!(newest.len(), 100);
        assert_eq!(newest.first().unwrap().at, 105);
        assert_eq!(newest.last().unwrap().at, 6);
        let file: Vec<CodexSwitchFailureRecord> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(file.first().unwrap().at, 6);
        assert_eq!(file.last().unwrap().at, 105);
    }

    #[test]
    fn corrupt_history_is_reported_and_never_overwritten_by_a_new_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let original = b"{broken history is still evidence";
        std::fs::write(&path, original).unwrap();
        assert!(read_records_at(&path).unwrap_err().contains("损坏"));
        assert!(append_record_at(&path, &example(1)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn oversized_history_has_a_bounded_read_and_preserves_original_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_FILE_BYTES + 100).unwrap();
        assert!(read_records_at(&path).unwrap_err().contains("过大"));
        assert!(append_record_at(&path, &example(1)).is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_FILE_BYTES + 100
        );
    }

    #[test]
    fn read_error_is_not_mistaken_for_missing_history_and_does_not_delete_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep.txt"), "evidence").unwrap();
        assert!(read_records_at(&path).is_err());
        assert!(append_record_at(&path, &example(1)).is_err());
        assert_eq!(
            std::fs::read_to_string(path.join("keep.txt")).unwrap(),
            "evidence"
        );
    }

    #[test]
    fn candidate_reasons_are_sanitized_and_structured_context_is_bounded() {
        let reasons = vec!["provider-c：5h 用量 100%；access_token=abc".into(); 100];
        let record = new_record(
            "blocked",
            "找不到可用账号",
            Some("provider-a"),
            None,
            1,
            FailureContext {
                operation_id: None,
                source: Some("automatic"),
                stage: Some("selecting"),
                candidate_failures: &reasons,
            },
        );
        assert_eq!(record.candidate_failures.len(), MAX_CANDIDATE_FAILURES);
        assert!(!serde_json::to_string(&record)
            .unwrap()
            .contains("access_token"));
        assert!(record.candidate_failures[0].contains("5h 用量 100%"));
        let long = sanitize_reason(&"重开失败".repeat(3000));
        assert!(long.ends_with("[原因过长已截断]"));
    }
}
