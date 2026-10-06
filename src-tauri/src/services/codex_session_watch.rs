//! Native Windows session events invalidate decisions even across a brief
//! lock/unlock between quota checks. The window procedure only updates atomics;
//! it never takes the account-selection lock or performs a switch.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

struct SessionState {
    ready: AtomicBool,
    blocked: AtomicBool,
    epoch: AtomicU64,
}

impl SessionState {
    const fn new() -> Self {
        Self {
            ready: AtomicBool::new(false),
            blocked: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
        }
    }

    fn observe(&self, event: u32) {
        match event {
            // Console disconnect, remote disconnect, logoff, lock, terminate.
            2 | 4 | 6 | 7 | 11 => {
                self.blocked.store(true, Ordering::SeqCst);
                self.epoch.fetch_add(1, Ordering::SeqCst);
            }
            // Reconnection/unlock allows a new timer decision only. It does
            // not reset the epoch or dispatch any pending operation.
            1 | 3 | 5 | 8 => self.blocked.store(false, Ordering::SeqCst),
            _ => {}
        }
    }

    fn validate(&self, expected: u64) -> Result<(), String> {
        if !self.ready.load(Ordering::SeqCst) {
            return Err("Windows 会话事件监测不可用；自动换号已停止".into());
        }
        if self.blocked.load(Ordering::SeqCst) || self.epoch.load(Ordering::SeqCst) != expected {
            return Err("Windows 会话曾锁屏或断开，本次旧换号计划已作废；等待新的定时检查".into());
        }
        Ok(())
    }
}

static SESSION: SessionState = SessionState::new();
static START_RESULT: OnceLock<Result<(), String>> = OnceLock::new();

pub fn ensure_started() -> Result<(), String> {
    START_RESULT.get_or_init(start_native).clone()?;
    if !SESSION.ready.load(Ordering::SeqCst) {
        return Err("Windows 会话事件监测已经退出；请重开 CC Switch 后再启用自动换号".into());
    }
    Ok(())
}

pub fn capture_epoch() -> Result<u64, String> {
    ensure_started()?;
    let epoch = SESSION.epoch.load(Ordering::SeqCst);
    SESSION.validate(epoch)?;
    Ok(epoch)
}

/// Manual Enable is preserved if the automatic watcher could not be started.
pub fn current_epoch_if_ready() -> Option<u64> {
    SESSION
        .ready
        .load(Ordering::SeqCst)
        .then(|| SESSION.epoch.load(Ordering::SeqCst))
}

pub fn validate_epoch(expected: u64) -> Result<(), String> {
    SESSION.validate(expected)
}

#[cfg(not(target_os = "windows"))]
fn start_native() -> Result<(), String> {
    Err("当前平台没有 Windows 会话事件监测；自动桌面换号不可用".into())
}

#[cfg(target_os = "windows")]
fn start_native() -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("codex-session-watch".into())
        .spawn(move || unsafe { native::run(tx) })
        .map_err(|error| format!("无法启动 Windows 会话监测线程：{error}"))?;
    rx.recv_timeout(std::time::Duration::from_secs(3))
        .map_err(|_| "Windows 会话事件监测注册超时；未启动自动换号".to_string())?
}

#[cfg(target_os = "windows")]
mod native {
    use super::*;
    use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        PostQuitMessage, RegisterClassW, TranslateMessage, UnregisterClassW, MSG, WM_DESTROY,
        WM_WTSSESSION_CHANGE, WNDCLASSW, WS_OVERLAPPED,
    };

    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        word: WPARAM,
        parameter: LPARAM,
    ) -> LRESULT {
        if message == WM_WTSSESSION_CHANGE {
            SESSION.observe(word as u32);
            return 0;
        }
        if message == WM_DESTROY {
            if SESSION.ready.swap(false, Ordering::SeqCst) {
                SESSION.epoch.fetch_add(1, Ordering::SeqCst);
            }
            PostQuitMessage(0);
            return 0;
        }
        DefWindowProcW(window, message, word, parameter)
    }

    pub(super) unsafe fn run(ready: std::sync::mpsc::SyncSender<Result<(), String>>) {
        let class: Vec<u16> = "CCSwitchCodexSessionWatch"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let instance = GetModuleHandleW(std::ptr::null());
        if instance.is_null() {
            let _ = ready.send(Err(format!(
                "无法取得会话监测模块（Windows {}）",
                GetLastError()
            )));
            return;
        }
        let definition = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        if RegisterClassW(&definition) == 0 {
            let _ = ready.send(Err(format!(
                "无法注册会话监测窗口（Windows {}）",
                GetLastError()
            )));
            return;
        }
        // A hidden top-level window receives WTS notifications. Never show it,
        // activate it, or rely on CC Switch's visible Tauri window.
        let window = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if window.is_null() {
            let code = GetLastError();
            UnregisterClassW(class.as_ptr(), instance);
            let _ = ready.send(Err(format!("无法创建会话监测窗口（Windows {code}）")));
            return;
        }
        if WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION) == 0 {
            let code = GetLastError();
            DestroyWindow(window);
            UnregisterClassW(class.as_ptr(), instance);
            let _ = ready.send(Err(format!(
                "无法注册本机会话事件（Windows {code}）；自动换号不可用，手动启用仍可使用"
            )));
            return;
        }
        SESSION.ready.store(true, Ordering::SeqCst);
        if ready.send(Ok(())).is_err() {
            WTSUnRegisterSessionNotification(window);
            DestroyWindow(window);
            UnregisterClassW(class.as_ptr(), instance);
            return;
        }
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, std::ptr::null_mut(), 0, 0);
            if result <= 0 {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        SESSION.ready.store(false, Ordering::SeqCst);
        SESSION.epoch.fetch_add(1, Ordering::SeqCst);
        WTSUnRegisterSessionNotification(window);
        DestroyWindow(window);
        UnregisterClassW(class.as_ptr(), instance);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_lock_unlock_invalidates_old_epoch_without_dispatching_a_plan() {
        let state = SessionState::new();
        state.ready.store(true, Ordering::SeqCst);
        state.observe(7);
        state.observe(8);
        assert!(state.validate(0).is_err());
        assert!(state.validate(1).is_ok());
        assert_eq!(state.epoch.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn disconnect_logoff_and_termination_reject_old_plans_after_reconnection() {
        for event in [2, 4, 6, 11] {
            let state = SessionState::new();
            state.ready.store(true, Ordering::SeqCst);
            state.observe(event);
            assert!(state.validate(1).is_err());
            state.observe(1);
            assert!(state.validate(0).is_err());
            assert!(state.validate(1).is_ok());
        }
    }

    #[test]
    fn unavailable_watcher_blocks_automatic_epoch_validation() {
        let state = SessionState::new();
        assert!(state.validate(0).is_err());
    }
}
