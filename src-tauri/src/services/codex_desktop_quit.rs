//! Request the packaged desktop's ordinary application quit through its own
//! preload bridge. No window visibility, focus, auth query, or generic script
//! surface is involved. Once a write is attempted, its outcome is uncertain:
//! callers must only confirm process exit, never fall back to another close.

use std::{future::Future, time::Duration};

use futures::{Sink, SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::codex_desktop_identity::verify_process_binding;

const MAX_MESSAGE: usize = 64 * 1024;
const MAX_PORTS: usize = 8;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const OVERALL_TIMEOUT: Duration = Duration::from_secs(8);
const BINDING_ERROR: &str = "正常退出前 Codex 进程或本机端口归属已变化；未发送退出请求";

// The shipped preload exposes this exact bridge and the main process handles
// quit-app by app.quit(), with relaunch disabled. These are the only scripts
// available in this module; neither accepts caller-supplied code or parameters.
const PROBE_SCRIPT: &str =
    "(() => typeof window.electronBridge?.sendMessageFromView === 'function')()";
const QUIT_SCRIPT: &str = r#"(() => {
  const bridge = window.electronBridge;
  if (!bridge || typeof bridge.sendMessageFromView !== 'function') return false;
  const once = Symbol.for('cc-switch.normal-quit.v1');
  if (window[once] === true) return true;
  window[once] = true;
  Promise.resolve(bridge.sendMessageFromView({type:'quit-app',relaunch:false})).catch(() => {});
  return true;
})()"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuitRequest {
    /// No quit command was attempted. A separate normal-close method is safe.
    Unavailable,
    /// A quit command may have reached the desktop. Only reconcile its exit.
    Sent,
}

/// A read-only prepared connection. Ownership transfers exactly once to the
/// send function after the caller's final task and desktop inventory checks.
pub(crate) struct PreparedQuit {
    pid: u32,
    birth: u64,
    port: u16,
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

#[derive(Default)]
struct QuitAttempt {
    write_attempted: bool,
}

impl QuitAttempt {
    fn begin(&mut self) -> bool {
        if self.write_attempted {
            false
        } else {
            self.write_attempted = true;
            true
        }
    }

    fn finish(&self, result: Result<QuitRequest, String>) -> Result<QuitRequest, String> {
        if self.write_attempted {
            Ok(QuitRequest::Sent)
        } else {
            result
        }
    }
}

fn is_main_target(row: &Value) -> bool {
    if row["type"] != "page" {
        return false;
    }
    let Some(text) = row["url"].as_str().filter(|text| text.len() <= 2048) else {
        return false;
    };
    let Ok(url) = url::Url::parse(text) else {
        return false;
    };
    let query: Vec<_> = url.query_pairs().collect();
    url.scheme() == "app"
        && url.host_str() == Some("-")
        && url.path() == "/index.html"
        && (query.is_empty()
            || (query.len() == 1 && query[0].0 == "initialRoute" && query[0].1 == "/"))
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

fn valid_endpoint(endpoint: &str, port: u16) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    let target = url.path().strip_prefix("/devtools/page/");
    port != 0
        && url.scheme() == "ws"
        && url.host_str() == Some("127.0.0.1")
        && url.port() == Some(port)
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && target.is_some_and(|target| {
            !target.is_empty()
                && target.len() <= 128
                && target
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        })
}

fn main_endpoints(targets: &Value, port: u16) -> Result<Vec<String>, String> {
    let Some(rows) = targets.as_array() else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for row in rows.iter().filter(|row| is_main_target(row)) {
        let endpoint = row["webSocketDebuggerUrl"]
            .as_str()
            .filter(|endpoint| valid_endpoint(endpoint, port))
            .ok_or_else(|| {
                "Codex 正常退出连接地址不符合本机主窗口约束；未发送退出请求".to_string()
            })?;
        result.push(endpoint.to_owned());
    }
    result.sort();
    result.dedup();
    Ok(result)
}

async fn guarded_read<T>(
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
    future: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    guard()?;
    tokio::pin!(future);
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    let deadline = tokio::time::sleep(READ_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            result = &mut future => { guard()?; return result; }
            _ = interval.tick() => guard()?,
            _ = &mut deadline => return Err("Codex 正常退出接口只读检查超时；未发送退出请求".into()),
        }
    }
}

async fn read_targets(client: &reqwest::Client, port: u16) -> Result<Option<Value>, String> {
    let Ok(mut response) = client
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .send()
        .await
    else {
        return Ok(None);
    };
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_MESSAGE as u64)
    {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Codex 正常退出接口只读响应中断；未发送退出请求".to_string())?
    {
        if bytes.len() + chunk.len() > MAX_MESSAGE {
            return Ok(None);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

fn evaluation(id: u32, script: &'static str) -> Message {
    Message::Text(
        json!({"id":id,"method":"Runtime.evaluate","params":{
            "expression":script,"awaitPromise":false,"returnByValue":true,
            "silent":true,"timeout":4000
        }})
        .to_string()
        .into(),
    )
}

async fn send_quit_once<S: Sink<Message> + Unpin>(
    socket: &mut S,
    attempt: &mut QuitAttempt,
    write_timeout: Duration,
) -> QuitRequest {
    if attempt.begin() {
        // Even a failed/partial write may have delivered the command. Never
        // classify a send failure as Unavailable or issue another close.
        let _ = tokio::time::timeout(write_timeout, socket.send(evaluation(2, QUIT_SCRIPT))).await;
    }
    QuitRequest::Sent
}

async fn prepare_with(
    pid: u32,
    birth: u64,
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
) -> Result<Option<PreparedQuit>, String> {
    guard()?;
    let ports = listening_ports(pid)?;
    // Do not silently omit extra candidate listeners while choosing an owner.
    if ports.is_empty() || ports.len() > MAX_PORTS {
        return Ok(None);
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(READ_TIMEOUT)
        .build()
        .map_err(|_| "无法建立 Codex 本机退出接口只读连接".to_string())?;
    let mut candidates = Vec::new();
    for port in ports {
        guard()?;
        verify_process_binding(pid, birth, port).map_err(|_| BINDING_ERROR.to_string())?;
        if let Some(targets) = guarded_read(guard, read_targets(&client, port)).await? {
            for endpoint in main_endpoints(&targets, port)? {
                candidates.push((endpoint, port));
            }
        }
    }
    // Quit is application-wide. Multiple windows below the same bound root
    // are valid; pick one deterministically and send exactly one request.
    candidates.sort();
    candidates.dedup();
    let Some((endpoint, port)) = candidates.into_iter().next() else {
        return Ok(None);
    };
    guard()?;
    verify_process_binding(pid, birth, port).map_err(|_| BINDING_ERROR.to_string())?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let connection = guarded_read(guard, async {
        tokio_tungstenite::connect_async_with_config(&endpoint, Some(config), false)
            .await
            .map_err(|_| "Codex 正常退出接口不可连接；未发送退出请求".to_string())
    })
    .await;
    let Ok((mut socket, _)) = connection else {
        guard()?;
        verify_process_binding(pid, birth, port).map_err(|_| BINDING_ERROR.to_string())?;
        return Ok(None);
    };
    verify_process_binding(pid, birth, port).map_err(|_| BINDING_ERROR.to_string())?;
    let probe = guarded_read(guard, async {
        socket
            .send(evaluation(1, PROBE_SCRIPT))
            .await
            .map_err(|_| "Codex 正常退出桥只读检查无法发送".to_string())?;
        while let Some(frame) = socket.next().await {
            match frame.map_err(|_| "Codex 正常退出桥只读检查断开".to_string())? {
                Message::Text(text) => {
                    let Ok(reply) = serde_json::from_str::<Value>(&text) else {
                        return Ok(false);
                    };
                    if reply["id"] != 1 {
                        continue;
                    }
                    return Ok(reply.get("error").is_none()
                        && reply["result"].get("exceptionDetails").is_none()
                        && reply["result"]["result"]["value"] == true);
                }
                Message::Ping(bytes) => {
                    socket
                        .send(Message::Pong(bytes))
                        .await
                        .map_err(|_| "Codex 正常退出桥只读连接断开".to_string())?;
                }
                Message::Close(_) | Message::Binary(_) => return Ok(false),
                _ => {}
            }
        }
        Ok(false)
    })
    .await;
    guard()?;
    verify_process_binding(pid, birth, port).map_err(|_| BINDING_ERROR.to_string())?;
    if probe != Ok(true) {
        return Ok(None);
    }
    Ok(Some(PreparedQuit {
        pid,
        birth,
        port,
        socket,
    }))
}

/// This stage performs only bounded reads and a bridge-availability probe.
/// The caller must complete its last task-state checks after this returns.
pub(crate) async fn prepare_normal_quit(
    pid: u32,
    birth: u64,
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
) -> Result<Option<PreparedQuit>, String> {
    let result = tokio::time::timeout(OVERALL_TIMEOUT, prepare_with(pid, birth, guard))
        .await
        .unwrap_or(Ok(None));
    guard()?;
    result
}

/// Consume a prepared connection after final task checks. Any attempted write
/// is Sent even if the socket closes or times out; only reconcile process exit.
pub(crate) async fn send_prepared_quit(
    mut prepared: PreparedQuit,
    guard: &(dyn Fn() -> Result<(), String> + Send + Sync),
) -> Result<QuitRequest, String> {
    guard()?;
    verify_process_binding(prepared.pid, prepared.birth, prepared.port)
        .map_err(|_| BINDING_ERROR.to_string())?;
    guard()?;
    let mut attempt = QuitAttempt::default();
    let result = send_quit_once(&mut prepared.socket, &mut attempt, READ_TIMEOUT).await;
    attempt.finish(Ok(result))
}

#[cfg(not(target_os = "windows"))]
fn listening_ports(_: u32) -> Result<Vec<u16>, String> {
    Ok(Vec::new())
}

#[cfg(target_os = "windows")]
fn listening_ports(pid: u32) -> Result<Vec<u16>, String> {
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Foundation::ERROR_INSUFFICIENT_BUFFER,
        NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
            TCP_TABLE_OWNER_PID_LISTENER,
        },
        Networking::WinSock::AF_INET,
    };
    unsafe {
        let mut size = 0u32;
        let first = GetExtendedTcpTable(
            null_mut(),
            &mut size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        if first != ERROR_INSUFFICIENT_BUFFER || !(4..=1024 * 1024).contains(&size) {
            return Err("无法读取 Codex 本机退出接口监听端口；未发送退出请求".into());
        }
        let mut storage = vec![0u64; (size as usize + 7) / 8];
        let capacity = storage.len() * 8;
        if GetExtendedTcpTable(
            storage.as_mut_ptr().cast(),
            &mut size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        ) != 0
            || size as usize > capacity
            || size < 4
        {
            return Err("无法核对 Codex 本机退出接口监听端口；未发送退出请求".into());
        }
        let table = storage.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
        let count = (*table).dwNumEntries as usize;
        if count > (size as usize - 4) / std::mem::size_of::<MIB_TCPROW_OWNER_PID>() {
            return Err("Codex 本机退出接口监听表无效；未发送退出请求".into());
        }
        let rows = std::slice::from_raw_parts(std::ptr::addr_of!((*table).table[0]), count);
        let mut ports: Vec<_> = rows
            .iter()
            .filter(|row| {
                row.dwOwningPid == pid && row.dwLocalAddr == u32::from_ne_bytes([127, 0, 0, 1])
            })
            .map(|row| u16::from_be(row.dwLocalPort as u16))
            .filter(|port| *port != 0)
            .collect();
        ports.sort();
        ports.dedup();
        Ok(ports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        task::{Context, Poll},
    };

    fn target(id: &str, route: &str) -> Value {
        json!({"type":"page","url":route,"webSocketDebuggerUrl":format!("ws://127.0.0.1:45123/devtools/page/{id}")})
    }

    #[test]
    fn multiple_main_windows_choose_one_deterministically_and_ignore_sidecars() {
        let endpoints = main_endpoints(
            &json!([
                target("z", "app://-/index.html"),
                target(
                    "overlay",
                    "app://-/index.html?initialRoute=%2Favatar-overlay"
                ),
                target("a", "app://-/index.html?initialRoute=%2F"),
                target("a", "app://-/index.html?initialRoute=%2F"),
                target("browser", "https://example.invalid/")
            ]),
            45123,
        )
        .unwrap();
        assert_eq!(
            endpoints,
            vec![
                "ws://127.0.0.1:45123/devtools/page/a",
                "ws://127.0.0.1:45123/devtools/page/z"
            ]
        );
    }

    #[test]
    fn endpoint_cannot_escape_bound_loopback_port_or_page_target() {
        for endpoint in [
            "ws://localhost:45123/devtools/page/a",
            "ws://127.0.0.1:45124/devtools/page/a",
            "ws://user@127.0.0.1:45123/devtools/page/a",
            "wss://127.0.0.1:45123/devtools/page/a",
            "ws://127.0.0.1:45123/devtools/browser/a",
            "ws://127.0.0.1:45123/devtools/page/a?x=1",
            "ws://127.0.0.1:45123/devtools/page/a#x",
            "ws://127.0.0.1:45123/devtools/page/a/b",
        ] {
            assert!(!valid_endpoint(endpoint, 45123), "{endpoint}");
        }
        let bad = json!([{"type":"page","url":"app://-/index.html","webSocketDebuggerUrl":"ws://example.invalid/devtools/page/a"}]);
        assert!(main_endpoints(&bad, 45123).is_err());
    }

    #[test]
    fn quit_command_is_fixed_and_never_uses_browser_close_auth_or_relaunch() {
        let Message::Text(probe) = evaluation(1, PROBE_SCRIPT) else {
            panic!()
        };
        let Message::Text(quit) = evaluation(2, QUIT_SCRIPT) else {
            panic!()
        };
        let probe: Value = serde_json::from_str(&probe).unwrap();
        let quit: Value = serde_json::from_str(&quit).unwrap();
        assert_eq!(probe["params"]["expression"], PROBE_SCRIPT);
        assert!(!PROBE_SCRIPT.contains("quit-app"));
        assert_eq!(quit["params"]["expression"], QUIT_SCRIPT);
        assert_eq!(QUIT_SCRIPT.matches("sendMessageFromView({").count(), 1);
        assert!(QUIT_SCRIPT.contains("type:'quit-app',relaunch:false"));
        for prohibited in [
            "Browser.close",
            "account/read",
            "getAuthStatus",
            "mcp-request",
            "relaunch:true",
        ] {
            assert!(!QUIT_SCRIPT.contains(prohibited));
        }
    }

    struct TestSink {
        writes: Arc<AtomicUsize>,
        fail: bool,
        hang: bool,
    }
    impl Sink<Message> for TestSink {
        type Error = ();
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), ()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(())
            } else {
                Ok(())
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
            if self.hang {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn successful_partial_and_timed_out_writes_are_all_sent_and_never_replayed() {
        for (fail, hang) in [(false, false), (true, false), (false, true)] {
            let writes = Arc::new(AtomicUsize::new(0));
            let mut sink = TestSink {
                writes: writes.clone(),
                fail,
                hang,
            };
            let mut attempt = QuitAttempt::default();
            assert_eq!(
                send_quit_once(&mut sink, &mut attempt, Duration::from_millis(5)).await,
                QuitRequest::Sent
            );
            assert_eq!(
                send_quit_once(&mut sink, &mut attempt, Duration::from_millis(5)).await,
                QuitRequest::Sent
            );
            assert_eq!(writes.load(Ordering::SeqCst), 1);
            assert_eq!(
                attempt.finish(Ok(QuitRequest::Unavailable)),
                Ok(QuitRequest::Sent)
            );
            assert_eq!(
                attempt.finish(Err("late disconnect".into())),
                Ok(QuitRequest::Sent)
            );
        }
    }

    #[tokio::test]
    async fn cancellation_during_readonly_probe_returns_without_any_quit_write() {
        let checks = AtomicUsize::new(0);
        let guard = || {
            if checks.fetch_add(1, Ordering::SeqCst) > 1 {
                Err("cancelled".into())
            } else {
                Ok(())
            }
        };
        let result = guarded_read::<()>(&guard, std::future::pending()).await;
        assert_eq!(result, Err("cancelled".into()));
        let attempt = QuitAttempt::default();
        assert_eq!(
            attempt.finish(result.map(|()| QuitRequest::Sent)),
            Err("cancelled".into())
        );
    }
}
