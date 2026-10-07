//! Shared CC Switch account restart and original-desktop-chat recovery.
//! Account activation uses the original ProviderService; no app-server method
//! is incorrectly sent to the desktop coordinator and no model tool is used.

use crate::app_config::AppType;
use crate::services::codex_desktop_identity::{self as identity, DesktopIdentity};
use crate::services::codex_desktop_session::{
    self, DesktopSession, PausedTask, ResumeConfirmation, TaskInventory, TaskSnapshot,
};
use crate::services::ProviderService;
use crate::services::{codex_auto_switch as monitor, codex_desktop_restart as restart};
use crate::store::AppState;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use tauri::{Emitter, Manager};

static FLOW_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));
static RECORD_LOCK: LazyLock<std::sync::Mutex<()>> = LazyLock::new(|| std::sync::Mutex::new(()));

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryRecord {
    operation_id: String,
    generation: u64,
    source_provider_id: String,
    target_provider_id: String,
    phase: String,
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

fn inventory_block_reason(inventory: &TaskInventory) -> String {
    let details = inventory
        .blocked
        .iter()
        .take(8)
        .map(|task| {
            format!(
                "{}:{} (runtime={}, turn={})",
                task.thread_id,
                task.waiting_reason
                    .map(|reason| reason.as_str())
                    .unwrap_or("unknownTaskState"),
                task.runtime_status,
                task.status.as_deref().unwrap_or("unknown")
            )
        })
        .collect::<Vec<_>>()
        .join("；");
    format!(
        "需处理 {}，未确认 {}；{}",
        inventory.blocked.len(),
        inventory.unresolved_thread_ids.len(),
        details
    )
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
        if record["phase"] == "needs-review" {
            return Some("上次退出或原聊天恢复结果未确认；已停止自动重放。请核对桌面状态，可重新手动启用账号".into());
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
    current.thread_id == saved.thread_id
        && current.turn_id.as_deref() == Some(saved.turn_id.as_str())
        && current.context == baseline.context
        && !current.waiting
        && !current.is_child
        && !current.ephemeral
        && match saved.origin {
            codex_desktop_session::RecoveryOrigin::QuotaExhausted => {
                current.safely_quota_failed() && current.turn_ended_at_ms == saved.turn_ended_at_ms
            }
            codex_desktop_session::RecoveryOrigin::CcSwitchPause => {
                current.safely_idle() && current.status.as_deref() == Some("interrupted")
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
    let _flow = FLOW_LOCK.lock().await;
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
    let _flow = FLOW_LOCK.lock().await;
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
    let mut ignored_quota_failures = 0usize;
    // A new explicit Enable may reopen a desktop left closed by a cancelled
    // flow. Its target identity must be checked even when none is running now.
    let expected_identity = target_identity(&app, &target).await?;
    check_selection(&app, generation, &source)?;
    if desktop_running {
        monitor::lifecycle_status(
            &app,
            generation,
            "preflight",
            "正在核对桌面任务，不读取旧聊天全文",
        );
        let ids = codex_desktop_session::discover_thread_ids(
            &crate::codex_config::get_codex_config_dir(),
        )
        .map_err(|e| e.to_string())?;
        let mut session = DesktopSession::connect().await.map_err(|e| e.to_string())?;
        let mut inventory = session
            .snapshot_safe_running_tasks(&ids)
            .await
            .map_err(|e| e.to_string())?;
        inventory.candidate_coverage_complete = true;
        restart::ensure_same_desktop(&desktop_receipt)?;
        if !inventory.safe_to_restart() {
            return Err(format!(
                "桌面有等待审批或状态未确认的任务（{}）；未关闭桌面",
                inventory_block_reason(&inventory)
            ));
        }
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
        // Check every running chat before the first interruption. An unknown
        // permission field in a later chat must not leave earlier chats paused.
        for task in &inventory.running {
            session
                .can_preserve_running_task(task)
                .map_err(|e| format!("原聊天设置无法完整保存：{e}；未暂停任何任务"))?;
        }
        for task in &recent_quota_failures {
            session
                .can_preserve_quota_failed_task(task)
                .map_err(|e| format!("额度耗尽聊天设置无法完整保存：{e}；未暂停任何任务"))?;
        }
        for task in recent_quota_failures {
            check_selection(&app, generation, &source)?;
            record.phase = format!("quota-recovery-intent:{}", task.thread_id);
            record.planned_tasks.push(task.clone());
            record.persist()?;
            let ticket = session
                .capture_quota_failed_task(&task, &record.operation_id, || {
                    check_selection(&app, generation, &source).is_ok()
                        && restart::ensure_same_desktop(&desktop_receipt).is_ok()
                })
                .await
                .map_err(|e| format!("额度耗尽任务状态核对未完成：{e}；未继续关闭或换号"))?;
            record.paused_tasks.push(ticket);
            record.phase = "quota-recovery-captured".into();
            record.persist()?;
        }
        for task in inventory.running {
            check_selection(&app, generation, &source)?;
            monitor::lifecycle_status(
                &app,
                generation,
                "pausing",
                "正在暂停本次运行任务并保存原聊天和轮次",
            );
            record.phase = format!("pause-intent:{}", task.thread_id);
            record.planned_tasks.push(task.clone());
            record.persist()?;
            let paused = session
                .pause_and_confirm(&task, &record.operation_id, || {
                    check_selection(&app, generation, &source).is_ok()
                        && restart::ensure_same_desktop(&desktop_receipt).is_ok()
                })
                .await
                .map_err(|e| format!("安全暂停未完成：{e}；未继续关闭或换号"))?;
            record.paused_tasks.push(paused);
            record.phase = "paused".into();
            record.persist()?;
        }
        // Fresh connection + freshly discovered metadata also checks tasks
        // started or changed while quota queries / interruptions were running.
        drop(session);
        check_selection(&app, generation, &source)?;
        let fresh_ids = codex_desktop_session::discover_thread_ids(
            &crate::codex_config::get_codex_config_dir(),
        )
        .map_err(|e| e.to_string())?;
        let mut verifier = DesktopSession::connect().await.map_err(|e| e.to_string())?;
        let mut latest = verifier
            .snapshot_safe_running_tasks(&fresh_ids)
            .await
            .map_err(|e| e.to_string())?;
        latest.candidate_coverage_complete = true;
        if !latest.safe_to_restart() || !latest.running.is_empty() {
            record.phase = "state-changed-needs-review".into();
            record.persist()?;
            return Err("暂停后任务状态出现变化；已取消退出，未继续切号或自动继续任务".into());
        }
        for paused in &record.paused_tasks {
            let actual = latest
                .idle
                .iter()
                .chain(latest.quota_failed.iter())
                .find(|task| task.thread_id == paused.thread_id)
                .ok_or("本次记录的原聊天状态未确认；未继续关闭桌面")?;
            if !actual.matches_paused(paused) {
                return Err("本次记录的原轮次、模型或权限已改变；旧恢复计划失效".into());
            }
        }
    }
    record.phase = "close-intent".into();
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
    let runtime = tokio::runtime::Handle::current();
    // Run from the native blocking task immediately before RmShutdown, after
    // Windows has prepared and verified its exact process resource list.
    let before_shutdown: Box<dyn FnOnce() -> Result<(), String> + Send> = Box::new(move || {
        final_guard()?;
        restart::ensure_same_desktop(&final_receipt)?;
        runtime.block_on(confirm_tasks_before_shutdown(&final_paused))?;
        final_guard()?;
        restart::ensure_same_desktop(&final_receipt)
    });
    let restarted = if automatic {
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
        .await?
    } else {
        restart::restart_bound(
            desktop_receipt.clone(),
            guard.clone(),
            Box::new(|| Ok(())),
            before_shutdown,
            true,
        )
        .await?
    };
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
) -> Result<(), String> {
    let navigation_receipt = receipt.clone();
    let previous_guard = guard.clone();
    let resume_guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync> = Arc::new(move || {
        previous_guard()?;
        restart::ensure_identity_desktop(&receipt)
    });
    monitor::lifecycle_status(
        &app,
        generation,
        "resuming",
        "正在恢复本次记录的原聊天，沿用原模型和权限",
    );
    for paused in record.paused_tasks.clone() {
        resume_guard()?;
        if let Some(expected) = expected_identity {
            confirm_identity_receipt(expected, &navigation_receipt, &resume_guard).await?;
        }
        // Subscribe before navigation so the original owner registering
        // during startup cannot be missed by a one-shot subscription.
        let mut session = connect_reopened_desktop(&resume_guard).await?;
        session
            .begin_follow(&paused.thread_id)
            .await
            .map_err(|e| e.to_string())?;
        codex_desktop_session::open_original_chat(&paused.thread_id).map_err(|e| e.to_string())?;
        restart::wait_for_navigation(&navigation_receipt, guard.as_ref()).await?;
        if let Some(expected) = expected_identity {
            confirm_identity_receipt(expected, &navigation_receipt, &resume_guard).await?;
        }
        monitor::lifecycle_status(
            &app,
            generation,
            "waiting-for-chat",
            "正在等待原聊天加载并核对本次记录的轮次，尚未发送继续",
        );
        let actual = session
            .wait_for_reopened_paused_chat(&paused, || resume_guard().is_ok())
            .await
            .map_err(|e| e.to_string())?;
        if !record.wait_baselines.is_empty() {
            let baseline = record
                .wait_baselines
                .iter()
                .find(|task| task.thread_id == paused.thread_id)
                .ok_or("额度等待任务缺少原设置基线；未继续")?;
            if actual.context != baseline.context {
                return Err("等待期间原任务的项目、模型或权限已被用户修改；未覆盖用户设置".into());
            }
        }
        monitor::lifecycle_status(
            &app,
            generation,
            "resuming",
            "正在用原聊天恢复请求继续任务，保留本次换号前的模型和权限",
        );
        if let Some(expected) = expected_identity {
            confirm_identity_receipt(expected, &navigation_receipt, &resume_guard).await?;
        }
        // Persist the unique intent before sending once. A crash/timeout
        // leaves a needs-review record; startup never replays continuation.
        record.phase = "resume-intent".into();
        record.resume_intents.push(paused.thread_id.clone());
        record.persist()?;
        let resume = session
            .resume_and_confirm(&paused, &record.operation_id, || resume_guard().is_ok())
            .await;
        let confirmation = match resume {
            Ok(confirmation) => confirmation,
            Err(error) => {
                // Reconciliation reads the new turn; it never repeats start.
                match session
                    .reconcile_resume(&paused, &record.operation_id)
                    .await
                {
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
        record.resume_confirmations.push(confirmation);
        // The single original resume request resolves these settings as
        // part of starting its new turn. Record them only after the live
        // new turn's model, project and permission semantics are confirmed.
        record
            .settings_restored_tasks
            .push(paused.thread_id.clone());
        record.resumed_tasks.push(paused.thread_id.clone());
        record.phase = "resumed".into();
        record.persist()?;
    }
    Ok(())
}

async fn confirm_tasks_before_shutdown(paused: &[PausedTask]) -> Result<(), String> {
    let ids =
        codex_desktop_session::discover_thread_ids(&crate::codex_config::get_codex_config_dir())
            .map_err(|e| e.to_string())?;
    let mut session = DesktopSession::connect().await.map_err(|e| e.to_string())?;
    let mut inventory = session
        .snapshot_safe_running_tasks(&ids)
        .await
        .map_err(|e| e.to_string())?;
    inventory.candidate_coverage_complete = true;
    validate_tasks_before_shutdown(&inventory, paused)
}

fn validate_tasks_before_shutdown(
    inventory: &TaskInventory,
    paused: &[PausedTask],
) -> Result<(), String> {
    if !inventory.safe_to_restart() || !inventory.running.is_empty() {
        return Err("正常退出前发现新运行任务、审批或未确认状态；本次未关闭桌面".into());
    }
    for saved in paused {
        let current = inventory
            .idle
            .iter()
            .chain(inventory.quota_failed.iter())
            .find(|task| task.thread_id == saved.thread_id)
            .ok_or("正常退出前原聊天状态未确认；未关闭桌面")?;
        if !current.matches_paused(saved) {
            return Err("正常退出前原轮次或权限已改变；旧恢复计划失效".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn final_shutdown_check_rejects_new_work_approval_and_unknown_coverage() {
        let original = original_task();
        let saved = paused(&original);
        let good = TaskInventory {
            idle: vec![original.clone()],
            candidate_coverage_complete: true,
            ..TaskInventory::default()
        };
        assert!(validate_tasks_before_shutdown(&good, &[saved.clone()]).is_ok());
        for changed in 0..4 {
            let mut inventory = good.clone();
            match changed {
                0 => inventory.running.push(original.clone()),
                1 => inventory.blocked.push(original.clone()),
                2 => inventory.unresolved_thread_ids.push("unknown".into()),
                _ => inventory.candidate_coverage_complete = false,
            }
            assert!(validate_tasks_before_shutdown(&inventory, &[saved.clone()]).is_err());
        }
    }

    #[test]
    fn final_shutdown_check_rejects_changed_or_missing_original_turn_and_permissions() {
        let original = original_task();
        let saved = paused(&original);
        for changed in 0..5 {
            let mut inventory = TaskInventory {
                idle: vec![original.clone()],
                candidate_coverage_complete: true,
                ..TaskInventory::default()
            };
            match changed {
                0 => inventory.idle.clear(),
                1 => inventory.idle[0].turn_id = Some("user-new-turn".into()),
                2 => inventory.idle[0].status = Some("completed".into()),
                3 => inventory.idle[0].context.model = Some("user-new-model".into()),
                _ => inventory.idle[0].context.current_permissions = json!("user-new-permissions"),
            }
            assert!(validate_tasks_before_shutdown(&inventory, &[saved.clone()]).is_err());
        }
    }

    #[test]
    fn final_shutdown_accepts_exact_quota_failure_but_rejects_changed_recovery_intent() {
        let mut original = original_task();
        original.status = Some("failed".into());
        original.runtime_status = "systemError".into();
        original.turn_error_code = Some("usageLimitExceeded".into());
        original.turn_ended_at_ms = Some(1_000);
        let saved: PausedTask = serde_json::from_value(json!({
            "threadId":original.thread_id, "turnId":original.turn_id,
            "context":original.context,"pauseOperationId":"operation-A",
            "confirmedByCcSwitch":false,"origin":"quotaExhausted"
            ,"turnEndedAtMs":original.turn_ended_at_ms
        }))
        .unwrap();
        let good = TaskInventory {
            quota_failed: vec![original.clone()],
            candidate_coverage_complete: true,
            ..TaskInventory::default()
        };
        assert!(validate_tasks_before_shutdown(&good, &[saved.clone()]).is_ok());
        for changed in 0..8 {
            let mut inventory = good.clone();
            let current = &mut inventory.quota_failed[0];
            match changed {
                0 => current.status = Some("interrupted".into()),
                1 => current.status = Some("completed".into()),
                2 => current.turn_id = Some("user-new-turn".into()),
                3 => current.turn_error_code = None,
                4 => current.waiting = true,
                5 => current.runtime_status = "active".into(),
                6 => current.context.model = Some("user-new-model".into()),
                _ => current.context.current_permissions = json!("changed-permissions"),
            }
            assert!(
                validate_tasks_before_shutdown(&inventory, &[saved.clone()]).is_err(),
                "case {changed}"
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
            Ok(session) => return Ok(session),
            Err(error) if tokio::time::Instant::now() >= deadline => {
                return Err(format!("重开后桌面协作连接未就绪：{error}"))
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
}
