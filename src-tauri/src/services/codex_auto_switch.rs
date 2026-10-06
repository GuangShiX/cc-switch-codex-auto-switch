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
use crate::services::subscription::{SubscriptionQuota, TIER_FIVE_HOUR, TIER_SEVEN_DAY};
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
        && crate::services::codex_desktop_bridge::pending_recovery_reason().is_some();
    let status = {
        let mut inner = lock_runtime();
        if inner.generation != generation {
            return;
        }
        inner.running = false;
        inner.started_at = None;
        inner.status.phase = phase.into();
        inner.status.message = message;
        inner.status.can_cancel = has_uncertain_recovery;
        inner.status.operation_id = None;
        inner.status.target_provider_id = None;
        if phase == "completed" {
            inner.last_desktop_followup_failure = None;
        }
        inner.status.clone()
    };
    publish(app, &status);
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
        if let Some(error) = watcher_error {
            inner.status.phase = "blocked".into();
            inner.status.message = error;
        }
    }
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // leave startup and desktop state time to settle
        loop {
            interval.tick().await;
            if !lock_runtime().status.enabled {
                continue;
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
    app.state::<AppState>()
        .db
        .set_setting(ENABLED_KEY, if enabled { "true" } else { "false" })
        .map_err(|e| e.to_string())?;
    let status = {
        let mut inner = lock_runtime();
        inner.status.enabled = enabled;
        // This toggle controls the timer. It does not cancel a manual Enable
        // whose successful activation is already being followed by a restart.
        if inner.manual_generation.is_none() {
            inner.generation = inner.generation.wrapping_add(1);
            inner.running = false;
            inner.started_at = None;
            inner.status.phase = if enabled { "monitoring" } else { "disabled" }.into();
            inner.status.message = if enabled {
                "每 5 分钟后台监测已启用"
            } else {
                "自动换号已关闭"
            }
            .into();
            inner.status.operation_id = None;
            inner.status.target_provider_id = None;
            inner.status.can_cancel = false;
        }
        inner.status.clone()
    };
    publish(app, &status);
    Ok(status)
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
    inner.generation = inner.generation.wrapping_add(1);
    inner.running = false;
    inner.manual_generation = None;
    inner.started_at = None;
    inner.session_epoch = None;
    inner.last_desktop_followup_failure = None;
    inner.status.phase = if inner.status.enabled {
        "cancelled"
    } else {
        "disabled"
    }
    .into();
    inner.status.message = reason.into();
    inner.status.operation_id = None;
    inner.status.target_provider_id = None;
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
    inner.generation = inner.generation.wrapping_add(1);
    inner.running = false;
    inner.manual_generation = Some(inner.generation);
    inner.started_at = Some(now_millis());
    inner.session_epoch = crate::services::codex_session_watch::current_epoch_if_ready();
    inner.last_desktop_followup_failure = None;
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

fn window_utilization(quota: &SubscriptionQuota, name: &str) -> Option<f64> {
    quota
        .tiers
        .iter()
        .find(|tier| tier.name == name)
        .map(|tier| tier.utilization)
        // Over-limit values (>100) prove exhaustion; invalid values are unknown.
        .filter(|used| used.is_finite() && *used >= 0.0)
}

/// Missing or invalid windows are unknown, never implicitly available.
fn quota_decision(quota: &SubscriptionQuota) -> Result<QuotaDecision, String> {
    if !quota.success {
        return Err(quota
            .error
            .clone()
            .or_else(|| quota.credential_message.clone())
            .unwrap_or_else(|| "额度查询未成功".into()));
    }
    let five_hour_used = window_utilization(quota, TIER_FIVE_HOUR);
    let weekly_used = window_utilization(quota, TIER_SEVEN_DAY);
    // Either exhausted window proves a switch is needed even if the other is
    // absent. Conversely, a candidate is usable only when both windows prove
    // availability.
    if five_hour_used.is_some_and(|used| used > 95.0)
        || weekly_used.is_some_and(|used| used >= 100.0)
    {
        return Ok(QuotaDecision::Exhausted);
    }
    if five_hour_used.is_none() || weekly_used.is_none() {
        return Err("缺少有效的 5 小时或周额度窗口".into());
    }
    Ok(QuotaDecision::Usable)
}

/// The active account may keep exactly 5% remaining, but a replacement must
/// have strictly more than 5%. Smaller usage ranks ahead of larger usage.
fn candidate_five_hour_used(quota: &SubscriptionQuota) -> Result<Option<f64>, String> {
    if quota_decision(quota)? != QuotaDecision::Usable {
        return Ok(None);
    }
    Ok(window_utilization(quota, TIER_FIVE_HOUR).filter(|used| *used < 95.0))
}

async fn query_account(
    app: &tauri::AppHandle,
    account_id: &str,
    generation: u64,
    current_provider: &str,
) -> Result<SubscriptionQuota, String> {
    let state = app.state::<AppState>();
    let result = query_with_session_watch(
        query_codex_oauth_quota_for(&state.codex_oauth_manager, account_id),
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
        if candidate_five_hour_used(&target)?.is_none() {
            return Err("已选择的目标账号额度已不可用；本次不暂停或重开桌面".into());
        }
    }
    check()?;
    Ok(true)
}

async fn run_automatic(app: tauri::AppHandle, generation: u64) {
    if let Some(reason) = crate::services::codex_desktop_bridge::pending_recovery_reason() {
        finish(&app, generation, "blocked", reason);
        return;
    }
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
    set_status(&app, generation, |status| {
        status.current_provider_id = Some(current.clone());
        status.checked_at = Some(now_millis());
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
    match quota_decision(&current_quota) {
        Ok(QuotaDecision::Usable) => {
            let (phase, message) = usable_account_outcome(&lock_runtime(), &current);
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
    let selection = select_most_remaining(
        providers,
        &current,
        &current_account_id,
        |account| {
            let app = app.clone();
            let current = current.clone();
            async move { query_account(&app, &account, generation, &current).await }
        },
        || plan_is_current(&app, generation, &current),
    )
    .await;
    let (target, failures) = match selection {
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
            "[CodexAutoSwitchSelection] provider={} five_hour_used={} five_hour_remaining={} checked_at={} candidate_requires_used_below=95",
            target.provider_id,
            target.five_hour_used,
            100.0 - target.five_hour_used,
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
    } else {
        finish(
            &app,
            generation,
            "waiting",
            "所有托管候选账号均不可用；等待后续额度恢复".into(),
        );
    }
}

#[derive(Debug)]
struct SelectedCandidate {
    provider_id: String,
    five_hour_used: f64,
    checked_at: u128,
}

/// Query candidates in their original order and select the most 5h remaining.
/// Equal results keep the earlier account. An unused window is the theoretical
/// maximum, so later candidates cannot improve it and are not queried.
async fn select_most_remaining<Q, F, C>(
    providers: indexmap::IndexMap<String, Provider>,
    current: &str,
    current_account: &str,
    mut query: Q,
    check: C,
) -> Result<(Option<SelectedCandidate>, Vec<String>), String>
where
    Q: FnMut(String) -> F,
    F: std::future::Future<Output = Result<SubscriptionQuota, String>>,
    C: Fn() -> Result<(), String>,
{
    let mut seen = HashSet::from([current_account.to_owned()]);
    let mut failures = Vec::new();
    let mut queried = false;
    let mut selected: Option<SelectedCandidate> = None;
    for (id, provider) in providers {
        if id == current {
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
            candidate_five_hour_used(&quota).map(|used| {
                used.map(|five_hour_used| SelectedCandidate {
                    provider_id: id,
                    five_hour_used,
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
                    .is_none_or(|best| candidate.five_hour_used < best.five_hour_used)
                {
                    selected = Some(candidate);
                }
                if selected
                    .as_ref()
                    .is_some_and(|best| best.five_hour_used == 0.0)
                {
                    return Ok((selected, failures));
                }
            }
            Ok(None) => {
                failures.push(format!(
                    "{}：候选账号需 5 小时剩余严格大于 5%，且周额度未耗尽",
                    provider.name
                ));
            }
            Err(error) => failures.push(format!("{}：{error}", provider.name)),
        }
    }
    if !queried {
        failures.push("账号列表中没有其他独立的托管 Codex 账号".into());
    }
    Ok((selected, failures))
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
    use crate::provider::{AuthBinding, AuthBindingSource, ProviderMeta};
    use crate::services::subscription::{CredentialStatus, QuotaTier};

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
        let started = std::sync::atomic::AtomicBool::new(false);
        let result = query_with_session_watch(
            async {
                started.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            || Err("session locked".into()),
        )
        .await;
        assert_eq!(result.unwrap_err(), "session locked");
        assert!(!started.load(std::sync::atomic::Ordering::SeqCst));
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
        assert_eq!(candidate_five_hour_used(&quota(95.0, 99.0)).unwrap(), None);
        assert_eq!(
            candidate_five_hour_used(&quota(94.99999, 99.0)).unwrap(),
            Some(94.99999)
        );
        assert_eq!(
            candidate_five_hour_used(&quota(95.00001, 99.0)).unwrap(),
            None
        );
        assert_eq!(candidate_five_hour_used(&quota(0.0, 100.0)).unwrap(), None);
    }

    #[test]
    fn unknown_window_is_never_used_as_available_quota() {
        let mut value = quota(0.0, 0.0);
        value.tiers.pop();
        assert!(quota_decision(&value).is_err());
        value = quota(f64::NAN, 0.0);
        assert!(quota_decision(&value).is_err());
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
