//! Restart the installed Codex desktop after CC Switch has enabled an account.
//! The ordinary provider switch is never blocked by this optional operation.
//! Windows Restart Manager requests normal session cleanup (no force flag),
//! and package activation preserves the desktop's original launch environment.

use std::sync::{Arc, LazyLock, Mutex};

static RESTART_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopReceipt {
    pid: u32,
    birth: u64,
}

pub fn desktop_receipt() -> Result<Option<DesktopReceipt>, String> {
    #[cfg(target_os = "windows")]
    {
        let rows = native::desktops()?;
        if rows.len() > 1 {
            return Err("检测到多个独立 Codex 桌面；未选择关闭目标".into());
        }
        Ok(rows.first().map(|desktop| DesktopReceipt {
            pid: desktop.pid,
            birth: desktop.birth,
        }))
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
    validate_same_desktop(expected, &desktop_receipt()?)
}

pub fn ensure_identity_desktop(expected: &IdentityReceipt) -> Result<(), String> {
    ensure_same_desktop(&Some(DesktopReceipt {
        pid: expected.pid,
        birth: expected.birth,
    }))
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
        let expected = DesktopReceipt {
            pid: expected.pid,
            birth: expected.birth,
        };
        wait_for_navigation_with(&expected, guard, || {
            Ok(native::desktops()?
                .into_iter()
                .map(|desktop| DesktopReceipt {
                    pid: desktop.pid,
                    birth: desktop.birth,
                })
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
    let _lock = RESTART_LOCK.lock().await;
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct Desktop {
    pid: u32,
    birth: u64,
    family: String,
    application_id: String,
}

trait RestartRuntime {
    fn running_desktop(&mut self) -> Result<Option<Desktop>, String>;
    fn pending_closed_desktop(&mut self) -> Option<Desktop> {
        None
    }
    fn remember_closed_desktop(&mut self, _: &Desktop) {}
    fn clear_pending_desktop(&mut self) {}
    fn close_and_confirm(
        &mut self,
        desktop: &Desktop,
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
    fn running_desktop(&mut self) -> Result<Option<Desktop>, String> {
        let current = self.inner.running_desktop()?;
        let receipt = current.as_ref().map(|desktop| DesktopReceipt {
            pid: desktop.pid,
            birth: desktop.birth,
        });
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
        desktop: &Desktop,
        guard: &dyn Fn() -> Result<(), String>,
    ) -> Result<(), String> {
        self.inner.close_and_confirm(desktop, guard)
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
    let Some(desktop) = runtime.running_desktop()? else {
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
    runtime.close_and_confirm(&desktop, check)?;
    runtime.remember_closed_desktop(&desktop);
    // A separate post-exit guard records invalidation before launching. No
    // retry will replay the previous close when either guard fails.
    check()?;
    after_exit()?;
    check()?;
    runtime.activate_and_confirm(&desktop, check)?;
    runtime.clear_pending_desktop();
    Ok(true)
}

#[cfg(target_os = "windows")]
mod native {
    use super::{Desktop, IdentityReceipt, RestartRuntime, IDENTITY_RECEIPT};
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
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
    use windows_sys::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmShutdown, RmStartSession, RM_PROCESS_INFO,
        RM_UNIQUE_PROCESS,
    };
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::Shell::AO_NOERRORUI;

    pub(super) struct WindowsRuntime {
        pub(super) before_shutdown: Option<Box<dyn FnOnce() -> Result<(), String> + Send>>,
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

    fn desktop_process_rows() -> Result<Vec<(Desktop, u32)>, String> {
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
                        rows.push((desktop, entry.th32ParentProcessID));
                    }
                }
                available = Process32NextW(snapshot.0, &mut entry);
            }
            Ok(rows)
        }
    }

    pub(super) fn desktops() -> Result<Vec<Desktop>, String> {
        let rows = desktop_process_rows()?;
        let ids: std::collections::HashSet<_> = rows.iter().map(|r| r.0.pid).collect();
        Ok(rows
            .into_iter()
            .filter(|(_, parent)| !ids.contains(parent))
            .map(|(desktop, _)| desktop)
            .collect())
    }

    fn process_tree(desktop: &Desktop) -> Result<std::collections::HashSet<(u32, u64)>, String> {
        let rows = desktop_process_rows()?;
        let mut ids = std::collections::HashSet::from([desktop.pid]);
        loop {
            let mut changed = false;
            for (row, parent) in &rows {
                if ids.contains(parent) && ids.insert(row.pid) {
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Ok(rows
            .into_iter()
            .filter(|(row, _)| ids.contains(&row.pid))
            .map(|(row, _)| (row.pid, row.birth))
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
        fn running_desktop(&mut self) -> Result<Option<Desktop>, String> {
            let mut rows = desktops()?;
            match rows.len() {
                0 => Ok(None),
                1 => Ok(rows.pop()),
                _ => Err("检测到多个独立 Codex 桌面；请自行选择需要重开的桌面".into()),
            }
        }

        fn close_and_confirm(
            &mut self,
            desktop: &Desktop,
            guard: &dyn Fn() -> Result<(), String>,
        ) -> Result<(), String> {
            if process(desktop.pid)?.as_ref() != Some(desktop) {
                return Err("原 Codex 桌面进程已变化；本次不会关闭其他进程".into());
            }
            let handle = exact_process_handle(desktop)?;
            let tree = process_tree(desktop)?;
            unsafe {
                let mut session = 0;
                let mut key = [0u16; 33];
                let result = RmStartSession(&mut session, 0, key.as_mut_ptr());
                if result != 0 {
                    return Err(format!("无法发起 Codex 正常退出（Windows {result}）"));
                }
                let session = RestartSession(session);
                let registered = RM_UNIQUE_PROCESS {
                    dwProcessId: desktop.pid,
                    ProcessStartTime: FILETIME {
                        dwLowDateTime: desktop.birth as u32,
                        dwHighDateTime: (desktop.birth >> 32) as u32,
                    },
                };
                let result = RmRegisterResources(session.0, 0, null(), 1, &registered, 0, null());
                if result != 0 {
                    return Err(format!("正常退出登记失败（Windows {result}）"));
                }
                let mut needed = 0;
                let mut count = 1;
                let mut info = RM_PROCESS_INFO::default();
                let mut reasons = 0;
                let result = RmGetList(session.0, &mut needed, &mut count, &mut info, &mut reasons);
                if result == ERROR_MORE_DATA || result != 0 || reasons != 0 || count != 1 {
                    return Err(format!("正常退出目标未能精确核对（Windows {result}，原因 {reasons}）；本次不关闭桌面"));
                }
                if info.Process.dwProcessId != desktop.pid
                    || filetime(&info.Process.ProcessStartTime) != desktop.birth
                {
                    return Err("正常退出清单出现了其他进程；本次不关闭桌面".into());
                }
                // Do not use RmForceShutdown or TerminateProcess. This asks
                // Electron to process its ordinary end-session cleanup.
                if let Some(check_tasks) = self.before_shutdown.take() {
                    check_tasks()?;
                }
                guard()?;
                let app_servers = app_server_handles(&tree)?;
                let result = RmShutdown(session.0, NORMAL_SHUTDOWN_FLAGS, None);
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline {
                    let waited = WaitForSingleObject(handle.0, 100);
                    if waited == 0 {
                        let remaining = desktop_process_rows()?;
                        if remaining.is_empty() && app_servers_exited(&app_servers)? {
                            return Ok(());
                        }
                        if remaining
                            .iter()
                            .any(|(row, _)| !tree.contains(&(row.pid, row.birth)))
                        {
                            return Err(
                                "原 Codex 已退出，但检测到其他桌面进程；不会覆盖用户启动或重复重开"
                                    .into(),
                            );
                        }
                        // The old renderer/GPU children may finish cleanup
                        // slightly after their main process. Wait for those
                        // known identities, without touching any new launch.
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if waited == u32::MAX {
                        return Err("原 Codex 的退出状态无法确认；不会重复启动".into());
                    }
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
            if !desktops()?.is_empty() {
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
                        let roots = desktops()?;
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
            if !desktops()?.is_empty() {
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

    #[test]
    fn navigation_waits_for_late_single_instance_handoff_without_ignoring_extra_desktops() {
        use std::time::Duration;
        let expected = DesktopReceipt { pid: 10, birth: 20 };
        let extra = DesktopReceipt { pid: 11, birth: 21 };
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
        let expected = DesktopReceipt { pid: 10, birth: 20 };
        let extra = DesktopReceipt { pid: 11, birth: 21 };
        assert!(NavigationWatch::default()
            .observe(
                &expected,
                &[expected.clone(), extra.clone()],
                Duration::from_secs(5)
            )
            .is_err());
        for roots in [
            vec![],
            vec![extra],
            vec![DesktopReceipt { pid: 10, birth: 21 }],
        ] {
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
        let expected = DesktopReceipt { pid: 10, birth: 20 };
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
                Ok(vec![
                    expected.clone(),
                    DesktopReceipt { pid: 11, birth: 21 },
                ])
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
        let original = Some(DesktopReceipt { pid: 10, birth: 20 });
        assert!(validate_same_desktop(&original, &original).is_ok());
        for changed in [
            None,
            Some(DesktopReceipt { pid: 11, birth: 20 }),
            Some(DesktopReceipt { pid: 10, birth: 21 }),
        ] {
            assert!(validate_same_desktop(&original, &changed).is_err());
        }
        assert!(validate_same_desktop(&None, &original).is_err());
    }

    struct Fake {
        running: bool,
        events: Vec<&'static str>,
        close_failure: bool,
        start_failure: bool,
        closed: Rc<Cell<bool>>,
        pending: Option<Desktop>,
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
            }
        }
    }

    #[test]
    fn snapshot_of_old_desktop_cannot_close_a_user_reopened_desktop() {
        let mut runtime = BoundRuntime {
            inner: Fake::new(),
            expected: Some(DesktopReceipt {
                pid: u32::MAX,
                birth: 1,
            }),
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
        fn running_desktop(&mut self) -> Result<Option<Desktop>, String> {
            self.events.push("inspect");
            Ok(self.running.then(|| Desktop {
                pid: 10,
                birth: 20,
                family: "OpenAI.Codex_test".into(),
                application_id: "OpenAI.Codex_test!App".into(),
            }))
        }
        fn close_and_confirm(
            &mut self,
            _: &Desktop,
            _: &dyn Fn() -> Result<(), String>,
        ) -> Result<(), String> {
            self.events.push("close-confirm");
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
