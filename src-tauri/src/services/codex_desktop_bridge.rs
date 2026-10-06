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
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use tauri::{Emitter, Manager};

static FLOW_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

#[derive(Serialize)]
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
        if self.phase == "completed" || self.phase == "checking" {
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
            Some("completed" | "cancelled" | "superseded")
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
        use std::io::Write;
        let directory = crate::config::get_app_config_dir().join("codex-desktop-recovery");
        std::fs::create_dir_all(&directory).map_err(|_| "无法创建本次聊天恢复记录目录")?;
        let mut file =
            tempfile::NamedTempFile::new_in(&directory).map_err(|_| "无法保存本次聊天恢复记录")?;
        serde_json::to_writer(&mut file, self).map_err(|_| "本次聊天恢复记录编码失败")?;
        file.flush().map_err(|_| "无法刷新本次聊天恢复记录")?;
        file.as_file()
            .sync_all()
            .map_err(|_| "无法同步本次聊天恢复记录")?;
        file.persist(directory.join(format!("{}.json", self.operation_id)))
            .map_err(|_| "无法提交本次聊天恢复记录")?;
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
    )
    .await
    .map(|result| result.0)
}

async fn run_lifecycle(
    app: tauri::AppHandle,
    source: String,
    target: String,
    generation: u64,
    automatic: bool,
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
        planned_tasks: Vec::new(),
        paused_tasks: Vec::new(),
        resume_intents: Vec::new(),
        settings_restore_intents: Vec::new(),
        settings_restored_tasks: Vec::new(),
        resumed_tasks: Vec::new(),
        resume_confirmations: Vec::new(),
    };
    let desktop_receipt = restart::desktop_receipt()?;
    let desktop_running = desktop_receipt.is_some();
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
                "桌面有等待审批或状态未确认的任务（需处理 {}，未确认 {}）；未关闭桌面",
                inventory.blocked.len(),
                inventory.unresolved_thread_ids.len()
            ));
        }
        if !inventory.running.is_empty() && expected_identity.is_none() {
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
                .find(|task| task.thread_id == paused.thread_id)
                .ok_or("本次暂停的原聊天状态未确认；未继续关闭桌面")?;
            if actual.turn_id.as_deref() != Some(paused.turn_id.as_str())
                || actual.status.as_deref() != Some("interrupted")
                || actual.context != paused.context
            {
                return Err("本次暂停的原轮次、模型或权限已改变；旧恢复计划失效".into());
            }
        }
    }
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
    if restarted && !record.paused_tasks.is_empty() {
        let receipt = restart::last_identity_receipt().ok_or("本次重开记录已失效；未恢复原聊天")?;
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
            "正在恢复本次暂停的原聊天，沿用原模型和权限",
        );
        for paused in record.paused_tasks.clone() {
            resume_guard()?;
            if let Some(expected) = expected_identity.as_ref() {
                confirm_reopened_identity(expected, &resume_guard).await?;
            }
            // Subscribe before navigation so the original owner registering
            // during startup cannot be missed by a one-shot subscription.
            let mut session = connect_reopened_desktop(&resume_guard).await?;
            session
                .begin_follow(&paused.thread_id)
                .await
                .map_err(|e| e.to_string())?;
            codex_desktop_session::open_original_chat(&paused.thread_id)
                .map_err(|e| e.to_string())?;
            restart::wait_for_navigation(&navigation_receipt, guard.as_ref()).await?;
            if let Some(expected) = expected_identity.as_ref() {
                confirm_reopened_identity(expected, &resume_guard).await?;
            }
            monitor::lifecycle_status(
                &app,
                generation,
                "waiting-for-chat",
                "正在等待原聊天加载并核对本次暂停的轮次，尚未发送继续",
            );
            session
                .wait_for_reopened_paused_chat(&paused, || resume_guard().is_ok())
                .await
                .map_err(|e| e.to_string())?;
            monitor::lifecycle_status(
                &app,
                generation,
                "resuming",
                "正在用原聊天恢复请求继续任务，保留本次暂停前的模型和权限",
            );
            if let Some(expected) = expected_identity.as_ref() {
                confirm_reopened_identity(expected, &resume_guard).await?;
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
    }
    record.phase = "completed".into();
    record.persist()?;
    let message = if restarted {
        format!(
            "目标账号已启用，Codex 已正常重开；恢复本次暂停的 {} 个原任务",
            record.resumed_tasks.len()
        )
    } else {
        "目标账号已启用；Codex 原本未运行，未额外启动桌面".into()
    };
    monitor::lifecycle_status(&app, generation, "completed", &message);
    Ok((restarted, message))
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
            .find(|task| task.thread_id == saved.thread_id)
            .ok_or("正常退出前原聊天状态未确认；未关闭桌面")?;
        if current.turn_id.as_deref() != Some(saved.turn_id.as_str())
            || current.status.as_deref() != Some("interrupted")
            || current.context != saved.context
        {
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
