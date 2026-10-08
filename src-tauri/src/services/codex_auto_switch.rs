//! Background Codex managed-account monitor.
//!
//! This service deliberately owns no browser or renderer state. It reads the
//! same ordered provider list and uses the same managed OAuth quota query and
//! provider activation as CC Switch's account cards. The restart and recovery
//! service is shared by automatic selection and manual activation.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::app_config::AppType;
use crate::commands::{publish_codex_oauth_quota, query_codex_oauth_quota_for};
use crate::provider::Provider;
use crate::services::subscription::{
    CodexLimitPolicy, SubscriptionQuota, TIER_FIVE_HOUR, TIER_SEVEN_DAY,
};
use crate::services::ProviderService;
use crate::store::AppState;

const ENABLED_KEY: &str = "codex_auto_switch_enabled_v1";
const EVENT_NAME: &str = "codex-auto-switch-status";
const CHECK_INTERVAL: Duration = Duration::from_secs(300);
const PLAN_MAX_AGE_MS: u128 = 180_000;
const SESSION_OBSERVATION_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAutoSwitchStatus {
    pub enabled: bool,
    pub phase: String,
    pub message: String,
    pub operation_id: Option<String>,
    pub current_provider_id: Option<String>,
    pub target_provider_id: Option<String>,
    pub checked_at: Option<u128>,
    /// Earliest known 5-hour reset while no replacement account is usable.
    /// This is a hint for the background waiter, never a permission to resume
    /// a task without a fresh quota query.
    pub wait_until: Option<i64>,
    pub candidate_failures: Vec<String>,
    pub can_cancel: bool,
}

impl Default for CodexAutoSwitchStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            phase: "disabled".into(),
            message: "自动换号已关闭".into(),
            operation_id: None,
            current_provider_id: None,
            target_provider_id: None,
            checked_at: None,
            wait_until: None,
            candidate_failures: Vec::new(),
            can_cancel: false,
        }
    }
}

struct Runtime {
    status: CodexAutoSwitchStatus,
    generation: u64,
    running: bool,
    started: bool,
    manual_generation: Option<u64>,
    started_at: Option<u128>,
    session_epoch: Option<u64>,
    last_desktop_followup_failure: Option<(String, String)>,
    last_check_failure: Option<CheckFailure>,
}

/// Repeated timer checks may rediscover an unresolved problem without making
/// a new switch attempt. Correlate those observations by their actual context,
/// not by the fresh operation UUID or generation reserved for each check.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckFailure {
    phase: String,
    source: String,
    stage: String,
    current_provider_id: Option<String>,
    target_provider_id: Option<String>,
    message: String,
    candidate_failures: Vec<String>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            status: CodexAutoSwitchStatus::default(),
            generation: 0,
            running: false,
            started: false,
            manual_generation: None,
            started_at: None,
            session_epoch: None,
            last_desktop_followup_failure: None,
            last_check_failure: None,
        }
    }
}

fn runtime() -> &'static Mutex<Runtime> {
    static RUNTIME: OnceLock<Mutex<Runtime>> = OnceLock::new();
    RUNTIME.get_or_init(|| Mutex::new(Runtime::default()))
}

fn lock_runtime() -> std::sync::MutexGuard<'static, Runtime> {
    runtime()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn publish(app: &tauri::AppHandle, status: &CodexAutoSwitchStatus) {
    // Native diagnostics remain available while the renderer is hidden or
    // destroyed. Only operation state is recorded, never quota credentials.
    log::info!(
        "[CodexAutoSwitch] phase={} enabled={} operation={} message={}",
        status.phase,
        status.enabled,
        status.operation_id.as_deref().unwrap_or("none"),
        status.message
    );
    if let Err(error) = app.emit(EVENT_NAME, status.clone()) {
        log::debug!("Codex auto-switch status event unavailable: {error}");
    }
}

fn set_status(
    app: &tauri::AppHandle,
    generation: u64,
    update: impl FnOnce(&mut CodexAutoSwitchStatus),
) -> bool {
    let status = {
        let mut inner = lock_runtime();
        if inner.generation != generation {
            return false;
        }
        update(&mut inner.status);
        inner.status.clone()
    };
    publish(app, &status);
    true
}

pub fn lifecycle_status(app: &tauri::AppHandle, generation: u64, phase: &str, message: &str) {
    set_status(app, generation, |status| {
        status.phase = phase.into();
        status.message = message.into();
    });
}

fn finish(app: &tauri::AppHandle, generation: u64, phase: &str, message: String) {
    let has_uncertain_recovery = matches!(phase, "blocked" | "failed")
        && (crate::services::codex_desktop_bridge::pending_recovery_reason().is_some()
            || crate::services::codex_desktop_bridge::quota_reset_wait()
                .ok()
                .flatten()
                .is_some());
    let (status, failure_context) = {
        let mut inner = lock_runtime();
        if inner.generation != generation {
            return;
        }
        inner.running = false;
        inner.started_at = None;
        let failed_stage = inner.status.phase.clone();
        let failed_operation = inner.status.operation_id.clone();
        let source = if inner.manual_generation == Some(generation) {
            "manual"
        } else if inner.status.wait_until.is_some() {
            "reset-wait"
        } else {
            "automatic"
        };
        inner.status.phase = phase.into();
        inner.status.message = message;
        inner.status.can_cancel = has_uncertain_recovery;
        let failure_context = if should_record_failure(&mut inner, phase, source, &failed_stage) {
            Some((
                inner.status.current_provider_id.clone(),
                inner.status.target_provider_id.clone(),
                failed_stage,
                failed_operation,
                inner.status.candidate_failures.clone(),
                source,
            ))
        } else {
            None
        };
        inner.status.operation_id = None;
        inner.status.target_provider_id = None;
        inner.status.wait_until = None;
        if phase == "completed" {
            inner.last_desktop_followup_failure = None;
        }
        let status = inner.status.clone();
        (status, failure_context)
    };
    if let Some((current_provider_id, target_provider_id, stage, operation_id, failures, source)) =
        failure_context
    {
        crate::services::codex_switch_history::record_failure_with_context(
            phase,
            &status.message,
            current_provider_id.as_deref(),
            target_provider_id.as_deref(),
            now_millis(),
            crate::services::codex_switch_history::FailureContext {
                operation_id: operation_id.as_deref(),
                source: Some(source),
                stage: Some(&stage),
                candidate_failures: &failures,
            },
        );
    }
    publish(app, &status);
}

fn should_record_failure(inner: &mut Runtime, phase: &str, source: &str, stage: &str) -> bool {
    if !matches!(phase, "blocked" | "failed" | "waiting") {
        if matches!(phase, "monitoring" | "completed") {
            inner.last_check_failure = None;
        }
        return false;
    }
    let failure = CheckFailure {
        phase: phase.into(),
        source: source.into(),
        stage: stage.into(),
        current_provider_id: inner.status.current_provider_id.clone(),
        target_provider_id: inner.status.target_provider_id.clone(),
        message: inner.status.message.clone(),
        candidate_failures: inner.status.candidate_failures.clone(),
    };
    let observation_only =
        source != "manual" && matches!(stage, "checking" | "selecting" | "preflight" | "waiting");
    if observation_only && inner.last_check_failure.as_ref() == Some(&failure) {
        return false;
    }
    // Explicit user retries and failures after side effects remain individual
    // audit events, even if the error text matches the previous attempt.
    inner.last_check_failure = Some(failure);
    true
}

fn begin_operation(app: &tauri::AppHandle, reason: &str, preempt: bool) -> Option<u64> {
    let session_epoch = crate::services::codex_session_watch::capture_epoch().ok()?;
    let (generation, status) = {
        let mut inner = lock_runtime();
        if (inner.running || inner.manual_generation.is_some()) && !preempt {
            return None;
        }
        inner.generation = inner.generation.wrapping_add(1);
        inner.running = true;
        inner.started_at = Some(now_millis());
        inner.session_epoch = Some(session_epoch);
        inner.status.phase = "checking".into();
        inner.status.message = reason.into();
        inner.status.operation_id = Some(uuid::Uuid::new_v4().to_string());
        inner.status.current_provider_id = None;
        inner.status.target_provider_id = None;
        inner.status.wait_until = None;
        inner.status.candidate_failures.clear();
        inner.status.can_cancel = true;
        (inner.generation, inner.status.clone())
    };
    publish(app, &status);
    Some(generation)
}

fn begin(app: &tauri::AppHandle, reason: &str) -> Option<u64> {
    begin_operation(app, reason, false)
}

/// Called once by Tauri setup. The loop lives in the native process, including
/// while the window is minimized or hidden to the tray. Missed ticks are
/// discarded, so unlocking a machine cannot replay queued switches.
pub fn start(app: tauri::AppHandle) {
    let enabled = app
        .state::<AppState>()
        .db
        .get_bool_flag(ENABLED_KEY)
        .unwrap_or(false);
    let watcher_error = crate::services::codex_session_watch::ensure_started().err();
    let saved_wait = crate::services::codex_desktop_bridge::quota_reset_wait();
    {
        let mut inner = lock_runtime();
        if inner.started {
            return;
        }
        inner.started = true;
        inner.status.enabled = enabled;
        inner.status.phase = if enabled { "monitoring" } else { "disabled" }.into();
        inner.status.message = if enabled {
            "后台监测已启用".into()
        } else {
            "自动换号已关闭".into()
        };
        if enabled {
            match saved_wait {
                Ok(Some(wait)) => {
                    inner.status.phase = "waiting-for-reset".into();
                    inner.status.message =
                        "已加载额度等待记录；重置时重新核对账号、桌面和原任务后再继续".into();
                    inner.status.wait_until = Some(wait.reset_at);
                    inner.status.current_provider_id = Some(wait.provider_id.clone());
                    inner.status.target_provider_id = Some(wait.provider_id);
                    inner.status.operation_id = Some(wait.operation_id);
                    inner.status.can_cancel = true;
                }
                Err(error) => {
                    inner.status.phase = "blocked".into();
                    inner.status.message = error;
                }
                Ok(None) => {}
            }
        }
        if let Some(error) = watcher_error {
            inner.status.phase = "blocked".into();
            inner.status.message = error;
        }
    }
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut next_check = tokio::time::Instant::now() + CHECK_INTERVAL;
        let mut observed_reset = None;
        loop {
            interval.tick().await;
            if !lock_runtime().status.enabled {
                continue;
            }
            let reset = lock_runtime().status.wait_until;
            let reset_due =
                reset.is_some_and(|at| at <= now_millis() as i64 && Some(at) != observed_reset);
            if tokio::time::Instant::now() < next_check && !reset_due {
                continue;
            }
            next_check = tokio::time::Instant::now() + CHECK_INTERVAL;
            if reset_due {
                observed_reset = reset;
            }
            // Do not begin quota work on a locked/secure desktop. A later tick
            // makes a new decision rather than continuing this locked tick.
            if let Err(reason) = automatic_session_ready() {
                let status = {
                    let mut inner = lock_runtime();
                    if inner.running || inner.manual_generation.is_some() {
                        continue;
                    }
                    inner.status.phase = "waiting".into();
                    inner.status.message = reason;
                    inner.status.clone()
                };
                publish(&app, &status);
                continue;
            }
            if let Some(generation) = begin(&app, "正在查询当前托管账号额度") {
                run_automatic(app.clone(), generation).await;
            }
        }
    });
}

pub fn get_status(_app: &tauri::AppHandle) -> CodexAutoSwitchStatus {
    lock_runtime().status.clone()
}

pub fn set_enabled(app: &tauri::AppHandle, enabled: bool) -> Result<CodexAutoSwitchStatus, String> {
    if enabled {
        crate::services::codex_session_watch::ensure_started()?;
    }
    let persisted = app
        .state::<AppState>()
        .db
        .set_setting(ENABLED_KEY, if enabled { "true" } else { "false" })
        .map_err(|e| e.to_string());
    if enabled {
        persisted.as_ref().map_err(|error| error.clone())?;
    }
    let status = {
        let mut inner = lock_runtime();
        apply_enabled_choice(
            &mut inner,
            enabled,
            crate::services::codex_desktop_bridge::invalidate_quota_waits,
        );
        if let Err(error) = &persisted {
            inner.status.message = format!(
                "本次自动换号已关闭，旧计划已停止，但开关保存失败：{error}；下次启动前需核对开关"
            );
        }
        inner.status.clone()
    };
    publish(app, &status);
    Ok(status)
}

fn apply_enabled_choice(
    inner: &mut Runtime,
    enabled: bool,
    cancel_wait: impl FnOnce() -> Result<(), String>,
) {
    let changed = inner.status.enabled != enabled;
    inner.status.enabled = enabled;
    // Idempotent reads/reconnects and a manual Enable do not lose owned work.
    if inner.manual_generation.is_some() || !changed {
        return;
    }
    inner.generation = inner.generation.wrapping_add(1);
    inner.running = false;
    inner.started_at = None;
    inner.last_check_failure = None;
    inner.status.phase = if enabled { "monitoring" } else { "disabled" }.into();
    inner.status.message = if enabled {
        "每 5 分钟后台监测已启用"
    } else {
        "自动换号已关闭"
    }
    .into();
    inner.status.operation_id = None;
    inner.status.target_provider_id = None;
    inner.status.wait_until = None;
    inner.status.can_cancel = false;
    if !enabled {
        if let Err(error) = cancel_wait() {
            inner.status.message =
                format!("自动换号已关闭，旧计划已失效；等待记录仍需核对：{error}");
            inner.status.can_cancel = true;
        }
    }
}

/// Discard the old plan before any explicit user provider change. A running
/// quota request may still finish, but every subsequent side effect must
/// revalidate the generation and the currently selected provider.
pub fn invalidate_pending(app: &tauri::AppHandle, reason: &str) {
    let status = invalidate_pending_without_app(reason);
    publish(app, &status);
}

/// Same generation invalidation for service paths that do not receive an
/// AppHandle (profiles and deep-link imports). The UI's status polling still
/// observes this state; callers with an AppHandle should use the event variant.
pub fn invalidate_pending_without_app(reason: &str) -> CodexAutoSwitchStatus {
    let mut inner = lock_runtime();
    invalidate_locked(&mut inner, reason);
    inner.status.clone()
}

fn invalidate_locked(inner: &mut Runtime, reason: &str) {
    if let Err(error) = crate::services::codex_desktop_bridge::invalidate_quota_waits() {
        log::warn!("取消额度等待记录失败：{error}");
    }
    inner.generation = inner.generation.wrapping_add(1);
    inner.running = false;
    inner.manual_generation = None;
    inner.started_at = None;
    inner.session_epoch = None;
    inner.last_desktop_followup_failure = None;
    inner.last_check_failure = None;
    inner.status.phase = if inner.status.enabled {
        "cancelled"
    } else {
        "disabled"
    }
    .into();
    inner.status.message = reason.into();
    inner.status.operation_id = None;
    inner.status.target_provider_id = None;
    inner.status.wait_until = None;
    inner.status.can_cancel = false;
}

/// Hold the same intent lock as original account activation while a user
/// changes a provider binding/live selection. A timer cannot reserve a new
/// plan from the old binding while the original service waits for its app
/// switch lock or commits the mutation. Lock order stays runtime -> provider,
/// matching activate_reserved_account; the closure must not call this module.
pub fn mutate_provider_selection<T>(reason: &str, mutation: impl FnOnce() -> T) -> T {
    let mut inner = lock_runtime();
    invalidate_locked(&mut inner, reason);
    let result = mutation();
    drop(inner);
    result
}

/// Reserve a manual activation before the original provider service runs.
/// Timer checks cannot seize the interval between enabling and restarting.
pub fn invalidate_pending_with_generation(reason: &str) -> u64 {
    let mut inner = lock_runtime();
    if let Err(error) = crate::services::codex_desktop_bridge::invalidate_quota_waits() {
        log::warn!("手动启用时取消额度等待记录失败：{error}");
    }
    inner.generation = inner.generation.wrapping_add(1);
    inner.running = false;
    inner.manual_generation = Some(inner.generation);
    inner.started_at = Some(now_millis());
    inner.session_epoch = crate::services::codex_session_watch::current_epoch_if_ready();
    inner.last_desktop_followup_failure = None;
    inner.last_check_failure = None;
    inner.status.phase = "enabling".into();
    inner.status.message = reason.into();
    inner.status.operation_id = Some(uuid::Uuid::new_v4().to_string());
    inner.status.target_provider_id = None;
    inner.status.can_cancel = true;
    inner.generation
}

pub fn operation_generation() -> u64 {
    lock_runtime().generation
}

pub fn operation_id_for_generation(generation: u64) -> Option<String> {
    let inner = lock_runtime();
    (inner.generation == generation)
        .then(|| inner.status.operation_id.clone())
        .flatten()
}

pub fn retain_owned_recovery(generation: u64) -> Result<(), String> {
    let mut inner = lock_runtime();
    validate_generation(&inner, generation, now_millis(), true)?;
    if !inner.running {
        return Err("额度等待恢复计划已停止".into());
    }
    inner.started_at = None;
    Ok(())
}

pub fn set_manual_activation_context(generation: u64, current: Option<String>, target: &str) {
    let mut inner = lock_runtime();
    if inner.generation == generation && inner.manual_generation == Some(generation) {
        inner.status.current_provider_id = current;
        inner.status.target_provider_id = Some(target.into());
    }
}

pub fn generation_is_current(generation: u64) -> Result<(), String> {
    let inner = lock_runtime();
    validate_generation(&inner, generation, now_millis(), true)
}

fn validate_generation(
    inner: &Runtime,
    generation: u64,
    now: u128,
    require_fresh: bool,
) -> Result<(), String> {
    if inner.generation != generation {
        return Err("本次换号已取消或被新的用户操作取代".into());
    }
    if let Some(epoch) = inner.session_epoch {
        crate::services::codex_session_watch::validate_epoch(epoch)?;
    }
    if require_fresh
        && inner
            .started_at
            .is_some_and(|at| now.saturating_sub(at) > PLAN_MAX_AGE_MS)
    {
        return Err("本次换号计划已过期；等待重新检查，不执行积压操作".into());
    }
    Ok(())
}

pub fn finish_manual_operation(generation: u64) {
    let mut inner = lock_runtime();
    if inner.manual_generation == Some(generation) {
        inner.manual_generation = None;
        inner.started_at = None;
        inner.status.can_cancel = false;
        inner.status.operation_id = None;
    }
}

/// Original activation, serialized with intent invalidation. Once a newer
/// user choice has been reserved, an older worker cannot write the live login.
pub fn activate_reserved_account(
    state: &AppState,
    id: &str,
    generation: u64,
) -> Result<crate::services::SwitchResult, crate::error::AppError> {
    activate_reserved_with(generation, || {
        ProviderService::switch(state, AppType::Codex, id)
    })
}

fn activate_reserved_with<T>(
    generation: u64,
    activate: impl FnOnce() -> Result<T, crate::error::AppError>,
) -> Result<T, crate::error::AppError> {
    let mut inner = lock_runtime();
    validate_generation(&inner, generation, now_millis(), true)
        .map_err(crate::error::AppError::Message)?;
    let result = activate()?;
    // Account activation has committed. Its owned restart/recovery must not
    // expire because an earlier quota sample aged while the desktop loaded.
    // Keep the generation unchanged: cancellation and user choices still win.
    inner.started_at = None;
    Ok(result)
}

/// Follow-up lifecycle for an account that the original Enable service has
/// already selected. Failures must remain warnings on that successful result.
pub async fn restart_activated_account(
    app: tauri::AppHandle,
    target: String,
    generation: u64,
) -> Result<bool, String> {
    let result = crate::services::codex_desktop_bridge::restart_activated_account(
        app.clone(),
        target.clone(),
        generation,
    )
    .await;
    record_desktop_followup_result(&mut lock_runtime(), generation, &target, &result);
    match &result {
        Ok(true) => {
            let status = get_status(&app);
            finish(&app, generation, "completed", status.message);
        }
        Ok(false) => finish(
            &app,
            generation,
            "completed",
            "账号已启用；桌面原本未运行".into(),
        ),
        Err(error) => finish(
            &app,
            generation,
            "blocked",
            format!("账号已启用，但桌面后续流程未完成：{error}"),
        ),
    }
    finish_manual_operation(generation);
    result
}

/// A quota sample proves account availability, not that the running desktop
/// finished restarting. Keep that distinction across later monitoring ticks.
fn record_desktop_followup_result(
    inner: &mut Runtime,
    generation: u64,
    provider_id: &str,
    result: &Result<bool, String>,
) {
    if inner.generation != generation {
        return;
    }
    inner.last_desktop_followup_failure = result
        .as_ref()
        .err()
        .map(|error| (provider_id.into(), error.clone()));
}

fn usable_account_outcome(inner: &Runtime, current_provider: &str) -> (&'static str, String) {
    if let Some((provider, error)) = &inner.last_desktop_followup_failure {
        if provider == current_provider {
            return (
                "blocked",
                format!(
                    "当前账号额度可用，但桌面重开或任务恢复尚未完成：{error}；请点击当前账号“重开 Codex”完成后续流程"
                ),
            );
        }
    }
    ("monitoring", "当前账号额度仍可使用".into())
}

pub fn plan_is_current(
    app: &tauri::AppHandle,
    generation: u64,
    expected_provider: &str,
) -> Result<(), String> {
    automatic_selection_is_current(app, generation, expected_provider, true)
}

fn discard_automatic_generation(app: &tauri::AppHandle, generation: u64, reason: &str) {
    let status = {
        let mut inner = lock_runtime();
        if inner.generation != generation || !inner.running || inner.manual_generation.is_some() {
            return;
        }
        invalidate_locked(&mut inner, reason);
        inner.status.clone()
    };
    publish(app, &status);
}

fn automatic_selection_is_current(
    app: &tauri::AppHandle,
    generation: u64,
    expected_provider: &str,
    require_fresh: bool,
) -> Result<(), String> {
    {
        let inner = lock_runtime();
        if !inner.running || inner.generation != generation {
            return Err("自动换号计划已取消或被新的用户操作取代".into());
        }
        if let Err(error) = validate_generation(&inner, generation, now_millis(), require_fresh) {
            drop(inner);
            discard_automatic_generation(app, generation, &error);
            return Err(error);
        }
    }
    let actual = ProviderService::current(&app.state::<AppState>(), AppType::Codex)
        .map_err(|e| e.to_string())?;
    if actual != expected_provider {
        let message = "当前账号已改变，旧换号计划失效".to_string();
        discard_automatic_generation(app, generation, &message);
        return Err(message);
    }
    if let Err(error) = crate::services::codex_desktop_restart::ensure_interactive_session() {
        discard_automatic_generation(app, generation, &error);
        return Err(error);
    }
    validate_generation(&lock_runtime(), generation, now_millis(), require_fresh)
}

fn automatic_session_ready() -> Result<(), String> {
    let epoch = crate::services::codex_session_watch::capture_epoch()?;
    crate::services::codex_desktop_restart::ensure_interactive_session()?;
    crate::services::codex_session_watch::validate_epoch(epoch)
}

fn managed_account_id(provider: &Provider) -> Option<String> {
    // Official Codex account cards are marked `category = official` with an
    // authBinding; they do not necessarily set `providerType = codex_oauth`.
    if provider.category.as_deref() != Some("official") && !provider.is_codex_oauth() {
        return None;
    }
    provider
        .meta
        .as_ref()?
        .managed_account_id_for("codex_oauth")
        .filter(|id| !id.trim().is_empty())
}

#[derive(Debug, PartialEq)]
enum QuotaDecision {
    Usable,
    Exhausted,
}

fn five_hour_reset_at(quota: &SubscriptionQuota) -> Option<i64> {
    let value = quota
        .tiers
        .iter()
        .find(|tier| tier.name == TIER_FIVE_HOUR)
        .and_then(|tier| tier.resets_at.as_deref())?;
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date| date.timestamp_millis())
}

/// A reset is useful for waiting only when the weekly window is still usable.
/// A weekly-exhausted account must never be selected merely because its 5-hour
/// timer is earlier.
fn candidate_wait_reset_at(quota: &SubscriptionQuota, now: i64) -> Option<i64> {
    let five = window_utilization(quota, TIER_FIVE_HOUR)?;
    let weekly = window_utilization(quota, TIER_SEVEN_DAY)?;
    if !quota.success
        || !matches!(
            quota.credential_status,
            crate::services::subscription::CredentialStatus::Valid
        )
        || weekly >= 100.0
        || five < 95.0
    {
        return None;
    }
    let reset = five_hour_reset_at(quota)?;
    reset
        .checked_sub(now)
        .filter(|delay| *delay > 0 && *delay <= 24 * 60 * 60 * 1_000)
        .map(|_| reset)
}

fn candidate_wait_reset(quota: &SubscriptionQuota) -> Option<i64> {
    candidate_wait_reset_at(quota, now_millis() as i64)
}

fn window_utilization(quota: &SubscriptionQuota, name: &str) -> Option<f64> {
    quota
        .tiers
        .iter()
        .find(|tier| tier.name == name)
        .map(|tier| tier.utilization)
        // Over-limit values (>100) prove exhaustion; invalid values are unknown.
        .filter(|used| used.is_finite() && *used >= 0.0)
}

fn has_weekly_only_policy(quota: &SubscriptionQuota) -> bool {
    quota.codex_limit_policy == Some(CodexLimitPolicy::WeeklyOnly)
        && matches!(quota.tool.as_str(), "codex" | "codex_oauth")
        && !quota.tiers.is_empty()
        && quota.tiers.iter().all(|tier| tier.name == TIER_SEVEN_DAY)
}

/// Missing windows stay unknown unless the API explicitly confirmed weekly-only Pro.
fn quota_decision(quota: &SubscriptionQuota) -> Result<QuotaDecision, String> {
    if !quota.success {
        return Err(quota
            .error
            .clone()
            .or_else(|| quota.credential_message.clone())
            .unwrap_or_else(|| "额度查询未成功".into()));
    }
    if !matches!(
        quota.credential_status,
        crate::services::subscription::CredentialStatus::Valid
    ) {
        return Err("账号登录授权状态未确认".into());
    }
    let five_hour_used = window_utilization(quota, TIER_FIVE_HOUR);
    let weekly_used = window_utilization(quota, TIER_SEVEN_DAY);
    // Any exhausted known window proves exhaustion. Weekly-only Pro has no
    // five-hour requirement; never infer that policy from a missing tier alone.
    if five_hour_used.is_some_and(|used| used > 95.0)
        || weekly_used.is_some_and(|used| used >= 100.0)
    {
        return Ok(QuotaDecision::Exhausted);
    }
    if weekly_used.is_none() || (five_hour_used.is_none() && !has_weekly_only_policy(quota)) {
        return Err("缺少有效的 5 小时或周额度窗口".into());
    }
    Ok(QuotaDecision::Usable)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CandidateCapacity {
    FiveHour(f64),
    WeeklyOnly,
}

impl CandidateCapacity {
    // Weekly-only and a completely unused five-hour window share the highest
    // availability band. Ties preserve original list order without inventing
    // a five-hour utilization value for Pro.
    fn better_than(self, other: Self) -> bool {
        match (self, other) {
            (Self::FiveHour(used), Self::FiveHour(previous)) => used < previous,
            (Self::WeeklyOnly, Self::FiveHour(previous)) => previous > 0.0,
            _ => false,
        }
    }

    fn is_maximum(self) -> bool {
        matches!(self, Self::WeeklyOnly | Self::FiveHour(0.0))
    }
}

/// A replacement needs strictly more than 5% when it has a five-hour window;
/// explicitly weekly-only Pro needs a valid, unexhausted weekly window.
fn candidate_capacity(quota: &SubscriptionQuota) -> Result<Option<CandidateCapacity>, String> {
    if !matches!(
        quota.credential_status,
        crate::services::subscription::CredentialStatus::Valid
    ) {
        return Err("候选账号登录授权状态未确认；未作为可用账号".into());
    }
    if quota_decision(quota)? != QuotaDecision::Usable {
        return Ok(None);
    }
    if has_weekly_only_policy(quota) {
        return Ok(Some(CandidateCapacity::WeeklyOnly));
    }
    Ok(window_utilization(quota, TIER_FIVE_HOUR)
        .filter(|used| *used < 95.0)
        .map(CandidateCapacity::FiveHour))
}

async fn query_account(
    app: &tauri::AppHandle,
    account_id: &str,
    generation: u64,
    current_provider: &str,
) -> Result<SubscriptionQuota, String> {
    let state = app.state::<AppState>();
    let manager = state.codex_oauth_manager.clone();
    let queried_account = account_id.to_owned();
    let result = committed_query_with_session_watch(
        async move { query_codex_oauth_quota_for(&manager, &queried_account).await },
        || automatic_selection_is_current(app, generation, current_provider, false),
    )
    .await;
    // Each request is bounded and continuously checks the live generation,
    // account and session. Serial candidate work may exceed the age of the
    // first source sample; final selection rechecks that source if necessary.
    // Progress must never renew a generation invalidated during the request.
    {
        let mut inner = lock_runtime();
        validate_generation(&inner, generation, now_millis(), false)?;
        if !inner.running {
            return Err("自动换号计划已停止，未使用返回的额度".into());
        }
        inner.started_at = Some(now_millis());
    }
    if let Ok(quota) = &result {
        publish_codex_oauth_quota(app, &state, account_id, quota);
        log::info!(
            "[CodexAutoSwitchQuota] account={} success={} five_hour_used={:?} weekly_used={:?} queried_at={:?}",
            account_id,
            quota.success,
            window_utilization(quota, TIER_FIVE_HOUR),
            window_utilization(quota, TIER_SEVEN_DAY),
            quota.queried_at,
        );
    }
    result
}

/// Dropping a quota request can drop an OAuth refresh after the server rotated
/// its token but before our existing manager persisted the reply. Cancel the
/// switching decision promptly while allowing this already-started request to
/// finish its native commit; its result cannot authorize the cancelled plan.
async fn committed_query_with_session_watch<T: Send + 'static>(
    query: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
    check: impl Fn() -> Result<(), String>,
) -> Result<T, String> {
    check()?;
    let task = tokio::spawn(query);
    // Tokio detaches on JoinHandle drop. Do not abort the task on a watcher
    // error or timeout: account refresh mutex/CAS still protect its commit.
    query_with_session_watch(
        async move { task.await.map_err(|_| "托管额度查询任务失败".to_string())? },
        check,
    )
    .await
}

async fn query_with_session_watch<T>(
    query: impl std::future::Future<Output = Result<T, String>>,
    check: impl Fn() -> Result<(), String>,
) -> Result<T, String> {
    check()?;
    tokio::pin!(query);
    let timeout = tokio::time::sleep(Duration::from_secs(45));
    tokio::pin!(timeout);
    let mut observation = tokio::time::interval(SESSION_OBSERVATION_INTERVAL);
    observation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    observation.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = &mut timeout => return Err("额度查询超时；本次不依据旧额度切号".into()),
            _ = observation.tick() => check()?,
            result = &mut query => {
                check()?;
                return result;
            }
        }
    }
}

fn sample_is_stale(checked_at: u128, now: u128) -> bool {
    now.saturating_sub(checked_at) > PLAN_MAX_AGE_MS
}

/// Recheck only stale source/selected-target samples, never the full list.
/// Returning false means the source recovered and no desktop work is needed.
async fn revalidate_quota_samples<Q, F, C, N>(
    source_account: &str,
    target_account: &str,
    source_checked_at: u128,
    target_checked_at: u128,
    mut query: Q,
    check: C,
    now: N,
) -> Result<bool, String>
where
    Q: FnMut(String) -> F,
    F: std::future::Future<Output = Result<SubscriptionQuota, String>>,
    C: Fn() -> Result<(), String>,
    N: Fn() -> u128,
{
    check()?;
    if sample_is_stale(source_checked_at, now()) {
        let source = query(source_account.into()).await?;
        check()?;
        if quota_decision(&source)? == QuotaDecision::Usable {
            return Ok(false);
        }
    }
    if sample_is_stale(target_checked_at, now()) {
        let target = query(target_account.into()).await?;
        check()?;
        if candidate_capacity(&target)?.is_none() {
            return Err("已选择的目标账号额度已不可用；本次不暂停或重开桌面".into());
        }
    }
    check()?;
    Ok(true)
}

async fn run_automatic(app: tauri::AppHandle, generation: u64) {
    let state = app.state::<AppState>();
    let current = match ProviderService::current(&state, AppType::Codex) {
        Ok(id) if !id.is_empty() => id,
        Ok(_) => {
            finish(
                &app,
                generation,
                "blocked",
                "当前没有启用的 Codex 账号".into(),
            );
            return;
        }
        Err(error) => {
            finish(
                &app,
                generation,
                "failed",
                format!("读取当前账号失败：{error}"),
            );
            return;
        }
    };
    let providers = match ProviderService::list(&state, AppType::Codex) {
        Ok(providers) => providers,
        Err(error) => {
            finish(
                &app,
                generation,
                "failed",
                format!("读取账号列表失败：{error}"),
            );
            return;
        }
    };
    let Some(current_provider) = providers.get(&current) else {
        finish(&app, generation, "blocked", "当前账号不在账号列表中".into());
        return;
    };
    let Some(current_account_id) = managed_account_id(current_provider) else {
        finish(
            &app,
            generation,
            "blocked",
            "当前账号不是已托管的 Codex OAuth 账号".into(),
        );
        return;
    };
    let waiting = match crate::services::codex_desktop_bridge::quota_reset_wait() {
        Ok(waiting) => waiting,
        Err(error) => {
            finish(&app, generation, "blocked", error);
            return;
        }
    };
    set_status(&app, generation, |status| {
        status.current_provider_id = Some(current.clone());
        status.checked_at = Some(now_millis());
        if let Some(wait) = waiting
            .as_ref()
            .filter(|wait| wait.provider_id == current && wait.account_id == current_account_id)
        {
            // Even a failed quota request belongs to the original saved wait,
            // allowing diagnostics to correlate retries across process restarts.
            status.operation_id = Some(wait.operation_id.clone());
            status.target_provider_id = Some(wait.provider_id.clone());
            status.wait_until = Some(wait.reset_at);
        }
    });
    if plan_is_current(&app, generation, &current).is_err() {
        return;
    }
    let current_quota = match query_account(&app, &current_account_id, generation, &current).await {
        Ok(quota) => quota,
        Err(error) => {
            finish(
                &app,
                generation,
                "blocked",
                format!("当前账号额度查询失败：{error}"),
            );
            return;
        }
    };
    let current_checked_at = now_millis();
    if plan_is_current(&app, generation, &current).is_err() {
        return;
    }
    // A previous desktop recovery must not freeze quota monitoring. Refresh
    // the current account first, then reconcile only provably obsolete tickets.
    if let Err(error) = crate::services::codex_desktop_bridge::reconcile_obsolete_recovery(
        &app, generation, &current,
    )
    .await
    {
        finish(&app, generation, "blocked", error);
        return;
    }
    if plan_is_current(&app, generation, &current).is_err() {
        return;
    }
    if let Some(reason) = crate::services::codex_desktop_bridge::pending_recovery_reason() {
        finish(&app, generation, "blocked", reason);
        return;
    }
    if let Some(wait) = waiting {
        if wait.provider_id != current || wait.account_id != current_account_id {
            if let Err(error) = crate::services::codex_desktop_bridge::invalidate_quota_waits() {
                finish(&app, generation, "blocked", error);
            } else {
                finish(
                    &app,
                    generation,
                    "cancelled",
                    "当前账号或绑定已改变，旧额度等待计划已失效".into(),
                );
            }
            return;
        }
        set_status(&app, generation, |status| {
            status.operation_id = Some(wait.operation_id.clone());
            status.target_provider_id = Some(wait.provider_id.clone());
            status.wait_until = Some(wait.reset_at);
        });
        match candidate_capacity(&current_quota) {
            Ok(Some(_)) => {
                match crate::services::codex_desktop_bridge::resume_quota_reset_wait(
                    app.clone(),
                    wait,
                    generation,
                )
                .await
                {
                    Ok(message) => finish(&app, generation, "completed", message),
                    Err(error) => finish(&app, generation, "blocked", error),
                }
            }
            Ok(None) => {
                let reset = candidate_wait_reset(&current_quota).unwrap_or(wait.reset_at);
                if let Err(error) = crate::services::codex_desktop_bridge::update_quota_wait_time(
                    &wait.operation_id,
                    reset,
                ) {
                    finish(&app, generation, "blocked", error);
                } else {
                    finish_wait(
                        &app,
                        generation,
                        &wait.provider_id,
                        reset,
                        "等待账号额度尚未恢复到可用阈值；已保存原任务，不重复重启，重置后重新查询"
                            .into(),
                    );
                }
            }
            Err(error) => finish(
                &app,
                generation,
                "blocked",
                format!("额度等待账号查询未确认：{error}；未继续原任务"),
            ),
        }
        return;
    }
    match quota_decision(&current_quota) {
        Ok(QuotaDecision::Usable) => {
            let (phase, mut message) = usable_account_outcome(&lock_runtime(), &current);
            if phase == "monitoring" && has_weekly_only_policy(&current_quota) {
                message = "当前 Pro 账号无 5 小时窗口，周额度仍可使用；每 5 分钟检查周额度".into();
            }
            finish(&app, generation, phase, message);
            return;
        }
        Err(error) => {
            finish(
                &app,
                generation,
                "blocked",
                format!("当前账号额度不明确：{error}"),
            );
            return;
        }
        Ok(QuotaDecision::Exhausted) => {}
    }

    set_status(&app, generation, |status| {
        status.phase = "selecting".into();
        status.message = "当前账号达到切换阈值，依次查询并选择 5 小时剩余额度最多的账号".into();
    });
    let selection = select_most_remaining_with_reset(
        providers,
        &current,
        &current_account_id,
        Some(&current_quota),
        |account| {
            let app = app.clone();
            let current = current.clone();
            async move { query_account(&app, &account, generation, &current).await }
        },
        || plan_is_current(&app, generation, &current),
    )
    .await;
    let (target, failures, earliest_reset) = match selection {
        Ok(selection) => selection,
        Err(error) => {
            finish(&app, generation, "cancelled", error);
            return;
        }
    };
    set_status(&app, generation, |status| {
        status.candidate_failures = failures
    });
    if let Some(target) = target {
        let target_checked_at = target.checked_at;
        log::info!(
            "[CodexAutoSwitchSelection] provider={} capacity={:?} checked_at={} five_hour_candidate_requires_used_below=95",
            target.provider_id,
            target.capacity,
            target_checked_at,
        );
        let target = target.provider_id;
        let target_account = match state
            .db
            .get_provider_by_id(&target, AppType::Codex.as_str())
        {
            Ok(Some(provider)) => managed_account_id(&provider),
            _ => None,
        };
        let Some(target_account) = target_account else {
            finish(
                &app,
                generation,
                "cancelled",
                "已选择的目标账号已移除或不再托管；等待重新检查".into(),
            );
            return;
        };
        let recheck = revalidate_quota_samples(
            &current_account_id,
            &target_account,
            current_checked_at,
            target_checked_at,
            |account| {
                let app = app.clone();
                let current = current.clone();
                async move { query_account(&app, &account, generation, &current).await }
            },
            || plan_is_current(&app, generation, &current),
            now_millis,
        )
        .await;
        match recheck {
            Ok(true) => {}
            Ok(false) => {
                let (phase, message) = usable_account_outcome(&lock_runtime(), &current);
                finish(
                    &app,
                    generation,
                    phase,
                    if phase == "monitoring" {
                        "当前账号额度已经恢复，本次不切号".into()
                    } else {
                        message
                    },
                );
                return;
            }
            Err(error) => {
                finish(
                    &app,
                    generation,
                    "blocked",
                    format!("切号前额度重新核对未通过：{error}"),
                );
                return;
            }
        }
        set_status(&app, generation, |status| {
            status.phase = "preflight".into();
            status.message = "正在核对桌面任务和目标账号".into();
            status.target_provider_id = Some(target.clone());
        });
        match crate::services::codex_desktop_bridge::switch_desktop_account(
            app.clone(),
            current,
            target,
            generation,
        )
        .await
        {
            Ok(message) => finish(&app, generation, "completed", message),
            Err(error) => finish(&app, generation, "blocked", error),
        }
    } else if let Some(wait) = earliest_reset {
        let target = wait.provider_id;
        let target_account = providers_for_reset(&state, &target);
        let Some(target_account) = target_account else {
            finish(
                &app,
                generation,
                "cancelled",
                "最早重置账号的绑定已改变；等待重新检查".into(),
            );
            return;
        };
        if sample_is_stale(current_checked_at, now_millis()) {
            match query_account(&app, &current_account_id, generation, &current).await {
                Ok(quota) if quota_decision(&quota) == Ok(QuotaDecision::Usable) => {
                    finish(
                        &app,
                        generation,
                        "monitoring",
                        "当前账号额度已经恢复；本次不切至耗尽账号等待".into(),
                    );
                    return;
                }
                Ok(quota) if quota_decision(&quota) == Ok(QuotaDecision::Exhausted) => {}
                Ok(_) => {
                    finish(
                        &app,
                        generation,
                        "blocked",
                        "当前账号额度重新核对未确认；未切号或重开".into(),
                    );
                    return;
                }
                Err(error) => {
                    finish(
                        &app,
                        generation,
                        "blocked",
                        format!("当前账号额度重新核对失败：{error}"),
                    );
                    return;
                }
            }
        }
        // The first sample may have aged during sequential account queries.
        // Revalidate the selected reset account only, never replay activation
        // based on an expired sample or a stale user selection.
        if sample_is_stale(wait.checked_at, now_millis()) {
            match query_account(&app, &target_account, generation, &current).await {
                Ok(quota) if candidate_wait_reset(&quota) == Some(wait.reset_at) => {}
                Ok(_) => {
                    finish(
                        &app,
                        generation,
                        "waiting",
                        "最早重置账号的额度或重置时间已改变；等待重新选择".into(),
                    );
                    return;
                }
                Err(error) => {
                    finish(
                        &app,
                        generation,
                        "blocked",
                        format!("等待账号重新核对失败：{error}"),
                    );
                    return;
                }
            }
        }
        set_status(&app, generation, |status| {
            status.phase = "preflight".into();
            status.target_provider_id = Some(target.clone());
            status.wait_until = Some(wait.reset_at);
            status.message = "无立即可用账号，正在切至最早 5 小时重置账号并保存原任务".into();
        });
        match crate::services::codex_desktop_bridge::switch_and_wait_for_reset(
            app.clone(),
            current,
            target.clone(),
            generation,
            wait.reset_at,
        )
        .await
        {
            Ok(message) => finish_wait(&app, generation, &target, wait.reset_at, message),
            Err(error) => finish(&app, generation, "blocked", error),
        }
    } else {
        finish(&app, generation, "waiting",
            "所有托管账号均不可用；没有周额度仍可用且 5 小时重置时间明确的账号，等待后续额度恢复，未重启桌面".into());
    }
}

fn providers_for_reset(state: &AppState, id: &str) -> Option<String> {
    state
        .db
        .get_provider_by_id(id, AppType::Codex.as_str())
        .ok()
        .flatten()
        .and_then(|provider| managed_account_id(&provider))
}

fn finish_wait(
    app: &tauri::AppHandle,
    generation: u64,
    target: &str,
    reset_at: i64,
    message: String,
) {
    let status = {
        let mut inner = lock_runtime();
        if inner.generation != generation {
            return;
        }
        inner.running = false;
        inner.started_at = None;
        inner.status.phase = "waiting-for-reset".into();
        inner.status.current_provider_id = Some(target.into());
        inner.status.target_provider_id = Some(target.into());
        inner.status.wait_until = Some(reset_at);
        inner.status.message = message;
        inner.status.can_cancel = true;
        inner.status.clone()
    };
    publish(app, &status);
}

#[derive(Debug)]
struct ResetCandidate {
    provider_id: String,
    reset_at: i64,
    checked_at: u128,
}

fn update_earliest_reset(
    selected: &mut Option<ResetCandidate>,
    id: &str,
    quota: &SubscriptionQuota,
) {
    if let Some(reset_at) = candidate_wait_reset(quota) {
        if selected
            .as_ref()
            .is_none_or(|best| reset_at < best.reset_at)
        {
            *selected = Some(ResetCandidate {
                provider_id: id.into(),
                reset_at,
                checked_at: quota
                    .queried_at
                    .and_then(|time| time.try_into().ok())
                    .unwrap_or_else(now_millis),
            });
        }
    }
}

#[derive(Debug)]
struct SelectedCandidate {
    provider_id: String,
    capacity: CandidateCapacity,
    checked_at: u128,
}

/// Query candidates in their original order and select the most 5h remaining.
/// Equal results keep the earlier account. A weekly-only Pro account or an
/// unused five-hour window reaches the maximum band and stops further queries.
#[cfg(test)]
async fn select_most_remaining<Q, F, C>(
    providers: indexmap::IndexMap<String, Provider>,
    current: &str,
    current_account: &str,
    query: Q,
    check: C,
) -> Result<(Option<SelectedCandidate>, Vec<String>), String>
where
    Q: FnMut(String) -> F,
    F: std::future::Future<Output = Result<SubscriptionQuota, String>>,
    C: Fn() -> Result<(), String>,
{
    let (selected, failures, _) =
        select_most_remaining_with_reset(providers, current, current_account, None, query, check)
            .await?;
    Ok((selected, failures))
}

async fn select_most_remaining_with_reset<Q, F, C>(
    providers: indexmap::IndexMap<String, Provider>,
    current: &str,
    current_account: &str,
    current_quota: Option<&SubscriptionQuota>,
    mut query: Q,
    check: C,
) -> Result<
    (
        Option<SelectedCandidate>,
        Vec<String>,
        Option<ResetCandidate>,
    ),
    String,
>
where
    Q: FnMut(String) -> F,
    F: std::future::Future<Output = Result<SubscriptionQuota, String>>,
    C: Fn() -> Result<(), String>,
{
    let mut seen = HashSet::from([current_account.to_owned()]);
    let mut failures = Vec::new();
    let mut queried = false;
    let mut selected: Option<SelectedCandidate> = None;
    let mut earliest_reset: Option<ResetCandidate> = None;
    for (id, provider) in providers {
        if id == current {
            if let Some(quota) = current_quota {
                update_earliest_reset(&mut earliest_reset, &id, quota);
            }
            continue;
        }
        let Some(account) = managed_account_id(&provider) else {
            continue;
        };
        if !seen.insert(account.clone()) {
            continue;
        }
        check()?;
        queried = true;
        let result = query(account).await;
        check()?;
        match result.and_then(|quota| {
            update_earliest_reset(&mut earliest_reset, &id, &quota);
            candidate_capacity(&quota).map(|capacity| {
                capacity.map(|capacity| SelectedCandidate {
                    provider_id: id.clone(),
                    capacity,
                    checked_at: quota
                        .queried_at
                        .and_then(|timestamp| u128::try_from(timestamp).ok())
                        .unwrap_or_else(now_millis),
                })
            })
        }) {
            Ok(Some(candidate)) => {
                if selected
                    .as_ref()
                    .is_none_or(|best| candidate.capacity.better_than(best.capacity))
                {
                    selected = Some(candidate);
                }
                if selected
                    .as_ref()
                    .is_some_and(|best| best.capacity.is_maximum())
                {
                    return Ok((selected, failures, earliest_reset));
                }
            }
            Ok(None) => {
                failures.push(format!(
                    "{}：候选账号需周额度未耗尽；有 5 小时窗口时剩余须严格大于 5%",
                    id
                ));
            }
            Err(error) => failures.push(format!("{id}：{error}")),
        }
    }
    if !queried {
        failures.push("账号列表中没有其他独立的托管 Codex 账号".into());
    }
    Ok((selected, failures, earliest_reset))
}

#[tauri::command]
pub fn get_codex_auto_switch_status(app: tauri::AppHandle) -> CodexAutoSwitchStatus {
    get_status(&app)
}

#[tauri::command]
pub fn set_codex_auto_switch_enabled(
    app: tauri::AppHandle,
    enabled: bool,
) -> Result<CodexAutoSwitchStatus, String> {
    set_enabled(&app, enabled)
}

#[tauri::command]
pub fn cancel_codex_auto_switch(app: tauri::AppHandle) -> CodexAutoSwitchStatus {
    let captured = crate::services::codex_desktop_bridge::recovery_records_for_cancellation();
    invalidate_pending(&app, "本次换号已取消");
    let result = captured.and_then(|paths| {
        crate::services::codex_desktop_bridge::cancel_captured_recovery_records(&paths)
    });
    if let Err(error) = result {
        let generation = operation_generation();
        set_status(&app, generation, |status| {
            status.message = format!("本次计划已取消，但旧恢复记录仍需核对：{error}");
            status.can_cancel = true;
        });
    }
    get_status(&app)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked_check() -> Runtime {
        let mut inner = Runtime::default();
        inner.status.phase = "blocked".into();
        inner.status.message = "fixture unresolved desktop recovery".into();
        inner.status.current_provider_id = Some("fixture-current".into());
        inner.status.can_cancel = true;
        inner
    }

    #[test]
    fn repeated_check_failure_ignores_new_generation_and_operation_id() {
        let mut inner = blocked_check();
        inner.generation = 7;
        inner.status.operation_id = Some("fixture-first-operation".into());
        assert!(should_record_failure(
            &mut inner,
            "blocked",
            "automatic",
            "checking"
        ));
        inner.generation = 8;
        inner.status.operation_id = Some("fixture-next-operation".into());
        assert!(!should_record_failure(
            &mut inner,
            "blocked",
            "automatic",
            "checking"
        ));
        // De-duplicating history is not cancellation, clearing the warning,
        // or granting an older worker permission to continue.
        assert_eq!(inner.generation, 8);
        assert_eq!(inner.status.phase, "blocked");
        assert!(inner.status.can_cancel);
        assert_eq!(
            inner.status.operation_id.as_deref(),
            Some("fixture-next-operation")
        );
    }

    #[test]
    fn check_failure_context_changes_are_each_recorded_once() {
        let mut inner = blocked_check();
        assert!(should_record_failure(
            &mut inner,
            "blocked",
            "automatic",
            "checking"
        ));
        for change in 0..4 {
            match change {
                0 => inner.status.current_provider_id = Some("fixture-other-current".into()),
                1 => inner.status.target_provider_id = Some("fixture-new-target".into()),
                2 => inner.status.message = "fixture different blocker".into(),
                _ => inner
                    .status
                    .candidate_failures
                    .push("fixture changed quota".into()),
            }
            assert!(should_record_failure(
                &mut inner,
                "blocked",
                "automatic",
                "checking"
            ));
            assert!(!should_record_failure(
                &mut inner,
                "blocked",
                "automatic",
                "checking"
            ));
        }
        assert!(should_record_failure(
            &mut inner,
            "blocked",
            "reset-wait",
            "checking"
        ));
        assert!(!should_record_failure(
            &mut inner,
            "blocked",
            "reset-wait",
            "checking"
        ));
        assert!(should_record_failure(
            &mut inner,
            "blocked",
            "reset-wait",
            "preflight"
        ));
    }

    #[test]
    fn manual_retries_and_actual_lifecycle_failures_are_not_de_duplicated() {
        let mut inner = blocked_check();
        for (source, stage) in [
            ("manual", "checking"),
            ("manual", "preflight"),
            ("automatic", "pausing"),
            ("automatic", "closing"),
            ("automatic", "enabling"),
            ("automatic", "starting"),
            ("automatic", "verifying"),
            ("automatic", "resuming"),
            ("reset-wait", "resuming"),
        ] {
            for _ in 0..2 {
                assert!(should_record_failure(&mut inner, "blocked", source, stage));
            }
        }
    }

    #[test]
    fn resolved_warning_allows_a_later_recurrence_to_be_recorded() {
        for resolved_phase in ["monitoring", "completed"] {
            let mut inner = blocked_check();
            assert!(should_record_failure(
                &mut inner,
                "blocked",
                "automatic",
                "checking"
            ));
            assert!(!should_record_failure(
                &mut inner,
                resolved_phase,
                "automatic",
                "checking"
            ));
            assert!(inner.last_check_failure.is_none());
            assert!(should_record_failure(
                &mut inner,
                "blocked",
                "automatic",
                "checking"
            ));
        }
    }

    #[test]
    fn an_explicit_toggle_rearms_failure_history_but_redundant_enable_does_not() {
        let mut inner = blocked_check();
        inner.status.enabled = true;
        assert!(should_record_failure(
            &mut inner,
            "blocked",
            "automatic",
            "checking"
        ));
        apply_enabled_choice(&mut inner, true, || {
            panic!("must not cancel a redundant enable")
        });
        assert!(inner.last_check_failure.is_some());
        apply_enabled_choice(&mut inner, false, || Ok(()));
        assert!(inner.last_check_failure.is_none());
    }

    #[test]
    fn apparently_successful_quota_does_not_override_invalid_candidate_login_status() {
        for invalid in [
            CredentialStatus::Expired,
            CredentialStatus::NotFound,
            CredentialStatus::ParseError,
        ] {
            let mut response = quota(0.0, 0.0);
            response.credential_status = invalid;
            assert!(candidate_capacity(&response).is_err());
        }
    }
    use crate::provider::{AuthBinding, AuthBindingSource, ProviderMeta};
    use crate::services::subscription::{CredentialStatus, QuotaTier};

    fn resetting_quota(used: f64, weekly: f64, at: i64) -> SubscriptionQuota {
        let mut result = quota(used, weekly);
        result.tiers[0].resets_at = Some(
            chrono::DateTime::from_timestamp_millis(at)
                .unwrap()
                .to_rfc3339(),
        );
        result
    }

    #[test]
    fn reset_wait_rejects_weekly_exhausted_unknown_expired_past_and_implausible_windows() {
        let now = 1_800_000_000_000;
        let valid = resetting_quota(100.0, 99.0, now + 60_000);
        assert_eq!(candidate_wait_reset_at(&valid, now), Some(now + 60_000));
        assert_eq!(
            candidate_wait_reset_at(&resetting_quota(95.0, 1.0, now + 1), now),
            Some(now + 1)
        );
        for mut invalid in [
            resetting_quota(100.0, 100.0, now + 1),
            resetting_quota(94.99, 0.0, now + 1),
            resetting_quota(100.0, 0.0, now),
            resetting_quota(100.0, 0.0, now - 1),
            resetting_quota(100.0, 0.0, now + 25 * 3_600_000),
            resetting_quota(f64::NAN, 0.0, now + 1),
            resetting_quota(100.0, f64::NAN, now + 1),
        ] {
            assert_eq!(candidate_wait_reset_at(&invalid, now), None);
            invalid.success = false;
            assert_eq!(candidate_wait_reset_at(&invalid, now), None);
        }
        let mut invalid = valid.clone();
        invalid.credential_status = CredentialStatus::Expired;
        assert!(candidate_wait_reset_at(&invalid, now).is_none());
        invalid = valid.clone();
        invalid.tiers[0].resets_at = Some("not-a-time".into());
        assert!(candidate_wait_reset_at(&invalid, now).is_none());
        invalid = valid;
        invalid.tiers.pop();
        assert!(candidate_wait_reset_at(&invalid, now).is_none());
    }

    #[tokio::test]
    async fn earliest_reset_includes_current_without_requery_and_ties_follow_list_order() {
        let now = now_millis() as i64;
        let rows = indexmap::IndexMap::from([
            ("first".into(), card("first", "first-account")),
            ("current".into(), card("current", "current-account")),
            ("last".into(), card("last", "last-account")),
        ]);
        let seen = std::sync::Mutex::new(Vec::new());
        let current = resetting_quota(100.0, 40.0, now + 60_000);
        let (usable, _, wait) = select_most_remaining_with_reset(
            rows.clone(),
            "current",
            "current-account",
            Some(&current),
            |account| {
                seen.lock().unwrap().push(account.clone());
                let reset = if account == "first-account" {
                    now + 60_000
                } else {
                    now + 120_000
                };
                async move { Ok(resetting_quota(100.0, 40.0, reset)) }
            },
            || Ok(()),
        )
        .await
        .unwrap();
        assert!(usable.is_none());
        assert_eq!(wait.unwrap().provider_id, "first");
        assert_eq!(*seen.lock().unwrap(), ["first-account", "last-account"]);
        let (_, _, wait) = select_most_remaining_with_reset(
            rows,
            "current",
            "current-account",
            Some(&current),
            |_| async { Ok(resetting_quota(100.0, 40.0, now + 120_000)) },
            || Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(wait.unwrap().provider_id, "current");
    }

    #[tokio::test]
    async fn usable_candidate_always_wins_over_an_earlier_exhausted_reset_account() {
        let now = now_millis() as i64;
        let rows = indexmap::IndexMap::from([
            ("reset".into(), card("reset", "reset-account")),
            ("usable".into(), card("usable", "usable-account")),
        ]);
        let (usable, _, wait) = select_most_remaining_with_reset(
            rows,
            "current",
            "current-account",
            None,
            |account| async move {
                Ok(if account == "reset-account" {
                    resetting_quota(100.0, 0.0, now + 1_000)
                } else {
                    quota(10.0, 90.0)
                })
            },
            || Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(usable.unwrap().provider_id, "usable");
        assert_eq!(wait.unwrap().provider_id, "reset");
    }

    #[test]
    fn redundant_enable_preserves_owned_wait_and_disable_wins_even_if_journal_cannot_be_read() {
        let mut inner = Runtime::default();
        inner.status.enabled = true;
        inner.status.phase = "waiting-for-reset".into();
        inner.status.operation_id = Some("owned-wait".into());
        inner.status.wait_until = Some(123_000);
        inner.generation = 44;
        inner.running = true;
        apply_enabled_choice(&mut inner, true, || {
            panic!("Redundant enable must not cancel wait")
        });
        assert_eq!(inner.generation, 44);
        assert_eq!(inner.status.operation_id.as_deref(), Some("owned-wait"));
        apply_enabled_choice(&mut inner, false, || {
            Err("fixture damaged wait record".into())
        });
        assert_eq!(inner.generation, 45);
        assert!(!inner.running);
        assert!(!inner.status.enabled);
        assert!(validate_generation(&inner, 44, now_millis(), false).is_err());
        assert!(inner.status.message.contains("fixture damaged wait record"));
    }

    #[test]
    fn usable_quota_after_failed_followup_keeps_actionable_desktop_warning() {
        let mut inner = Runtime::default();
        inner.generation = 7;
        record_desktop_followup_result(
            &mut inner,
            7,
            "selected",
            &Err("fixture desktop shutdown blocked".into()),
        );
        // Timer checks reserve newer generations without choosing another
        // provider. A usable quota must not erase an unfinished desktop flow.
        inner.generation = 8;
        let (phase, message) = usable_account_outcome(&inner, "selected");
        assert_eq!(phase, "blocked");
        assert!(message.contains("fixture desktop shutdown blocked"));
        assert!(message.contains("重开 Codex"));
        assert_eq!(usable_account_outcome(&inner, "other").0, "monitoring");
    }

    #[test]
    fn new_user_choice_clears_old_warning_and_late_failure_cannot_restore_it() {
        let mut inner = Runtime::default();
        inner.generation = 7;
        record_desktop_followup_result(
            &mut inner,
            7,
            "old-selection",
            &Err("fixture old failure".into()),
        );
        invalidate_locked(&mut inner, "fixture user selected new account");
        record_desktop_followup_result(
            &mut inner,
            7,
            "old-selection",
            &Err("fixture late old failure".into()),
        );
        assert!(inner.last_desktop_followup_failure.is_none());
        assert_eq!(
            usable_account_outcome(&inner, "new-selection").0,
            "monitoring"
        );
        assert_eq!(
            usable_account_outcome(&inner, "old-selection").0,
            "monitoring"
        );
    }

    #[test]
    fn successful_current_followup_clears_failure_but_outdated_success_does_not() {
        let mut inner = Runtime::default();
        inner.generation = 9;
        record_desktop_followup_result(
            &mut inner,
            9,
            "selected",
            &Err("fixture restart incomplete".into()),
        );
        record_desktop_followup_result(&mut inner, 8, "selected", &Ok(true));
        assert_eq!(usable_account_outcome(&inner, "selected").0, "blocked");
        record_desktop_followup_result(&mut inner, 9, "selected", &Ok(true));
        assert!(inner.last_desktop_followup_failure.is_none());
        assert_eq!(usable_account_outcome(&inner, "selected").0, "monitoring");
    }

    #[test]
    #[serial_test::serial]
    fn committed_enable_does_not_expire_owned_recovery_but_user_cancellation_still_wins() {
        let generation = invalidate_pending_with_generation("fixture enable");
        {
            let mut inner = lock_runtime();
            inner.session_epoch = None;
            inner.started_at = Some(now_millis().saturating_sub(PLAN_MAX_AGE_MS - 1_000));
        }
        activate_reserved_with(generation, || Ok(())).unwrap();
        let much_later = now_millis() + PLAN_MAX_AGE_MS * 2;
        assert!(validate_generation(&lock_runtime(), generation, much_later, true).is_ok());
        invalidate_pending_without_app("user selected another account after commit");
        assert!(validate_generation(&lock_runtime(), generation, much_later, true).is_err());
    }

    #[test]
    #[serial_test::serial]
    fn expired_plan_never_enables_and_failed_enable_does_not_mark_commit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let generation = invalidate_pending_with_generation("fixture expired enable");
        let calls = AtomicUsize::new(0);
        {
            let mut inner = lock_runtime();
            inner.session_epoch = None;
            inner.started_at = Some(now_millis().saturating_sub(PLAN_MAX_AGE_MS + 1));
        }
        assert!(activate_reserved_with(generation, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let started_at = now_millis();
        lock_runtime().started_at = Some(started_at);
        let failure: Result<(), crate::error::AppError> =
            activate_reserved_with(generation, || {
                Err(crate::error::AppError::Message(
                    "fixture activation failed".into(),
                ))
            });
        assert!(failure.is_err());
        assert!(validate_generation(
            &lock_runtime(),
            generation,
            started_at + PLAN_MAX_AGE_MS + 1,
            true
        )
        .is_err());
        finish_manual_operation(generation);
    }

    #[tokio::test]
    async fn stale_source_recovery_abandons_selected_target_without_refreshing_other_accounts() {
        let queried = Mutex::new(Vec::new());
        let result = revalidate_quota_samples(
            "source",
            "first-usable",
            0,
            PLAN_MAX_AGE_MS + 1,
            |account| {
                queried.lock().unwrap().push(account);
                std::future::ready(Ok(quota(95.0, 99.0)))
            },
            || Ok(()),
            || PLAN_MAX_AGE_MS + 1,
        )
        .await
        .unwrap();
        assert!(!result);
        assert_eq!(*queried.lock().unwrap(), vec!["source"]);
    }

    #[tokio::test]
    async fn slow_ordered_selection_rechecks_only_stale_source_and_keeps_selected_target() {
        let queried = Mutex::new(Vec::new());
        let result = revalidate_quota_samples(
            "source",
            "first-usable",
            0,
            PLAN_MAX_AGE_MS + 1,
            |account| {
                queried.lock().unwrap().push(account);
                std::future::ready(Ok(quota(96.0, 0.0)))
            },
            || Ok(()),
            || PLAN_MAX_AGE_MS + 1,
        )
        .await
        .unwrap();
        assert!(result);
        assert_eq!(*queried.lock().unwrap(), vec!["source"]);
    }

    #[tokio::test]
    async fn stale_selected_target_is_rechecked_once_and_exhaustion_prevents_lifecycle() {
        let queried = Mutex::new(Vec::new());
        let result = revalidate_quota_samples(
            "source",
            "selected",
            0,
            0,
            |account| {
                queried.lock().unwrap().push(account);
                std::future::ready(Ok(quota(0.0, 100.0)))
            },
            || Ok(()),
            || PLAN_MAX_AGE_MS + 1,
        )
        .await;
        assert!(result.unwrap_err().contains("目标账号额度已不可用"));
        assert_eq!(*queried.lock().unwrap(), vec!["source", "selected"]);
    }

    #[tokio::test]
    async fn stale_selected_target_at_exact_five_percent_remaining_is_rejected() {
        let queried = Mutex::new(Vec::new());
        let result = revalidate_quota_samples(
            "source",
            "selected",
            PLAN_MAX_AGE_MS + 1,
            0,
            |account| {
                queried.lock().unwrap().push(account);
                std::future::ready(Ok(quota(95.0, 99.0)))
            },
            || Ok(()),
            || PLAN_MAX_AGE_MS + 1,
        )
        .await;
        assert!(result.unwrap_err().contains("目标账号额度已不可用"));
        assert_eq!(*queried.lock().unwrap(), vec!["selected"]);
    }

    #[tokio::test]
    async fn blocked_session_before_query_sends_no_quota_request() {
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_started = started.clone();
        let result = committed_query_with_session_watch(
            async move {
                worker_started.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            || Err("session locked".into()),
        )
        .await;
        assert_eq!(result.unwrap_err(), "session locked");
        assert!(!started.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelled_selection_discards_quota_but_allows_inflight_refresh_commit() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let (release, finish) = tokio::sync::oneshot::channel::<()>();
        let (committed, observe_commit) = tokio::sync::oneshot::channel::<()>();
        let decision = committed_query_with_session_watch(
            async move {
                // Equivalent to a server accepting token rotation before the
                // local manager receives and atomically persists its reply.
                worker_cancelled.store(true, Ordering::SeqCst);
                finish.await.unwrap();
                committed.send(()).unwrap();
                Ok("old-account-quota")
            },
            || {
                if cancelled.load(Ordering::SeqCst) {
                    Err("user changed account".into())
                } else {
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(decision.unwrap_err(), "user changed account");
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), observe_commit)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn session_change_during_pending_query_discards_result_before_completion() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let invalidated = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let result = query_with_session_watch(
            async {
                tokio::time::sleep(Duration::from_millis(25)).await;
                // Equivalent to observing a lock/unlock WTS epoch change;
                // the session may already be interactive when next checked.
                invalidated.store(true, Ordering::SeqCst);
                std::future::pending::<Result<(), String>>().await
            },
            || {
                if invalidated.load(Ordering::SeqCst) {
                    Err("old session epoch".into())
                } else {
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), "old session epoch");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[serial_test::serial]
    fn provider_mutation_commits_before_a_new_selection_can_be_reserved() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};
        let old = invalidate_pending_with_generation("candidate checked before edit");
        let committed = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mutation_committed = committed.clone();
        let mutation = std::thread::spawn(move || {
            mutate_provider_selection("rebind waiting for provider lock", || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                mutation_committed.store(true, Ordering::SeqCst);
            });
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (requested_tx, requested_rx) = mpsc::channel();
        let (reserved_tx, reserved_rx) = mpsc::channel();
        let selection = std::thread::spawn(move || {
            requested_tx.send(()).unwrap();
            let next = invalidate_pending_with_generation("new selection during edit");
            reserved_tx
                .send((next, committed.load(Ordering::SeqCst)))
                .unwrap();
        });
        requested_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // A reservation cannot observe the old binding while the original
        // provider service is waiting to commit its mutation.
        let early = reserved_rx.recv_timeout(Duration::from_millis(100));
        release_tx.send(()).unwrap();
        mutation.join().unwrap();
        selection.join().unwrap();
        assert!(early.is_err(), "a new selection interleaved before commit");
        let (next, saw_commit) = reserved_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            saw_commit,
            "new selection must observe the committed binding"
        );
        assert!(generation_is_current(old).is_err());
        assert!(generation_is_current(next).is_ok());
        finish_manual_operation(next);
    }

    #[test]
    #[serial_test::serial]
    fn failed_provider_mutation_releases_selection_lock_for_the_next_operation() {
        let old = invalidate_pending_with_generation("candidate checked before failed edit");
        let result: Result<(), &str> =
            mutate_provider_selection("provider edit failed", || Err("fixture failure"));
        assert_eq!(result, Err("fixture failure"));
        assert!(generation_is_current(old).is_err());
        let next = invalidate_pending_with_generation("manual selection after failed edit");
        assert!(generation_is_current(next).is_ok());
        finish_manual_operation(next);
    }

    fn card(id: &str, account: &str) -> Provider {
        let mut provider = Provider::with_id(id.into(), id.into(), serde_json::json!({}), None);
        provider.category = Some("official".into());
        provider.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".into()),
                account_id: Some(account.into()),
            }),
            ..Default::default()
        });
        provider
    }

    #[tokio::test]
    async fn selection_queries_in_original_order_and_chooses_most_remaining() {
        let providers = [
            card("current", "active"),
            card("same-login", "active"),
            card("first", "empty"),
            card("second", "usable"),
            card("last", "more-remaining"),
        ]
        .into_iter()
        .map(|card| (card.id.clone(), card))
        .collect();
        let queried = std::sync::Mutex::new(Vec::new());
        let (target, failures) = select_most_remaining(
            providers,
            "current",
            "active",
            |account| {
                queried.lock().unwrap().push(account.clone());
                std::future::ready(Ok(match account.as_str() {
                    "empty" => quota(99.0, 0.0),
                    "usable" => quota(90.0, 99.0),
                    _ => quota(20.0, 50.0),
                }))
            },
            || Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(target.unwrap().provider_id, "last");
        assert_eq!(
            *queried.lock().unwrap(),
            vec!["empty", "usable", "more-remaining"]
        );
        assert_eq!(failures.len(), 1);
    }

    #[tokio::test]
    async fn unused_candidate_stops_later_queries_and_skips_duplicate_bindings() {
        let providers = [
            card("current", "active"),
            card("same-login", "active"),
            card("first", "used"),
            card("duplicate-login", "used"),
            card("unused", "unused"),
            card("last", "must-not-query"),
        ]
        .into_iter()
        .map(|card| (card.id.clone(), card))
        .collect();
        let queried = Mutex::new(Vec::new());
        let (target, failures) = select_most_remaining(
            providers,
            "current",
            "active",
            |account| {
                queried.lock().unwrap().push(account.clone());
                std::future::ready(Ok(quota(
                    if account == "unused" { 0.0 } else { 50.0 },
                    99.0,
                )))
            },
            || Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(target.unwrap().provider_id, "unused");
        assert_eq!(*queried.lock().unwrap(), vec!["used", "unused"]);
        assert!(failures.is_empty());
    }

    #[tokio::test]
    async fn equal_remaining_keeps_original_order_even_when_weekly_quota_differs() {
        let providers = [card("first", "first"), card("second", "second")]
            .into_iter()
            .map(|card| (card.id.clone(), card))
            .collect();
        let queried = Mutex::new(Vec::new());
        let (target, failures) = select_most_remaining(
            providers,
            "current",
            "active",
            |account| {
                queried.lock().unwrap().push(account.clone());
                std::future::ready(Ok(quota(25.0, if account == "first" { 99.0 } else { 0.0 })))
            },
            || Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(target.unwrap().provider_id, "first");
        assert_eq!(*queried.lock().unwrap(), vec!["first", "second"]);
        assert!(failures.is_empty());
    }

    #[tokio::test]
    async fn first_query_failure_and_exact_threshold_continue_to_eligible_candidate() {
        let providers = [
            card("failure", "failure"),
            card("exact-threshold", "exact-threshold"),
            card("eligible", "eligible"),
        ]
        .into_iter()
        .map(|card| (card.id.clone(), card))
        .collect();
        let queried = Mutex::new(Vec::new());
        let (target, failures) = select_most_remaining(
            providers,
            "current",
            "active",
            |account| {
                queried.lock().unwrap().push(account.clone());
                std::future::ready(match account.as_str() {
                    "failure" => Err("fixture transport failure".into()),
                    "exact-threshold" => Ok(quota(95.0, 10.0)),
                    _ => Ok(quota(94.99999, 99.0)),
                })
            },
            || Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(target.unwrap().provider_id, "eligible");
        assert_eq!(
            *queried.lock().unwrap(),
            vec!["failure", "exact-threshold", "eligible"]
        );
        assert_eq!(failures.len(), 2);
        assert!(failures[0].contains("transport failure"));
        assert!(failures[1].contains("严格大于 5%"));
    }

    #[tokio::test]
    async fn best_candidate_retains_its_original_query_time_after_later_queries() {
        let providers = [card("best", "best"), card("later", "later")]
            .into_iter()
            .map(|card| (card.id.clone(), card))
            .collect();
        let (target, _) = select_most_remaining(
            providers,
            "current",
            "active",
            |account| {
                let mut value = quota(if account == "best" { 10.0 } else { 50.0 }, 0.0);
                value.queried_at = Some(if account == "best" { 1 } else { 10_000 });
                std::future::ready(Ok(value))
            },
            || Ok(()),
        )
        .await
        .unwrap();
        let selected = target.unwrap();
        assert_eq!(selected.provider_id, "best");
        assert_eq!(selected.checked_at, 1);
        assert!(sample_is_stale(selected.checked_at, PLAN_MAX_AGE_MS + 2));
    }

    #[tokio::test]
    async fn cancellation_after_a_usable_candidate_discards_it_and_stops_later_queries() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let providers = [
            card("first", "first"),
            card("second", "second"),
            card("last", "last"),
        ]
        .into_iter()
        .map(|card| (card.id.clone(), card))
        .collect();
        let queried = AtomicUsize::new(0);
        let result = select_most_remaining(
            providers,
            "current",
            "active",
            |_| {
                queried.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(quota(50.0, 0.0)))
            },
            || {
                if queried.load(Ordering::SeqCst) >= 2 {
                    Err("fixture user cancelled".into())
                } else {
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), "fixture user cancelled");
        assert_eq!(queried.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn manual_choice_during_candidate_query_prevents_selection_and_later_requests() {
        let providers = [card("first", "first"), card("later", "later")]
            .into_iter()
            .map(|card| (card.id.clone(), card))
            .collect();
        let changed = std::sync::atomic::AtomicBool::new(false);
        let result = select_most_remaining(
            providers,
            "current",
            "active",
            |_| {
                changed.store(true, std::sync::atomic::Ordering::SeqCst);
                std::future::ready(Ok(quota(0.0, 0.0)))
            },
            || {
                if changed.load(std::sync::atomic::Ordering::SeqCst) {
                    Err("用户已换号".into())
                } else {
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), "用户已换号");
    }

    #[tokio::test]
    async fn all_unavailable_accounts_return_reasons_without_enabling_or_restarting() {
        let providers = [
            card("expired", "expired"),
            card("weekly-zero", "weekly-zero"),
        ]
        .into_iter()
        .map(|card| (card.id.clone(), card))
        .collect();
        let (target, failures) = select_most_remaining(
            providers,
            "current",
            "active",
            |account| {
                std::future::ready(if account == "expired" {
                    Err("授权过期".into())
                } else {
                    Ok(quota(0.0, 100.0))
                })
            },
            || Ok(()),
        )
        .await
        .unwrap();
        assert!(target.is_none());
        assert_eq!(failures.len(), 2);
        assert!(failures[0].contains("授权过期"));
        assert!(failures[1].contains("周额度未耗尽"));
    }

    fn quota(five_hour_used: f64, weekly_used: f64) -> SubscriptionQuota {
        SubscriptionQuota {
            tool: "codex_oauth".into(),
            credential_status: CredentialStatus::Valid,
            credential_message: None,
            success: true,
            tiers: vec![
                QuotaTier {
                    name: TIER_FIVE_HOUR.into(),
                    utilization: five_hour_used,
                    resets_at: None,
                    used_value_usd: None,
                    max_value_usd: None,
                },
                QuotaTier {
                    name: TIER_SEVEN_DAY.into(),
                    utilization: weekly_used,
                    resets_at: None,
                    used_value_usd: None,
                    max_value_usd: None,
                },
            ],
            extra_usage: None,
            codex_limit_policy: None,
            error: None,
            queried_at: None,
        }
    }

    #[test]
    fn exact_thresholds_are_respected() {
        assert_eq!(
            quota_decision(&quota(95.0, 99.0)).unwrap(),
            QuotaDecision::Usable
        );
        assert_eq!(
            quota_decision(&quota(95.00001, 99.0)).unwrap(),
            QuotaDecision::Exhausted
        );
        assert_eq!(
            quota_decision(&quota(94.0, 100.0)).unwrap(),
            QuotaDecision::Exhausted
        );
        assert_eq!(
            quota_decision(&quota(94.0, 101.0)).unwrap(),
            QuotaDecision::Exhausted
        );
    }

    #[test]
    fn active_and_candidate_thresholds_differ_at_exact_five_percent_remaining() {
        assert_eq!(
            quota_decision(&quota(95.0, 99.0)).unwrap(),
            QuotaDecision::Usable
        );
        assert_eq!(candidate_capacity(&quota(95.0, 99.0)).unwrap(), None);
        assert_eq!(
            candidate_capacity(&quota(94.99999, 99.0)).unwrap(),
            Some(CandidateCapacity::FiveHour(94.99999))
        );
        assert_eq!(candidate_capacity(&quota(95.00001, 99.0)).unwrap(), None);
        assert_eq!(candidate_capacity(&quota(0.0, 100.0)).unwrap(), None);
    }

    #[test]
    fn unknown_window_is_never_used_as_available_quota() {
        let mut value = quota(0.0, 0.0);
        value.tiers.pop();
        assert!(quota_decision(&value).is_err());
        value = quota(f64::NAN, 0.0);
        assert!(quota_decision(&value).is_err());
    }

    fn weekly_only_quota(weekly_used: f64) -> SubscriptionQuota {
        let mut value = quota(0.0, weekly_used);
        value.tiers.remove(0);
        value.codex_limit_policy = Some(CodexLimitPolicy::WeeklyOnly);
        value
    }

    #[test]
    fn confirmed_weekly_only_pro_uses_only_the_weekly_threshold() {
        for used in [0.0, 18.0, 99.0, 99.999] {
            let value = weekly_only_quota(used);
            assert_eq!(quota_decision(&value).unwrap(), QuotaDecision::Usable);
            assert_eq!(
                candidate_capacity(&value).unwrap(),
                Some(CandidateCapacity::WeeklyOnly)
            );
        }
        for used in [100.0, 101.0] {
            let value = weekly_only_quota(used);
            assert_eq!(quota_decision(&value).unwrap(), QuotaDecision::Exhausted);
            assert_eq!(candidate_capacity(&value).unwrap(), None);
            assert_eq!(candidate_wait_reset(&value), None);
        }
    }

    #[test]
    fn missing_five_hour_tier_does_not_implicitly_confirm_weekly_only_pro() {
        let mut value = weekly_only_quota(18.0);
        value.codex_limit_policy = None;
        assert!(quota_decision(&value).is_err());
        assert!(candidate_capacity(&value).is_err());
        value.codex_limit_policy = Some(CodexLimitPolicy::WeeklyOnly);
        value.tiers.push(quota(f64::NAN, 0.0).tiers.remove(0));
        assert!(quota_decision(&value).is_err());
        assert!(candidate_capacity(&value).is_err());
    }

    #[test]
    fn weekly_only_policy_does_not_bypass_login_or_invalid_weekly_usage() {
        for used in [f64::NAN, f64::INFINITY, -1.0] {
            let value = weekly_only_quota(used);
            assert!(quota_decision(&value).is_err());
            assert!(candidate_capacity(&value).is_err());
        }
        let mut value = weekly_only_quota(18.0);
        value.credential_status = CredentialStatus::Expired;
        assert!(quota_decision(&value).is_err());
        assert!(candidate_capacity(&value).is_err());
        value.credential_status = CredentialStatus::Valid;
        value.success = false;
        assert!(quota_decision(&value).is_err());
    }

    #[test]
    fn weekly_only_and_unused_windows_share_the_best_band_without_fake_usage() {
        assert!(CandidateCapacity::WeeklyOnly.better_than(CandidateCapacity::FiveHour(1.0)));
        assert!(!CandidateCapacity::WeeklyOnly.better_than(CandidateCapacity::FiveHour(0.0)));
        assert!(!CandidateCapacity::FiveHour(0.0).better_than(CandidateCapacity::WeeklyOnly));
        assert!(CandidateCapacity::WeeklyOnly.is_maximum());
        assert!(CandidateCapacity::FiveHour(0.0).is_maximum());
    }

    #[tokio::test]
    async fn weekly_only_pro_candidate_stops_queries_once_maximum_is_found() {
        let providers = indexmap::IndexMap::from([
            ("current".into(), card("current", "a")),
            ("used".into(), card("used", "b")),
            ("pro".into(), card("pro", "c")),
            ("later".into(), card("later", "d")),
        ]);
        let queried = std::sync::Arc::new(Mutex::new(Vec::new()));
        let calls = queried.clone();
        let (selected, _) = select_most_remaining(
            providers,
            "current",
            "a",
            move |account| {
                calls.lock().unwrap().push(account.clone());
                async move {
                    Ok(if account == "c" {
                        weekly_only_quota(18.0)
                    } else {
                        quota(10.0, 0.0)
                    })
                }
            },
            || Ok(()),
        )
        .await
        .unwrap();
        let selected = selected.unwrap();
        assert_eq!(selected.provider_id, "pro");
        assert_eq!(selected.capacity, CandidateCapacity::WeeklyOnly);
        assert_eq!(*queried.lock().unwrap(), vec!["b", "c"]);
    }

    #[tokio::test]
    async fn weekly_only_source_recovery_invalidates_stale_switch_without_target_query() {
        let queried = std::sync::Arc::new(Mutex::new(Vec::new()));
        let calls = queried.clone();
        let needed = revalidate_quota_samples(
            "source",
            "target",
            0,
            0,
            move |account| {
                calls.lock().unwrap().push(account);
                async { Ok(weekly_only_quota(18.0)) }
            },
            || Ok(()),
            || PLAN_MAX_AGE_MS + 1,
        )
        .await
        .unwrap();
        assert!(!needed);
        assert_eq!(*queried.lock().unwrap(), vec!["source"]);
    }

    #[test]
    fn either_exhausted_window_is_sufficient_even_if_other_is_missing() {
        let mut weekly_only = quota(0.0, 100.0);
        weekly_only.tiers.remove(0);
        assert_eq!(
            quota_decision(&weekly_only).unwrap(),
            QuotaDecision::Exhausted
        );
        let mut five_hour_only = quota(95.01, 0.0);
        five_hour_only.tiers.pop();
        assert_eq!(
            quota_decision(&five_hour_only).unwrap(),
            QuotaDecision::Exhausted
        );
    }

    #[test]
    fn official_managed_card_does_not_need_provider_type_marker() {
        let mut provider = Provider::with_id(
            "card".into(),
            "Managed".into(),
            serde_json::json!({"auth": {}}),
            None,
        );
        provider.category = Some("official".into());
        provider.meta = Some(ProviderMeta {
            auth_binding: Some(AuthBinding {
                source: AuthBindingSource::ManagedAccount,
                auth_provider: Some("codex_oauth".into()),
                account_id: Some("account".into()),
            }),
            ..Default::default()
        });
        assert_eq!(managed_account_id(&provider).as_deref(), Some("account"));
    }
}
