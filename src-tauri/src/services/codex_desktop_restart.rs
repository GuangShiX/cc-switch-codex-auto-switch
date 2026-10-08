//! Restart the installed Codex desktop after CC Switch has enabled an account.
//! The ordinary provider switch is never blocked by this optional operation.
//! Windows Restart Manager requests normal session cleanup (no force flag),
//! and package activation preserves the desktop's original launch environment.
//! Multiple windows/renderers below one packaged root are one desktop. The
//! bridge rejects independent multi-root instances until their task-owner
//! coverage is proven; the restart layer binds exact roots rather than picking
//! one process or using an executable-name-wide termination.

use std::sync::{Arc, LazyLock, Mutex};

static RESTART_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// A timed-out Restart Manager call cannot be terminated safely. Its only
/// remaining authority is the already registered normal shutdown request.
/// Retain this lease until both shutdown and cancellation have returned, so
/// neither a timer nor a manual restart can replay that request meanwhile.
#[derive(Default)]
struct ShutdownGate {
    active: std::sync::atomic::AtomicBool,
}

struct ShutdownLease {
    gate: Arc<ShutdownGate>,
}

impl Drop for ShutdownLease {
    fn drop(&mut self) {
        self.gate
            .active
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl ShutdownGate {
    fn ensure_idle(&self) -> Result<(), String> {
        if self.active.load(std::sync::atomic::Ordering::SeqCst) {
            Err(
                "上一条 Codex 正常退出请求仍待 Windows 确认；未排队或重复关闭桌面，请稍后核对"
                    .into(),
            )
        } else {
            Ok(())
        }
    }

    fn acquire(self: &Arc<Self>) -> Result<Arc<ShutdownLease>, String> {
        self.active
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .map_err(|_| "上一条 Codex 正常退出请求尚未完成；未重复关闭桌面".to_string())?;
        Ok(Arc::new(ShutdownLease { gate: self.clone() }))
    }
}

static SHUTDOWN_GATE: LazyLock<Arc<ShutdownGate>> =
    LazyLock::new(|| Arc::new(ShutdownGate::default()));

/// Call before pausing tasks as well as before entering the restart runtime.
pub fn ensure_no_shutdown_pending() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    native::reconcile_unobserved_quit();
    SHUTDOWN_GATE.ensure_idle()
}

#[cfg(any(target_os = "windows", test))]
fn abandon_shutdown<Cancel>(cancel: Cancel, lease: Arc<ShutdownLease>, reason: &str) -> String
where
    Cancel: FnOnce() -> u32 + Send + 'static,
{
    log::warn!("Codex 正常退出等待已停止：{reason}；请求取消 Windows 正常退出，结果仍待核对");
    // RmCancelCurrentTask can itself wait. It must never extend the caller's
    // deadline or own account activation / desktop launch / task recovery.
    let spawned = std::thread::Builder::new()
        .name("codex-normal-shutdown-cancel".into())
        .spawn(move || {
            let _lease = lease;
            log::info!("Codex Windows 正常退出取消请求开始");
            let result = cancel();
            log::info!("Codex Windows 正常退出取消请求返回（Windows {result}）；不代表退出已取消");
        });
    if spawned.is_ok() {
        format!("{reason}；已请求取消，退出结果仍需核对；未修改登录、重启或继续")
    } else {
        format!("{reason}；取消请求线程无法启动，退出结果仍需核对；未修改登录、重启或继续")
    }
}

/// Only `shutdown` and `cancel` cross into detached threads. In particular,
/// no late successful return can run the caller's after-exit action. Dropping
/// the waiter invalidates the operation while the lease guards against replay.
#[cfg(any(target_os = "windows", test))]
fn bounded_normal_shutdown<Shutdown, Cancel>(
    gate: Arc<ShutdownGate>,
    shutdown: Shutdown,
    cancel: Cancel,
    guard: &dyn Fn() -> Result<(), String>,
    timeout: std::time::Duration,
    guard_interval: std::time::Duration,
) -> Result<u32, String>
where
    Shutdown: FnOnce() -> u32 + Send + 'static,
    Cancel: FnOnce() -> u32 + Send + 'static,
{
    guard()?;
    let lease = gate.acquire()?;
    let worker_lease = lease.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("codex-normal-shutdown".into())
        .spawn(move || {
            let _lease = worker_lease;
            let started = std::time::Instant::now();
            log::info!("Codex Windows 正常退出调用开始（不强制结束）");
            let result = shutdown();
            log::info!(
                "Codex Windows 正常退出调用返回（Windows {result}，耗时 {} 毫秒）",
                started.elapsed().as_millis()
            );
            let _ = sender.send(result);
        })
        .map_err(|_| "无法启动 Codex 正常退出协调线程；未关闭桌面、未修改登录".to_string())?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Err(reason) = guard() {
            return Err(abandon_shutdown(cancel, lease, &reason));
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            let duration = if timeout.as_secs() > 0 {
                format!("{} 秒", timeout.as_secs())
            } else {
                format!("{} 毫秒", timeout.as_millis())
            };
            return Err(abandon_shutdown(
                cancel,
                lease,
                &format!("Codex 正常退出调用超过 {duration} 未返回"),
            ));
        }
        match receiver.recv_timeout((deadline - now).min(guard_interval)) {
            Ok(result) => {
                guard()?;
                return Ok(result);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(
                    "Codex 正常退出协调线程意外结束；退出结果仍需核对，未修改登录、重启或继续"
                        .into(),
                );
            }
        }
    }
}

/// An activation receipt belongs only to the fresh package process created by
/// this operation. The reader checks process birth and listener ownership again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityReceipt {
    pub pid: u32,
    pub birth: u64,
    pub port: u16,
}

static IDENTITY_RECEIPT: Mutex<Option<IdentityReceipt>> = Mutex::new(None);

pub fn last_identity_receipt() -> Option<IdentityReceipt> {
    IDENTITY_RECEIPT.lock().ok()?.clone()
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DesktopReceipt {
    roots: Vec<Desktop>,
}

impl DesktopReceipt {
    pub fn root_count(&self) -> usize {
        self.roots.len()
    }
}

fn receipt_for(roots: &[Desktop]) -> Option<DesktopReceipt> {
    if roots.is_empty() {
        return None;
    }
    let mut roots = roots.to_vec();
    roots.sort_by_key(|root| (root.pid, root.birth));
    Some(DesktopReceipt { roots })
}

pub fn desktop_receipt() -> Result<Option<DesktopReceipt>, String> {
    #[cfg(target_os = "windows")]
    {
        let rows = native::settled_desktops()?;
        validate_restart_roots(&rows)?;
        Ok(receipt_for(&rows))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(None)
    }
}

fn validate_same_desktop(
    expected: &Option<DesktopReceipt>,
    actual: &Option<DesktopReceipt>,
) -> Result<(), String> {
    if actual != expected {
        return Err("桌面已由用户关闭或重开，旧任务快照已失效；未操作新的桌面".into());
    }
    Ok(())
}

pub fn ensure_same_desktop(expected: &Option<DesktopReceipt>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let roots = native::desktops()?;
        validate_restart_roots(&roots)?;
        let actual = receipt_for(&roots);
        if actual == *expected {
            return Ok(());
        }
        // Only an added protocol/single-instance entry can settle back to
        // the bound set. Missing/reborn roots cannot become the same process
        // again. Normal guard checks therefore do not sleep for every task.
        let may_be_handoff = expected
            .as_ref()
            .is_some_and(|expected| expected.roots.iter().all(|root| roots.contains(root)));
        if may_be_handoff {
            validate_same_desktop(expected, &desktop_receipt()?)
        } else {
            validate_same_desktop(expected, &actual)
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        validate_same_desktop(expected, &desktop_receipt()?)
    }
}

pub fn ensure_identity_desktop(expected: &IdentityReceipt) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let actual = receipt_for(&native::desktops()?);
    #[cfg(not(target_os = "windows"))]
    let actual = desktop_receipt()?;
    if actual.as_ref().is_some_and(|receipt| {
        receipt.roots.len() == 1
            && receipt.roots[0].pid == expected.pid
            && receipt.roots[0].birth == expected.birth
    }) {
        Ok(())
    } else {
        Err("本次新桌面已退出、被重开或出现其他实例；未恢复旧任务".into())
    }
}

/// A protocol navigation can briefly launch a second single-instance entry
/// process. Only wait for that handoff here; the global shutdown checks remain
/// strict. The confirmed main must survive every observation, and no navigation
/// or continuation is repeated while the result is uncertain.
#[derive(Default)]
struct NavigationWatch {
    unique_since: Option<std::time::Duration>,
}

impl NavigationWatch {
    fn observe(
        &mut self,
        expected: &DesktopReceipt,
        roots: &[DesktopReceipt],
        elapsed: std::time::Duration,
    ) -> Result<bool, String> {
        use std::time::Duration;
        if !roots.contains(expected) {
            return Err("打开原聊天时本次新桌面已退出或被重开；未恢复旧任务".into());
        }
        if elapsed >= Duration::from_secs(5) {
            return Err("打开原聊天后桌面进程未收敛到本次唯一实例；未重复导航或发送继续".into());
        }
        if roots.len() == 1 {
            let since = self.unique_since.get_or_insert(elapsed);
            // ShellExecute can return before the second entry is visible.
            // Observe the initial launch interval as well as a stable period.
            if elapsed >= Duration::from_secs(1)
                && elapsed.saturating_sub(*since) >= Duration::from_millis(400)
            {
                return Ok(true);
            }
        } else {
            self.unique_since = None;
        }
        Ok(false)
    }
}

pub async fn wait_for_navigation(
    expected: &IdentityReceipt,
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let actual = native::desktops()?;
        let root = actual
            .iter()
            .find(|desktop| desktop.pid == expected.pid && desktop.birth == expected.birth)
            .ok_or_else(|| "本次新桌面已退出或被重开；未恢复旧任务".to_string())?;
        let expected = receipt_for(std::slice::from_ref(root)).unwrap();
        wait_for_navigation_with(&expected, guard, || {
            Ok(native::desktops()?
                .into_iter()
                .map(|desktop| receipt_for(std::slice::from_ref(&desktop)).unwrap())
                .collect())
        })
        .await?;
        ensure_same_desktop(&Some(expected))?;
        guard()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (expected, guard);
        Err("当前平台无法核对原聊天导航后的桌面进程".into())
    }
}

async fn wait_for_navigation_with(
    expected: &DesktopReceipt,
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
    mut inspect: impl FnMut() -> Result<Vec<DesktopReceipt>, String> + Send,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let mut watch = NavigationWatch::default();
    loop {
        guard()?;
        let roots = inspect()?;
        guard()?;
        if watch.observe(expected, &roots, started.elapsed())? {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Locked/secure desktops discard the pending plan. The monitor must make a
/// fresh quota and account decision after unlock, never replay an old plan.
pub fn ensure_interactive_session() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        native::ensure_interactive_session()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("无法核对当前交互桌面；本次自动重开已跳过".into())
    }
}

pub async fn restart_bound(
    expected: Option<DesktopReceipt>,
    guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    after_exit: Box<dyn FnOnce() -> Result<(), String> + Send>,
    before_shutdown: Box<dyn FnOnce() -> Result<(), String> + Send>,
    manual: bool,
) -> Result<bool, String> {
    ensure_no_shutdown_pending()?;
    let _lock = RESTART_LOCK
        .try_lock()
        .map_err(|_| "另一条 Codex 桌面重开流程仍在处理；本次未排队、未重复关闭桌面".to_string())?;
    ensure_no_shutdown_pending()?;
    *IDENTITY_RECEIPT
        .lock()
        .map_err(|_| "无法核对本次桌面启动记录")? = None;
    guard()?;
    #[cfg(target_os = "windows")]
    {
        tokio::task::spawn_blocking(move || {
            let check = || {
                guard()?;
                ensure_interactive_session()
            };
            let mut runtime = BoundRuntime {
                inner: native::WindowsRuntime {
                    before_shutdown: Some(before_shutdown),
                    quit_guard: guard.clone(),
                },
                expected,
            };
            restart_with_action_mode(&mut runtime, &check, after_exit, manual)
        })
        .await
        .map_err(|_| "桌面重开任务意外结束；请核对状态".to_string())?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (expected, after_exit, before_shutdown, manual);
        Err("桌面重开仅支持 Windows".into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Desktop {
    pid: u32,
    birth: u64,
    family: String,
    application_id: String,
}

/// A process row captured from one ToolHelp snapshot.  The parent PID alone
/// is not enough to identify an ancestor: Windows may reuse a PID while a
/// stale row is still visible.  Every grouping decision therefore compares
/// the parent's birth time with the child's birth time as well.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DesktopProcessRow {
    desktop: Desktop,
    parent_pid: u32,
}

fn same_package(a: &Desktop, b: &Desktop) -> bool {
    a.family == b.family && a.application_id == b.application_id
}

/// Several windows may belong to one Electron root, or several independent
/// roots of the same installed application. Bind the complete root set, never
/// select an arbitrary process or close a different package/channel.
fn validate_restart_roots(roots: &[Desktop]) -> Result<(), String> {
    let Some(first) = roots.first() else {
        return Ok(());
    };
    let mut ids = std::collections::HashSet::new();
    if roots
        .iter()
        .any(|root| !same_package(root, first) || !ids.insert(root.pid))
    {
        return Err("Codex 桌面清单包含不同包身份或不明确的进程；未关闭任何桌面".into());
    }
    Ok(())
}

fn same_process_set(expected: &[(u32, u64)], actual: &[(u32, u64)]) -> bool {
    let expected_set: std::collections::HashSet<_> = expected.iter().copied().collect();
    let actual_set: std::collections::HashSet<_> = actual.iter().copied().collect();
    expected.len() == expected_set.len()
        && actual.len() == actual_set.len()
        && expected
            .iter()
            .map(|(pid, _)| pid)
            .collect::<std::collections::HashSet<_>>()
            .len()
            == expected.len()
        && actual
            .iter()
            .map(|(pid, _)| pid)
            .collect::<std::collections::HashSet<_>>()
            .len()
            == actual.len()
        && expected_set == actual_set
}

#[derive(Default)]
struct DesktopSettleWatch {
    previous: Option<Vec<Desktop>>,
    stable_since: std::time::Duration,
}

impl DesktopSettleWatch {
    fn observe(&mut self, roots: &[Desktop], elapsed: std::time::Duration) -> Result<bool, String> {
        use std::time::Duration;
        if self.previous.as_deref() != Some(roots) {
            self.previous = Some(roots.to_vec());
            self.stable_since = elapsed;
        }
        let stable = elapsed.saturating_sub(self.stable_since) >= Duration::from_millis(250);
        // Allow a transient single-instance entry to exit before binding
        // multiple roots. Persistent, stable roots are a legitimate group.
        if stable && (roots.len() <= 1 || elapsed >= Duration::from_secs(2)) {
            return Ok(true);
        }
        if elapsed >= Duration::from_secs(2) {
            return Err("Codex 桌面进程清单仍在变化；未关闭桌面，请稍后重试".into());
        }
        Ok(false)
    }
}

/// Return the top-level ChatGPT processes represented by one process
/// snapshot.  ChatGPT can keep several renderer/main entries below one
/// packaged root; those entries are one desktop.  Two roots with no valid
/// birth-ordered ChatGPT ancestry remain separate and are intentionally
/// reported to the caller instead of choosing one at random.
fn root_desktops_from_rows(rows: &[DesktopProcessRow]) -> Vec<Desktop> {
    let by_pid: std::collections::HashMap<u32, &DesktopProcessRow> =
        rows.iter().map(|row| (row.desktop.pid, row)).collect();
    let mut roots = rows
        .iter()
        .filter(|row| {
            let mut current_pid = row.parent_pid;
            let mut child_birth = row.desktop.birth;
            let mut visited = std::collections::HashSet::new();
            let mut found_chatgpt_parent = false;
            let mut cycle = false;
            while current_pid != 0 {
                if !visited.insert(current_pid) {
                    cycle = true;
                    break;
                }
                let Some(parent) = by_pid.get(&current_pid) else {
                    // The parent is another executable (or it disappeared
                    // between snapshots), so no ChatGPT ancestor was proven.
                    break;
                };
                // A parent born after its child is a reused PID, not an
                // ancestor.  Treat the child as an independent root.
                if !same_package(&parent.desktop, &row.desktop) {
                    break;
                }
                if parent.desktop.birth >= child_birth {
                    return true;
                }
                found_chatgpt_parent = true;
                current_pid = parent.parent_pid;
                child_birth = parent.desktop.birth;
            }
            cycle || !found_chatgpt_parent
        })
        .map(|row| row.desktop.clone())
        .collect::<Vec<_>>();
    roots.sort_by_key(|desktop| (desktop.pid, desktop.birth));
    roots.dedup_by_key(|desktop| (desktop.pid, desktop.birth));
    roots
}

trait RestartRuntime {
    fn running_desktops(&mut self) -> Result<Vec<Desktop>, String>;
    fn pending_closed_desktop(&mut self) -> Option<Desktop> {
        None
    }
    fn remember_closed_desktop(&mut self, _: &Desktop) {}
    fn clear_pending_desktop(&mut self) {}
    fn close_and_confirm(
        &mut self,
        desktops: &[Desktop],
        guard: &dyn Fn() -> Result<(), String>,
    ) -> Result<(), String>;
    fn activate_and_confirm(
        &mut self,
        desktop: &Desktop,
        guard: &dyn Fn() -> Result<(), String>,
    ) -> Result<(), String>;
}

struct BoundRuntime<R> {
    inner: R,
    expected: Option<DesktopReceipt>,
}
impl<R: RestartRuntime> RestartRuntime for BoundRuntime<R> {
    fn running_desktops(&mut self) -> Result<Vec<Desktop>, String> {
        let current = self.inner.running_desktops()?;
        validate_restart_roots(&current)?;
        let receipt = receipt_for(&current);
        validate_same_desktop(&self.expected, &receipt)?;
        Ok(current)
    }
    fn pending_closed_desktop(&mut self) -> Option<Desktop> {
        self.inner.pending_closed_desktop()
    }
    fn remember_closed_desktop(&mut self, desktop: &Desktop) {
        self.inner.remember_closed_desktop(desktop);
    }
    fn clear_pending_desktop(&mut self) {
        self.inner.clear_pending_desktop();
    }
    fn close_and_confirm(
        &mut self,
        desktops: &[Desktop],
        guard: &dyn Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        self.inner.close_and_confirm(desktops, guard)
    }
    fn activate_and_confirm(
        &mut self,
        desktop: &Desktop,
        guard: &dyn Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        self.inner.activate_and_confirm(desktop, guard)
    }
}

#[cfg(test)]
fn restart_with(
    runtime: &mut impl RestartRuntime,
    check: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    restart_with_action(runtime, check, || Ok(()))
}

#[cfg(test)]
fn restart_with_action(
    runtime: &mut impl RestartRuntime,
    check: &dyn Fn() -> Result<(), String>,
    after_exit: impl FnOnce() -> Result<(), String>,
) -> Result<bool, String> {
    restart_with_action_mode(runtime, check, after_exit, false)
}

fn restart_with_action_mode(
    runtime: &mut impl RestartRuntime,
    check: &dyn Fn() -> Result<(), String>,
    after_exit: impl FnOnce() -> Result<(), String>,
    recover_pending: bool,
) -> Result<bool, String> {
    check()?;
    let desktops = runtime.running_desktops()?;
    validate_restart_roots(&desktops)?;
    let Some(desktop) = desktops.first() else {
        let pending = recover_pending
            .then(|| runtime.pending_closed_desktop())
            .flatten();
        check()?;
        after_exit()?;
        check()?;
        if let Some(desktop) = pending {
            runtime.activate_and_confirm(&desktop, check)?;
            runtime.clear_pending_desktop();
            return Ok(true);
        }
        return Ok(false);
    };
    // A desktop appeared after an uncertain/cancelled activation. Its process
    // state takes precedence; an old activation receipt is never replayed.
    runtime.clear_pending_desktop();
    check()?;
    runtime.close_and_confirm(&desktops, check)?;
    runtime.remember_closed_desktop(desktop);
    // A separate post-exit guard records invalidation before launching. No
    // retry will replay the previous close when either guard fails.
    check()?;
    after_exit()?;
    check()?;
    runtime.activate_and_confirm(desktop, check)?;
    runtime.clear_pending_desktop();
    Ok(true)
}

#[cfg(target_os = "windows")]
mod native {
    use super::{
        root_desktops_from_rows, same_package, same_process_set, validate_restart_roots, Desktop,
        DesktopProcessRow, DesktopSettleWatch, IdentityReceipt, RestartRuntime, IDENTITY_RECEIPT,
    };
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use windows_sys::core::GUID;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, FILETIME, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::Storage::Packaging::Appx::{
        GetApplicationUserModelId, GetPackageFamilyName,
    };
    use windows_sys::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_LOCAL_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::RestartManager::{
        RmCancelCurrentTask, RmEndSession, RmGetList, RmRegisterResources, RmShutdown,
        RmStartSession, RM_PROCESS_INFO, RM_UNIQUE_PROCESS,
    };
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetProcessTimes, OpenProcess, WaitForSingleObject,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::Shell::AO_NOERRORUI;

    pub(super) struct WindowsRuntime {
        pub(super) before_shutdown: Option<Box<dyn FnOnce() -> Result<(), String> + Send>>,
        pub(super) quit_guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    }
    // Only an explicit new manual activation may consume this marker. It
    // contains no auth, task, or old operation data, and is never a timer job.
    static PENDING_CLOSED_DESKTOP: std::sync::Mutex<Option<Desktop>> = std::sync::Mutex::new(None);

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn filetime(value: &FILETIME) -> u64 {
        ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
    }

    fn exact_process_handle(desktop: &Desktop) -> Result<Handle, String> {
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                0,
                desktop.pid,
            );
            if handle.is_null() {
                return Err("原 Codex 桌面进程已退出或无法访问；本次不关闭其他进程".into());
            }
            let handle = Handle(handle);
            let mut created = FILETIME::default();
            let mut exited = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            if GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) == 0
                || filetime(&created) != desktop.birth
                || WaitForSingleObject(handle.0, 0) != 258
            {
                return Err("原 Codex 进程启动时间或运行状态已变化；本次不关闭其他进程".into());
            }
            // Retain this kernel object until exit confirmation. PID reuse
            // cannot turn a different process into the one being waited for.
            Ok(handle)
        }
    }

    fn process(pid: u32) -> Result<Option<Desktop>, String> {
        unsafe {
            let mut current_session = 0;
            let mut process_session = 0;
            if ProcessIdToSessionId(GetCurrentProcessId(), &mut current_session) == 0 {
                return Err("无法核对 CC Switch 的 Windows 用户会话；未操作桌面".into());
            }
            if ProcessIdToSessionId(pid, &mut process_session) == 0 {
                if GetLastError() == ERROR_INVALID_PARAMETER {
                    return Ok(None);
                }
                return Err("无法核对 Codex 的 Windows 用户会话；未操作桌面".into());
            }
            // Never adopt an identically named package running in another
            // logged-in user's/RDP session as this user's restart target.
            if process_session != current_session {
                return Ok(None);
            }
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                let error = GetLastError();
                if error == ERROR_INVALID_PARAMETER {
                    return Ok(None);
                }
                return Err(format!(
                    "无法核对 Codex 进程状态（Windows {error}）；不会把不明确的结果当作已退出"
                ));
            }
            let handle = Handle(handle);
            let wait = WaitForSingleObject(handle.0, 0);
            if wait == 0 {
                return Ok(None);
            }
            if wait == u32::MAX {
                return Err("无法核对 Codex 是否退出；不会重复启动桌面".into());
            }
            let mut created = FILETIME::default();
            let mut exited = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            if GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) == 0 {
                return Err("无法核对 Codex 进程的启动时间".into());
            }
            let read_identity =
                |get: unsafe extern "system" fn(HANDLE, *mut u32, *mut u16) -> u32| {
                    let mut size = 0;
                    get(handle.0, &mut size, null_mut());
                    if size == 0 || size > 4096 {
                        return String::new();
                    }
                    let mut text = vec![0u16; size as usize];
                    if get(handle.0, &mut size, text.as_mut_ptr()) != 0 {
                        return String::new();
                    }
                    let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
                    String::from_utf16_lossy(&text[..end])
                };
            let family = read_identity(GetPackageFamilyName);
            if !family.starts_with("OpenAI.Codex_") {
                return Ok(None);
            }
            let application_id = read_identity(GetApplicationUserModelId);
            if !application_id.starts_with(&format!("{family}!")) {
                return Err("无法读取正在运行的 Codex 包启动身份；请手动重开桌面".into());
            }
            Ok(Some(Desktop {
                pid,
                birth: filetime(&created),
                family,
                application_id,
            }))
        }
    }

    fn desktop_process_rows() -> Result<Vec<DesktopProcessRow>, String> {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err("无法读取 Codex 桌面进程清单".into());
            }
            let snapshot = Handle(snapshot);
            let mut entry = PROCESSENTRY32W::default();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut available = Process32FirstW(snapshot.0, &mut entry);
            let mut rows = Vec::new();
            while available != 0 {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                if String::from_utf16_lossy(&entry.szExeFile[..end])
                    .eq_ignore_ascii_case("ChatGPT.exe")
                {
                    if let Some(desktop) = process(entry.th32ProcessID)? {
                        rows.push(DesktopProcessRow {
                            desktop,
                            parent_pid: entry.th32ParentProcessID,
                        });
                    }
                }
                available = Process32NextW(snapshot.0, &mut entry);
            }
            if GetLastError() != windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES {
                return Err("Codex 桌面进程清单不完整；未关闭桌面".into());
            }
            Ok(rows)
        }
    }

    pub(super) fn desktops() -> Result<Vec<Desktop>, String> {
        let rows = desktop_process_rows()?;
        Ok(root_desktops_from_rows(&rows))
    }

    /// Process creation and single-instance handoff are observable in several
    /// snapshots. Brief secondary single-instance entries are allowed to
    /// disappear; stable independent roots remain part of the bound group.
    pub(super) fn settled_desktops() -> Result<Vec<Desktop>, String> {
        let started = Instant::now();
        let mut watch = DesktopSettleWatch::default();
        loop {
            let current = desktops()?;
            if watch.observe(&current, started.elapsed())? {
                return Ok(current);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn process_tree(desktop: &Desktop) -> Result<std::collections::HashSet<(u32, u64)>, String> {
        let rows = desktop_process_rows()?;
        let mut ids = std::collections::HashSet::from([(desktop.pid, desktop.birth)]);
        loop {
            let mut changed = false;
            for row in &rows {
                if ids
                    .iter()
                    .any(|(pid, birth)| *pid == row.parent_pid && *birth < row.desktop.birth)
                    && same_package(desktop, &row.desktop)
                    && ids.insert((row.desktop.pid, row.desktop.birth))
                {
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Ok(rows
            .into_iter()
            .filter(|row| ids.contains(&(row.desktop.pid, row.desktop.birth)))
            .map(|row| (row.desktop.pid, row.desktop.birth))
            .collect())
    }

    // The GUI's normal quit must also finish its old native app-server before
    // account files change. Retain exact kernel objects; never kill these
    // children or confuse a reused PID with the previous account's server.
    fn app_server_handles(
        tree: &std::collections::HashSet<(u32, u64)>,
    ) -> Result<Vec<Handle>, String> {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err("无法核对原 Codex 后台进程；未修改登录".into());
            }
            let snapshot = Handle(snapshot);
            let parents: std::collections::HashMap<_, _> = tree.iter().copied().collect();
            let mut entry = PROCESSENTRY32W::default();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut available = Process32FirstW(snapshot.0, &mut entry);
            let mut handles = Vec::new();
            while available != 0 {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let belongs_to_desktop = parents.get(&entry.th32ParentProcessID);
                if belongs_to_desktop.is_some()
                    && String::from_utf16_lossy(&entry.szExeFile[..end])
                        .eq_ignore_ascii_case("codex.exe")
                {
                    // An old renderer PID can be reused between snapshots.
                    // Validate the current parent's complete birth identity
                    // before adopting a backend into the exit wait set.
                    if process(entry.th32ParentProcessID)?
                        .as_ref()
                        .is_none_or(|parent| parent.birth != *belongs_to_desktop.unwrap())
                    {
                        return Err(
                            "原 Codex 后台的父进程身份已变化；未关闭桌面、未修改登录".into()
                        );
                    }
                    let raw = OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                        0,
                        entry.th32ProcessID,
                    );
                    if raw.is_null() {
                        if GetLastError() != ERROR_INVALID_PARAMETER {
                            return Err("原 Codex 后台进程状态无法核对；未修改登录".into());
                        }
                    } else {
                        let handle = Handle(raw);
                        let mut created = FILETIME::default();
                        let mut exited = FILETIME::default();
                        let mut kernel = FILETIME::default();
                        let mut user = FILETIME::default();
                        if GetProcessTimes(raw, &mut created, &mut exited, &mut kernel, &mut user)
                            == 0
                        {
                            return Err("原 Codex 后台启动时间无法核对；未修改登录".into());
                        }
                        // A stale parent PID from before this GUI's birth is
                        // not a child of the desktop being shut down.
                        if filetime(&created) >= *belongs_to_desktop.unwrap() {
                            handles.push(handle);
                        }
                    }
                }
                available = Process32NextW(snapshot.0, &mut entry);
            }
            if GetLastError() != windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES {
                return Err("原 Codex 后台清单不完整；未修改登录".into());
            }
            Ok(handles)
        }
    }

    fn app_servers_exited(handles: &[Handle]) -> Result<bool, String> {
        for handle in handles {
            match unsafe { WaitForSingleObject(handle.0, 0) } {
                0 => {}
                258 => return Ok(false),
                _ => return Err("无法确认原 Codex 后台退出；未修改登录、未重复启动".into()),
            }
        }
        Ok(true)
    }

    pub(super) fn ensure_interactive_session() -> Result<(), String> {
        unsafe {
            let desktop = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
            if desktop.is_null() {
                return Err(
                    "Windows 当前已锁屏或位于安全桌面；本次自动计划已丢弃，解锁后重新检查额度"
                        .into(),
                );
            }
            let mut text = [0u16; 256];
            let mut needed = 0;
            let success = GetUserObjectInformationW(
                desktop,
                UOI_NAME,
                text.as_mut_ptr().cast(),
                std::mem::size_of_val(&text) as u32,
                &mut needed,
            );
            CloseDesktop(desktop);
            let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
            if success == 0
                || !String::from_utf16_lossy(&text[..end]).eq_ignore_ascii_case("Default")
            {
                return Err(
                    "Windows 当前不在可操作的用户桌面；本次自动计划已丢弃，不会积压重开".into(),
                );
            }
            Ok(())
        }
    }

    struct RestartSession(u32);
    impl Drop for RestartSession {
        fn drop(&mut self) {
            unsafe { RmEndSession(self.0) };
        }
    }

    const NORMAL_SHUTDOWN_FLAGS: u32 = 0;

    // Windows kernel handles can be waited on from another thread. This
    // wrapper owns one validated process object and only exposes a wait;
    // Arc keeps it alive until every observer has finished, with one CloseHandle.
    struct QuitObserverHandle(Handle);
    unsafe impl Send for QuitObserverHandle {}
    unsafe impl Sync for QuitObserverHandle {}
    impl QuitObserverHandle {
        fn exited(&self) -> bool {
            unsafe { WaitForSingleObject(self.0 .0, 0) == 0 }
        }
    }
    type UnobservedQuit = (Arc<super::ShutdownLease>, Arc<QuitObserverHandle>);
    static UNOBSERVED_QUIT: std::sync::Mutex<Option<UnobservedQuit>> = std::sync::Mutex::new(None);

    pub(super) fn reconcile_unobserved_quit() {
        let mut pending = UNOBSERVED_QUIT.lock().unwrap_or_else(|p| p.into_inner());
        if pending.as_ref().is_some_and(|(_, handle)| handle.exited()) {
            pending.take();
        }
    }

    fn retain_quit_gate_until_original_exit(
        lease: Arc<super::ShutdownLease>,
        handle: Arc<QuitObserverHandle>,
    ) -> Result<(), String> {
        let observer_lease = lease.clone();
        let observer_handle = handle.clone();
        let spawned = std::thread::Builder::new()
            .name("codex-normal-quit-observer".into())
            .spawn(move || {
                let _lease = observer_lease;
                loop {
                    if observer_handle.exited() {
                        log::info!("Codex 原生退出请求所绑定的原进程已退出；解除防重放保护");
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            });
        if spawned.is_err() {
            // Keep the gate, with its kernel object available for a later
            // read-only check. Never treat an observer failure as confirmed exit.
            *UNOBSERVED_QUIT.lock().unwrap_or_else(|p| p.into_inner()) = Some((lease, handle));
            return Err(
                "原生退出请求已发送，但退出观察器无法启动；结果待核对，未重复关闭或执行后续流程"
                    .into(),
            );
        }
        Ok(())
    }

    impl RestartRuntime for WindowsRuntime {
        fn pending_closed_desktop(&mut self) -> Option<Desktop> {
            PENDING_CLOSED_DESKTOP
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }
        fn remember_closed_desktop(&mut self, desktop: &Desktop) {
            *PENDING_CLOSED_DESKTOP
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(desktop.clone());
        }
        fn clear_pending_desktop(&mut self) {
            *PENDING_CLOSED_DESKTOP
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = None;
        }
        fn running_desktops(&mut self) -> Result<Vec<Desktop>, String> {
            let rows = desktops()?;
            validate_restart_roots(&rows)?;
            Ok(rows)
        }

        fn close_and_confirm(
            &mut self,
            desktops: &[Desktop],
            guard: &dyn Fn() -> Result<(), String>,
        ) -> Result<(), String> {
            super::ensure_no_shutdown_pending()?;
            log::info!("Codex 正常退出准备：核对桌面进程及 Windows 退出资源");
            validate_restart_roots(desktops)?;
            if desktops.is_empty() || self::desktops()? != desktops {
                return Err("原 Codex 桌面进程清单已变化；本次不会关闭其他进程".into());
            }
            let mut handles = Vec::new();
            let mut tree = std::collections::HashSet::new();
            for desktop in desktops {
                if process(desktop.pid)?.as_ref() != Some(desktop) {
                    return Err("原 Codex 桌面身份已变化；本次不会关闭其他进程".into());
                }
                handles.push(exact_process_handle(desktop)?);
                tree.extend(process_tree(desktop)?);
            }
            unsafe {
                let mut session = 0;
                let mut key = [0u16; 33];
                let result = RmStartSession(&mut session, 0, key.as_mut_ptr());
                if result != 0 {
                    return Err(format!("无法发起 Codex 正常退出（Windows {result}）"));
                }
                // Cancellation can still be running after the controller has
                // returned. EndSession belongs to the last holder, never to
                // the timed-out caller alone.
                let session = Arc::new(RestartSession(session));
                let registered: Vec<_> = desktops
                    .iter()
                    .map(|desktop| RM_UNIQUE_PROCESS {
                        dwProcessId: desktop.pid,
                        ProcessStartTime: FILETIME {
                            dwLowDateTime: desktop.birth as u32,
                            dwHighDateTime: (desktop.birth >> 32) as u32,
                        },
                    })
                    .collect();
                let result = RmRegisterResources(
                    session.0,
                    0,
                    null(),
                    registered.len() as u32,
                    registered.as_ptr(),
                    0,
                    null(),
                );
                if result != 0 {
                    return Err(format!("正常退出登记失败（Windows {result}）"));
                }
                let mut needed = 0;
                let mut count = registered.len() as u32;
                let mut info = vec![RM_PROCESS_INFO::default(); registered.len()];
                let mut reasons = 0;
                let result = RmGetList(
                    session.0,
                    &mut needed,
                    &mut count,
                    info.as_mut_ptr(),
                    &mut reasons,
                );
                if result == ERROR_MORE_DATA
                    || result != 0
                    || reasons != 0
                    || count as usize != registered.len()
                {
                    return Err(format!("正常退出目标未能精确核对（Windows {result}，原因 {reasons}）；本次不关闭桌面"));
                }
                let expected: Vec<_> = desktops.iter().map(|d| (d.pid, d.birth)).collect();
                let actual: Vec<_> = info
                    .iter()
                    .map(|i| (i.Process.dwProcessId, filetime(&i.Process.ProcessStartTime)))
                    .collect();
                if !same_process_set(&expected, &actual) {
                    return Err("正常退出清单出现了其他进程；本次不关闭桌面".into());
                }
                let prepared_quit = if desktops.len() == 1 {
                    tauri::async_runtime::block_on(
                        crate::services::codex_desktop_quit::prepare_normal_quit(
                            desktops[0].pid,
                            desktops[0].birth,
                            self.quit_guard.as_ref(),
                        ),
                    )?
                } else {
                    None
                };
                // Do not use RmForceShutdown or TerminateProcess. This asks
                // Electron to process its ordinary end-session cleanup.
                if let Some(check_tasks) = self.before_shutdown.take() {
                    log::info!("Codex 正常退出准备：正在最后核对已保存任务");
                    check_tasks()?;
                }
                guard()?;
                // The task check may take time. A new/closed root invalidates
                // the whole operation before the sole normal shutdown call.
                if self::desktops()? != desktops {
                    return Err("关闭前 Codex 桌面清单已变化；未关闭其他桌面，未修改登录".into());
                }
                // Include renderer children created during task suspension.
                // Old known identities remain in the union for cleanup waits.
                for desktop in desktops {
                    tree.extend(process_tree(desktop)?);
                }
                let app_servers = app_server_handles(&tree)?;
                guard()?;
                let deadline = Instant::now() + Duration::from_secs(15);
                let quit_handle = if prepared_quit.is_some() {
                    Some(Arc::new(QuitObserverHandle(exact_process_handle(
                        &desktops[0],
                    )?)))
                } else {
                    None
                };
                let quit_lease = prepared_quit
                    .as_ref()
                    .map(|_| super::SHUTDOWN_GATE.acquire())
                    .transpose()?;
                let quit_request = if let Some(prepared) = prepared_quit {
                    tauri::async_runtime::block_on(
                        crate::services::codex_desktop_quit::send_prepared_quit(
                            prepared,
                            self.quit_guard.as_ref(),
                        ),
                    )?
                } else {
                    crate::services::codex_desktop_quit::QuitRequest::Unavailable
                };
                let result = match quit_request {
                    crate::services::codex_desktop_quit::QuitRequest::Sent => {
                        retain_quit_gate_until_original_exit(
                            quit_lease.ok_or("原生退出请求缺少本次防重放保护")?,
                            quit_handle.ok_or("原生退出请求缺少本次原进程核验对象")?,
                        )?;
                        log::info!("Codex 原生退出应用请求已发送，正在核对进程退出；不追加 Windows 关闭请求");
                        0
                    }
                    crate::services::codex_desktop_quit::QuitRequest::Unavailable => {
                        drop(quit_lease);
                        guard()?;
                        let shutdown_session = session.clone();
                        let cancel_session = session.clone();
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return Err(
                                "Codex 正常退出准备超过 15 秒；未发送 Windows 关闭请求".into()
                            );
                        }
                        super::bounded_normal_shutdown(
                            super::SHUTDOWN_GATE.clone(),
                            move || RmShutdown(shutdown_session.0, NORMAL_SHUTDOWN_FLAGS, None),
                            move || RmCancelCurrentTask(cancel_session.0),
                            guard,
                            remaining,
                            Duration::from_millis(100),
                        )?
                    }
                };
                while Instant::now() < deadline {
                    guard()?;
                    let mut roots_exited = true;
                    for handle in &handles {
                        match WaitForSingleObject(handle.0, 0) {
                            0 => {}
                            258 => roots_exited = false,
                            _ => return Err("原 Codex 的退出状态无法确认；不会重复启动".into()),
                        }
                    }
                    let remaining = desktop_process_rows()?;
                    if remaining
                        .iter()
                        .any(|row| !tree.contains(&(row.desktop.pid, row.desktop.birth)))
                    {
                        return Err(
                            "正常退出期间检测到其他桌面进程；不会覆盖用户启动或重复重开".into()
                        );
                    }
                    if roots_exited {
                        if remaining.is_empty() && app_servers_exited(&app_servers)? {
                            log::info!("Codex 原桌面及原后台退出已确认");
                            return Ok(());
                        }
                        // The old renderer/GPU children may finish cleanup
                        // slightly after their main process. Wait for those
                        // known identities, without touching any new launch.
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(format!(
                    "Codex 桌面或原后台未正常退出（Windows {result}）；未强制结束，未修改登录，未重复启动"
                ))
            }
        }

        fn activate_and_confirm(
            &mut self,
            desktop: &Desktop,
            guard: &dyn Fn() -> Result<(), String>,
        ) -> Result<(), String> {
            if !settled_desktops()?.is_empty() {
                return Err("Codex 桌面已由其他操作启动；本次不重复启动".into());
            }
            guard()?;
            let receipt = activate(&desktop.application_id, guard)?;
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                if let Some(next) = process(receipt.pid)? {
                    if next.family == desktop.family
                        && next.application_id == desktop.application_id
                        && (next.pid != desktop.pid || next.birth != desktop.birth)
                        && next.birth >= receipt.not_before
                    {
                        let roots = settled_desktops()?;
                        if expected_activated_main(desktop, &next, &receipt, &roots) {
                            guard()?;
                            *IDENTITY_RECEIPT
                                .lock()
                                .map_err(|_| "无法保存本次新桌面记录")? = Some(IdentityReceipt {
                                pid: next.pid,
                                birth: next.birth,
                                port: receipt.port,
                            });
                            return Ok(());
                        }
                        if !roots.is_empty() {
                            return Err("包启动返回的进程不是唯一的新 Codex 主进程；请核对桌面状态，不会重复启动".into());
                        }
                    } else {
                        return Err(
                            "重开后的 Codex 包身份、进程或启动时间不符合预期；请核对桌面状态"
                                .into(),
                        );
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err("Codex 启动请求已发送，但新进程未确认；不会盲目重复启动".into())
        }
    }

    #[repr(C)]
    struct ActivationVtable {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        activate_application:
            unsafe extern "system" fn(*mut c_void, *const u16, *const u16, u32, *mut u32) -> i32,
        activate_for_file: usize,
        activate_for_protocol: usize,
    }

    struct ActivationManager(*mut c_void);
    impl Drop for ActivationManager {
        fn drop(&mut self) {
            unsafe {
                let table = &**(self.0 as *mut *const ActivationVtable);
                (table.release)(self.0);
            }
        }
    }

    struct ComApartment;
    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    struct ActivationReceipt {
        pid: u32,
        not_before: u64,
        port: u16,
    }

    fn expected_activated_main(
        old: &Desktop,
        next: &Desktop,
        receipt: &ActivationReceipt,
        roots: &[Desktop],
    ) -> bool {
        next.pid == receipt.pid
            && next.family == old.family
            && next.application_id == old.application_id
            && (next.pid != old.pid || next.birth != old.birth)
            && next.birth >= receipt.not_before
            && roots.len() == 1
            && roots[0] == *next
    }

    fn activate(
        application_id: &str,
        guard: &dyn Fn() -> Result<(), String>,
    ) -> Result<ActivationReceipt, String> {
        unsafe {
            let result = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
            if result < 0 {
                return Err(format!(
                    " Windows 包启动服务初始化失败（HRESULT {result:#x}）；请手动启动 Codex"
                ));
            }
            let _apartment = ComApartment;
            let class = GUID::from_u128(0x45ba127d_10a8_46ea_8ab7_56ea9078943c);
            // This is IApplicationActivationManager's SDK IID, not another
            // shell activation interface with a similarly named entry point.
            let interface = GUID::from_u128(0x2e941141_7f97_4756_ba1d_9decde894a3d);
            let mut manager = null_mut();
            let result = CoCreateInstance(
                &class,
                null_mut(),
                CLSCTX_LOCAL_SERVER,
                &interface,
                &mut manager,
            );
            if result < 0 || manager.is_null() {
                return Err(format!(
                    " Windows 包启动服务不可用（HRESULT {result:#x}）；请手动启动 Codex"
                ));
            }
            let manager = ActivationManager(manager);
            let table = &**(manager.0 as *mut *const ActivationVtable);
            let name: Vec<u16> = application_id.encode_utf16().chain(Some(0)).collect();
            // Keep Windows package activation, the original CLI resolver,
            // configuration and permissions. Only add a loopback read channel
            // used to confirm the desktop's actual signed-in principal.
            let reservation = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .map_err(|_| "无法分配本机桌面账号核验端口")?;
            let port = reservation
                .local_addr()
                .map_err(|_| "无法核对本机桌面账号核验端口")?
                .port();
            let arguments: Vec<u16> =
                format!("--remote-debugging-port={port} --remote-debugging-address=127.0.0.1")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
            let mut pid = 0;
            // Recheck after COM setup: another user launch must not be adopted
            // as this operation's newly created desktop.
            if !settled_desktops()?.is_empty() {
                return Err("Codex 已由其他操作启动；本次不重复激活桌面".into());
            }
            guard()?;
            let not_before = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| "无法核对桌面启动时钟")?
                .as_nanos()
                / 100) as u64
                + 116_444_736_000_000_000;
            drop(reservation);
            let result = (table.activate_application)(
                manager.0,
                name.as_ptr(),
                arguments.as_ptr(),
                AO_NOERRORUI as u32,
                &mut pid,
            );
            if result < 0 || pid == 0 {
                return Err(format!(
                    " Windows 未能启动 Codex（HRESULT {result:#x}）；不会重复启动"
                ));
            }
            Ok(ActivationReceipt {
                pid,
                not_before,
                port,
            })
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn an_old_background_handle_must_exit_before_login_can_change() {
            use super::*;
            use std::process::{Command, Stdio};
            // An unrelated synthetic process waits for EOF. This exercises
            // actual Windows kernel exit status without touching Codex.
            let mut child = Command::new("cmd.exe")
                .args(["/d", "/c", "more"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let raw = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                    0,
                    child.id(),
                )
            };
            assert!(!raw.is_null());
            let handles = vec![Handle(raw)];
            assert!(!app_servers_exited(&handles).unwrap());
            drop(child.stdin.take());
            child.wait().unwrap();
            assert!(app_servers_exited(&handles).unwrap());
        }

        #[test]
        fn shutdown_never_requests_force() {
            assert_eq!(super::NORMAL_SHUTDOWN_FLAGS, 0);
        }

        #[test]
        fn activation_confirms_new_unique_main_not_old_or_user_launched_instance() {
            use super::*;
            let old = Desktop {
                pid: 10,
                birth: 20,
                family: "OpenAI.Codex_test".into(),
                application_id: "OpenAI.Codex_test!App".into(),
            };
            let new = Desktop {
                pid: 30,
                birth: 50,
                ..old.clone()
            };
            let receipt = ActivationReceipt {
                pid: 30,
                not_before: 40,
                port: 9222,
            };
            assert!(expected_activated_main(
                &old,
                &new,
                &receipt,
                &[new.clone()]
            ));
            assert!(!expected_activated_main(
                &old,
                &old,
                &receipt,
                &[old.clone()]
            ));
            let user_launched = Desktop {
                birth: 39,
                ..new.clone()
            };
            assert!(!expected_activated_main(
                &old,
                &user_launched,
                &receipt,
                &[user_launched.clone()]
            ));
            assert!(!expected_activated_main(
                &old,
                &new,
                &receipt,
                &[new.clone(), old.clone()]
            ));
            let wrong = Desktop {
                family: "OpenAI.Other_test".into(),
                ..new.clone()
            };
            assert!(!expected_activated_main(
                &old,
                &wrong,
                &receipt,
                &[wrong.clone()]
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn await_shutdown_gate_idle(gate: &ShutdownGate) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while gate.ensure_idle().is_err() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            gate.ensure_idle().is_ok(),
            "synthetic shutdown workers must finish"
        );
    }

    #[test]
    fn normal_shutdown_worker_success_is_observed_once_without_cancellation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        let gate = Arc::new(ShutdownGate::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let shutdown_calls = calls.clone();
        assert_eq!(
            bounded_normal_shutdown(
                gate.clone(),
                move || {
                    shutdown_calls.fetch_add(1, Ordering::SeqCst);
                    0
                },
                || panic!("a confirmed shutdown must not be cancelled"),
                &|| Ok(()),
                Duration::from_secs(1),
                Duration::from_millis(1),
            )
            .unwrap(),
            0
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        await_shutdown_gate_idle(&gate);
    }

    struct BlockedShutdownRuntime {
        gate: Arc<ShutdownGate>,
        release: Option<std::sync::mpsc::Receiver<()>>,
        cancelled: Arc<std::sync::atomic::AtomicUsize>,
        activations: usize,
    }

    impl RestartRuntime for BlockedShutdownRuntime {
        fn running_desktops(&mut self) -> Result<Vec<Desktop>, String> {
            Ok(vec![synthetic_desktop(10, 20)])
        }

        fn close_and_confirm(
            &mut self,
            _: &[Desktop],
            guard: &dyn Fn() -> Result<(), String>,
        ) -> Result<(), String> {
            let release = self.release.take().unwrap();
            let cancelled = self.cancelled.clone();
            bounded_normal_shutdown(
                self.gate.clone(),
                move || {
                    release.recv().unwrap();
                    0
                },
                move || {
                    cancelled.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    0
                },
                guard,
                std::time::Duration::from_millis(10),
                std::time::Duration::from_millis(1),
            )?;
            Ok(())
        }

        fn activate_and_confirm(
            &mut self,
            _: &Desktop,
            _: &dyn Fn() -> Result<(), String>,
        ) -> Result<(), String> {
            self.activations += 1;
            Ok(())
        }
    }

    #[test]
    fn blocked_shutdown_times_out_and_late_success_never_enables_or_activates() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};
        let gate = Arc::new(ShutdownGate::default());
        let (release, waiting) = std::sync::mpsc::channel();
        let cancelled = Arc::new(AtomicUsize::new(0));
        let enabled = Cell::new(0);
        let mut runtime = BlockedShutdownRuntime {
            gate: gate.clone(),
            release: Some(waiting),
            cancelled: cancelled.clone(),
            activations: 0,
        };
        let started = Instant::now();
        let error = restart_with_action(&mut runtime, &|| Ok(()), || {
            enabled.set(enabled.get() + 1);
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("超过 10 毫秒"));
        assert!(error.contains("退出结果仍需核对"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            gate.ensure_idle().is_err(),
            "late RM call still owns its lease"
        );
        assert!(gate.acquire().is_err(), "a second shutdown must not queue");
        release.send(()).unwrap();
        await_shutdown_gate_idle(&gate);
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(enabled.get(), 0);
        assert_eq!(runtime.activations, 0);
    }

    #[test]
    fn shutdown_guard_cancellation_returns_before_the_blocked_worker() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};
        let gate = Arc::new(ShutdownGate::default());
        let (release, waiting) = std::sync::mpsc::channel();
        let checks = Cell::new(0);
        let cancellations = Arc::new(AtomicUsize::new(0));
        let cancel_calls = cancellations.clone();
        let started = Instant::now();
        let error = bounded_normal_shutdown(
            gate.clone(),
            move || {
                waiting.recv().unwrap();
                0
            },
            move || {
                cancel_calls.fetch_add(1, Ordering::SeqCst);
                0
            },
            &|| {
                checks.set(checks.get() + 1);
                if checks.get() >= 3 {
                    Err("用户已取消或选择了另一个账号".into())
                } else {
                    Ok(())
                }
            },
            Duration::from_secs(10),
            Duration::from_millis(1),
        )
        .unwrap_err();
        assert!(error.contains("用户已取消"));
        assert!(error.contains("已请求取消，退出结果仍需核对"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(gate.ensure_idle().is_err());
        release.send(()).unwrap();
        await_shutdown_gate_idle(&gate);
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn blocked_cancellation_also_keeps_the_shutdown_gate_until_both_workers_finish() {
        use std::time::{Duration, Instant};
        let gate = Arc::new(ShutdownGate::default());
        let (release_shutdown, shutdown_waiting) = std::sync::mpsc::channel();
        let (shutdown_finished, finished) = std::sync::mpsc::channel();
        let (cancel_started, cancelling) = std::sync::mpsc::channel();
        let (release_cancel, cancel_waiting) = std::sync::mpsc::channel();
        let started = Instant::now();
        assert!(bounded_normal_shutdown(
            gate.clone(),
            move || {
                shutdown_waiting.recv().unwrap();
                shutdown_finished.send(()).unwrap();
                0
            },
            move || {
                cancel_started.send(()).unwrap();
                cancel_waiting.recv().unwrap();
                0
            },
            &|| Ok(()),
            Duration::from_millis(10),
            Duration::from_millis(1),
        )
        .is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        cancelling.recv_timeout(Duration::from_secs(1)).unwrap();
        release_shutdown.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            gate.ensure_idle().is_err(),
            "pending cancellation retains the session"
        );
        release_cancel.send(()).unwrap();
        await_shutdown_gate_idle(&gate);
        assert!(
            gate.acquire().is_ok(),
            "a completed operation releases the gate"
        );
    }

    #[test]
    fn navigation_waits_for_late_single_instance_handoff_without_ignoring_extra_desktops() {
        use std::time::Duration;
        let expected = synthetic_receipt(10, 20);
        let extra = synthetic_receipt(11, 21);
        let mut watch = NavigationWatch::default();
        for (millis, roots, ready) in [
            (0, vec![expected.clone()], false),
            (500, vec![expected.clone(), extra], false),
            (900, vec![expected.clone()], false),
            (1000, vec![expected.clone()], false),
            (1300, vec![expected.clone()], true),
        ] {
            assert_eq!(
                watch
                    .observe(&expected, &roots, Duration::from_millis(millis))
                    .unwrap(),
                ready
            );
        }
    }

    #[test]
    fn navigation_rejects_persistent_extra_desktop_and_changed_main() {
        use std::time::Duration;
        let expected = synthetic_receipt(10, 20);
        let extra = synthetic_receipt(11, 21);
        assert!(NavigationWatch::default()
            .observe(
                &expected,
                &[expected.clone(), extra.clone()],
                Duration::from_secs(5)
            )
            .is_err());
        for roots in [vec![], vec![extra], vec![synthetic_receipt(10, 21)]] {
            assert!(NavigationWatch::default()
                .observe(&expected, &roots, Duration::ZERO)
                .is_err());
        }
        let mut late = NavigationWatch::default();
        assert!(!late
            .observe(&expected, &[expected.clone()], Duration::from_millis(4600))
            .unwrap());
        assert!(late
            .observe(&expected, &[expected.clone()], Duration::from_millis(5100))
            .is_err());
    }

    #[tokio::test]
    async fn cancellation_during_navigation_wait_prevents_continuation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let expected = synthetic_receipt(10, 20);
        let checks = AtomicUsize::new(0);
        let inspections = AtomicUsize::new(0);
        let result = wait_for_navigation_with(
            &expected,
            &|| {
                if checks.fetch_add(1, Ordering::SeqCst) < 2 {
                    Ok(())
                } else {
                    Err("user cancelled or chose another account".into())
                }
            },
            || {
                inspections.fetch_add(1, Ordering::SeqCst);
                Ok(vec![expected.clone(), synthetic_receipt(11, 21)])
            },
        )
        .await;
        assert_eq!(
            result.unwrap_err(),
            "user cancelled or chose another account"
        );
        assert_eq!(inspections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_reopened_desktop_invalidates_the_inventory_before_any_pause() {
        let original = Some(synthetic_receipt(10, 20));
        assert!(validate_same_desktop(&original, &original).is_ok());
        for changed in [
            None,
            Some(synthetic_receipt(11, 20)),
            Some(synthetic_receipt(10, 21)),
        ] {
            assert!(validate_same_desktop(&original, &changed).is_err());
        }
        assert!(validate_same_desktop(&None, &original).is_err());
    }

    fn synthetic_desktop(pid: u32, birth: u64) -> Desktop {
        Desktop {
            pid,
            birth,
            family: "OpenAI.Codex_test".into(),
            application_id: "OpenAI.Codex_test!App".into(),
        }
    }

    fn synthetic_receipt(pid: u32, birth: u64) -> DesktopReceipt {
        receipt_for(&[synthetic_desktop(pid, birth)]).unwrap()
    }

    #[test]
    fn same_package_independent_roots_are_bound_and_all_closed_before_enable() {
        let roots = vec![synthetic_desktop(10, 20), synthetic_desktop(30, 40)];
        let mut fake = Fake::new();
        fake.extra_roots.push(roots[1].clone());
        let closed = fake.closed.clone();
        let mut runtime = BoundRuntime {
            inner: fake,
            expected: receipt_for(&roots),
        };
        let enables = Cell::new(0);
        assert!(restart_with_action_mode(
            &mut runtime,
            &|| Ok(()),
            || {
                assert!(closed.get());
                enables.set(enables.get() + 1);
                Ok(())
            },
            false
        )
        .unwrap());
        assert_eq!(runtime.inner.close_targets, roots);
        assert_eq!(enables.get(), 1);
        assert_eq!(
            runtime.inner.events,
            ["inspect", "close-confirm", "activate-confirm"]
        );
        assert!(runtime.inner.extra_roots.is_empty());
    }

    #[test]
    fn changed_root_set_invalidates_all_close_targets() {
        let first = synthetic_desktop(10, 20);
        let second = synthetic_desktop(30, 40);
        let expected = receipt_for(&[first.clone(), second.clone()]);
        for changed in [
            vec![],
            vec![synthetic_desktop(30, 41)],
            vec![synthetic_desktop(31, 40)],
            vec![second.clone(), synthetic_desktop(50, 60)],
        ] {
            let mut fake = Fake::new();
            fake.extra_roots = changed;
            let mut runtime = BoundRuntime {
                inner: fake,
                expected: expected.clone(),
            };
            assert!(restart_with_action_mode(
                &mut runtime,
                &|| Ok(()),
                || panic!("must not enable"),
                false
            )
            .is_err());
            assert_eq!(runtime.inner.events, ["inspect"]);
            assert!(runtime.inner.close_targets.is_empty());
        }
        // Serialized wait receipts preserve every root and ignore enumeration
        // ordering; user close/reopen of either root still invalidates them.
        let encoded = serde_json::to_string(&expected).unwrap();
        let decoded: Option<DesktopReceipt> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, expected);
        assert_eq!(receipt_for(&[second, first]), expected);
    }

    #[test]
    fn foreign_package_or_application_never_joins_shutdown_group() {
        let first = synthetic_desktop(10, 20);
        for foreign in [
            Desktop {
                family: "OpenAI.Codex_other".into(),
                ..synthetic_desktop(30, 40)
            },
            Desktop {
                application_id: "OpenAI.Codex_test!Other".into(),
                ..synthetic_desktop(30, 40)
            },
            synthetic_desktop(10, 21),
        ] {
            let mut fake = Fake::new();
            fake.extra_roots.push(foreign.clone());
            assert!(validate_restart_roots(&[first.clone(), foreign]).is_err());
            assert!(restart_with_action_mode(
                &mut fake,
                &|| Ok(()),
                || panic!("must not enable"),
                false
            )
            .is_err());
            assert_eq!(fake.events, ["inspect"]);
            assert!(fake.close_targets.is_empty());
        }
    }

    #[test]
    fn rm_list_must_match_all_registered_births_exactly() {
        let expected = [(10, 20), (30, 40)];
        assert!(same_process_set(&expected, &[(30, 40), (10, 20)]));
        for actual in [
            vec![(10, 20)],
            vec![(10, 20), (30, 41)],
            vec![(10, 20), (30, 40), (50, 60)],
            vec![(10, 20), (10, 20)],
            vec![(10, 20), (10, 40)],
        ] {
            assert!(!same_process_set(&expected, &actual));
        }
    }

    #[test]
    fn transient_handoff_settles_but_persistent_multi_root_is_supported() {
        use std::time::Duration;
        let first = synthetic_desktop(10, 20);
        let second = synthetic_desktop(30, 40);
        let mut handoff = DesktopSettleWatch::default();
        assert!(!handoff
            .observe(&[first.clone(), second.clone()], Duration::ZERO)
            .unwrap());
        assert!(!handoff
            .observe(&[first.clone()], Duration::from_millis(100))
            .unwrap());
        assert!(handoff
            .observe(&[first.clone()], Duration::from_millis(350))
            .unwrap());
        let roots = [first.clone(), second];
        let mut persistent = DesktopSettleWatch::default();
        assert!(!persistent.observe(&roots, Duration::ZERO).unwrap());
        assert!(!persistent
            .observe(&roots, Duration::from_millis(300))
            .unwrap());
        assert!(persistent.observe(&roots, Duration::from_secs(2)).unwrap());
        let mut changing = DesktopSettleWatch::default();
        assert!(!changing
            .observe(&[first], Duration::from_millis(1900))
            .unwrap());
        assert!(changing.observe(&roots, Duration::from_secs(2)).is_err());
    }

    #[test]
    fn chatgpt_parent_from_another_package_is_a_separate_root() {
        let parent = synthetic_desktop(10, 20);
        let child = Desktop {
            family: "OpenAI.Codex_other".into(),
            application_id: "OpenAI.Codex_other!App".into(),
            ..synthetic_desktop(30, 40)
        };
        let rows = [
            DesktopProcessRow {
                desktop: parent.clone(),
                parent_pid: 1,
            },
            DesktopProcessRow {
                desktop: child.clone(),
                parent_pid: 10,
            },
        ];
        let roots = root_desktops_from_rows(&rows);
        assert_eq!(roots, [parent, child]);
        assert!(validate_restart_roots(&roots).is_err());
    }

    #[test]
    fn descendants_of_one_root_are_one_desktop() {
        let rows = vec![
            DesktopProcessRow {
                desktop: synthetic_desktop(10, 100),
                parent_pid: 1,
            },
            DesktopProcessRow {
                desktop: synthetic_desktop(20, 200),
                parent_pid: 10,
            },
            DesktopProcessRow {
                desktop: synthetic_desktop(30, 300),
                parent_pid: 20,
            },
        ];
        assert_eq!(
            root_desktops_from_rows(&rows),
            vec![synthetic_desktop(10, 100)]
        );
        assert_eq!(
            receipt_for(&root_desktops_from_rows(&rows))
                .unwrap()
                .root_count(),
            1
        );
    }

    #[test]
    fn independent_roots_remain_explicitly_multiple() {
        let rows = vec![
            DesktopProcessRow {
                desktop: synthetic_desktop(10, 100),
                parent_pid: 1,
            },
            DesktopProcessRow {
                desktop: synthetic_desktop(20, 200),
                parent_pid: 2,
            },
            DesktopProcessRow {
                desktop: synthetic_desktop(30, 300),
                parent_pid: 10,
            },
        ];
        assert_eq!(
            root_desktops_from_rows(&rows),
            vec![synthetic_desktop(10, 100), synthetic_desktop(20, 200)]
        );
        assert_eq!(
            receipt_for(&root_desktops_from_rows(&rows))
                .unwrap()
                .root_count(),
            2
        );
    }

    #[test]
    fn reused_parent_pid_is_not_treated_as_ancestry() {
        let rows = vec![
            DesktopProcessRow {
                desktop: synthetic_desktop(10, 500),
                parent_pid: 1,
            },
            // PID 10 was reused after this child was born. It is an
            // independent desktop, not a renderer below the first root.
            DesktopProcessRow {
                desktop: synthetic_desktop(20, 400),
                parent_pid: 10,
            },
        ];
        assert_eq!(
            root_desktops_from_rows(&rows),
            vec![synthetic_desktop(10, 500), synthetic_desktop(20, 400)]
        );
    }

    #[test]
    fn malformed_parent_cycle_fails_closed_as_multiple_roots() {
        let rows = vec![
            DesktopProcessRow {
                desktop: synthetic_desktop(10, 100),
                parent_pid: 20,
            },
            DesktopProcessRow {
                desktop: synthetic_desktop(20, 200),
                parent_pid: 10,
            },
        ];
        assert_eq!(
            root_desktops_from_rows(&rows),
            vec![synthetic_desktop(10, 100), synthetic_desktop(20, 200)]
        );
    }

    struct Fake {
        running: bool,
        events: Vec<&'static str>,
        close_failure: bool,
        start_failure: bool,
        closed: Rc<Cell<bool>>,
        pending: Option<Desktop>,
        extra_roots: Vec<Desktop>,
        close_targets: Vec<Desktop>,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                running: true,
                events: vec![],
                close_failure: false,
                start_failure: false,
                closed: Rc::new(Cell::new(false)),
                pending: None,
                extra_roots: vec![],
                close_targets: vec![],
            }
        }
    }

    #[test]
    fn snapshot_of_old_desktop_cannot_close_a_user_reopened_desktop() {
        let mut runtime = BoundRuntime {
            inner: Fake::new(),
            expected: Some(synthetic_receipt(u32::MAX, 1)),
        };
        assert!(restart_with_action_mode(
            &mut runtime,
            &|| Ok(()),
            || panic!("must not enable"),
            false
        )
        .is_err());
        assert_eq!(runtime.inner.events, vec!["inspect"]);
    }

    #[test]
    fn no_desktop_snapshot_cannot_close_a_later_user_launch() {
        let mut runtime = BoundRuntime {
            inner: Fake::new(),
            expected: None,
        };
        assert!(restart_with_action_mode(
            &mut runtime,
            &|| Ok(()),
            || panic!("must not enable"),
            false
        )
        .is_err());
        assert_eq!(runtime.inner.events, vec!["inspect"]);
    }
    impl RestartRuntime for Fake {
        fn pending_closed_desktop(&mut self) -> Option<Desktop> {
            self.pending.clone()
        }
        fn remember_closed_desktop(&mut self, desktop: &Desktop) {
            self.pending = Some(desktop.clone());
        }
        fn clear_pending_desktop(&mut self) {
            self.pending = None;
        }
        fn running_desktops(&mut self) -> Result<Vec<Desktop>, String> {
            self.events.push("inspect");
            if self.running {
                let mut roots = vec![synthetic_desktop(10, 20)];
                roots.extend(self.extra_roots.clone());
                Ok(roots)
            } else {
                Ok(vec![])
            }
        }
        fn close_and_confirm(
            &mut self,
            desktops: &[Desktop],
            _: &dyn Fn() -> Result<(), String>,
        ) -> Result<(), String> {
            self.events.push("close-confirm");
            self.close_targets = desktops.to_vec();
            if self.close_failure {
                Err("not exited".into())
            } else {
                self.closed.set(true);
                self.running = false;
                Ok(())
            }
        }
        fn activate_and_confirm(
            &mut self,
            _: &Desktop,
            _: &dyn Fn() -> Result<(), String>,
        ) -> Result<(), String> {
            self.events.push("activate-confirm");
            if self.start_failure {
                Err("activation failed".into())
            } else {
                self.running = true;
                self.extra_roots.clear();
                Ok(())
            }
        }
    }

    #[test]
    fn closed_desktop_is_not_started() {
        let mut fake = Fake::new();
        fake.running = false;
        assert!(!restart_with(&mut fake, &|| Ok(())).unwrap());
        assert_eq!(fake.events, ["inspect"]);
    }
    #[test]
    fn unconfirmed_exit_does_not_launch_again() {
        let mut fake = Fake::new();
        fake.close_failure = true;
        assert!(restart_with(&mut fake, &|| Ok(())).is_err());
        assert_eq!(fake.events, ["inspect", "close-confirm"]);
    }
    #[test]
    fn exit_is_confirmed_before_activation() {
        let mut fake = Fake::new();
        assert!(restart_with(&mut fake, &|| Ok(())).unwrap());
        assert_eq!(
            fake.events,
            ["inspect", "close-confirm", "activate-confirm"]
        );
    }
    #[test]
    fn activation_failure_is_reported_without_replaying_close_or_start() {
        let mut fake = Fake::new();
        fake.start_failure = true;
        assert_eq!(
            restart_with(&mut fake, &|| Ok(())).unwrap_err(),
            "activation failed"
        );
        assert_eq!(
            fake.events,
            ["inspect", "close-confirm", "activate-confirm"]
        );
    }
    #[test]
    fn superseded_plan_does_not_close() {
        let mut fake = Fake::new();
        let count = Cell::new(0);
        assert!(restart_with(&mut fake, &|| {
            count.set(count.get() + 1);
            if count.get() < 2 {
                Ok(())
            } else {
                Err("cancelled".into())
            }
        })
        .is_err());
        assert_eq!(fake.events, ["inspect"]);
    }
    #[test]
    fn cancellation_after_exit_does_not_start() {
        let mut fake = Fake::new();
        let count = Cell::new(0);
        assert!(restart_with(&mut fake, &|| {
            count.set(count.get() + 1);
            if count.get() < 3 {
                Ok(())
            } else {
                Err("cancelled".into())
            }
        })
        .is_err());
        assert_eq!(fake.events, ["inspect", "close-confirm"]);
    }

    #[test]
    fn enabling_runs_once_after_confirmed_exit() {
        let mut fake = Fake::new();
        let closed = fake.closed.clone();
        let count = Cell::new(0);
        assert!(restart_with_action(&mut fake, &|| Ok(()), || {
            assert!(closed.get());
            count.set(count.get() + 1);
            Ok(())
        })
        .unwrap());
        assert_eq!(count.get(), 1);
        assert_eq!(
            fake.events,
            ["inspect", "close-confirm", "activate-confirm"]
        );
    }

    #[test]
    fn enable_failure_does_not_activate_or_replay() {
        let mut fake = Fake::new();
        let count = Cell::new(0);
        assert_eq!(
            restart_with_action(&mut fake, &|| Ok(()), || {
                count.set(count.get() + 1);
                Err("enable failed".into())
            })
            .unwrap_err(),
            "enable failed"
        );
        assert_eq!(count.get(), 1);
        assert_eq!(fake.events, ["inspect", "close-confirm"]);
    }

    #[test]
    fn desktop_absent_enables_without_starting() {
        let mut fake = Fake::new();
        fake.running = false;
        let count = Cell::new(0);
        assert!(!restart_with_action(&mut fake, &|| Ok(()), || {
            count.set(count.get() + 1);
            Ok(())
        })
        .unwrap());
        assert_eq!(count.get(), 1);
        assert_eq!(fake.events, ["inspect"]);
    }

    #[test]
    fn failed_close_never_enables_an_account() {
        let mut fake = Fake::new();
        fake.close_failure = true;
        let count = Cell::new(0);
        assert!(restart_with_action(&mut fake, &|| Ok(()), || {
            count.set(count.get() + 1);
            Ok(())
        })
        .is_err());
        assert_eq!(count.get(), 0);
        assert_eq!(fake.events, ["inspect", "close-confirm"]);
    }

    #[test]
    fn new_manual_action_can_reopen_desktop_closed_by_cancelled_old_plan() {
        let mut fake = Fake::new();
        let count = Cell::new(0);
        assert!(restart_with_action_mode(
            &mut fake,
            &|| {
                count.set(count.get() + 1);
                if count.get() < 3 {
                    Ok(())
                } else {
                    Err("cancelled".into())
                }
            },
            || Ok(()),
            true
        )
        .is_err());
        assert!(!fake.running);
        assert!(fake.pending.is_some());
        assert!(restart_with_action_mode(&mut fake, &|| Ok(()), || Ok(()), true).unwrap());
        assert!(fake.pending.is_none());
        assert_eq!(
            fake.events,
            ["inspect", "close-confirm", "inspect", "activate-confirm"]
        );
    }

    #[test]
    fn automatic_checks_do_not_replay_cancelled_pending_desktop_launch() {
        let mut fake = Fake::new();
        fake.start_failure = true;
        assert!(restart_with_action_mode(&mut fake, &|| Ok(()), || Ok(()), true).is_err());
        fake.start_failure = false;
        assert!(!fake.running);
        assert!(fake.pending.is_some());
        assert!(!restart_with_action_mode(&mut fake, &|| Ok(()), || Ok(()), false).unwrap());
        assert!(fake.pending.is_some());
        assert_eq!(
            fake.events,
            ["inspect", "close-confirm", "activate-confirm", "inspect"]
        );
    }

    #[test]
    fn uncertain_activation_is_reconciled_by_fresh_process_inventory() {
        let mut fake = Fake::new();
        fake.start_failure = true;
        assert!(restart_with_action_mode(&mut fake, &|| Ok(()), || Ok(()), true).is_err());
        assert!(fake.pending.is_some());
        // The launch actually occurred even though its acknowledgement was
        // unclear. A new user click inspects that desktop and performs one
        // fresh restart, instead of replaying the old activation request.
        fake.running = true;
        fake.start_failure = false;
        assert!(restart_with_action_mode(&mut fake, &|| Ok(()), || Ok(()), true).unwrap());
        assert!(fake.pending.is_none());
        assert_eq!(
            fake.events,
            [
                "inspect",
                "close-confirm",
                "activate-confirm",
                "inspect",
                "close-confirm",
                "activate-confirm"
            ]
        );
    }
}
