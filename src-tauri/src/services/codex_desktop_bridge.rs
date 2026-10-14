//! Shared CC Switch account restart and original-desktop-chat recovery.
//! Account activation uses the original ProviderService; no app-server method
//! is incorrectly sent to the desktop coordinator and no model tool is used.

use crate::app_config::AppType;
use crate::services::codex_desktop_identity::{self as identity, DesktopIdentity};
use crate::services::codex_desktop_session::{
    self, DesktopSession, PausedTask, ResumeConfirmation, SessionError, SessionErrorKind,
    TaskInventory, TaskSnapshot,
};
use crate::services::ProviderService;
use crate::services::{codex_auto_switch as monitor, codex_desktop_restart as restart};
use crate::store::AppState;
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use tauri::{Emitter, Manager};

static FLOW_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));
static RECORD_LOCK: LazyLock<std::sync::Mutex<()>> = LazyLock::new(|| std::sync::Mutex::new(()));

/// Durable evidence of the intended close, never proof that a quit request
/// was sent or that the desktop exited. It does not authorize recovery replay.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClosingCheckpoint {
    schema_version: u32,
    intent_at: i64,
    manager_pid: u32,
    manager_birth: Option<u64>,
    original_desktop: Option<restart::DesktopReceipt>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryRecord {
    operation_id: String,
    generation: u64,
    source_provider_id: String,
    target_provider_id: String,
    phase: String,
    #[serde(default)]
    close_checkpoint: Option<ClosingCheckpoint>,
    desktop_restarted: bool,
    runtime_identity_confirmed: bool,
    reopened_pid: Option<u32>,
    reopened_birth: Option<u64>,
    #[serde(default)]
    reopened_port: Option<u16>,
    #[serde(default)]
    wait_until: Option<i64>,
    #[serde(default)]
    waiting_account_id: Option<String>,
    #[serde(default)]
    wait_baselines: Vec<TaskSnapshot>,
    #[serde(default)]
    abandoned_tasks: Vec<String>,
    #[serde(skip, default = "loaded_record")]
    loaded: bool,
    planned_tasks: Vec<TaskSnapshot>,
    paused_tasks: Vec<PausedTask>,
    resume_intents: Vec<String>,
    settings_restore_intents: Vec<String>,
    settings_restored_tasks: Vec<String>,
    resumed_tasks: Vec<String>,
    resume_confirmations: Vec<ResumeConfirmation>,
    #[serde(default)]
    resume_not_sent_tasks: Vec<String>,
}

impl Drop for RecoveryRecord {
    fn drop(&mut self) {
        if self.loaded
            || matches!(
                self.phase.as_str(),
                "completed" | "checking" | "waiting-for-reset" | "cancelled" | "superseded"
            )
        {
            return;
        }
        self.phase = if monitor::operation_generation() != self.generation {
            "cancelled"
        } else {
            "needs-review"
        }
        .into();
        if let Err(error) = self.persist() {
            log::warn!("Codex recovery journal final state failed: {error}");
        }
    }
}

fn loaded_record() -> bool {
    true
}

#[derive(Clone, Debug)]
pub struct QuotaResetWait {
    pub operation_id: String,
    pub provider_id: String,
    pub account_id: String,
    pub reset_at: i64,
}

fn read_record(path: &std::path::Path) -> Result<RecoveryRecord, String> {
    let value = read_recovery_value(path)?;
    let record: RecoveryRecord =
        serde_json::from_value(value).map_err(|_| "额度等待记录损坏；未重放操作")?;
    let id = uuid::Uuid::parse_str(&record.operation_id).map_err(|_| "额度等待操作标识无效")?;
    if path.file_stem().and_then(|name| name.to_str()) != Some(id.to_string().as_str()) {
        return Err("额度等待文件与操作标识不一致".into());
    }
    Ok(record)
}

fn read_recovery_value(path: &std::path::Path) -> Result<serde_json::Value, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| "额度等待记录无法读取；未重放操作")?;
    let mut bytes = Vec::new();
    file.take(1_048_577)
        .read_to_end(&mut bytes)
        .map_err(|_| "额度等待记录读取失败")?;
    if bytes.len() > 1_048_576 {
        return Err("额度等待记录大小异常；未重放操作".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "额度等待记录损坏；未重放操作".into())
}

pub fn quota_reset_wait() -> Result<Option<QuotaResetWait>, String> {
    quota_reset_wait_in(&crate::config::get_app_config_dir().join("codex-desktop-recovery"))
}

fn quota_reset_wait_in(directory: &std::path::Path) -> Result<Option<QuotaResetWait>, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("无法检查额度等待记录".into()),
    };
    let mut waiting = None;
    for entry in entries {
        let path = entry.map_err(|_| "无法枚举额度等待记录")?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        // Older completed recovery journals need not have the new wait fields.
        // Parse the wait schema only for a live wait, not for old task history.
        let value = read_recovery_value(&path)?;
        if value["phase"] != "waiting-for-reset" {
            continue;
        }
        let record = read_record(&path)?;
        if waiting.is_some() {
            return Err("发现多个未结束的额度等待计划；需核对，不自动继续".into());
        }
        if !record.resume_intents.is_empty()
            || !record.resumed_tasks.is_empty()
            || record.wait_baselines.len() != record.paused_tasks.len()
        {
            return Err("额度等待记录已有继续意图；需核对，不重复继续".into());
        }
        let mut task_ids = std::collections::HashSet::new();
        for saved in &record.paused_tasks {
            let baseline = record
                .wait_baselines
                .iter()
                .find(|task| task.thread_id == saved.thread_id);
            if !task_ids.insert(&saved.thread_id)
                || !baseline.is_some_and(|task| matches_wait_baseline(task, task, saved))
            {
                return Err("额度等待记录的原聊天或轮次不一致；未自动继续".into());
            }
        }
        waiting = Some(QuotaResetWait {
            operation_id: record.operation_id.clone(),
            provider_id: record.target_provider_id.clone(),
            account_id: record
                .waiting_account_id
                .clone()
                .ok_or("额度等待记录缺少账号绑定")?,
            reset_at: record.wait_until.ok_or("额度等待记录缺少重置时间")?,
        });
    }
    Ok(waiting)
}

fn wait_record_path(operation_id: &str) -> Result<std::path::PathBuf, String> {
    let id = uuid::Uuid::parse_str(operation_id).map_err(|_| "额度等待操作标识无效")?;
    Ok(crate::config::get_app_config_dir()
        .join("codex-desktop-recovery")
        .join(format!("{id}.json")))
}

fn cancelled_quota_wait_tickets() -> Result<Vec<PausedTask>, String> {
    let directory = crate::config::get_app_config_dir().join("codex-desktop-recovery");
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("无法核对已取消的额度等待任务".into()),
    };
    let mut cancelled = Vec::new();
    for entry in entries {
        let path = entry.map_err(|_| "无法枚举已取消的额度等待任务")?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let value = read_recovery_value(&path)?;
        if value["phase"] == "cancelled" && !value["waitUntil"].is_null() {
            let tickets: Vec<PausedTask> = serde_json::from_value(value["pausedTasks"].clone())
                .map_err(|_| "已取消的等待任务记录无法核对")?;
            cancelled.extend(tickets);
        }
    }
    Ok(cancelled)
}

fn ensure_desktop_task_coverage(root_count: usize) -> Result<(), String> {
    if root_count > 1 {
        return Err(format!("检测到 {root_count} 个独立 Codex 桌面实例，协调接口无法核实每个实例的聊天覆盖范围；未暂停或关闭桌面。请保留一个独立实例后重试（同一实例的多个窗口可用）"));
    }
    Ok(())
}

pub fn invalidate_quota_waits() -> Result<(), String> {
    if let Some(wait) = quota_reset_wait()? {
        let mut record = read_record(&wait_record_path(&wait.operation_id)?)?;
        record.phase = "cancelled".into();
        record.persist()?;
    }
    Ok(())
}

pub fn update_quota_wait_time(operation_id: &str, reset_at: i64) -> Result<(), String> {
    let mut record = read_record(&wait_record_path(operation_id)?)?;
    if record.phase != "waiting-for-reset" {
        return Err("额度等待计划已失效".into());
    }
    record.wait_until = Some(reset_at);
    record.persist()
}

/// Uncertain mutations survive CC Switch restart as a visible reason to wait,
/// never as a queue of close/enable/continue operations to replay at night.
pub fn pending_recovery_reason() -> Option<String> {
    let directory = crate::config::get_app_config_dir().join("codex-desktop-recovery");
    let entries = std::fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            return Some("上次桌面恢复记录无法读取；请先核对桌面状态或手动启用账号".into());
        };
        if bytes.len() > 1_048_576 {
            return Some("上次桌面恢复记录大小异常；未自动重放操作".into());
        }
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Some("上次桌面恢复记录损坏；未自动重放操作".into());
        };
        if recoverable_resume_phase(record["phase"].as_str()) {
            return Some(recovery_pending_description(&record));
        }
        if !matches!(
            record["phase"].as_str(),
            Some("completed" | "cancelled" | "superseded" | "waiting-for-reset")
        ) {
            return Some(
                "上次桌面流程被中断，结果需要核对；未重放关闭、换号或继续，可手动启用账号重新开始"
                    .into(),
            );
        }
    }
    None
}

fn confirmed_recovery_candidate(record: &RecoveryRecord, current: &str) -> bool {
    if !recoverable_resume_phase(Some(&record.phase))
        || record.target_provider_id != current
        || !record.desktop_restarted
        || !record.runtime_identity_confirmed
        || record.reopened_pid.is_none()
        || record.reopened_birth.is_none()
        || record.reopened_port.is_none()
        || record.wait_until.is_some()
        || record.paused_tasks.is_empty()
        || record.planned_tasks.len() != record.paused_tasks.len() + record.abandoned_tasks.len()
    {
        return false;
    }
    let mut planned_ids = std::collections::HashSet::new();
    if !record
        .planned_tasks
        .iter()
        .all(|task| planned_ids.insert(&task.thread_id))
    {
        return false;
    }
    let mut ids = std::collections::HashSet::new();
    if !record
        .abandoned_tasks
        .iter()
        .all(|id| planned_ids.contains(id) && ids.insert(id))
    {
        return false;
    }
    record.paused_tasks.iter().all(|saved| {
        if saved.pause_operation_id != record.operation_id || !ids.insert(&saved.thread_id) {
            return false;
        }
        let Some(planned) = record
            .planned_tasks
            .iter()
            .find(|task| task.thread_id == saved.thread_id)
        else {
            return false;
        };
        match saved.origin {
            codex_desktop_session::RecoveryOrigin::CcSwitchPause => {
                if !planned.safely_running() {
                    return false;
                }
                // Validate the serialized pause confirmation through the same
                // exact-origin predicate used before shutdown; no private flag
                // is inferred from just the fact that a ticket exists.
                let mut baseline = planned.clone();
                baseline.status = Some("interrupted".into());
                baseline.runtime_status = "idle".into();
                baseline.matches_paused(saved)
            }
            codex_desktop_session::RecoveryOrigin::QuotaExhausted => planned.matches_paused(saved),
            codex_desktop_session::RecoveryOrigin::DesktopShutdown => {
                planned.safely_running()
                    && planned.turn_id.as_deref() == Some(saved.turn_id.as_str())
                    && planned.context == saved.context
                    && saved.is_desktop_shutdown_ticket()
            }
        }
    })
}

fn recoverable_resume_phase(phase: Option<&str>) -> bool {
    matches!(
        phase,
        Some(
            "needs-review"
                | "reopened"
                | "recovering-saved-tasks"
                | "resume-intent"
                | "resume-outcome-needs-review"
                | "resume-not-sent"
                | "resumed"
        )
    )
}

#[cfg(test)]
fn obsolete_recovery_candidate(record: &RecoveryRecord, current: &str) -> bool {
    confirmed_recovery_candidate(record, current)
        && record.resume_intents.is_empty()
        && record.settings_restore_intents.is_empty()
        && record.settings_restored_tasks.is_empty()
        && record.resumed_tasks.is_empty()
        && record.resume_confirmations.is_empty()
}

fn late_resume_candidate(record: &RecoveryRecord, current: &str) -> bool {
    // Mark completed only when every owned chat has exactly one persisted
    // intent and that exact operation's new turn is observed. Partial flows
    // may be superseded by proven user changes, never started from here.
    changed_recovery_candidate(record, current)
        && record.resume_intents.len() == record.paused_tasks.len()
}

fn recovery_pending_description(record: &serde_json::Value) -> String {
    let tasks = record["pausedTasks"].as_array().map_or(0, Vec::len);
    let sent = record["resumeIntents"].as_array().map_or(0, Vec::len);
    if record["runtimeIdentityConfirmed"] == true {
        format!("账号和重开桌面已确认；原任务 {} 条，恢复请求已记录 {} 条。后台会核对原轮次并补完尚未发送的任务，已发送的请求只确认结果", tasks, sent)
    } else {
        "桌面退出或实际账号核验未完成；具体原因已保存在失败记录，确认前不恢复任务".into()
    }
}

fn unsent_recovery_candidate(record: &RecoveryRecord, current: &str) -> bool {
    changed_recovery_candidate(record, current)
        && record
            .paused_tasks
            .iter()
            .any(|task| !record.resume_intents.contains(&task.thread_id))
        && record
            .paused_tasks
            .iter()
            .all(|task| task.saved_settings.is_some())
}

fn changed_recovery_candidate(record: &RecoveryRecord, current: &str) -> bool {
    let mut intents = std::collections::HashSet::new();
    confirmed_recovery_candidate(record, current)
        && record.settings_restore_intents.is_empty()
        && record.resume_intents.iter().all(|id| {
            intents.insert(id)
                && record
                    .paused_tasks
                    .iter()
                    .any(|saved| saved.thread_id == *id)
        })
        && record
            .resumed_tasks
            .iter()
            .all(|id| record.resume_intents.contains(id))
        && record
            .settings_restored_tasks
            .iter()
            .all(|id| record.resume_intents.contains(id))
        && record.resume_confirmations.iter().all(|confirmation| {
            confirmation.operation_id == record.operation_id
                && record.resume_intents.contains(&confirmation.thread_id)
        })
        && record.resumed_tasks.iter().all(|id| {
            record.settings_restored_tasks.contains(id)
                && record.resume_confirmations.iter().any(|confirmation| {
                    confirmation.thread_id == *id
                        && confirmation.operation_id == record.operation_id
                        && !confirmation.turn_id.is_empty()
                        && record.paused_tasks.iter().any(|saved| {
                            saved.thread_id == *id && saved.turn_id != confirmation.turn_id
                        })
                })
        })
}

fn saved_task_was_changed(saved: &PausedTask, actual: &TaskSnapshot) -> bool {
    if actual.thread_id != saved.thread_id
        || actual.is_child
        || actual.ephemeral
        || actual.matches_paused(saved)
        || actual.turn_id.as_deref().is_none_or(str::is_empty)
    {
        return false;
    }
    let new_turn = actual.turn_id.as_deref() != Some(saved.turn_id.as_str());
    let known_idle = actual.safely_idle()
        && matches!(
            actual.status.as_deref(),
            Some("completed" | "interrupted" | "failed")
        );
    if new_turn && (actual.safely_running() || known_idle || actual.safely_quota_failed()) {
        return true;
    }
    // A real request for user input/approval also invalidates the owned pause.
    // Unknown runtime flags and unconfirmed submission are never terminal proof.
    if actual.waiting
        && matches!(
            actual.waiting_reason,
            Some(
                codex_desktop_session::WaitingReason::PendingApproval
                    | codex_desktop_session::WaitingReason::PendingUserInput
                    | codex_desktop_session::WaitingReason::PendingElicitation
            )
        )
    {
        return true;
    }
    actual.safely_idle() && actual.status.as_deref() == Some("completed")
}

fn all_saved_tasks_were_changed(record: &RecoveryRecord, inventory: &TaskInventory) -> bool {
    inventory.candidate_coverage_complete
        && !record.paused_tasks.is_empty()
        && record.paused_tasks.iter().all(|saved| {
            let mut matches = inventory
                .running
                .iter()
                .chain(inventory.idle.iter())
                .chain(inventory.quota_failed.iter())
                .chain(inventory.blocked.iter())
                .filter(|task| task.thread_id == saved.thread_id);
            matches.next().is_some_and(|actual| {
                matches.next().is_none()
                    // Our own unconfirmed new turn is not a user choice. In
                    // particular a model/permission mismatch must stay visible,
                    // rather than being discarded as an obsolete pause.
                    && actual.client_user_message_id.as_deref() != Some(record.operation_id.as_str())
                    && saved_task_was_changed(saved, actual)
            })
        })
}

/// Read-only settlement of failed restoration. Unsent tickets become obsolete
/// after proven user changes. Sent intents complete only after observing every
/// exact operation's new turn with the saved model/project/permissions. This
/// never starts, interrupts, navigates, activates or repeats uncertain actions.
pub async fn reconcile_obsolete_recovery(
    app: &tauri::AppHandle,
    generation: u64,
    current: &str,
    allow_unsent_recovery: bool,
) -> Result<usize, String> {
    let Ok(_flow) = FLOW_LOCK.try_lock() else {
        return Ok(0);
    };
    check_selection(app, generation, current)?;
    let directory = crate::config::get_app_config_dir().join("codex-desktop-recovery");
    let paths = match std::fs::read_dir(&directory) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "无法只读核对旧恢复记录")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(_) => return Err("无法只读核对旧恢复记录".into()),
    };
    let mut reconciled = 0;
    for path in paths {
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let value = read_recovery_value(&path)?;
        if !recoverable_resume_phase(value["phase"].as_str()) {
            continue;
        }
        let mut record = read_record(&path)?;
        let late_resume = late_resume_candidate(&record, current);
        if !changed_recovery_candidate(&record, current) {
            continue;
        }
        let receipt = receipt_for_wait(&record)?;
        let guard_app = app.clone();
        let guard_current = current.to_owned();
        let guard_receipt = receipt.clone();
        let guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync> = Arc::new(move || {
            check_selection(&guard_app, generation, &guard_current)?;
            restart::ensure_identity_desktop(&guard_receipt)
        });
        guard()?;
        // Use the already owned live login, not target_identity(), which can
        // refresh OAuth tokens. No credentials or identity values are logged.
        let expected = current_login_identity(app, current).await?;
        guard()?;
        confirm_identity_receipt(&expected, &receipt, &guard).await?;
        if allow_unsent_recovery && unsent_recovery_candidate(&record, current) {
            monitor::retain_owned_recovery(generation)?;
            if !claim_unsent_recovery(&path, &value, generation, current, guard.as_ref())? {
                continue;
            }
            record.generation = generation;
            record.phase = "recovering-saved-tasks".into();
            record.loaded = false;
            guard()?;
            monitor::saved_recovery_status(app, generation, &record.operation_id, current);
            resume_saved_tasks(
                app,
                generation,
                &mut record,
                guard.clone(),
                Some(&expected),
                receipt,
                true,
            )
            .await?;
            record.phase = "completed".into();
            record.persist()?;
            reconciled += 1;
            continue;
        }
        if late_resume {
            let mut session = connect_reopened_desktop(&guard).await?;
            let mut confirmations = Vec::new();
            for saved in &record.paused_tasks {
                guard()?;
                match session.reconcile_resume(saved, &record.operation_id).await {
                    Ok(confirmation) => confirmations.push(confirmation),
                    Err(_) => break, // Still uncertain; no start or other mutation.
                }
            }
            guard()?;
            if confirmations.len() == record.paused_tasks.len() {
                confirm_identity_receipt(&expected, &receipt, &guard).await?;
                if complete_late_recovery(&path, &value, &confirmations, guard.as_ref())? {
                    reconciled += 1;
                }
                continue;
            }
            // A later user turn can supersede a previously sent intent too.
            // Verify all owned tasks below before discarding any old plan.
        }
        let (_session, inventory) = snapshot_original_desktop(guard.as_ref()).await?;
        if !all_saved_tasks_were_changed(&record, &inventory) {
            continue;
        }
        guard()?;
        if supersede_obsolete_recovery(&path, &value, &record.paused_tasks, guard.as_ref())? {
            reconciled += 1;
        }
    }
    Ok(reconciled)
}

async fn current_login_identity(
    app: &tauri::AppHandle,
    provider_id: &str,
) -> Result<DesktopIdentity, String> {
    let state = app.state::<AppState>();
    let provider = state
        .db
        .get_provider_by_id(provider_id, AppType::Codex.as_str())
        .map_err(|error| error.to_string())?
        .ok_or("旧恢复记录的当前供应商已移除")?;
    let account = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
        .ok_or("旧恢复记录的当前供应商没有托管身份")?;
    let auth = crate::config::read_json_file::<serde_json::Value>(
        &crate::codex_config::get_codex_auth_path(),
    )
    .map_err(|_| "无法只读核对当前登录身份；旧恢复记录保持待处理")?;
    if !crate::codex_config::codex_auth_matches_recorded_managed_oauth(&auth, &account)
        .map_err(|_| "无法只读核对当前托管登录归属")?
    {
        return Err("当前登录归属与托管账号不一致；旧恢复记录保持待处理".into());
    }
    let expected = identity::identity_from_token(
        auth.pointer("/tokens/access_token")
            .and_then(serde_json::Value::as_str)
            .ok_or("当前登录缺少可只读核对的身份")?,
    )?;
    let workspace = state
        .codex_oauth_manager
        .chatgpt_account_id_for_account(&account)
        .await
        .map_err(|_| "当前托管账号的工作区无法只读确认")?;
    if expected.account_id != workspace {
        return Err("当前登录与托管工作区不一致；旧恢复记录保持待处理".into());
    }
    Ok(expected)
}

fn complete_late_recovery(
    path: &std::path::Path,
    expected: &serde_json::Value,
    confirmations: &[ResumeConfirmation],
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
) -> Result<bool, String> {
    guard()?;
    let record_lock = RECORD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut value = read_recovery_value(path)?;
    if value != *expected {
        return Ok(false);
    }
    let record = read_record(path)?;
    if !late_resume_candidate(&record, &record.target_provider_id)
        || confirmations.len() != record.paused_tasks.len()
        || !record.paused_tasks.iter().all(|saved| {
            let mut matching = confirmations
                .iter()
                .filter(|item| item.thread_id == saved.thread_id);
            matching.next().is_some_and(|item| {
                item.operation_id == record.operation_id
                    && !item.turn_id.is_empty()
                    && item.turn_id != saved.turn_id
                    && item.already_observed
                    && matching.next().is_none()
            })
        })
    {
        return Ok(false);
    }
    let ids: Vec<_> = record
        .paused_tasks
        .iter()
        .map(|saved| &saved.thread_id)
        .collect();
    value["phase"] = serde_json::json!("completed");
    value["resumeConfirmations"] =
        serde_json::to_value(confirmations).map_err(|_| "无法保存已确认的原聊天恢复结果")?;
    value["resumedTasks"] = serde_json::json!(ids);
    value["settingsRestoredTasks"] = serde_json::json!(ids);
    value["settlementReason"] =
        serde_json::json!("只读核对已确认本次恢复的新轮次及原模型、项目和权限；未重复发送继续");
    commit_recovery_value(path, &value)?;
    drop(record_lock);
    guard()?;
    Ok(true)
}

fn claim_unsent_recovery(
    path: &std::path::Path,
    expected: &serde_json::Value,
    generation: u64,
    current: &str,
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
) -> Result<bool, String> {
    guard()?;
    let record_lock = RECORD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut value = read_recovery_value(path)?;
    if value != *expected || !unsent_recovery_candidate(&read_record(path)?, current) {
        return Ok(false);
    }
    value["generation"] = serde_json::json!(generation);
    value["phase"] = serde_json::json!("recovering-saved-tasks");
    commit_recovery_value(path, &value)?;
    drop(record_lock);
    guard()?;
    Ok(true)
}

fn supersede_obsolete_recovery(
    path: &std::path::Path,
    expected: &serde_json::Value,
    saved: &[PausedTask],
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
) -> Result<bool, String> {
    guard()?;
    let record_lock = RECORD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut value = read_recovery_value(path)?;
    if value != *expected {
        return Ok(false);
    }
    value["phase"] = serde_json::json!("superseded");
    if value["abandonedTasks"].is_null() {
        value["abandonedTasks"] = serde_json::json!([]);
    }
    let abandoned = value["abandonedTasks"]
        .as_array_mut()
        .ok_or("旧恢复记录的失效任务列表无法核对")?;
    for task in saved {
        if !abandoned
            .iter()
            .any(|id| id.as_str() == Some(task.thread_id.as_str()))
        {
            abandoned.push(serde_json::json!(task.thread_id));
        }
    }
    value["supersededReason"] = serde_json::json!(
        "只读核对确认本次保存的原任务均已被继续、完成或等待用户处理；旧计划失效，未重放任何操作"
    );
    commit_recovery_value(path, &value)?;
    drop(record_lock);
    guard()?;
    Ok(true)
}

fn commit_recovery_value(path: &std::path::Path, value: &serde_json::Value) -> Result<(), String> {
    use std::io::Write;
    let encoded = serde_json::to_vec(&value).map_err(|_| "无法编码旧恢复记录终态")?;
    if encoded.len() > 1_048_576 {
        return Err("旧恢复记录终态超出安全读取上限；保持原记录待处理".into());
    }
    let mut file = tempfile::NamedTempFile::new_in(path.parent().ok_or("旧恢复记录路径无效")?)
        .map_err(|_| "无法保存旧恢复记录终态")?;
    file.write_all(&encoded)
        .map_err(|_| "无法编码旧恢复记录终态")?;
    file.flush().map_err(|_| "无法刷新旧恢复记录终态")?;
    file.as_file()
        .sync_all()
        .map_err(|_| "无法同步旧恢复记录终态")?;
    file.persist(path).map_err(|_| "无法提交旧恢复记录终态")?;
    Ok(())
}

/// Capture existing journal paths before invalidating the user's operation.
/// A subsequently started manual flow has a new UUID and is not cancelled by
/// this acknowledgement. No pause, login, close or continue action is replayed.
pub fn recovery_records_for_cancellation() -> Result<Vec<std::path::PathBuf>, String> {
    let directory = crate::config::get_app_config_dir().join("codex-desktop-recovery");
    match std::fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|_| "无法读取旧恢复记录".into())
            })
            .filter(|entry| {
                entry.as_ref().map_or(true, |path| {
                    path.extension()
                        .is_some_and(|extension| extension == "json")
                })
            })
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(_) => Err("无法读取旧恢复记录；本次计划已停止".into()),
    }
}

pub fn cancel_captured_recovery_records(paths: &[std::path::PathBuf]) -> Result<(), String> {
    use std::io::Write;
    let _record_lock = RECORD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for path in paths {
        let bytes = std::fs::read(path).map_err(|_| "旧恢复记录无法读取；未重放操作")?;
        if bytes.len() > 1_048_576 {
            return Err("旧恢复记录大小异常；未重放操作".into());
        }
        let mut record: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| "旧恢复记录损坏；请核对状态")?;
        let operation = record["operationId"]
            .as_str()
            .ok_or("旧恢复记录缺少操作标识")?;
        let operation = uuid::Uuid::parse_str(operation).map_err(|_| "旧恢复记录操作标识无效")?;
        if path.file_stem().and_then(|name| name.to_str()) != Some(operation.to_string().as_str()) {
            return Err("旧恢复记录文件与操作标识不一致".into());
        }
        if matches!(
            record["phase"].as_str(),
            Some("completed" | "cancelled" | "superseded")
        ) {
            continue;
        }
        record["phase"] = serde_json::json!("cancelled");
        let mut file = tempfile::NamedTempFile::new_in(path.parent().ok_or("恢复记录路径无效")?)
            .map_err(|_| "无法保存取消记录")?;
        serde_json::to_writer(&mut file, &record).map_err(|_| "无法编码取消记录")?;
        file.flush().map_err(|_| "无法刷新取消记录")?;
        file.as_file().sync_all().map_err(|_| "无法同步取消记录")?;
        file.persist(path).map_err(|_| "无法提交取消记录")?;
    }
    Ok(())
}

fn supersede_old_recovery_records() -> Result<(), String> {
    let _record_lock = RECORD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = crate::config::get_app_config_dir().join("codex-desktop-recovery");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|_| "无法核对旧聊天恢复记录")?;
        if bytes.len() > 1_048_576 {
            return Err("旧聊天恢复记录大小异常；未继续重开".into());
        }
        let mut record: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| "旧聊天恢复记录损坏；需人工核对")?;
        if !matches!(
            record["phase"].as_str(),
            Some("completed" | "cancelled" | "superseded")
        ) {
            record["phase"] = serde_json::json!("superseded");
            std::fs::write(
                &path,
                serde_json::to_vec(&record).map_err(|_| "无法编码旧恢复记录")?,
            )
            .map_err(|_| "无法标记旧恢复计划失效")?;
        }
    }
    Ok(())
}

impl RecoveryRecord {
    fn persist(&self) -> Result<(), String> {
        self.persist_in(&crate::config::get_app_config_dir().join("codex-desktop-recovery"))
    }

    fn persist_in(&self, directory: &std::path::Path) -> Result<(), String> {
        use std::io::Write;
        let _record_lock = RECORD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::fs::create_dir_all(directory).map_err(|_| "无法创建本次聊天恢复记录目录")?;
        let path = directory.join(format!("{}.json", self.operation_id));
        if path.exists() {
            let existing = read_recovery_value(&path)?;
            if matches!(
                existing["phase"].as_str(),
                Some("cancelled" | "superseded" | "completed")
            ) && self.phase != existing["phase"].as_str().unwrap_or("")
            {
                return Err("旧恢复计划已经结束或取消；未覆盖记录或重复继续".into());
            }
            if self.loaded
                && self.phase == "waiting-for-reset"
                && existing["phase"] != "waiting-for-reset"
            {
                return Err("额度等待状态已改变；未覆盖正在恢复的任务".into());
            }
        }
        let encoded = serde_json::to_vec(self).map_err(|_| "本次聊天恢复记录编码失败")?;
        if encoded.len() > 1_048_576 {
            return Err("本次聊天恢复记录超出安全读取上限；未继续操作".into());
        }
        let mut file =
            tempfile::NamedTempFile::new_in(directory).map_err(|_| "无法保存本次聊天恢复记录")?;
        file.write_all(&encoded)
            .map_err(|_| "本次聊天恢复记录写入失败")?;
        file.flush().map_err(|_| "无法刷新本次聊天恢复记录")?;
        file.as_file()
            .sync_all()
            .map_err(|_| "无法同步本次聊天恢复记录")?;
        file.persist(path).map_err(|_| "无法提交本次聊天恢复记录")?;
        Ok(())
    }
}

fn check_selection(app: &tauri::AppHandle, generation: u64, expected: &str) -> Result<(), String> {
    monitor::generation_is_current(generation)?;
    let actual = ProviderService::current(&app.state::<AppState>(), AppType::Codex)
        .map_err(|e| e.to_string())?;
    if actual != expected {
        return Err("用户已选择其他账号，本次旧流程已失效".into());
    }
    restart::ensure_interactive_session()?;
    Ok(())
}

fn check_target_login(app: &tauri::AppHandle, target: &str) -> Result<(), String> {
    let provider = app
        .state::<AppState>()
        .db
        .get_provider_by_id(target, AppType::Codex.as_str())
        .map_err(|e| e.to_string())?
        .ok_or("目标供应商已移除")?;
    let account = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for("codex_oauth"));
    if let Some(account) = account {
        let auth = crate::config::read_json_file(&crate::codex_config::get_codex_auth_path())
            .map_err(|_| "目标登录文件无法核对；不会向桌面发送继续")?;
        if !crate::codex_config::codex_live_auth_is_managed_chatgpt_login(&auth, &account) {
            return Err(
                "目标账号未同步到 Codex 当前登录文件，可能仍处于代理接管；未发送继续".into(),
            );
        }
    }
    Ok(())
}

async fn target_identity(
    app: &tauri::AppHandle,
    target: &str,
) -> Result<Option<DesktopIdentity>, String> {
    let state = app.state::<AppState>();
    let provider = state
        .db
        .get_provider_by_id(target, AppType::Codex.as_str())
        .map_err(|e| e.to_string())?
        .ok_or("目标供应商已移除")?;
    let Some(account) = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
    else {
        return Ok(None);
    };
    let bundle = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        state
            .codex_oauth_manager
            .get_valid_token_bundle_for_account(&account),
    )
    .await
    .map_err(|_| "目标账号身份查询超时；未关闭桌面")?
    .map_err(|e| format!("目标账号身份无法确认：{e}"))?;
    let principal = identity::identity_from_token(&bundle.access_token)?;
    if principal.account_id != bundle.chatgpt_account_id {
        return Err("目标托管账号的身份不一致；未关闭桌面".into());
    }
    Ok(Some(principal))
}

async fn confirm_reopened_identity(
    expected: &DesktopIdentity,
    guard: &Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
) -> Result<(), String> {
    let receipt =
        restart::last_identity_receipt().ok_or("没有本次新桌面的账号核验记录；未恢复聊天")?;
    confirm_identity_receipt(expected, &receipt, guard).await
}

async fn confirm_identity_receipt(
    expected: &DesktopIdentity,
    receipt: &restart::IdentityReceipt,
    guard: &Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        guard()?;
        let result = identity::runtime_identity(receipt.port, receipt.pid, receipt.birth).await;
        guard()?;
        match result {
            Ok(actual) if actual == *expected => return Ok(()),
            Ok(_) => return Err("新桌面实际登录的账号与目标账号不同；未恢复聊天".into()),
            Err(error) if tokio::time::Instant::now() >= deadline => {
                return Err(format!(
                    "账号已启用且桌面已重开，但桌面实际登录未确认：{error}；未恢复聊天"
                ));
            }
            // Readiness checks are read-only. Do not repeat activation or send
            // continuation while the result is unknown.
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
}

/// Automatic selection pauses the original tasks, confirms normal exit, then
/// calls original Enable exactly once before restarting the same desktop app.
pub async fn switch_desktop_account(
    app: tauri::AppHandle,
    expected_current_provider: String,
    target_provider: String,
    generation: u64,
) -> Result<String, String> {
    monitor::plan_is_current(&app, generation, &expected_current_provider)?;
    if target_provider.is_empty() || target_provider == expected_current_provider {
        return Err("目标账号为空或已是当前账号".into());
    }
    run_lifecycle(
        app,
        expected_current_provider,
        target_provider,
        generation,
        true,
        None,
    )
    .await
    .map(|result| result.1)
}

/// Manual Enable is already successful when this function runs. An optional
/// desktop/recovery error never rolls it back or blocks further manual Enable.
pub async fn restart_activated_account(
    app: tauri::AppHandle,
    target_provider: String,
    generation: u64,
) -> Result<bool, String> {
    run_lifecycle(
        app,
        target_provider.clone(),
        target_provider,
        generation,
        false,
        None,
    )
    .await
    .map(|result| result.0)
}

/// Switch once to the earliest reset account, verifying its desktop before
/// parking owned tickets. There is no continuation while quota is exhausted.
pub async fn switch_and_wait_for_reset(
    app: tauri::AppHandle,
    source: String,
    target: String,
    generation: u64,
    reset_at: i64,
) -> Result<String, String> {
    monitor::plan_is_current(&app, generation, &source)?;
    run_lifecycle(app, source, target, generation, true, Some(reset_at))
        .await
        .map(|result| result.1)
}

fn receipt_for_wait(record: &RecoveryRecord) -> Result<restart::IdentityReceipt, String> {
    if !record.desktop_restarted || !record.runtime_identity_confirmed {
        return Err("等待任务的桌面重开或账号身份未确认；未继续".into());
    }
    Ok(restart::IdentityReceipt {
        pid: record.reopened_pid.ok_or("额度等待缺少桌面进程标识")?,
        birth: record.reopened_birth.ok_or("额度等待缺少桌面启动时间")?,
        port: record.reopened_port.ok_or("额度等待缺少本次账号核验连接")?,
    })
}

fn matches_wait_baseline(
    current: &TaskSnapshot,
    baseline: &TaskSnapshot,
    saved: &PausedTask,
) -> bool {
    baseline.thread_id == saved.thread_id
        && baseline.turn_id.as_deref() == Some(saved.turn_id.as_str())
        && current.thread_id == saved.thread_id
        && current.turn_id.as_deref() == Some(saved.turn_id.as_str())
        && current.context == baseline.context
        && !current.waiting
        && !current.is_child
        && !current.ephemeral
        && match saved.origin {
            codex_desktop_session::RecoveryOrigin::QuotaExhausted => {
                // Loading the persisted turn reconstructs its duration. The
                // original timestamp established freshness before restart;
                // subsequent waiting checks use the confirmed reopened value.
                current.safely_quota_failed()
                    && saved.turn_ended_at_ms.is_some()
                    && baseline.turn_ended_at_ms.is_some()
                    && current.turn_ended_at_ms == baseline.turn_ended_at_ms
            }
            codex_desktop_session::RecoveryOrigin::CcSwitchPause => {
                current.safely_idle() && current.status.as_deref() == Some("interrupted")
            }
            codex_desktop_session::RecoveryOrigin::DesktopShutdown => {
                saved.is_desktop_shutdown_ticket()
                    && matches!(
                        current.runtime_status.as_str(),
                        "idle" | "notLoaded" | "systemError"
                    )
                    && (matches!(
                        current.status.as_deref(),
                        Some("interrupted" | "inProgress")
                    ) || (current.safely_quota_failed()
                        && baseline.safely_quota_failed()
                        // This turn was captured while running. Compare the
                        // confirmed reopened failure during a long reset wait,
                        // without expiring its owned ticket after 15 minutes.
                        && current.turn_ended_at_ms == baseline.turn_ended_at_ms))
                    && current.runtime_status == baseline.runtime_status
                    && current.status == baseline.status
            }
        }
}

/// A timer may enter here only after a fresh usable quota sample. Persisted
/// tickets are reconciled against the exact desktop and original task; this
/// never repeats closing, enabling or starting the desktop.
pub async fn resume_quota_reset_wait(
    app: tauri::AppHandle,
    wait: QuotaResetWait,
    generation: u64,
) -> Result<String, String> {
    restart::ensure_no_shutdown_pending()?;
    let _flow = FLOW_LOCK
        .try_lock()
        .map_err(|_| "另一桌面流程仍在处理；本次未排队继续原任务")?;
    monitor::plan_is_current(&app, generation, &wait.provider_id)?;
    let mut record = read_record(&wait_record_path(&wait.operation_id)?)?;
    if record.phase != "waiting-for-reset"
        || record.waiting_account_id.as_deref() != Some(&wait.account_id)
    {
        return Err("额度等待计划已由用户操作取代；未继续".into());
    }
    let provider = app
        .state::<AppState>()
        .db
        .get_provider_by_id(&wait.provider_id, AppType::Codex.as_str())
        .map_err(|e| e.to_string())?
        .ok_or("等待账号已移除")?;
    if provider
        .meta
        .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
        .as_deref()
        != Some(&wait.account_id)
    {
        record.phase = "cancelled".into();
        record.persist()?;
        return Err("等待账号绑定已改变；旧任务恢复计划已失效".into());
    }
    check_target_login(&app, &wait.provider_id)?;
    let guard_app = app.clone();
    let provider_id = wait.provider_id.clone();
    let guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync> =
        Arc::new(move || check_selection(&guard_app, generation, &provider_id));
    // A desktop that was not running has no tasks to continue or process to
    // launch. Finish the wait after quota is truly available.
    if record.paused_tasks.is_empty() {
        record.phase = "completed".into();
        record.persist()?;
        return Ok("等待账号额度已恢复；本次没有需恢复的原任务".into());
    }
    let receipt = receipt_for_wait(&record)?;
    if let Err(error) = restart::ensure_identity_desktop(&receipt) {
        record.phase = "cancelled".into();
        record.persist()?;
        return Err(format!(
            "等待期间桌面已关闭或重开，旧恢复计划已失效：{error}"
        ));
    }
    let expected = target_identity(&app, &wait.provider_id)
        .await?
        .ok_or("等待账号身份无法核对")?;
    confirm_identity_receipt(&expected, &receipt, &guard).await?;
    guard()?;
    monitor::retain_owned_recovery(generation)?;
    let mut eligible = Vec::new();
    for saved in &record.paused_tasks {
        guard()?;
        let mut verifier = connect_reopened_desktop(&guard).await?;
        verifier
            .begin_follow(&saved.thread_id)
            .await
            .map_err(|error| error.to_string())?;
        codex_desktop_session::open_original_chat(&saved.thread_id)
            .map_err(|error| error.to_string())?;
        restart::wait_for_navigation(&receipt, guard.as_ref()).await?;
        confirm_identity_receipt(&expected, &receipt, &guard).await?;
        let current = match verifier
            .wait_for_reopened_paused_chat(saved, || guard().is_ok())
            .await
        {
            Ok(current) => current,
            Err(error)
                if error.kind == codex_desktop_session::SessionErrorKind::StateChanged
                    && !error.mutation_may_have_been_sent =>
            {
                record.abandoned_tasks.push(saved.thread_id.clone());
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        let baseline = record
            .wait_baselines
            .iter()
            .find(|task| task.thread_id == saved.thread_id)
            .ok_or("额度等待任务缺少保存的基线；未继续")?;
        if matches_wait_baseline(&current, baseline, saved) {
            eligible.push(saved.clone());
        } else {
            // Clear the exact old ticket without stopping or overwriting the
            // user's changed/completed/approval task. Other eligible tickets
            // may still resume; this is not an uncertain mutating request.
            record.abandoned_tasks.push(saved.thread_id.clone());
        }
    }
    record.paused_tasks = eligible;
    // This has left the reset-wait phase. Keep the baselines and abandoned
    // tickets for audit, but allow late confirmation of the ensuing starts.
    record.wait_until = None;
    record.waiting_account_id = None;
    // Refresh only the generation. Original operation UUID and task tickets
    // remain unchanged across hours of waiting and CC Switch restarts.
    record.generation = generation;
    record.loaded = false;
    record.phase = "quota-reset-confirmed".into();
    record.persist()?;
    if let Err(error) = resume_saved_tasks(
        &app,
        generation,
        &mut record,
        guard,
        Some(&expected),
        receipt,
        false,
    )
    .await
    {
        if record.resume_intents.is_empty() {
            record.phase = "cancelled".into();
            record.persist()?;
        }
        return Err(error);
    }
    record.phase = "completed".into();
    record.persist()?;
    Ok(format!(
        "额度已恢复并核对实际桌面账号；继续 {} 个原任务，{} 个被用户改变的任务保持原状",
        record.resumed_tasks.len(),
        record.abandoned_tasks.len()
    ))
}

async fn run_lifecycle(
    app: tauri::AppHandle,
    source: String,
    target: String,
    generation: u64,
    automatic: bool,
    wait_until: Option<i64>,
) -> Result<(bool, String), String> {
    restart::ensure_no_shutdown_pending()?;
    let _flow = FLOW_LOCK
        .try_lock()
        .map_err(|_| "另一桌面流程仍在处理；本次未排队关闭或重开")?;
    check_selection(&app, generation, &source)?;
    if !automatic {
        check_target_login(&app, &target)?;
    }
    if !automatic {
        supersede_old_recovery_records()?;
    }
    let operation_id = monitor::get_status(&app)
        .operation_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut record = RecoveryRecord {
        operation_id,
        generation,
        source_provider_id: source.clone(),
        target_provider_id: target.clone(),
        phase: "checking".into(),
        close_checkpoint: None,
        desktop_restarted: false,
        runtime_identity_confirmed: false,
        reopened_pid: None,
        reopened_birth: None,
        reopened_port: None,
        wait_until,
        waiting_account_id: app
            .state::<AppState>()
            .db
            .get_provider_by_id(&target, AppType::Codex.as_str())
            .map_err(|e| e.to_string())?
            .and_then(|provider| {
                provider
                    .meta
                    .and_then(|meta| meta.managed_account_id_for("codex_oauth"))
            }),
        loaded: false,
        wait_baselines: Vec::new(),
        abandoned_tasks: Vec::new(),
        planned_tasks: Vec::new(),
        paused_tasks: Vec::new(),
        resume_intents: Vec::new(),
        settings_restore_intents: Vec::new(),
        settings_restored_tasks: Vec::new(),
        resumed_tasks: Vec::new(),
        resume_confirmations: Vec::new(),
        resume_not_sent_tasks: Vec::new(),
    };
    monitor::lifecycle_status(
        &app,
        generation,
        "preflight",
        "正在核对目标账号、桌面进程和原任务",
    );
    let desktop_receipt = restart::desktop_receipt()?;
    ensure_desktop_task_coverage(
        desktop_receipt
            .as_ref()
            .map_or(0, restart::DesktopReceipt::root_count),
    )?;
    let desktop_running = desktop_receipt.is_some();
    if desktop_running {
        crate::startup_isolation::ensure_independent_for_desktop_shutdown()?;
    }
    let mut ignored_quota_failures = 0usize;
    let mut ignored_restart_tasks = 0usize;
    let mut original_session = None;
    // A new explicit Enable may reopen a desktop left closed by a cancelled
    // flow. Its target identity must be checked even when none is running now.
    let expected_identity = target_identity(&app, &target).await?;
    check_selection(&app, generation, &source)?;
    if desktop_running {
        monitor::lifecycle_status(
            &app,
            generation,
            "preflight",
            "正在保存运行任务清单，即将正常退出 Codex",
        );
        let snapshot_guard = || {
            check_selection(&app, generation, &source)?;
            restart::ensure_same_desktop(&desktop_receipt)
        };
        let snapshot_started = std::time::Instant::now();
        let (session, inventory) = read_original_desktop_metadata(&snapshot_guard)
            .await
            .map_err(|error| format!("原桌面任务清单未确认：{error}；未关闭桌面"))?;
        ignored_restart_tasks = inventory.blocked.len() + inventory.unresolved_thread_ids.len();
        log::info!(
            "Codex restart single snapshot: running={}, quota_failed={}, excluded_approval_or_unknown={}, elapsed_ms={}",
            inventory.running.len(), inventory.quota_failed.len(), ignored_restart_tasks,
            snapshot_started.elapsed().as_millis()
        );
        let cancelled_tickets = if automatic {
            cancelled_quota_wait_tickets()?
        } else {
            Vec::new()
        };
        let recent_quota_failures: Vec<_> = inventory
            .quota_failed
            .iter()
            .filter(|task| task.recent_quota_failure())
            .filter(|task| {
                !cancelled_tickets.iter().any(|saved| {
                    task.thread_id == saved.thread_id
                        && task.turn_id.as_deref() == Some(saved.turn_id.as_str())
                })
            })
            .cloned()
            .collect();
        ignored_quota_failures = inventory.quota_failed.len() - recent_quota_failures.len();
        if ignored_quota_failures > 0 {
            log::info!("Codex recovery left {ignored_quota_failures} old or time-unconfirmed quota failures stopped");
        }
        if (!inventory.running.is_empty() || !recent_quota_failures.is_empty())
            && expected_identity.is_none()
        {
            return Err(
                "当前供应商没有可核对的托管登录身份；账号启用不受影响，未暂停或自动恢复任务".into(),
            );
        }
        // One metadata snapshot owns the restart whitelist. Closing the desktop
        // interrupts running turns normally; no individual pause request or
        // additional inventory scan is needed. Unrelated approval/unknown chats
        // are excluded rather than blocking account activation.
        for task in inventory.running.iter().chain(recent_quota_failures.iter()) {
            snapshot_guard()?;
            record.planned_tasks.push(task.clone());
            match session.capture_restart_task(task, &record.operation_id) {
                Ok(ticket) => {
                    record.paused_tasks.push(ticket);
                }
                Err(error) => {
                    log::warn!("Codex restart recovery excluded a task: {error}");
                    record.abandoned_tasks.push(task.thread_id.clone());
                    ignored_restart_tasks += 1;
                }
            }
        }
        record.phase = "restart-tasks-captured".into();
        record.persist()?;
        original_session = Some(session);
    }
    if wait_until.is_some() && source == target && record.paused_tasks.is_empty() {
        // A cancelled wait with no owned work must not cause a fresh restart
        // of the same exhausted account on every later monitoring tick.
        record.phase = "waiting-for-reset".into();
        record.persist()?;
        check_selection(&app, generation, &source)?;
        return Ok((
            false,
            "当前账号最早重置；没有本次需恢复的任务，等待额度恢复，未重启桌面".into(),
        ));
    }
    let manager_identity = crate::startup_isolation::current_process_identity();
    record.close_checkpoint = Some(ClosingCheckpoint {
        schema_version: 1,
        intent_at: chrono::Utc::now().timestamp_millis(),
        manager_pid: std::process::id(),
        manager_birth: manager_identity
            .filter(|(pid, _)| *pid == std::process::id())
            .map(|(_, birth)| birth),
        original_desktop: desktop_receipt.clone(),
    });
    record.phase = "close-intent".into();
    record.persist()?;
    monitor::lifecycle_status(&app, generation, "closing", "正在正常关闭 Codex 并确认退出");
    let activated = Arc::new(AtomicBool::new(!automatic));
    let guard_app = app.clone();
    let guard_source = source.clone();
    let guard_target = target.clone();
    let guard_activated = activated.clone();
    let guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync> = Arc::new(move || {
        let expected = if guard_activated.load(Ordering::SeqCst) {
            &guard_target
        } else {
            &guard_source
        };
        check_selection(&guard_app, generation, expected)
    });
    let final_guard = guard.clone();
    let final_receipt = desktop_receipt.clone();
    let final_paused = record.paused_tasks.clone();
    let final_record = serde_json::to_value(&record).map_err(|_| "无法保存退出前任务清单")?;
    let excluded_before_quit = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let final_excluded = excluded_before_quit.clone();
    let runtime = tokio::runtime::Handle::current();
    // Run from the native blocking task immediately before RmShutdown, after
    // Windows has prepared and verified its exact process resource list.
    let before_shutdown: Box<dyn FnOnce() -> Result<(), String> + Send> = Box::new(move || {
        final_guard()?;
        restart::ensure_same_desktop(&final_receipt)?;
        if let Some(mut session) = original_session {
            let excluded = runtime
                .block_on(session.observe_restart_candidates_before_quit(&final_paused))
                .map_err(|error| format!("正常退出前原任务连接未确认：{error}；未关闭桌面"))?;
            final_guard()?;
            restart::ensure_same_desktop(&final_receipt)?;
            exclude_restart_candidates(
                &crate::config::get_app_config_dir().join("codex-desktop-recovery"),
                &final_record,
                &excluded,
            )?;
            *final_excluded
                .lock()
                .map_err(|_| "无法同步退出前任务清单")? = excluded;
        }
        final_guard()?;
        restart::ensure_same_desktop(&final_receipt)
    });
    let restart_result = if automatic {
        let enable_app = app.clone();
        let enable_target = target.clone();
        let enable_source = source.clone();
        let enable_activated = activated.clone();
        restart::restart_bound(
            desktop_receipt.clone(),
            guard.clone(),
            Box::new(move || {
                check_selection(&enable_app, generation, &enable_source)?;
                monitor::lifecycle_status(
                    &enable_app,
                    generation,
                    "enabling",
                    "原桌面已退出，正在通过原版服务启用目标账号",
                );
                monitor::activate_reserved_account(
                    &enable_app.state::<AppState>(),
                    &enable_target,
                    generation,
                )
                .map_err(|e| e.to_string())?;
                enable_activated.store(true, Ordering::SeqCst);
                // Publish the committed original Enable result even while the
                // renderer is hidden. This only refreshes the view/tray; the
                // native lifecycle does not depend on event delivery.
                let _ = enable_app.emit(
                    "provider-switched",
                    serde_json::json!({
                        "appType": "codex", "providerId": enable_target,
                    }),
                );
                if let Ok(menu) =
                    crate::tray::create_tray_menu(&enable_app, &enable_app.state::<AppState>())
                {
                    if let Some(tray) = enable_app.tray_by_id(crate::tray::TRAY_ID) {
                        let _ = tray.set_menu(Some(menu));
                    }
                }
                check_target_login(&enable_app, &enable_target)?;
                monitor::lifecycle_status(
                    &enable_app,
                    generation,
                    "starting",
                    "正在从 Windows 原应用入口重开 Codex",
                );
                Ok(())
            }),
            before_shutdown,
            false,
        )
        .await
    } else {
        restart::restart_bound(
            desktop_receipt.clone(),
            guard.clone(),
            Box::new(|| Ok(())),
            before_shutdown,
            true,
        )
        .await
    };
    // The native callback atomically excluded changed tasks from the journal.
    // Mirror that committed whitelist even if normal quit subsequently failed,
    // so this record's Drop cannot restore an obsolete recovery ticket.
    let excluded = excluded_before_quit
        .lock()
        .map_err(|_| "无法同步退出前任务清单")?
        .clone();
    apply_restart_exclusions(&mut record, &excluded);
    ignored_restart_tasks += excluded.len();
    let restarted = restart_result?;
    record.desktop_restarted = restarted;
    if let Some(receipt) = restart::last_identity_receipt() {
        record.reopened_pid = Some(receipt.pid);
        record.reopened_birth = Some(receipt.birth);
        record.reopened_port = Some(receipt.port);
    }
    if restarted {
        record.phase = "desktop-restarted".into();
        record.persist()?;
    }
    guard()?;
    check_target_login(&app, &target)?;
    monitor::lifecycle_status(
        &app,
        generation,
        "verifying",
        "正在核对新桌面实际登录的账号",
    );
    if restarted {
        if let Some(expected) = expected_identity.as_ref() {
            confirm_reopened_identity(expected, &guard).await?;
            record.runtime_identity_confirmed = true;
        }
    }
    record.phase = if restarted {
        "reopened"
    } else {
        "activated-without-running-desktop"
    }
    .into();
    record.persist()?;
    if wait_until.is_some() {
        guard()?;
        if !record.paused_tasks.is_empty() {
            let receipt = restart::last_identity_receipt().ok_or("额度等待缺少本次新桌面记录")?;
            for saved in record.paused_tasks.clone() {
                guard()?;
                let mut session = connect_reopened_desktop(&guard).await?;
                session
                    .begin_follow(&saved.thread_id)
                    .await
                    .map_err(|e| e.to_string())?;
                codex_desktop_session::open_original_chat(&saved.thread_id)
                    .map_err(|e| e.to_string())?;
                restart::wait_for_navigation(&receipt, guard.as_ref()).await?;
                if let Some(expected) = expected_identity.as_ref() {
                    confirm_identity_receipt(expected, &receipt, &guard).await?;
                }
                let baseline = session
                    .wait_for_reopened_paused_chat(&saved, || guard().is_ok())
                    .await
                    .map_err(|e| e.to_string())?;
                record.wait_baselines.push(baseline);
            }
        }
        record.phase = "waiting-for-reset".into();
        record.persist()?;
        if let Err(error) = guard() {
            record.phase = "cancelled".into();
            record.persist()?;
            return Err(error);
        }
        return Ok((
            restarted,
            format!(
                "已选择最早 5 小时重置的账号并核对桌面；保存 {} 个原任务等待重置，期间不发送继续",
                record.paused_tasks.len()
            ),
        ));
    }
    if restarted && !record.paused_tasks.is_empty() {
        let receipt = restart::last_identity_receipt().ok_or("本次重开记录已失效；未恢复原聊天")?;
        resume_saved_tasks(
            &app,
            generation,
            &mut record,
            guard.clone(),
            expected_identity.as_ref(),
            receipt,
            false,
        )
        .await?;
    }
    record.phase = "completed".into();
    record.persist()?;
    let mut message = if restarted {
        format!(
            "目标账号已启用，Codex 已正常重开；恢复本次记录的 {} 个原任务",
            record.resumed_tasks.len()
        )
    } else {
        "目标账号已启用；Codex 原本未运行，未额外启动桌面".into()
    };
    if ignored_quota_failures > 0 {
        message.push_str(&format!(
            "；{} 个较早或结束时间未确认的额度失败任务未自动继续",
            ignored_quota_failures
        ));
    }
    if ignored_restart_tasks > 0 {
        message.push_str(&format!(
            "；{} 个审批、状态未确认或退出前已改变的任务未自动继续",
            ignored_restart_tasks
        ));
    }
    monitor::lifecycle_status(&app, generation, "completed", &message);
    Ok((restarted, message))
}

async fn resume_saved_tasks(
    app: &tauri::AppHandle,
    generation: u64,
    record: &mut RecoveryRecord,
    guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    expected_identity: Option<&DesktopIdentity>,
    receipt: restart::IdentityReceipt,
    retry_unsent: bool,
) -> Result<(), String> {
    let navigation_receipt = receipt.clone();
    let previous_guard = guard.clone();
    let resume_guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync> = Arc::new(move || {
        previous_guard()?;
        restart::ensure_identity_desktop(&receipt)
    });
    monitor::lifecycle_status(
        app,
        generation,
        "resuming",
        "目标账号已生效；正在分别加载并恢复原聊天，保留原模型和权限",
    );
    let tasks = record.paused_tasks.clone();
    let resumed = record.resumed_tasks.clone();
    let intents = record.resume_intents.clone();
    let baselines = record.wait_baselines.clone();
    let operation_id = record.operation_id.clone();
    let prepare_guard = resume_guard.clone();
    let prepare_receipt = navigation_receipt.clone();
    let mut context = SavedTaskRecovery {
        record,
        resume_guard: resume_guard.clone(),
        expected_identity,
        navigation_receipt,
    };
    recover_tasks_as_ready(
        &tasks,
        &mut context,
        resume_guard.as_ref(),
        move |task| {
            let already_resumed = resumed.contains(&task.thread_id);
            let already_sent = intents.contains(&task.thread_id);
            let baseline = baselines
                .iter()
                .find(|saved| saved.thread_id == task.thread_id)
                .cloned();
            let has_baselines = !baselines.is_empty();
            let guard = prepare_guard.clone();
            let receipt = prepare_receipt.clone();
            let operation = operation_id.clone();
            async move {
                prepare_saved_task(
                    &task,
                    guard,
                    receipt,
                    retry_unsent,
                    already_resumed,
                    already_sent,
                    baseline.as_ref(),
                    has_baselines,
                    &operation,
                )
                .await
            }
        },
        |context, paused, prepared| {
            Box::pin(async move {
                let result = resume_prepared_task(context, paused, prepared).await;
                if result.is_err() {
                    context.record.phase = "resume-outcome-needs-review".into();
                    context.record.persist()?;
                }
                result
            })
        },
    )
    .await
}

struct SavedTaskRecovery<'a> {
    record: &'a mut RecoveryRecord,
    resume_guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    expected_identity: Option<&'a DesktopIdentity>,
    navigation_receipt: restart::IdentityReceipt,
}

enum PreparedTaskRecovery {
    AlreadyResumed,
    Reconciled(ResumeConfirmation),
    Abandoned,
    Ready(DesktopSession),
}

/// Owner loading is independent and shares one deadline. Whichever original
/// chat becomes ready first is resumed first. Journal writes and start requests
/// remain sequential, and a persisted intent is only reconciled, never replayed.
async fn recover_tasks_as_ready<C, Prepared, Prepare, PrepareFuture, Recover>(
    tasks: &[PausedTask],
    context: &mut C,
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
    mut prepare: Prepare,
    mut recover: Recover,
) -> Result<(), String>
where
    Prepared: Send + 'static,
    Prepare: FnMut(PausedTask) -> PrepareFuture,
    PrepareFuture: std::future::Future<Output = Result<Prepared, String>>,
    Recover: for<'a> FnMut(
        &'a mut C,
        &'a PausedTask,
        Prepared,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>,
    >,
{
    guard()?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut preparations = FuturesUnordered::new();
    for (index, task) in tasks.iter().cloned().enumerate() {
        let preparation = prepare(task.clone());
        preparations.push(async move {
            let result = tokio::time::timeout_at(deadline, preparation)
                .await
                .unwrap_or_else(|_| {
                    Err("目标账号已生效；原聊天 90 秒内尚未加载，未发送继续".into())
                });
            (index, task, result)
        });
    }
    let mut failures = Vec::new();
    while let Some((index, task, prepared)) = preparations.next().await {
        guard()?;
        let result = match prepared {
            Ok(prepared) => recover(context, &task, prepared).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            guard()?;
            failures.push(format!("第 {} 条：{error}", index + 1));
        }
    }
    guard()?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "目标账号已生效；原聊天恢复确认 {}/{} 条；{}；已分别核对其余聊天，未重复发送继续",
            tasks.len() - failures.len(),
            tasks.len(),
            failures.join("；")
        ))
    }
}

async fn prepare_saved_task(
    paused: &PausedTask,
    resume_guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    navigation_receipt: restart::IdentityReceipt,
    retry_unsent: bool,
    already_resumed: bool,
    already_sent: bool,
    wait_baseline: Option<&TaskSnapshot>,
    has_wait_baselines: bool,
    operation_id: &str,
) -> Result<PreparedTaskRecovery, String> {
    resume_guard()?;
    if already_resumed {
        return Ok(PreparedTaskRecovery::AlreadyResumed);
    }
    let mut session = connect_reopened_desktop(&resume_guard).await?;
    if already_sent {
        let confirmation = session
            .reconcile_resume(paused, operation_id)
            .await
            .map_err(|error| format!("正在核实已发送的原任务恢复：{error}"))?;
        return Ok(PreparedTaskRecovery::Reconciled(confirmation));
    }
    session
        .begin_follow(&paused.thread_id)
        .await
        .map_err(|error| error.to_string())?;
    resume_guard()?;
    codex_desktop_session::open_original_chat(&paused.thread_id)
        .map_err(|error| error.to_string())?;
    restart::wait_for_navigation(&navigation_receipt, resume_guard.as_ref()).await?;
    let actual = match session
        .wait_for_reopened_paused_chat(paused, || resume_guard().is_ok())
        .await
    {
        Ok(actual) => actual,
        Err(error) => {
            if retry_unsent && !error.mutation_may_have_been_sent {
                if let Ok(current) = session.fresh_snapshot(&paused.thread_id).await {
                    if saved_task_was_changed(paused, &current) {
                        return Ok(PreparedTaskRecovery::Abandoned);
                    }
                }
            }
            return Err(error.to_string());
        }
    };
    if retry_unsent {
        if let Err(error) = session.can_retry_saved_task(&actual, paused) {
            if error.kind == SessionErrorKind::StateChanged {
                return Ok(PreparedTaskRecovery::Abandoned);
            }
            return Err(error.to_string());
        }
    }
    if has_wait_baselines {
        let baseline = wait_baseline.ok_or("额度等待任务缺少原设置基线；未继续")?;
        if actual.context != baseline.context {
            return Err("等待期间原任务的项目、模型或权限已被用户修改；未覆盖用户设置".into());
        }
    }
    Ok(PreparedTaskRecovery::Ready(session))
}

async fn resume_prepared_task(
    context: &mut SavedTaskRecovery<'_>,
    paused: &PausedTask,
    prepared: PreparedTaskRecovery,
) -> Result<(), String> {
    let record = &mut *context.record;
    let resume_guard = context.resume_guard.clone();
    resume_guard()?;
    if let Some(expected) = context.expected_identity {
        confirm_identity_receipt(expected, &context.navigation_receipt, &resume_guard).await?;
    }
    let mut session = match prepared {
        PreparedTaskRecovery::AlreadyResumed => return Ok(()),
        PreparedTaskRecovery::Reconciled(confirmation) => {
            return save_resume_confirmation(record, confirmation);
        }
        PreparedTaskRecovery::Abandoned => return abandon_saved_task(record, &paused.thread_id),
        PreparedTaskRecovery::Ready(session) => session,
    };
    record.phase = "resume-intent".into();
    if record.resume_intents.contains(&paused.thread_id) {
        return Err("此聊天已有本次恢复意图；未重复发送继续".into());
    }
    record.resume_intents.push(paused.thread_id.clone());
    record.persist()?;
    let resume = session
        .resume_and_confirm(paused, &record.operation_id, || resume_guard().is_ok())
        .await;
    let confirmation = match resume {
        Ok(confirmation) => confirmation,
        Err(error) => {
            if withdraw_proven_unsent_intent(record, &paused.thread_id, &error) {
                record.phase = "resume-not-sent".into();
                record.persist()?;
                return Err(format!(
                    "原任务恢复请求尚未发送：{error}；后台将重新核对原轮次后处理"
                ));
            }
            match session.reconcile_resume(paused, &record.operation_id).await {
                Ok(confirmation) => confirmation,
                Err(_) => {
                    record.phase = "resume-outcome-needs-review".into();
                    record.persist()?;
                    return Err(format!(
                        "账号已启用且桌面已重开，但原聊天恢复未确认：{error}；未重复继续"
                    ));
                }
            }
        }
    };
    save_resume_confirmation(record, confirmation)
}

fn save_resume_confirmation(
    record: &mut RecoveryRecord,
    confirmation: ResumeConfirmation,
) -> Result<(), String> {
    let id = confirmation.thread_id.clone();
    record
        .resume_confirmations
        .retain(|saved| saved.thread_id != id);
    record.resume_confirmations.push(confirmation);
    // The single original resume request resolves these settings as
    // part of starting its new turn. Record them only after the live
    // new turn's model, project and permission semantics are confirmed.
    if !record.settings_restored_tasks.contains(&id) {
        record.settings_restored_tasks.push(id.clone());
    }
    if !record.resumed_tasks.contains(&id) {
        record.resumed_tasks.push(id);
    }
    record.phase = "resumed".into();
    record.persist()?;
    Ok(())
}

fn abandon_saved_task(record: &mut RecoveryRecord, id: &str) -> Result<(), String> {
    record.paused_tasks.retain(|task| task.thread_id != id);
    if !record.abandoned_tasks.iter().any(|task| task == id) {
        record.abandoned_tasks.push(id.into());
    }
    record.persist()
}

fn withdraw_proven_unsent_intent(
    record: &mut RecoveryRecord,
    id: &str,
    error: &SessionError,
) -> bool {
    if error.mutation_may_have_been_sent {
        return false;
    }
    record.resume_intents.retain(|saved| saved != id);
    if !record.resume_not_sent_tasks.iter().any(|saved| saved == id) {
        record.resume_not_sent_tasks.push(id.into());
    }
    true
}

fn apply_restart_exclusions(record: &mut RecoveryRecord, excluded: &[String]) {
    record
        .paused_tasks
        .retain(|saved| !excluded.contains(&saved.thread_id));
    for id in excluded {
        if !record.abandoned_tasks.contains(id) {
            record.abandoned_tasks.push(id.clone());
        }
    }
}

/// Commit only the exact close-intent document observed before the native
/// callback. Cancellation or another writer wins; a stale callback must never
/// overwrite it. No account/monitor guard runs under RECORD_LOCK.
fn exclude_restart_candidates(
    directory: &std::path::Path,
    expected: &serde_json::Value,
    excluded: &[String],
) -> Result<(), String> {
    use std::io::Write;
    let _lock = RECORD_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let operation = expected["operationId"]
        .as_str()
        .ok_or("退出记录缺少操作标识")?;
    let id = uuid::Uuid::parse_str(operation).map_err(|_| "退出记录操作标识无效")?;
    let path = directory.join(format!("{id}.json"));
    let actual = read_recovery_value(&path)?;
    if actual != *expected || actual["phase"] != "close-intent" {
        return Err("退出前本次恢复计划已经改变或取消；未覆盖用户选择".into());
    }
    if excluded.is_empty() {
        return Ok(());
    }
    let mut record: RecoveryRecord =
        serde_json::from_value(actual).map_err(|_| "退出记录无法解码")?;
    apply_restart_exclusions(&mut record, excluded);
    let mut file =
        tempfile::NamedTempFile::new_in(directory).map_err(|_| "无法保存退出前任务变化")?;
    serde_json::to_writer(&mut file, &record).map_err(|_| "退出前任务变化编码失败")?;
    file.flush().map_err(|_| "无法刷新退出前任务变化")?;
    file.as_file()
        .sync_all()
        .map_err(|_| "无法同步退出前任务变化")?;
    file.persist(path).map_err(|_| "无法提交退出前任务变化")?;
    Ok(())
}

const ORIGINAL_SNAPSHOT_ATTEMPTS: usize = 3;
const ORIGINAL_SNAPSHOT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
const READONLY_GUARD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Only read-only connection + task inventory attempts belong here. A broken
/// connection discards its partial snapshot; a new attempt discovers all IDs
/// again. Never pass pause, shutdown, activation or continuation to this helper.
async fn guarded_readonly_snapshot<T, Read, ReadFuture>(
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
    retry_delay: std::time::Duration,
    mut read: Read,
) -> Result<T, String>
where
    Read: FnMut() -> ReadFuture,
    ReadFuture: std::future::Future<Output = Result<T, SessionError>>,
{
    for attempt in 1..=ORIGINAL_SNAPSHOT_ATTEMPTS {
        guard()?;
        let reading = read();
        tokio::pin!(reading);
        let result = loop {
            tokio::select! {
                result = &mut reading => {
                    guard()?;
                    break result;
                }
                _ = tokio::time::sleep(READONLY_GUARD_INTERVAL) => guard()?,
            }
        };
        match result {
            Ok(value) => return Ok(value),
            Err(error)
                if error.kind == SessionErrorKind::Unavailable
                    && !error.mutation_may_have_been_sent
                    && attempt < ORIGINAL_SNAPSHOT_ATTEMPTS =>
            {
                log::info!(
                    "Codex 原桌面只读盘点连接暂时断开；重新连接并完整核对（第 {attempt} 次）"
                );
                tokio::time::sleep(retry_delay).await;
            }
            Err(error) => {
                return Err(format!(
                    "原桌面任务只读核对失败（尝试 {attempt}/{ORIGINAL_SNAPSHOT_ATTEMPTS} 次）：{error}；未重放暂停、关闭、换号或继续操作"
                ));
            }
        }
    }
    unreachable!("bounded read-only snapshot attempts always return")
}

async fn snapshot_original_desktop(
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
) -> Result<(DesktopSession, TaskInventory), String> {
    guarded_readonly_snapshot(guard, ORIGINAL_SNAPSHOT_RETRY_DELAY, || {
        read_original_desktop_metadata(guard)
    })
    .await
}

async fn read_original_desktop_metadata(
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
) -> Result<(DesktopSession, TaskInventory), SessionError> {
    let guard_error = |message| SessionError {
        kind: SessionErrorKind::Cancelled,
        message,
        mutation_may_have_been_sent: false,
    };
    guard().map_err(guard_error)?;
    let ids =
        codex_desktop_session::discover_thread_ids(&crate::codex_config::get_codex_config_dir())?;
    guard().map_err(guard_error)?;
    let mut session = DesktopSession::connect().await?;
    session.set_trusted_codex_home(crate::codex_config::get_codex_config_dir());
    guard().map_err(guard_error)?;
    let mut inventory = session.snapshot_safe_running_tasks(&ids).await?;
    guard().map_err(guard_error)?;
    inventory.candidate_coverage_complete = true;
    Ok((session, inventory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn synthetic_read_error(kind: SessionErrorKind, mutation_sent: bool) -> SessionError {
        SessionError {
            kind,
            message: "synthetic coordinator failure".into(),
            mutation_may_have_been_sent: mutation_sent,
        }
    }

    #[tokio::test]
    async fn readonly_disconnect_discards_old_snapshot_and_accepts_fresh_full_inventory() {
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let result = guarded_readonly_snapshot(&|| Ok(()), std::time::Duration::ZERO, || {
            let attempt = reads.fetch_add(1, Ordering::SeqCst);
            std::future::ready(if attempt == 0 {
                Err(synthetic_read_error(SessionErrorKind::Unavailable, false))
            } else {
                Ok(vec!["original-chat", "chat-created-during-reconnect"])
            })
        })
        .await
        .unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(
            result,
            vec!["original-chat", "chat-created-during-reconnect"]
        );
    }

    #[tokio::test]
    async fn readonly_coordinator_disconnect_stops_after_three_attempts() {
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let result: Result<(), _> =
            guarded_readonly_snapshot(&|| Ok(()), std::time::Duration::ZERO, || {
                reads.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err(synthetic_read_error(
                    SessionErrorKind::Unavailable,
                    false,
                )))
            })
            .await;
        assert_eq!(reads.load(Ordering::SeqCst), 3);
        assert!(result.unwrap_err().contains("尝试 3/3 次"));
    }

    #[tokio::test]
    async fn readonly_inventory_never_retries_blocked_changed_or_uncertain_results() {
        for kind in [
            SessionErrorKind::Blocked,
            SessionErrorKind::StateChanged,
            SessionErrorKind::Cancelled,
            SessionErrorKind::Unknown,
            SessionErrorKind::Incompatible,
            SessionErrorKind::OutcomeUnknown,
        ] {
            let reads = std::sync::atomic::AtomicUsize::new(0);
            let result: Result<(), _> =
                guarded_readonly_snapshot(&|| Ok(()), std::time::Duration::ZERO, || {
                    reads.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Err(synthetic_read_error(kind, false)))
                })
                .await;
            assert!(result.is_err());
            assert_eq!(reads.load(Ordering::SeqCst), 1, "{kind:?}");
        }
    }

    #[tokio::test]
    async fn readonly_inventory_never_replays_a_possibly_sent_mutation() {
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let result: Result<(), _> =
            guarded_readonly_snapshot(&|| Ok(()), std::time::Duration::ZERO, || {
                reads.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err(synthetic_read_error(
                    SessionErrorKind::Unavailable,
                    true,
                )))
            })
            .await;
        assert!(result.is_err());
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn readonly_reconnect_cancelled_after_failure_does_not_open_another_connection() {
        let cancelled = AtomicBool::new(false);
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let guard = || {
            if cancelled.load(Ordering::SeqCst) {
                Err("synthetic operation cancelled".into())
            } else {
                Ok(())
            }
        };
        let result: Result<(), _> =
            guarded_readonly_snapshot(&guard, std::time::Duration::ZERO, || {
                reads.fetch_add(1, Ordering::SeqCst);
                cancelled.store(true, Ordering::SeqCst);
                std::future::ready(Err(synthetic_read_error(
                    SessionErrorKind::Unavailable,
                    false,
                )))
            })
            .await;
        assert_eq!(result.unwrap_err(), "synthetic operation cancelled");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn readonly_source_change_before_connection_stops_without_reading() {
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let result: Result<(), _> = guarded_readonly_snapshot(
            &|| Err("synthetic current provider changed".into()),
            std::time::Duration::ZERO,
            || {
                reads.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(()))
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), "synthetic current provider changed");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn readonly_original_desktop_change_rejects_even_a_successful_snapshot() {
        let same_desktop = AtomicBool::new(true);
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let guard = || {
            if same_desktop.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("synthetic desktop receipt changed".into())
            }
        };
        let result = guarded_readonly_snapshot(&guard, std::time::Duration::ZERO, || {
            reads.fetch_add(1, Ordering::SeqCst);
            same_desktop.store(false, Ordering::SeqCst);
            std::future::ready(Ok("old desktop snapshot"))
        })
        .await;
        assert_eq!(result.unwrap_err(), "synthetic desktop receipt changed");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn readonly_pending_read_is_cancelled_without_waiting_for_request_timeout() {
        let cancelled = AtomicBool::new(false);
        let guard = || {
            if cancelled.load(Ordering::SeqCst) {
                Err("synthetic operation cancelled".into())
            } else {
                Ok(())
            }
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            guarded_readonly_snapshot(&guard, std::time::Duration::ZERO, || async {
                cancelled.store(true, Ordering::SeqCst);
                std::future::pending::<Result<(), SessionError>>().await
            }),
        )
        .await
        .expect("guard should cancel a pending read before its own timeout");
        assert_eq!(result.unwrap_err(), "synthetic operation cancelled");
    }

    #[tokio::test]
    async fn readonly_unresolved_inventory_is_returned_once_for_existing_safety_checks() {
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let result = guarded_readonly_snapshot(&|| Ok(()), std::time::Duration::ZERO, || {
            reads.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(TaskInventory {
                candidate_coverage_complete: true,
                unresolved_thread_ids: vec!["unresolved-chat".into()],
                ..Default::default()
            }))
        })
        .await
        .unwrap();
        assert!(!result.safe_to_restart());
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn explicit_cancellation_clears_only_captured_uncertain_records_without_replay() {
        let directory = tempfile::tempdir().unwrap();
        let old_path = directory
            .path()
            .join("11111111-1111-4111-8111-111111111111.json");
        let completed_path = directory
            .path()
            .join("22222222-2222-4222-8222-222222222222.json");
        let newer_path = directory
            .path()
            .join("33333333-3333-4333-8333-333333333333.json");
        let write = |path: &std::path::Path, id: &str, phase: &str| {
            std::fs::write(
                path,
                json!({"operationId":id,"phase":phase,
                "pausedTasks":[{"threadId":"original"}],"resumeIntents":["original"]})
                .to_string(),
            )
            .unwrap();
        };
        write(
            &old_path,
            "11111111-1111-4111-8111-111111111111",
            "needs-review",
        );
        write(
            &completed_path,
            "22222222-2222-4222-8222-222222222222",
            "completed",
        );
        let captured = vec![old_path.clone(), completed_path.clone()];
        write(
            &newer_path,
            "33333333-3333-4333-8333-333333333333",
            "resume-intent",
        );
        let completed_before = std::fs::read(&completed_path).unwrap();
        let newer_before = std::fs::read(&newer_path).unwrap();
        cancel_captured_recovery_records(&captured).unwrap();
        let old: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&old_path).unwrap()).unwrap();
        assert_eq!(old["phase"], "cancelled");
        assert_eq!(old["resumeIntents"], json!(["original"]));
        assert_eq!(std::fs::read(&completed_path).unwrap(), completed_before);
        assert_eq!(std::fs::read(&newer_path).unwrap(), newer_before);
        cancel_captured_recovery_records(&captured).unwrap();
    }

    fn original_task() -> TaskSnapshot {
        serde_json::from_value(json!({
            "threadId":"019ed000-1111-7111-8111-111111111111", "owner":"owner",
            "turnId":"turn-A", "status":"interrupted", "runtimeStatus":"idle",
            "waiting":false, "isChild":false, "ephemeral":false,
            "clientUserMessageId":null,
            "context":{"cwd":"C:\\project","model":"gpt-6.1-sol","modelProvider":"openai",
                "threadSettings":"settings-digest","currentPermissions":"permissions-digest"}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn first_chat_confirmation_failure_does_not_skip_second_owned_pause() {
        let first = paused(&original_task());
        let mut second = first.clone();
        second.thread_id = "019ed000-2222-7222-8222-222222222222".into();
        let tasks = vec![first.clone(), second.clone()];
        let mut attempts = Vec::new();
        let result = recover_tasks_as_ready(
            &tasks,
            &mut attempts,
            &|| Ok(()),
            |_| async { Ok(()) },
            |attempts, task, ()| {
                Box::pin(async move {
                    attempts.push(task.thread_id.clone());
                    if attempts.len() == 1 {
                        Err("synthetic uncertain first request".into())
                    } else {
                        Ok(())
                    }
                })
            },
        )
        .await
        .unwrap_err();
        assert_eq!(attempts, vec![first.thread_id, second.thread_id]);
        assert!(result.contains("1/2"));
        assert!(result.contains("第 1 条"));
        assert!(result.contains("未重复发送继续"));
    }

    #[tokio::test]
    async fn cancellation_after_first_uncertain_resume_stops_other_tasks_without_retry() {
        let task = paused(&original_task());
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = || {
            if cancelled.load(Ordering::SeqCst) {
                Err("cancelled generation".into())
            } else {
                Ok(())
            }
        };
        let mut attempts = (0, cancelled.clone());
        let error = recover_tasks_as_ready(
            &[task.clone(), task],
            &mut attempts,
            &guard,
            |_| async { Ok(()) },
            |attempts, _, ()| {
                Box::pin(async move {
                    attempts.0 += 1;
                    attempts.1.store(true, Ordering::SeqCst);
                    Err("synthetic uncertain result".into())
                })
            },
        )
        .await
        .unwrap_err();
        assert_eq!(attempts.0, 1);
        assert_eq!(error, "cancelled generation");
    }

    #[tokio::test]
    async fn ready_second_chat_resumes_before_first_chat_owner_finishes_loading() {
        let first = paused(&original_task());
        let mut second = first.clone();
        second.thread_id = "019ed000-2222-7222-8222-222222222222".into();
        let first_id = first.thread_id.clone();
        let mut resumed = Vec::new();
        recover_tasks_as_ready(
            &[first.clone(), second.clone()],
            &mut resumed,
            &|| Ok(()),
            move |task| {
                let slow = task.thread_id == first_id;
                async move {
                    if slow {
                        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    }
                    Ok(())
                }
            },
            |resumed, task, ()| {
                Box::pin(async move {
                    resumed.push(task.thread_id.clone());
                    Ok(())
                })
            },
        )
        .await
        .unwrap();
        assert_eq!(resumed, vec![second.thread_id, first.thread_id]);
    }

    #[tokio::test]
    async fn owner_preparation_failure_does_not_send_or_block_other_ready_chats() {
        let first = paused(&original_task());
        let mut second = first.clone();
        second.thread_id = "019ed000-2222-7222-8222-222222222222".into();
        let first_id = first.thread_id.clone();
        let mut resumed = Vec::new();
        let error = recover_tasks_as_ready(
            &[first, second.clone()],
            &mut resumed,
            &|| Ok(()),
            move |task| {
                let failed = task.thread_id == first_id;
                async move {
                    if failed {
                        Err("owner unavailable".into())
                    } else {
                        Ok(())
                    }
                }
            },
            |resumed, task, ()| {
                Box::pin(async move {
                    resumed.push(task.thread_id.clone());
                    Ok(())
                })
            },
        )
        .await
        .unwrap_err();
        assert_eq!(resumed, vec![second.thread_id]);
        assert!(error.contains("1/2"));
        assert!(error.contains("目标账号已生效"));
    }

    fn paused(task: &TaskSnapshot) -> PausedTask {
        serde_json::from_value(json!({"threadId":task.thread_id,"turnId":task.turn_id,
            "context":task.context,"pauseOperationId":"operation-A","confirmedByCcSwitch":true}))
        .unwrap()
    }

    fn wait_record(id: &str) -> RecoveryRecord {
        let task = original_task();
        serde_json::from_value(json!({
            "operationId":id,"generation":1,"sourceProviderId":"source",
            "targetProviderId":"target","phase":"waiting-for-reset",
            "desktopRestarted":true,"runtimeIdentityConfirmed":true,
            "reopenedPid":123,"reopenedBirth":456,"reopenedPort":9222,
            "waitUntil":1_800_000_000_000i64,"waitingAccountId":"account",
            "waitBaselines":[task],"plannedTasks":[],"pausedTasks":[paused(&task)],
            "resumeIntents":[],"settingsRestoreIntents":[],"settingsRestoredTasks":[],
            "resumedTasks":[],"resumeConfirmations":[]
        }))
        .unwrap()
    }

    const WAIT_ID: &str = "44444444-4444-4444-8444-444444444444";

    fn close_checkpoint() -> ClosingCheckpoint {
        ClosingCheckpoint {
            schema_version: 1,
            intent_at: 1_800_000_000_000,
            manager_pid: 1234,
            manager_birth: Some(987_654),
            original_desktop: Some(
                serde_json::from_value(json!({"roots":[{
                    "pid":4321,"birth":456_789,"family":"Synthetic.Codex",
                    "application_id":"Codex"
                }]}))
                .unwrap(),
            ),
        }
    }

    #[test]
    fn old_recovery_records_load_without_closing_checkpoint() {
        let record = wait_record(WAIT_ID);
        assert!(record.close_checkpoint.is_none());
        assert_eq!(record.phase, "waiting-for-reset");
        assert_eq!(record.paused_tasks.len(), 1);
    }

    #[test]
    fn closing_checkpoint_roundtrip_preserves_process_evidence_as_intent_only() {
        let directory = tempfile::tempdir().unwrap();
        let mut record = wait_record(WAIT_ID);
        record.phase = "close-intent".into();
        record.desktop_restarted = false;
        record.runtime_identity_confirmed = false;
        record.close_checkpoint = Some(close_checkpoint());
        record.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let value = read_recovery_value(&path).unwrap();
        assert_eq!(value["closeCheckpoint"]["schemaVersion"], 1);
        assert_eq!(value["closeCheckpoint"]["intentAt"], 1_800_000_000_000i64);
        assert_eq!(value["closeCheckpoint"]["managerPid"], 1234);
        assert_eq!(value["closeCheckpoint"]["managerBirth"], 987_654);
        assert_eq!(
            value["closeCheckpoint"]["originalDesktop"]["roots"][0]["pid"],
            4321
        );
        assert!(value["closeCheckpoint"].get("exitConfirmed").is_none());
        assert!(value["closeCheckpoint"].get("requestSent").is_none());
        let restored = read_record(&path).unwrap();
        assert_eq!(serde_json::to_value(&restored).unwrap(), value);
        assert!(!restored.desktop_restarted);
        assert!(!restored.runtime_identity_confirmed);
    }

    #[test]
    fn closing_checkpoint_does_not_authorize_saved_task_replay() {
        let mut record = retry_record();
        assert!(unsent_recovery_candidate(&record, "target"));
        record.close_checkpoint = Some(close_checkpoint());
        record.phase = "close-intent".into();
        assert!(!confirmed_recovery_candidate(&record, "target"));
        assert!(!unsent_recovery_candidate(&record, "target"));
        assert!(!late_resume_candidate(&record, "target"));
        // Drop may turn an interrupted close into needs-review. Its saved
        // intent still cannot replace confirmation of a reopened desktop.
        record.phase = "needs-review".into();
        record.desktop_restarted = false;
        record.runtime_identity_confirmed = false;
        assert!(!confirmed_recovery_candidate(&record, "target"));
        assert!(!unsent_recovery_candidate(&record, "target"));
    }

    #[test]
    fn closing_checkpoint_allows_unknown_manager_birth_and_no_original_desktop() {
        let mut checkpoint = close_checkpoint();
        checkpoint.manager_birth = None;
        checkpoint.original_desktop = None;
        let value = serde_json::to_value(&checkpoint).unwrap();
        let restored: ClosingCheckpoint = serde_json::from_value(value).unwrap();
        assert!(restored.manager_birth.is_none());
        assert!(restored.original_desktop.is_none());
    }

    #[test]
    fn closing_checkpoint_persist_failure_preserves_cancelled_operation() {
        let directory = tempfile::tempdir().unwrap();
        let mut cancelled = wait_record(WAIT_ID);
        cancelled.phase = "cancelled".into();
        cancelled.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let before = std::fs::read(&path).unwrap();
        let mut record = wait_record(WAIT_ID);
        record.phase = "close-intent".into();
        record.close_checkpoint = Some(close_checkpoint());
        assert!(record.persist_in(directory.path()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(read_recovery_value(&path).unwrap()["phase"], "cancelled");
    }

    fn obsolete_record() -> RecoveryRecord {
        let mut record = wait_record(WAIT_ID);
        record.phase = "needs-review".into();
        record.wait_until = None;
        record.wait_baselines.clear();
        record.paused_tasks[0].pause_operation_id = WAIT_ID.into();
        let mut planned = original_task();
        planned.status = Some("inProgress".into());
        planned.runtime_status = "active".into();
        record.planned_tasks.push(planned);
        record
    }

    #[test]
    fn obsolete_recovery_accepts_only_confirmed_pauses_without_sent_continuation() {
        assert!(obsolete_recovery_candidate(&obsolete_record(), "target"));
        for changed in 0..17 {
            let mut record = obsolete_record();
            match changed {
                0 => record.phase = "completed".into(),
                1 => record.desktop_restarted = false,
                2 => record.runtime_identity_confirmed = false,
                3 => record.reopened_pid = None,
                4 => record.reopened_birth = None,
                5 => record.reopened_port = None,
                6 => record.wait_until = Some(1_800_000_000_000),
                7 => record.target_provider_id = "another-provider".into(),
                8 => record.resume_intents.push("original".into()),
                9 => record.settings_restore_intents.push("original".into()),
                10 => record.settings_restored_tasks.push("original".into()),
                11 => record.resumed_tasks.push("original".into()),
                12 => record.paused_tasks.clear(),
                13 => record.planned_tasks.clear(),
                14 => record.paused_tasks[0].pause_operation_id = "another-operation".into(),
                15 => record.planned_tasks[0].context.model = Some("changed-model".into()),
                _ => {
                    let mut saved = serde_json::to_value(&record.paused_tasks[0]).unwrap();
                    saved["confirmedByCcSwitch"] = json!(false);
                    record.paused_tasks[0] = serde_json::from_value(saved).unwrap();
                }
            }
            assert!(
                !obsolete_recovery_candidate(&record, "target"),
                "case {changed}"
            );
        }
    }

    fn sent_resume_record() -> RecoveryRecord {
        let mut record = obsolete_record();
        record
            .resume_intents
            .push(record.paused_tasks[0].thread_id.clone());
        record
    }

    fn retry_record() -> RecoveryRecord {
        let mut record = obsolete_record();
        record.paused_tasks[0].saved_settings = Some(codex_desktop_session::SavedTaskSettings {
            thread_settings: json!({"model":"gpt-6.1-sol","approvalPolicy":"on-request","sandboxPolicy":{"type":"readOnly"}}),
            current_permissions: json!({"approvalPolicy":"on-request","sandboxPolicy":{"type":"readOnly"}}),
        });
        record
    }

    #[test]
    fn unsent_recovery_requires_confirmed_desktop_owned_ticket_and_saved_settings() {
        assert!(unsent_recovery_candidate(&retry_record(), "target"));
        for changed in 0..10 {
            let mut record = retry_record();
            match changed {
                0 => record.phase = "cancelled".into(),
                1 => record.phase = "superseded".into(),
                2 => record.target_provider_id = "another-account".into(),
                3 => record.desktop_restarted = false,
                4 => record.runtime_identity_confirmed = false,
                5 => record.reopened_pid = None,
                6 => record.reopened_birth = None,
                7 => record.reopened_port = None,
                8 => record.paused_tasks[0].saved_settings = None,
                _ => record
                    .resume_intents
                    .push(record.paused_tasks[0].thread_id.clone()),
            }
            assert!(
                !unsent_recovery_candidate(&record, "target"),
                "case {changed}"
            );
        }
    }

    #[test]
    fn partially_sent_recovery_can_continue_only_its_unsent_owned_tasks() {
        let mut record = retry_record();
        let mut second = record.paused_tasks[0].clone();
        second.thread_id = "019ed000-2222-7222-8222-222222222222".into();
        let mut planned = record.planned_tasks[0].clone();
        planned.thread_id = second.thread_id.clone();
        record.paused_tasks.push(second);
        record.planned_tasks.push(planned);
        record
            .resume_intents
            .push(record.paused_tasks[0].thread_id.clone());
        assert!(unsent_recovery_candidate(&record, "target"));
        assert!(!late_resume_candidate(&record, "target"));
        record
            .resume_intents
            .push(record.paused_tasks[1].thread_id.clone());
        assert!(!unsent_recovery_candidate(&record, "target"));
        assert!(late_resume_candidate(&record, "target"));
    }

    #[test]
    fn unsent_claim_preserves_operation_and_tickets_and_guards_outside_record_lock() {
        let directory = tempfile::tempdir().unwrap();
        let record = retry_record();
        record.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let before = read_recovery_value(&path).unwrap();
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let guard = || {
            checks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        assert!(claim_unsent_recovery(&path, &before, 9, "target", &guard).unwrap());
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        let after = read_recovery_value(&path).unwrap();
        assert_eq!(after["phase"], "recovering-saved-tasks");
        assert_eq!(after["generation"], 9);
        for field in [
            "operationId",
            "pausedTasks",
            "plannedTasks",
            "resumeIntents",
            "reopenedPid",
            "reopenedBirth",
            "reopenedPort",
        ] {
            assert_eq!(after[field], before[field]);
        }
        assert!(!claim_unsent_recovery(&path, &before, 10, "target", &guard).unwrap());
    }

    #[test]
    fn unsent_claim_never_revives_cancelled_or_newer_records() {
        let directory = tempfile::tempdir().unwrap();
        let record = retry_record();
        record.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let before = read_recovery_value(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(
            claim_unsent_recovery(&path, &before, 9, "target", &|| Err("cancelled".into()))
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let mut cancelled = before.clone();
        cancelled["phase"] = json!("cancelled");
        std::fs::write(&path, serde_json::to_vec(&cancelled).unwrap()).unwrap();
        assert!(!claim_unsent_recovery(&path, &before, 9, "target", &|| Ok(())).unwrap());
        assert!(!claim_unsent_recovery(&path, &cancelled, 9, "target", &|| Ok(())).unwrap());
        assert_eq!(read_recovery_value(&path).unwrap(), cancelled);
    }

    #[test]
    fn claimed_recovery_survives_guard_failure_without_a_permanent_stuck_phase() {
        let directory = tempfile::tempdir().unwrap();
        let record = retry_record();
        record.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let before = read_recovery_value(&path).unwrap();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let guard = || {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err("desktop temporarily unconfirmed".into())
            }
        };
        assert!(claim_unsent_recovery(&path, &before, 9, "target", &guard).is_err());
        let claimed = read_recovery_value(&path).unwrap();
        assert_eq!(claimed["phase"], "recovering-saved-tasks");
        assert!(unsent_recovery_candidate(
            &read_record(&path).unwrap(),
            "target"
        ));
        assert!(claim_unsent_recovery(&path, &claimed, 10, "target", &|| Ok(())).unwrap());
        let after = read_recovery_value(&path).unwrap();
        assert_eq!(after["operationId"], before["operationId"]);
        assert_eq!(after["resumeIntents"], json!([]));
    }

    #[test]
    fn crashed_resume_phases_reconcile_but_never_repeat_persisted_start_intents() {
        for phase in [
            "reopened",
            "recovering-saved-tasks",
            "resume-intent",
            "resume-not-sent",
            "resume-outcome-needs-review",
            "resumed",
        ] {
            let mut record = retry_record();
            record.phase = phase.into();
            assert!(
                unsent_recovery_candidate(&record, "target"),
                "phase {phase}"
            );
            record
                .resume_intents
                .push(record.paused_tasks[0].thread_id.clone());
            assert!(!unsent_recovery_candidate(&record, "target"));
            assert!(late_resume_candidate(&record, "target"));
        }
        for phase in [
            "close-intent",
            "desktop-restarted",
            "enabling",
            "cancelled",
            "superseded",
            "completed",
        ] {
            let mut record = retry_record();
            record.phase = phase.into();
            assert!(
                !unsent_recovery_candidate(&record, "target"),
                "phase {phase}"
            );
        }
    }

    #[test]
    fn proven_unsent_error_releases_intent_but_uncertain_write_never_does() {
        let mut record = sent_resume_record();
        let id = record.paused_tasks[0].thread_id.clone();
        let before = serde_json::to_value(&record).unwrap();
        let uncertain = synthetic_read_error(SessionErrorKind::OutcomeUnknown, true);
        assert!(!withdraw_proven_unsent_intent(&mut record, &id, &uncertain));
        assert_eq!(serde_json::to_value(&record).unwrap(), before);
        let not_sent = synthetic_read_error(SessionErrorKind::Unavailable, false);
        assert!(withdraw_proven_unsent_intent(&mut record, &id, &not_sent));
        assert!(record.resume_intents.is_empty());
        assert_eq!(record.resume_not_sent_tasks, vec![id.clone()]);
        assert!(withdraw_proven_unsent_intent(&mut record, &id, &not_sent));
        assert_eq!(record.resume_not_sent_tasks, vec![id]);
    }

    #[test]
    fn pending_description_reports_progress_without_generic_manual_restart_prompt() {
        let record = serde_json::to_value(&retry_record()).unwrap();
        let description = recovery_pending_description(&record);
        assert!(description.contains("原任务 1 条"));
        assert!(description.contains("已记录 0 条"));
        assert!(description.contains("后台"));
        assert!(!description.contains("手动启用"));
        assert!(!description.contains("停止自动重放"));
    }

    fn late_confirmation(record: &RecoveryRecord) -> ResumeConfirmation {
        ResumeConfirmation {
            thread_id: record.paused_tasks[0].thread_id.clone(),
            turn_id: "confirmed-new-turn".into(),
            operation_id: record.operation_id.clone(),
            already_observed: true,
        }
    }

    #[test]
    fn late_resume_requires_exactly_one_sent_intent_for_every_owned_task() {
        assert!(late_resume_candidate(&sent_resume_record(), "target"));
        assert!(!obsolete_recovery_candidate(
            &sent_resume_record(),
            "target"
        ));
        for changed in 0..8 {
            let mut record = sent_resume_record();
            match changed {
                0 => record.resume_intents.clear(),
                1 => record.resume_intents.push(record.resume_intents[0].clone()),
                2 => record.resume_intents[0] = "unrelated".into(),
                3 => record
                    .settings_restore_intents
                    .push("unconfirmed-setting-edit".into()),
                4 => record.resumed_tasks.push("unrelated".into()),
                5 => record.settings_restored_tasks.push("unrelated".into()),
                6 => {
                    let mut confirmation = late_confirmation(&record);
                    confirmation.operation_id = "different-operation".into();
                    record.resume_confirmations.push(confirmation);
                }
                _ => record.runtime_identity_confirmed = false,
            }
            assert!(!late_resume_candidate(&record, "target"), "case {changed}");
        }
    }

    #[test]
    fn late_resume_settlement_preserves_intent_and_unknown_fields_without_replay() {
        let directory = tempfile::tempdir().unwrap();
        let record = sent_resume_record();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let mut before = serde_json::to_value(&record).unwrap();
        before["unknownPreservedField"] = json!("preserve");
        std::fs::write(&path, serde_json::to_vec(&before).unwrap()).unwrap();
        assert!(
            complete_late_recovery(&path, &before, &[late_confirmation(&record)], &|| Ok(()))
                .unwrap()
        );
        let after = read_recovery_value(&path).unwrap();
        assert_eq!(after["phase"], "completed");
        assert_eq!(after["resumeIntents"], before["resumeIntents"]);
        assert_eq!(after["pausedTasks"], before["pausedTasks"]);
        assert_eq!(after["plannedTasks"], before["plannedTasks"]);
        assert_eq!(after["unknownPreservedField"], "preserve");
        assert_eq!(after["resumeConfirmations"][0]["alreadyObserved"], true);
        assert_eq!(after["resumedTasks"], before["resumeIntents"]);
        assert!(after["settlementReason"]
            .as_str()
            .unwrap()
            .contains("未重复发送继续"));
    }

    #[test]
    fn late_resume_settlement_rejects_missing_duplicate_or_wrong_operation_proof() {
        let directory = tempfile::tempdir().unwrap();
        let record = sent_resume_record();
        record.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let before = read_recovery_value(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        for changed in 0..7 {
            let mut confirmation = late_confirmation(&record);
            match changed {
                0 => confirmation.thread_id = "unrelated".into(),
                1 => confirmation.operation_id = "unrelated".into(),
                2 => confirmation.turn_id.clear(),
                3 => confirmation.turn_id = record.paused_tasks[0].turn_id.clone(),
                4 => confirmation.already_observed = false,
                _ => (),
            }
            let confirmations = match changed {
                5 => vec![],
                6 => vec![confirmation.clone(), confirmation],
                _ => vec![confirmation],
            };
            assert!(
                !complete_late_recovery(&path, &before, &confirmations, &|| Ok(())).unwrap(),
                "case {changed}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        assert!(
            complete_late_recovery(&path, &before, &[late_confirmation(&record)], &|| Err(
                "cancelled".into()
            ))
            .is_err()
        );
        let mut changed = before.clone();
        changed["phase"] = json!("cancelled");
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(
            !complete_late_recovery(&path, &before, &[late_confirmation(&record)], &|| Ok(()))
                .unwrap()
        );
        assert_eq!(read_recovery_value(&path).unwrap(), changed);
    }

    #[test]
    fn partial_sent_recovery_is_superseded_only_after_every_owned_pause_changed() {
        let mut record = sent_resume_record();
        let mut second = record.paused_tasks[0].clone();
        second.thread_id = "019ed000-2222-7222-8222-222222222222".into();
        record.paused_tasks.push(second.clone());
        let mut planned = record.planned_tasks[0].clone();
        planned.thread_id = second.thread_id.clone();
        record.planned_tasks.push(planned);
        assert!(changed_recovery_candidate(&record, "target"));
        assert!(!late_resume_candidate(&record, "target"));
        let mut first_live = original_task();
        first_live.turn_id = Some("resumed-first".into());
        first_live.status = Some("inProgress".into());
        first_live.runtime_status = "active".into();
        let mut second_live = original_task();
        second_live.thread_id = second.thread_id.clone();
        let mut inventory = TaskInventory {
            running: vec![first_live],
            idle: vec![second_live.clone()],
            candidate_coverage_complete: true,
            ..Default::default()
        };
        assert!(!all_saved_tasks_were_changed(&record, &inventory));
        second_live.turn_id = Some("user-continued-second".into());
        second_live.status = Some("inProgress".into());
        second_live.runtime_status = "active".into();
        inventory.idle.clear();
        inventory.running.push(second_live);
        assert!(all_saved_tasks_were_changed(&record, &inventory));
        let directory = tempfile::tempdir().unwrap();
        record.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let before = read_recovery_value(&path).unwrap();
        assert!(
            supersede_obsolete_recovery(&path, &before, &record.paused_tasks, &|| Ok(())).unwrap()
        );
        let after = read_recovery_value(&path).unwrap();
        assert_eq!(after["phase"], "superseded");
        assert_eq!(after["resumeIntents"], before["resumeIntents"]);
        assert_eq!(after["resumedTasks"], before["resumedTasks"]);
    }

    #[test]
    fn reset_wait_late_confirmation_retains_explicitly_abandoned_task_history() {
        let mut record = sent_resume_record();
        let mut abandoned = record.planned_tasks[0].clone();
        abandoned.thread_id = "019ed000-2222-7222-8222-222222222222".into();
        record.abandoned_tasks.push(abandoned.thread_id.clone());
        record.planned_tasks.push(abandoned);
        assert!(late_resume_candidate(&record, "target"));
        record.abandoned_tasks[0] = record.paused_tasks[0].thread_id.clone();
        assert!(!changed_recovery_candidate(&record, "target"));
    }

    #[test]
    fn old_recovery_is_obsolete_when_all_four_original_tasks_have_new_running_turns() {
        let mut record = obsolete_record();
        record.paused_tasks.clear();
        record.planned_tasks.clear();
        let mut inventory = TaskInventory {
            candidate_coverage_complete: true,
            ..Default::default()
        };
        for index in 0..4 {
            let mut task = original_task();
            task.thread_id = format!("019ed000-1111-7111-8111-{index:012}");
            let mut saved = paused(&task);
            saved.pause_operation_id = WAIT_ID.into();
            record.paused_tasks.push(saved);
            task.status = Some("inProgress".into());
            task.runtime_status = "active".into();
            record.planned_tasks.push(task.clone());
            task.turn_id = Some(format!("user-new-turn-{index}"));
            inventory.running.push(task);
        }
        assert!(obsolete_recovery_candidate(&record, "target"));
        assert!(all_saved_tasks_were_changed(&record, &inventory));
        inventory.running[3] = original_task();
        inventory.running[3].thread_id = record.paused_tasks[3].thread_id.clone();
        assert!(!all_saved_tasks_were_changed(&record, &inventory));
    }

    #[test]
    fn old_recovery_terminal_proof_accepts_completed_or_explicit_user_waits() {
        let record = obsolete_record();
        let saved = &record.paused_tasks[0];
        let mut completed = original_task();
        completed.status = Some("completed".into());
        assert!(saved_task_was_changed(saved, &completed));
        for reason in [
            codex_desktop_session::WaitingReason::PendingApproval,
            codex_desktop_session::WaitingReason::PendingUserInput,
            codex_desktop_session::WaitingReason::PendingElicitation,
        ] {
            let mut waiting = original_task();
            waiting.status = Some("inProgress".into());
            waiting.runtime_status = "active".into();
            waiting.waiting = true;
            waiting.waiting_reason = Some(reason);
            assert!(saved_task_was_changed(saved, &waiting));
        }
    }

    #[test]
    fn old_recovery_stays_pending_for_owned_missing_unknown_or_ambiguous_tasks() {
        let record = obsolete_record();
        let saved = &record.paused_tasks[0];
        assert!(!saved_task_was_changed(saved, &original_task()));
        for changed in 0..7 {
            let mut actual = original_task();
            actual.turn_id = Some("new-turn".into());
            match changed {
                0 => actual.turn_id = None,
                1 => actual.status = None,
                2 => actual.runtime_status = "unknown".into(),
                3 => actual.is_child = true,
                4 => actual.ephemeral = true,
                5 => {
                    actual.waiting = true;
                    actual.waiting_reason =
                        Some(codex_desktop_session::WaitingReason::UnknownRuntimeFlag);
                }
                _ => {
                    actual.waiting = true;
                    actual.waiting_reason =
                        Some(codex_desktop_session::WaitingReason::UnconfirmedTurnSubmission);
                }
            }
            assert!(!saved_task_was_changed(saved, &actual), "case {changed}");
        }
        let missing = TaskInventory {
            candidate_coverage_complete: true,
            ..Default::default()
        };
        assert!(!all_saved_tasks_were_changed(&record, &missing));
        let mut completed = original_task();
        completed.status = Some("completed".into());
        let duplicate = TaskInventory {
            idle: vec![completed.clone(), completed],
            candidate_coverage_complete: true,
            ..Default::default()
        };
        assert!(!all_saved_tasks_were_changed(&record, &duplicate));
    }

    #[test]
    fn own_unconfirmed_new_turn_never_supersedes_model_or_permission_failure() {
        let record = sent_resume_record();
        let mut task = original_task();
        task.turn_id = Some("this-operation-new-turn".into());
        task.status = Some("inProgress".into());
        task.runtime_status = "active".into();
        task.client_user_message_id = Some(record.operation_id.clone());
        for field in 0..3 {
            let mut changed = task.clone();
            match field {
                0 => changed.context.model = Some("wrong-model".into()),
                1 => changed.context.current_permissions = json!("wrong-permissions"),
                _ => (),
            }
            let inventory = TaskInventory {
                running: vec![changed],
                candidate_coverage_complete: true,
                ..Default::default()
            };
            assert!(!all_saved_tasks_were_changed(&record, &inventory));
        }
        task.client_user_message_id = Some("later-user-operation".into());
        let inventory = TaskInventory {
            running: vec![task],
            candidate_coverage_complete: true,
            ..Default::default()
        };
        assert!(all_saved_tasks_were_changed(&record, &inventory));
    }

    #[test]
    fn obsolete_recovery_commit_preserves_saved_tickets_and_records_terminal_reason_only() {
        let directory = tempfile::tempdir().unwrap();
        let record = obsolete_record();
        record.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let before = read_recovery_value(&path).unwrap();
        assert!(
            supersede_obsolete_recovery(&path, &before, &record.paused_tasks, &|| Ok(())).unwrap()
        );
        let after = read_recovery_value(&path).unwrap();
        assert_eq!(after["phase"], "superseded");
        assert_eq!(after["pausedTasks"], before["pausedTasks"]);
        assert_eq!(after["plannedTasks"], before["plannedTasks"]);
        assert_eq!(after["resumeIntents"], before["resumeIntents"]);
        assert_eq!(after["resumedTasks"], before["resumedTasks"]);
        assert!(after["supersededReason"]
            .as_str()
            .unwrap()
            .contains("未重放任何操作"));
        assert_eq!(after["abandonedTasks"][0], record.paused_tasks[0].thread_id);
    }

    #[test]
    fn obsolete_recovery_commit_never_overwrites_changed_or_cancelled_records() {
        let directory = tempfile::tempdir().unwrap();
        let record = obsolete_record();
        record.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let before = read_recovery_value(&path).unwrap();
        let mut newer = before.clone();
        newer["resumeIntents"] = json!(["newer-resume-intent"]);
        std::fs::write(&path, serde_json::to_vec(&newer).unwrap()).unwrap();
        let newer_bytes = std::fs::read(&path).unwrap();
        assert!(
            !supersede_obsolete_recovery(&path, &before, &record.paused_tasks, &|| Ok(())).unwrap()
        );
        assert_eq!(std::fs::read(&path).unwrap(), newer_bytes);
        assert!(
            supersede_obsolete_recovery(&path, &newer, &record.paused_tasks, &|| Err(
                "synthetic cancelled generation".into()
            ),)
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), newer_bytes);
    }

    #[test]
    fn obsolete_recovery_commit_does_not_make_a_readable_record_oversized() {
        let directory = tempfile::tempdir().unwrap();
        let record = obsolete_record();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let mut before = serde_json::to_value(&record).unwrap();
        let padding = 1_048_576 - serde_json::to_vec(&before).unwrap().len() - 32;
        before["unknownPreservedField"] = json!("x".repeat(padding));
        let bytes = serde_json::to_vec(&before).unwrap();
        assert!(bytes.len() <= 1_048_576);
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            supersede_obsolete_recovery(&path, &before, &record.paused_tasks, &|| Ok(()))
                .unwrap_err()
                .contains("读取上限")
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn ordinary_multi_window_instance_has_coverage_but_independent_instances_do_not() {
        assert!(ensure_desktop_task_coverage(0).is_ok());
        assert!(ensure_desktop_task_coverage(1).is_ok());
        let error = ensure_desktop_task_coverage(2).unwrap_err();
        assert!(error.contains("聊天覆盖范围"));
        assert!(error.contains("未暂停或关闭"));
    }

    #[test]
    fn waiting_journal_survives_reload_without_writing_or_replaying_actions() {
        let directory = tempfile::tempdir().unwrap();
        let record = wait_record(WAIT_ID);
        record.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let before = std::fs::read(&path).unwrap();
        let wait = quota_reset_wait_in(directory.path()).unwrap().unwrap();
        assert_eq!(wait.operation_id, WAIT_ID);
        assert_eq!(wait.provider_id, "target");
        assert_eq!(wait.reset_at, 1_800_000_000_000i64);
        let loaded = read_record(&path).unwrap();
        assert!(loaded.loaded);
        assert!(loaded.resume_intents.is_empty());
        assert!(loaded.resumed_tasks.is_empty());
        assert_eq!(receipt_for_wait(&loaded).unwrap().port, 9222);
        drop(loaded);
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn terminal_cancellation_cannot_be_overwritten_by_a_delayed_wait_update() {
        let directory = tempfile::tempdir().unwrap();
        let original = wait_record(WAIT_ID);
        original.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let mut stale = read_record(&path).unwrap();
        let mut cancelled = read_record(&path).unwrap();
        cancelled.phase = "cancelled".into();
        cancelled.persist_in(directory.path()).unwrap();
        let before = std::fs::read(&path).unwrap();
        stale.wait_until = Some(1_800_001_000_000i64);
        assert!(stale.persist_in(directory.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(quota_reset_wait_in(directory.path()).unwrap().is_none());
    }

    #[test]
    fn oversized_wait_write_preserves_the_last_readable_recovery_journal() {
        let directory = tempfile::tempdir().unwrap();
        let mut record = wait_record(WAIT_ID);
        record.persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let before = std::fs::read(&path).unwrap();
        record.abandoned_tasks.push("x".repeat(1_048_577));
        assert!(record
            .persist_in(directory.path())
            .unwrap_err()
            .contains("上限"));
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(quota_reset_wait_in(directory.path()).unwrap().is_some());
    }

    #[test]
    fn loaded_wait_update_cannot_restore_waiting_over_a_resume_intent() {
        let directory = tempfile::tempdir().unwrap();
        wait_record(WAIT_ID).persist_in(directory.path()).unwrap();
        let path = directory.path().join(format!("{WAIT_ID}.json"));
        let stale = read_record(&path).unwrap();
        let mut active = read_record(&path).unwrap();
        active.phase = "resume-intent".into();
        active.resume_intents.push("original".into());
        active.persist_in(directory.path()).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(stale.persist_in(directory.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn wait_reader_rejects_duplicate_plans_missing_baselines_and_existing_resume_intents() {
        let directory = tempfile::tempdir().unwrap();
        wait_record(WAIT_ID).persist_in(directory.path()).unwrap();
        let other_id = "55555555-5555-4555-8555-555555555555";
        wait_record(other_id).persist_in(directory.path()).unwrap();
        assert!(quota_reset_wait_in(directory.path())
            .unwrap_err()
            .contains("多个"));
        std::fs::remove_file(directory.path().join(format!("{other_id}.json"))).unwrap();
        for changed in 0..5 {
            let mut record = wait_record(WAIT_ID);
            match changed {
                0 => record.wait_baselines.clear(),
                1 => record.resume_intents.push("original".into()),
                2 => record.wait_baselines[0].turn_id = Some("another-turn".into()),
                3 => record.wait_baselines[0].waiting = true,
                _ => record.waiting_account_id = None,
            }
            record.persist_in(directory.path()).unwrap();
            assert!(
                quota_reset_wait_in(directory.path()).is_err(),
                "case {changed}"
            );
        }
    }

    #[test]
    fn wait_reader_ignores_completed_legacy_schema_but_rejects_corruption_and_wrong_id() {
        let directory = tempfile::tempdir().unwrap();
        let legacy = directory.path().join("legacy.json");
        std::fs::write(&legacy, r#"{"phase":"completed"}"#).unwrap();
        assert!(quota_reset_wait_in(directory.path()).unwrap().is_none());
        let record = wait_record(WAIT_ID);
        let wrong_path = directory.path().join("wrong-id.json");
        std::fs::write(&wrong_path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(quota_reset_wait_in(directory.path()).is_err());
        std::fs::remove_file(wrong_path).unwrap();
        std::fs::write(&legacy, b"broken-json").unwrap();
        assert!(quota_reset_wait_in(directory.path()).is_err());
        std::fs::write(&legacy, vec![b' '; 1_048_577]).unwrap();
        assert!(read_recovery_value(&legacy)
            .unwrap_err()
            .contains("大小异常"));
    }

    #[test]
    fn owned_quota_failure_survives_hours_of_wait_but_never_resumes_changed_user_work() {
        let mut original = original_task();
        original.status = Some("failed".into());
        original.runtime_status = "systemError".into();
        original.turn_error_code = Some("usageLimitExceeded".into());
        original.turn_ended_at_ms =
            Some(chrono::Utc::now().timestamp_millis() - 4 * 60 * 60 * 1_000);
        assert!(!original.recent_quota_failure());
        let saved: PausedTask = serde_json::from_value(json!({
            "threadId":original.thread_id,"turnId":original.turn_id,"context":original.context,
            "pauseOperationId":WAIT_ID,"confirmedByCcSwitch":false,"origin":"quotaExhausted",
            "turnEndedAtMs":original.turn_ended_at_ms
        }))
        .unwrap();
        assert!(matches_wait_baseline(&original, &original, &saved));
        for changed in 0..9 {
            let mut current = original.clone();
            match changed {
                0 => current.status = Some("interrupted".into()),
                1 => current.status = Some("completed".into()),
                2 => current.turn_id = Some("user-new-turn".into()),
                3 => current.waiting = true,
                4 => current.runtime_status = "active".into(),
                5 => current.context.model = Some("user-new-model".into()),
                6 => current.context.current_permissions = json!("user-changed-permissions"),
                7 => current.turn_error_code = Some("anotherError".into()),
                _ => current.turn_ended_at_ms = original.turn_ended_at_ms.map(|time| time + 1),
            }
            assert!(
                !matches_wait_baseline(&current, &original, &saved),
                "case {changed}"
            );
        }
    }

    #[test]
    fn quota_wait_uses_confirmed_reopened_time_not_original_duration_estimate() {
        let mut original = original_task();
        original.status = Some("failed".into());
        original.runtime_status = "systemError".into();
        original.turn_error_code = Some("usageLimitExceeded".into());
        original.turn_ended_at_ms = Some(1_791_799_168_552);
        let saved: PausedTask = serde_json::from_value(json!({
            "threadId":original.thread_id,"turnId":original.turn_id,"context":original.context,
            "pauseOperationId":WAIT_ID,"confirmedByCcSwitch":false,"origin":"quotaExhausted",
            "turnEndedAtMs":original.turn_ended_at_ms
        }))
        .unwrap();
        let mut reopened = original.clone();
        reopened.runtime_status = "idle".into();
        reopened.turn_ended_at_ms = Some(1_791_799_168_047);
        assert!(!reopened.matches_paused(&saved));
        assert!(matches_wait_baseline(&reopened, &reopened, &saved));
        let mut changed = reopened.clone();
        changed.turn_ended_at_ms = original.turn_ended_at_ms;
        assert!(!matches_wait_baseline(&changed, &reopened, &saved));
        let mut unrelated = reopened.clone();
        unrelated.turn_id = Some("another-turn".into());
        assert!(!matches_wait_baseline(&reopened, &unrelated, &saved));
    }

    #[test]
    fn final_shutdown_observation_excludes_only_changed_owned_tickets() {
        let directory = tempfile::tempdir().unwrap();
        let mut record = wait_record(WAIT_ID);
        record.phase = "close-intent".into();
        record.close_checkpoint = Some(close_checkpoint());
        let first = record.paused_tasks[0].thread_id.clone();
        let mut second = record.paused_tasks[0].clone();
        second.thread_id = "019ed000-2222-7222-8222-222222222222".into();
        record.paused_tasks.push(second.clone());
        record.persist_in(directory.path()).unwrap();
        let expected = serde_json::to_value(&record).unwrap();
        exclude_restart_candidates(directory.path(), &expected, &[first.clone()]).unwrap();
        let actual = read_record(&directory.path().join(format!("{WAIT_ID}.json"))).unwrap();
        assert_eq!(actual.phase, "close-intent");
        assert_eq!(actual.paused_tasks.len(), 1);
        assert_eq!(actual.paused_tasks[0].thread_id, second.thread_id);
        assert_eq!(actual.abandoned_tasks, vec![first.clone()]);
        assert!(actual.close_checkpoint.is_some());
        apply_restart_exclusions(&mut record, &[first]);
        assert_eq!(
            serde_json::to_value(record).unwrap(),
            serde_json::to_value(actual).unwrap()
        );
    }

    #[test]
    fn final_shutdown_observation_cannot_overwrite_cancel_or_newer_journal() {
        for phase in ["cancelled", "superseded", "resume-intent", "completed"] {
            let directory = tempfile::tempdir().unwrap();
            let mut record = wait_record(WAIT_ID);
            record.phase = "close-intent".into();
            record.persist_in(directory.path()).unwrap();
            let expected = serde_json::to_value(&record).unwrap();
            record.phase = phase.into();
            record.persist_in(directory.path()).unwrap();
            let path = directory.path().join(format!("{WAIT_ID}.json"));
            let before = std::fs::read(&path).unwrap();
            assert!(exclude_restart_candidates(directory.path(), &expected, &[]).is_err());
            assert!(exclude_restart_candidates(
                directory.path(),
                &expected,
                &[record.paused_tasks[0].thread_id.clone()]
            )
            .is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn desktop_shutdown_recovery_wait_rejects_active_completed_and_changed_baselines() {
        let mut original = original_task();
        original.status = Some("inProgress".into());
        original.runtime_status = "active".into();
        let saved: PausedTask = serde_json::from_value(json!({
            "threadId":original.thread_id,"turnId":original.turn_id,"context":original.context,
            "pauseOperationId":WAIT_ID,"origin":"desktopShutdown","confirmedByCcSwitch":false,
            "savedSettings":{"threadSettings":{},"currentPermissions":{}}
        }))
        .unwrap();
        let mut baseline = original.clone();
        baseline.runtime_status = "idle".into();
        assert!(matches_wait_baseline(&baseline, &baseline, &saved));
        for change in 0..5 {
            let mut actual = baseline.clone();
            match change {
                0 => actual.runtime_status = "active".into(),
                1 => actual.status = Some("completed".into()),
                2 => actual.waiting = true,
                3 => actual.turn_id = Some("user-new-turn".into()),
                _ => actual.context.current_permissions = json!("user-new-permissions"),
            }
            assert!(!matches_wait_baseline(&actual, &baseline, &saved));
        }
    }

    #[test]
    fn desktop_shutdown_ticket_keeps_confirmed_quota_failure_owned_across_reset_wait() {
        let mut original = original_task();
        original.status = Some("inProgress".into());
        original.runtime_status = "active".into();
        let saved: PausedTask = serde_json::from_value(json!({
            "threadId":original.thread_id,"turnId":original.turn_id,"context":original.context,
            "pauseOperationId":WAIT_ID,"origin":"desktopShutdown","confirmedByCcSwitch":false,
            "savedSettings":{"threadSettings":{},"currentPermissions":{}}
        }))
        .unwrap();
        let mut quota_baseline = original;
        quota_baseline.status = Some("failed".into());
        quota_baseline.runtime_status = "systemError".into();
        quota_baseline.turn_error_code = Some("usageLimitExceeded".into());
        // Deliberately historical: ownership survives waiting for quota reset.
        quota_baseline.turn_ended_at_ms = Some(1_000);
        assert!(matches_wait_baseline(
            &quota_baseline,
            &quota_baseline,
            &saved
        ));
        for change in 0..8 {
            let mut actual = quota_baseline.clone();
            match change {
                0 => actual.runtime_status = "active".into(),
                1 => actual.status = Some("completed".into()),
                2 => actual.status = Some("interrupted".into()),
                3 => actual.turn_error_code = Some("otherError".into()),
                4 => actual.turn_id = Some("user-new-turn".into()),
                5 => actual.context.model = Some("user-new-model".into()),
                6 => actual.turn_ended_at_ms = Some(2_000),
                _ => actual.waiting = true,
            }
            assert!(
                !matches_wait_baseline(&actual, &quota_baseline, &saved),
                "case {change}"
            );
        }
    }
}

async fn connect_reopened_desktop(
    guard: &Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
) -> Result<DesktopSession, String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        guard()?;
        match DesktopSession::connect().await {
            Ok(mut session) => {
                session.set_trusted_codex_home(crate::codex_config::get_codex_config_dir());
                return Ok(session);
            }
            Err(error) if tokio::time::Instant::now() >= deadline => {
                return Err(format!("重开后桌面协作连接未就绪：{error}"))
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
}
