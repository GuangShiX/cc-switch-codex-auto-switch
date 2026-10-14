//! Keep a Codex-launched manager out of the host's kill-on-close Job.
//! No account, settings, database or recovery records are accessed here.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static REPORT: OnceLock<String> = OnceLock::new();
static HOST_OWNED: AtomicBool = AtomicBool::new(false);

/// Guard only the known-dangerous fallback, never ordinary manual enabling.
pub fn ensure_independent_for_desktop_shutdown() -> Result<(), String> {
    if HOST_OWNED.load(Ordering::Acquire) {
        Err("CC Switch 启动隔离未确认，关闭 Codex 可能一并结束 CC Switch；已保留当前桌面。请退出 CC Switch 后从开始菜单独立打开再重试".into())
    } else {
        Ok(())
    }
}

/// `true` means this bootstrap must exit without initializing Tauri.
pub fn prepare_startup() -> bool {
    #[cfg(target_os = "windows")]
    {
        return windows::prepare();
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

pub fn log_startup_report() {
    if let Some(report) = REPORT.get() {
        log::info!("[HOST-ISOLATION] {report}");
    }
}

/// Exact Windows creation time in FILETIME units, with no account identifiers.
pub fn current_process_identity() -> Option<(u32, u64)> {
    #[cfg(target_os = "windows")]
    {
        windows::current_identity()
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

pub fn log_shutdown_context(target_pid: u32, target_birth: u64) {
    #[cfg(target_os = "windows")]
    log::info!("[HOST-ISOLATION] before desktop quit target_pid={target_pid} target_birth={target_birth} {}", windows::diagnostic());
    #[cfg(not(target_os = "windows"))]
    let _ = (target_pid, target_birth);
}

#[cfg(any(target_os = "windows", test))]
fn needs_handoff(codex_ancestor: bool, in_job: bool) -> bool {
    codex_ancestor && in_job
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{needs_handoff, Ordering, HOST_OWNED, REPORT};
    use std::{
        collections::HashMap,
        ffi::OsStr,
        mem::{size_of, zeroed},
        os::windows::ffi::OsStrExt,
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, FILETIME, HANDLE,
            INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Globalization::CompareStringOrdinal,
        Storage::Packaging::Appx::GetPackageFamilyName,
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            Environment::{FreeEnvironmentStringsW, GetCommandLineW, GetEnvironmentStringsW},
            JobObjects::{
                IsProcessInJob, JobObjectExtendedLimitInformation, QueryInformationJobObject,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            },
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{
                CreateEventW, CreateProcessW, GetCurrentProcess, GetCurrentProcessId,
                GetExitCodeProcess, GetProcessTimes, OpenEventW, OpenProcess,
                QueryFullProcessImageNameW, ResetEvent, SetEvent, WaitForMultipleObjects,
                WaitForSingleObject, CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW,
                CREATE_UNICODE_ENVIRONMENT, EVENT_MODIFY_STATE, INFINITE, PROCESS_INFORMATION,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, STARTUPINFOW,
            },
        },
    };
    const HOP: &str = "_CC_SWITCH_HOST_ISOLATION_V1";
    const FAMILY: &str = "OpenAI.Codex_2p2nqsd0c76g0";
    const TIMEOUT: u32 = 5_000;
    static ABORT_SIGNAL_FAILED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    struct Handoff {
        pid: u32,
        activation_confirmed: bool,
    }

    pub(super) struct Handle(pub HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    fn handle(value: HANDLE, operation: &str) -> Result<Handle, String> {
        if value.is_null() || value == INVALID_HANDLE_VALUE {
            Err(format!("{operation}: win32={}", unsafe { GetLastError() }))
        } else {
            Ok(Handle(value))
        }
    }
    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }
    unsafe fn read_wide(pointer: *const u16) -> Vec<u16> {
        let mut len = 0;
        while len < 32_768 && *pointer.add(len) != 0 {
            len += 1;
        }
        std::slice::from_raw_parts(pointer, len).to_vec()
    }
    fn birth(process: HANDLE) -> Result<u64, String> {
        let (mut created, mut exited, mut kernel, mut user): (
            FILETIME,
            FILETIME,
            FILETIME,
            FILETIME,
        ) = unsafe { zeroed() };
        if unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }
            == 0
        {
            return Err(format!("GetProcessTimes: win32={}", unsafe {
                GetLastError()
            }));
        }
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
    pub(super) fn current_identity() -> Option<(u32, u64)> {
        Some((
            unsafe { GetCurrentProcessId() },
            birth(unsafe { GetCurrentProcess() }).ok()?,
        ))
    }
    fn session(pid: u32) -> Result<u32, String> {
        let mut value = 0;
        if unsafe { ProcessIdToSessionId(pid, &mut value) } == 0 {
            return Err(format!("ProcessIdToSessionId: win32={}", unsafe {
                GetLastError()
            }));
        }
        Ok(value)
    }
    fn image(process: HANDLE) -> Result<Vec<u16>, String> {
        let mut buffer = vec![0; 32_768];
        let mut count = buffer.len() as u32;
        if unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut count) } == 0 {
            return Err(format!("QueryFullProcessImageNameW: win32={}", unsafe {
                GetLastError()
            }));
        }
        buffer.truncate(count as usize);
        Ok(buffer)
    }
    fn parents() -> Result<HashMap<u32, u32>, String> {
        let snap = handle(
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) },
            "CreateToolhelp32Snapshot",
        )?;
        let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut result = HashMap::new();
        let mut ok = unsafe { Process32FirstW(snap.0, &mut entry) };
        if ok == 0 {
            return Err(format!("Process32FirstW: win32={}", unsafe {
                GetLastError()
            }));
        }
        while ok != 0 {
            result.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            ok = unsafe { Process32NextW(snap.0, &mut entry) };
        }
        Ok(result)
    }
    fn codex_ancestor() -> Result<bool, String> {
        let map = parents()?;
        let mut pid = unsafe { GetCurrentProcessId() };
        let own_session = session(pid)?;
        let mut child_birth = birth(unsafe { GetCurrentProcess() })?;
        let mut visited = std::collections::HashSet::new();
        for _ in 0..32 {
            let Some(&parent_pid) = map.get(&pid) else {
                return Ok(false);
            };
            if parent_pid == 0 || !visited.insert(parent_pid) {
                return Ok(false);
            }
            let process = handle(
                unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, parent_pid) },
                "OpenProcess ancestor",
            )?;
            let parent_birth = birth(process.0)?;
            if parent_birth > child_birth || session(parent_pid)? != own_session {
                return Ok(false);
            }
            let path = String::from_utf16_lossy(&image(process.0)?)
                .replace('/', "\\")
                .to_ascii_lowercase();
            if (path.ends_with("\\codex.exe") || path.ends_with("\\chatgpt.exe"))
                && path.contains("\\windowsapps\\openai.codex_")
            {
                let mut name = vec![0u16; 512];
                let mut len = name.len() as u32;
                let code = unsafe { GetPackageFamilyName(process.0, &mut len, name.as_mut_ptr()) };
                if code == 0
                    && len > 0
                    && len <= name.len() as u32
                    && String::from_utf16_lossy(&name[..len as usize - 1]) == FAMILY
                {
                    return Ok(true);
                }
            }
            // An interactive Explorer origin ends the host chain. It may itself
            // belong to a Windows Job and must never be treated as unsafe.
            if path.ends_with("\\explorer.exe") {
                return Ok(false);
            }
            pid = parent_pid;
            child_birth = parent_birth;
        }
        Err("ancestor chain exceeded 32 entries".into())
    }
    fn in_job() -> Result<bool, String> {
        let mut value = 0;
        if unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut value) } == 0 {
            return Err(format!("IsProcessInJob: win32={}", unsafe {
                GetLastError()
            }));
        }
        Ok(value != 0)
    }
    fn job_flags() -> Result<Option<u32>, String> {
        if !in_job()? {
            return Ok(None);
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        if unsafe {
            QueryInformationJobObject(
                null_mut(),
                JobObjectExtendedLimitInformation,
                (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(format!("QueryInformationJobObject: win32={}", unsafe {
                GetLastError()
            }));
        }
        Ok(Some(info.BasicLimitInformation.LimitFlags))
    }
    pub(super) fn diagnostic() -> String {
        format!(
            "manager_pid={} manager_birth={:?} job_flags={:?} codex_ancestor={:?} abort_signal_error={}",
            unsafe { GetCurrentProcessId() },
            birth(unsafe { GetCurrentProcess() }),
            job_flags(),
            codex_ancestor(),
            ABORT_SIGNAL_FAILED.load(Ordering::Acquire)
        )
    }
    fn events(nonce: &str) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
        (
            wide(OsStr::new(&format!(
                "Local\\CCSwitch-Isolation-{nonce}-ack"
            ))),
            wide(OsStr::new(&format!(
                "Local\\CCSwitch-Isolation-{nonce}-commit"
            ))),
            wide(OsStr::new(&format!(
                "Local\\CCSwitch-Isolation-{nonce}-abort"
            ))),
        )
    }
    struct AbortOnDrop<'a> {
        event: &'a Handle,
        armed: bool,
    }
    impl Drop for AbortOnDrop<'_> {
        fn drop(&mut self) {
            if self.armed {
                if unsafe { SetEvent(self.event.0) } == 0 {
                    ABORT_SIGNAL_FAILED.store(unsafe { GetLastError() }, Ordering::Release);
                }
            }
        }
    }
    fn event(name: &[u16]) -> Result<Handle, String> {
        let value = unsafe { CreateEventW(null(), 1, 0, name.as_ptr()) };
        let error = unsafe { GetLastError() };
        let event = handle(value, "CreateEventW")?;
        if error == ERROR_ALREADY_EXISTS {
            return Err("isolation event unexpectedly existed".into());
        }
        Ok(event)
    }
    // Copy the Windows environment block, including drive-current-directory
    // pseudo variables, without exposing its values or mutating the parent.
    fn environment(marker: &str) -> Result<Vec<u16>, String> {
        let pointer = unsafe { GetEnvironmentStringsW() };
        if pointer.is_null() {
            return Err(format!("GetEnvironmentStringsW: win32={}", unsafe {
                GetLastError()
            }));
        }
        let mut entries = Vec::new();
        let mut offset = 0;
        unsafe {
            while *pointer.add(offset) != 0 {
                let entry = read_wide(pointer.add(offset));
                offset += entry.len() + 1;
                entries.push(entry);
            }
            FreeEnvironmentStringsW(pointer);
        }
        Ok(environment_block(entries, marker))
    }
    fn environment_block(mut entries: Vec<Vec<u16>>, marker: &str) -> Vec<u16> {
        entries.retain(|entry| {
            !String::from_utf16_lossy(entry)
                .to_ascii_uppercase()
                .starts_with(&format!("{HOP}="))
        });
        let mut hop = wide(OsStr::new(&format!("{HOP}={marker}")));
        hop.pop();
        entries.push(hop);
        // CreateProcess requires a case-insensitively sorted Unicode block.
        entries.sort_by(|a, b| {
            unsafe {
                CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1)
            }
            .cmp(&2)
        });
        let mut result = Vec::new();
        for entry in entries {
            result.extend(entry);
            result.push(0);
        }
        result.push(0);
        result
    }
    fn child(marker: &str) -> Result<(), u32> {
        // A bounded hop is never retried, even if its proof fails.
        std::env::remove_var(HOP);
        let fields: Vec<_> = marker.split(':').collect();
        if fields.len() != 3 || uuid::Uuid::parse_str(fields[0]).is_err() {
            return Err(124);
        }
        let parent_pid: u32 = fields[1].parse().map_err(|_| 124u32)?;
        let parent_birth: u64 = fields[2].parse().map_err(|_| 124u32)?;
        let map = parents().map_err(|_| 124u32)?;
        if map.get(&unsafe { GetCurrentProcessId() }) != Some(&parent_pid) {
            return Err(124);
        }
        let parent = handle(
            unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    parent_pid,
                )
            },
            "OpenProcess parent",
        )
        .map_err(|_| 124u32)?;
        if birth(parent.0).map_err(|_| 124u32)? != parent_birth
            || image(parent.0).map_err(|_| 124u32)?
                != image(unsafe { GetCurrentProcess() }).map_err(|_| 124u32)?
            || session(parent_pid).map_err(|_| 124u32)?
                != session(unsafe { GetCurrentProcessId() }).map_err(|_| 124u32)?
        {
            return Err(124);
        }
        // QueryInformationJobObject(NULL) only sees the innermost nested Job.
        // For this handoff, prove complete breakaway rather than assuming an
        // apparently harmless innermost Job excludes an outer kill-on-close.
        if in_job().map_err(|_| 123u32)? {
            return Err(122);
        }
        let (ack_name, commit_name, abort_name) = events(fields[0]);
        let ack = handle(
            unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, ack_name.as_ptr()) },
            "OpenEvent ack",
        )
        .map_err(|_| 125u32)?;
        let commit = handle(
            unsafe { OpenEventW(PROCESS_SYNCHRONIZE, 0, commit_name.as_ptr()) },
            "OpenEvent commit",
        )
        .map_err(|_| 125u32)?;
        let abort = handle(
            unsafe { OpenEventW(PROCESS_SYNCHRONIZE, 0, abort_name.as_ptr()) },
            "OpenEvent abort",
        )
        .map_err(|_| 125u32)?;
        if unsafe { SetEvent(ack.0) } == 0 {
            return Err(125);
        }
        let wait = unsafe {
            WaitForMultipleObjects(3, [commit.0, abort.0, parent.0].as_ptr(), 0, INFINITE)
        };
        if wait != WAIT_OBJECT_0 {
            return Err(121);
        }
        // The parent resets ACK before sending COMMIT. This second signal
        // proves that the child accepted ownership rather than merely reaching
        // the pre-application wait. No application work precedes COMMIT.
        if unsafe { SetEvent(ack.0) } == 0 {
            return Err(125);
        }
        let _ = REPORT.set(format!("handoff confirmed; {}", diagnostic()));
        Ok(())
    }
    fn handoff() -> Result<Handoff, String> {
        let nonce = uuid::Uuid::new_v4().to_string();
        let (ack_name, commit_name, abort_name) = events(&nonce);
        let ack = event(&ack_name)?;
        let commit = event(&commit_name)?;
        let abort = event(&abort_name)?;
        let marker = format!(
            "{nonce}:{}:{}",
            unsafe { GetCurrentProcessId() },
            birth(unsafe { GetCurrentProcess() })?
        );
        let env = environment(&marker)?;
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let exe = wide(exe.as_os_str());
        let mut command = unsafe { read_wide(GetCommandLineW()) };
        command.push(0);
        spawn_handoff(&exe, &mut command, &env, &ack, &commit, &abort)
    }
    fn spawn_handoff(
        exe: &[u16],
        command: &mut [u16],
        env: &[u16],
        ack: &Handle,
        commit: &Handle,
        abort: &Handle,
    ) -> Result<Handoff, String> {
        spawn_handoff_policy(exe, command, env, ack, commit, abort, true)
    }
    fn spawn_handoff_policy(
        exe: &[u16],
        command: &mut [u16],
        env: &[u16],
        ack: &Handle,
        commit: &Handle,
        abort: &Handle,
        allow_commit: bool,
    ) -> Result<Handoff, String> {
        let mut abort_guard = AbortOnDrop {
            event: abort,
            armed: true,
        };
        let mut startup: STARTUPINFOW = unsafe { zeroed() };
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        let mut process: PROCESS_INFORMATION = unsafe { zeroed() };
        if unsafe {
            CreateProcessW(
                exe.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                0,
                CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                env.as_ptr().cast(),
                null(),
                &startup,
                &mut process,
            )
        } == 0
        {
            return Err(format!("CreateProcessW breakaway: win32={}", unsafe {
                GetLastError()
            }));
        }
        let child_process = Handle(process.hProcess);
        let _thread = Handle(process.hThread);
        let result =
            unsafe { WaitForMultipleObjects(2, [ack.0, child_process.0].as_ptr(), 0, TIMEOUT) };
        if allow_commit
            && result == WAIT_OBJECT_0
            && unsafe { WaitForSingleObject(child_process.0, 0) } == WAIT_TIMEOUT
            && unsafe { ResetEvent(ack.0) } != 0
            && unsafe { SetEvent(commit.0) } != 0
        {
            // From this point the child owns application startup. An uncertain
            // final receipt must never fall back to starting a second manager
            // or signal ABORT to a possibly active child.
            abort_guard.armed = false;
            let activation =
                unsafe { WaitForMultipleObjects(2, [child_process.0, ack.0].as_ptr(), 0, TIMEOUT) };
            if activation == WAIT_OBJECT_0 {
                let mut exit_code = 0;
                unsafe { GetExitCodeProcess(child_process.0, &mut exit_code) };
                return Err(format!(
                    "committed bootstrap exited before activation: child_pid={} child_exit={exit_code}; original manager continues",
                    process.dwProcessId
                ));
            }
            return Ok(Handoff {
                pid: process.dwProcessId,
                activation_confirmed: activation == WAIT_OBJECT_0 + 1,
            });
        }
        // COMMIT was never sent, so the exact new bootstrap may safely be
        // aborted. Wait for that child handle, not a name or process tree,
        // and report whether it really exited before the parent continues.
        if unsafe { SetEvent(abort.0) } != 0 {
            abort_guard.armed = false;
        }
        let abort_wait = unsafe { WaitForSingleObject(child_process.0, TIMEOUT) };
        let mut exit_code = 0;
        unsafe {
            GetExitCodeProcess(child_process.0, &mut exit_code);
        }
        Err(format!("bootstrap proof/commit failed: wait={result} abort_wait={abort_wait} child_pid={} child_exit={exit_code}; original manager continues", process.dwProcessId))
    }
    pub(super) fn prepare() -> bool {
        if let Some(marker) = std::env::var_os(HOP) {
            match marker.to_str().ok_or(124).and_then(child) {
                Ok(()) => return false,
                Err(code) => std::process::exit(code as i32),
            }
        }
        let result: Result<bool, String> = (|| {
            let ancestor = codex_ancestor()?;
            // Only verified Codex ancestry creates a shutdown guard. Until a
            // no-Job result is proven, a failed Job query must not let this
            // manager blindly close a possibly owning desktop process.
            HOST_OWNED.store(ancestor, Ordering::Release);
            let in_job = in_job()?;
            // Nested outer jobs can kill their descendants even when the
            // innermost job reports no KILL flag. Attempt only for verified
            // Codex ancestry; normal user launches remain entirely unchanged.
            if !needs_handoff(ancestor, in_job) {
                HOST_OWNED.store(false, Ordering::Release);
                return Ok(false);
            }
            let handoff = handoff()?;
            let _ = REPORT.set(format!(
                "handoff committed to child_pid={} activation_confirmed={}",
                handoff.pid, handoff.activation_confirmed
            ));
            Ok(true)
        })();
        match result {
            Ok(exit) => {
                if !exit {
                    let _ = REPORT.set(format!("normal launch; {}", diagnostic()));
                }
                exit
            }
            Err(error) => {
                let _ = REPORT.set(format!(
                    "isolation unavailable: {error}; {}; continuing normal manager",
                    diagnostic()
                ));
                false
            }
        }
    }

    #[cfg(test)]
    pub(super) fn fixture_handoff(command: &str) -> Result<u32, String> {
        fixture_handoff_policy(command, true)
    }
    #[cfg(test)]
    pub(super) fn fixture_handoff_policy(command: &str, allow_commit: bool) -> Result<u32, String> {
        let nonce = uuid::Uuid::new_v4().to_string();
        let (a, c, b) = events(&nonce);
        let ack = event(&a)?;
        let commit = event(&c)?;
        let abort = event(&b)?;
        let marker = format!(
            "{nonce}:{}:{}",
            unsafe { GetCurrentProcessId() },
            birth(unsafe { GetCurrentProcess() })?
        );
        let env = environment(&marker)?;
        let exe = wide(std::env::current_exe().unwrap().as_os_str());
        let mut command = wide(OsStr::new(command));
        spawn_handoff_policy(
            &exe,
            &mut command,
            &env,
            &ack,
            &commit,
            &abort,
            allow_commit,
        )
        .map(|handoff| handoff.pid)
    }
    #[cfg(test)]
    #[test]
    fn environment_roundtrip_preserves_unicode_and_drive_pseudo_variables() {
        let entries = [
            "Zed=尾部",
            "=C:=C:\\原目录",
            "Alpha=中文 空格=值",
            "_cc_switch_host_isolation_v1=stale",
        ]
        .iter()
        .map(|s| s.encode_utf16().collect())
        .collect();
        let block = environment_block(entries, "nonce:123:456");
        assert_eq!(&block[block.len() - 2..], &[0, 0]);
        let restored: Vec<String> = block
            .split(|c| *c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect();
        assert_eq!(restored.len(), 4);
        assert_eq!(restored[0], "=C:=C:\\原目录");
        assert!(restored.iter().any(|s| s == "Alpha=中文 空格=值"));
        assert!(restored.iter().any(|s| s == "Zed=尾部"));
        assert_eq!(
            restored
                .iter()
                .filter(|s| s.to_ascii_uppercase().starts_with(HOP))
                .count(),
            1
        );
        assert!(restored
            .iter()
            .any(|s| s == &format!("{HOP}=nonce:123:456")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normal_windows_job_does_not_trigger_handoff() {
        assert!(!needs_handoff(false, true));
        assert!(!needs_handoff(false, false));
        assert!(!needs_handoff(true, false));
        assert!(needs_handoff(true, true));
    }
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "Subprocess fixture only; no database or desktop app is initialized"]
    fn isolated_child_fixture() {
        assert!(!windows::prepare());
        let path =
            std::env::var_os("CC_SWITCH_ISOLATION_FIXTURE_RECEIPT").expect("fixture receipt path");
        std::fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "pid": std::process::id(),
                "cwd": std::env::current_dir().unwrap(),
                "args": std::env::args().collect::<Vec<_>>(),
                "unicodeEnv": std::env::var("CC_SWITCH_ISOLATION_FIXTURE_UNICODE").ok(),
                "hopRemoved": std::env::var_os("_CC_SWITCH_HOST_ISOLATION_V1").is_none(),
            }))
            .unwrap(),
        )
        .unwrap();
        // Parent fixture can close its kill-on-close Job while this child lives.
        std::thread::sleep(std::time::Duration::from_secs(4));
    }
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "Run in an isolated fixture Job; never starts production CC Switch"]
    fn handoff_fixture_parent() {
        let exe = std::env::current_exe().unwrap();
        let command = format!(
            "\"{}\" --ignored --exact startup_isolation::tests::isolated_child_fixture --nocapture --skip \"中文 空格\"",
            exe.display()
        );
        let child = windows::fixture_handoff(&command).expect("breakaway child handshake");
        println!("ISOLATED_CHILD_PID={child}");
    }
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "Native subprocess fixture needs a Job permitting breakaway; no desktop app is initialized"]
    fn unacknowledged_child_never_receives_commit() {
        let exe = std::env::current_exe().unwrap();
        // This existing pure test immediately exits without consuming the hop.
        let command = format!(
            "\"{}\" --exact startup_isolation::tests::normal_windows_job_does_not_trigger_handoff",
            exe.display()
        );
        let error = windows::fixture_handoff(&command).unwrap_err();
        assert!(error.contains("bootstrap proof/commit failed"));
    }
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "Isolated subprocess fixture: acknowledged bootstrap is aborted before app initialization"]
    fn aborted_handoff_fixture_parent() {
        let exe = std::env::current_exe().unwrap();
        let command = format!(
            "\"{}\" --ignored --exact startup_isolation::tests::isolated_child_fixture --nocapture",
            exe.display()
        );
        let error = windows::fixture_handoff_policy(&command, false).unwrap_err();
        assert!(error.contains("bootstrap proof/commit failed"));
        assert!(
            error.contains("abort_wait=0"),
            "child exit must be observed: {error}"
        );
        assert!(
            error.contains("child_exit=121"),
            "child must reject missing COMMIT: {error}"
        );
        let receipt = std::env::var_os("CC_SWITCH_ISOLATION_FIXTURE_RECEIPT").unwrap();
        assert!(
            !std::path::Path::new(&receipt).exists(),
            "ABORT must not enter the app fixture"
        );
    }
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "Run only in a fixture Job with breakaway denied; never starts production CC Switch"]
    fn denied_handoff_fixture_parent() {
        let exe = std::env::current_exe().unwrap();
        let command = format!(
            "\"{}\" --ignored --exact startup_isolation::tests::isolated_child_fixture --nocapture",
            exe.display()
        );
        let error = windows::fixture_handoff(&command).unwrap_err();
        assert!(
            error.starts_with("CreateProcessW breakaway:"),
            "creation must be rejected before any proof or COMMIT: {error}"
        );
        let receipt = std::env::var_os("CC_SWITCH_ISOLATION_FIXTURE_RECEIPT").unwrap();
        assert!(
            !std::path::Path::new(&receipt).exists(),
            "denied breakaway must not enter the app fixture"
        );
        println!("HANDOFF_DENIED={error}");
    }
}
