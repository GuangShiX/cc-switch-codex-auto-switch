//! Read the identity of the packaged desktop's existing app-server connection.
//! No login, token refresh, process launch, thread read, or generic CDP API is exposed.
//! The renderer decodes its token in place; only stable identity fields cross CDP.

use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const MAX_MESSAGE: usize = 64 * 1024;
const MAX_TOKEN: usize = 32 * 1024;
const AUTH_NAMESPACE: &str = "https://api.openai.com/auth";
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
const EVALUATION_TIMEOUT: Duration = Duration::from_secs(12);
const UNCONFIRMED: &str = "无法确认新 Codex 桌面的真实账号；未恢复聊天";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopIdentity {
    pub user_id: String,
    pub account_id: String,
}

fn identity_field(value: &Value) -> Result<String, String> {
    value
        .as_str()
        .filter(|text| {
            !text.is_empty() && text.len() <= 1024 && !text.chars().any(char::is_control)
        })
        .map(str::to_owned)
        .ok_or_else(|| UNCONFIRMED.to_owned())
}

fn identity_from_claims(claims: &Value) -> Result<DesktopIdentity, String> {
    let expiry = claims["exp"]
        .as_u64()
        .filter(|expiry| *expiry > 0 && *expiry <= 9_007_199_254_740_991);
    if expiry.is_none() || !claims[AUTH_NAMESPACE].is_object() {
        return Err(UNCONFIRMED.into());
    }
    let auth = &claims[AUTH_NAMESPACE];
    // Mirror the desktop decoder: an invalid primary field must never cause
    // fallback to another user's secondary field.
    let account = if auth["chatgpt_account_id"].is_null() {
        &auth["account_id"]
    } else {
        &auth["chatgpt_account_id"]
    };
    let user = if auth["user_id"].is_null() {
        &auth["chatgpt_user_id"]
    } else {
        &auth["user_id"]
    };
    Ok(DesktopIdentity {
        user_id: identity_field(user)?,
        account_id: identity_field(account)?,
    })
}

/// Parse the managed account's already approved access token with the same
/// claim rules as the renderer. This does not read files or refresh credentials.
pub fn identity_from_token(token: &str) -> Result<DesktopIdentity, String> {
    if token.len() > MAX_TOKEN {
        return Err(UNCONFIRMED.into());
    }
    let pieces: Vec<&str> = token.split('.').collect();
    if pieces.len() != 3 || pieces.iter().any(|piece| piece.is_empty()) {
        return Err(UNCONFIRMED.into());
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(pieces[1])
        .map_err(|_| UNCONFIRMED.to_owned())?;
    let claims: Value = serde_json::from_slice(&decoded).map_err(|_| UNCONFIRMED.to_owned())?;
    identity_from_claims(&claims)
}

fn parse_renderer_identity(value: &Value) -> Result<DesktopIdentity, String> {
    if value["ok"] != true {
        return Err(UNCONFIRMED.into());
    }
    let object = value.as_object().ok_or(UNCONFIRMED)?;
    if object.len() != 3
        || !["ok", "userId", "accountId"]
            .iter()
            .all(|key| object.contains_key(*key))
    {
        return Err(UNCONFIRMED.into());
    }
    Ok(DesktopIdentity {
        user_id: identity_field(&value["userId"])?,
        account_id: identity_field(&value["accountId"])?,
    })
}

fn is_main_target(value: &Value) -> bool {
    if value["type"] != "page" {
        return false;
    }
    let Some(text) = value["url"].as_str().filter(|text| text.len() <= 2048) else {
        return false;
    };
    let Ok(url) = url::Url::parse(text) else {
        return false;
    };
    let query: Vec<_> = url.query_pairs().collect();
    url.scheme() == "app"
        && url.host_str() == Some("-")
        && url.path() == "/index.html"
        // Avatar overlays also load index.html, with an initialRoute query.
        // They are sidecar renderers, not a second desktop auth owner.
        && (query.is_empty()
            || (query.len() == 1 && query[0].0 == "initialRoute" && query[0].1 == "/"))
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn validate_endpoint(endpoint: &str, port: u16) -> Result<(), String> {
    let url = url::Url::parse(endpoint).map_err(|_| UNCONFIRMED.to_owned())?;
    let target = url.path().strip_prefix("/devtools/page/");
    if port == 0
        || url.scheme() != "ws"
        || url.host_str() != Some("127.0.0.1")
        || url.port() != Some(port)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !target.is_some_and(|target| {
            !target.is_empty()
                && target.len() <= 128
                && target
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        })
    {
        return Err(UNCONFIRMED.into());
    }
    Ok(())
}

fn select_endpoint(targets: &Value, port: u16) -> Result<String, String> {
    let rows = targets.as_array().ok_or(UNCONFIRMED)?;
    let candidates: Vec<&Value> = rows.iter().filter(|row| is_main_target(row)).collect();
    if candidates.len() != 1 {
        return Err(UNCONFIRMED.into());
    }
    let endpoint = candidates[0]["webSocketDebuggerUrl"]
        .as_str()
        .ok_or(UNCONFIRMED)?;
    validate_endpoint(endpoint, port)?;
    Ok(endpoint.to_owned())
}

// Do not return the account object, token, response error text, or any other
// application data. Response events are bound to two unpredictable IDs and the
// local host. Each of the only two permitted RPCs has a five second deadline.
const READ_IDENTITY_SCRIPT: &str = r#"(async () => {
  const baseId = __CC_SWITCH_REQUEST_ID__;
  const fail = () => ({ok:false});
  const bridge = window.electronBridge;
  if (!bridge || typeof bridge.sendMessageFromView !== 'function') return fail();
  const field = value => typeof value === 'string' && value.length > 0 &&
    value.length <= 1024 && !/[\u0000-\u001f\u007f-\u009f]/.test(value);
  const rpc = (suffix, method, params) => new Promise(resolve => {
    if (method !== 'account/read' && method !== 'getAuthStatus') {resolve(null);return;}
    const id = baseId + ':' + suffix;
    let finished = false;
    let timer;
    const done = value => {
      if (finished) return;
      finished = true;
      clearTimeout(timer);
      window.removeEventListener('message', listener);
      resolve(value);
    };
    const listener = event => {
      const data = event && event.data;
      if (!data || data.type !== 'mcp-response' || data.hostId !== 'local' ||
          !data.message || data.message.id !== id ||
          (data.requestMethod != null && data.requestMethod !== method)) return;
      if (data.message.error != null || !Object.prototype.hasOwnProperty.call(data.message,'result')) {
        done(null);return;
      }
      done(data.message.result);
    };
    window.addEventListener('message', listener);
    timer = setTimeout(() => done(null), 5000);
    try {
      Promise.resolve(bridge.sendMessageFromView({
        type:'mcp-request', hostId:'local', timeoutMs:5000,
        request:{id,method,params}
      })).catch(() => done(null));
    } catch (_) { done(null); }
  });
  try {
    const account = await rpc('account','account/read',{refreshToken:false});
    if (!account || !account.account || account.account.type !== 'chatgpt') return fail();
    const routing = account.workspaceRouting;
    if (routing != null && !field(routing.chatgptAccountId)) return fail();
    const status = await rpc('auth','getAuthStatus',{includeToken:true,refreshToken:false});
    if (!status || (status.authMethod !== 'chatgpt' && status.authMethod !== 'chatgptAuthTokens') ||
        typeof status.authToken !== 'string' || status.authToken.length > 32768) return fail();
    const pieces = status.authToken.split('.');
    if (pieces.length !== 3 || pieces.some(part => !part)) return fail();
    let payload = pieces[1].replace(/-/g,'+').replace(/_/g,'/');
    payload += '='.repeat((4-payload.length%4)%4);
    const bytes = Uint8Array.from(atob(payload), char => char.charCodeAt(0));
    const claims = JSON.parse(new TextDecoder('utf-8',{fatal:true}).decode(bytes));
    const auth = claims['https://api.openai.com/auth'];
    if (!Number.isSafeInteger(claims.exp) || claims.exp <= 0 || !auth ||
        typeof auth !== 'object' || Array.isArray(auth)) return fail();
    const accountId = auth.chatgpt_account_id ?? auth.account_id;
    const userId = auth.user_id ?? auth.chatgpt_user_id;
    if (!field(accountId) || !field(userId) ||
        (routing != null && routing.chatgptAccountId !== accountId)) return fail();
    return {ok:true,userId,accountId};
  } catch (_) { return fail(); }
})()"#;

fn identity_expression(id: &str) -> Result<String, String> {
    let encoded = serde_json::to_string(id).map_err(|_| UNCONFIRMED.to_owned())?;
    Ok(READ_IDENTITY_SCRIPT.replace("__CC_SWITCH_REQUEST_ID__", &encoded))
}

async fn evaluate_identity(
    socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    expression: String,
) -> Result<DesktopIdentity, String> {
    // This is the complete CDP method surface. There is no getProperties,
    // callFunctionOn, inspector, arbitrary expression, or tool invocation API.
    let request = json!({"id":1,"method":"Runtime.evaluate","params":{
        "expression":expression,"awaitPromise":true,"returnByValue":true,
        "silent":true,"timeout":11000
    }});
    socket
        .send(Message::Text(request.to_string().into()))
        .await
        .map_err(|_| UNCONFIRMED.to_owned())?;
    while let Some(frame) = socket.next().await {
        match frame.map_err(|_| UNCONFIRMED.to_owned())? {
            Message::Text(text) => {
                let reply: Value =
                    serde_json::from_str(&text).map_err(|_| UNCONFIRMED.to_owned())?;
                if reply["id"] != 1 {
                    continue;
                }
                if reply.get("error").is_some() || reply["result"].get("exceptionDetails").is_some()
                {
                    return Err(UNCONFIRMED.into());
                }
                return parse_renderer_identity(&reply["result"]["result"]["value"]);
            }
            Message::Ping(payload) => {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|_| UNCONFIRMED.to_owned())?;
            }
            Message::Close(_) | Message::Binary(_) => return Err(UNCONFIRMED.into()),
            _ => {}
        }
    }
    Err(UNCONFIRMED.into())
}

/// Read only the desktop started by this activation. Process creation time,
/// package identity, port owner, main page, and WebSocket address are all checked.
pub async fn runtime_identity(
    port: u16,
    expected_pid: u32,
    expected_birth: u64,
) -> Result<DesktopIdentity, String> {
    verify_process_binding(expected_pid, expected_birth, port)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(RPC_TIMEOUT)
        .build()
        .map_err(|_| UNCONFIRMED.to_owned())?;
    let mut response = client
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .send()
        .await
        .map_err(|_| UNCONFIRMED.to_owned())?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_MESSAGE as u64)
    {
        return Err(UNCONFIRMED.into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| UNCONFIRMED.to_owned())? {
        if bytes.len() + chunk.len() > MAX_MESSAGE {
            return Err(UNCONFIRMED.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let targets: Value = serde_json::from_slice(&bytes).map_err(|_| UNCONFIRMED.to_owned())?;
    let endpoint = select_endpoint(&targets, port)?;
    verify_process_binding(expected_pid, expected_birth, port)?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (mut socket, _) = tokio::time::timeout(
        RPC_TIMEOUT,
        tokio_tungstenite::connect_async_with_config(&endpoint, Some(config), false),
    )
    .await
    .map_err(|_| UNCONFIRMED.to_owned())?
    .map_err(|_| UNCONFIRMED.to_owned())?;
    verify_process_binding(expected_pid, expected_birth, port)?;
    let id = format!("cc-switch-identity:{}", uuid::Uuid::new_v4());
    let result = tokio::time::timeout(
        EVALUATION_TIMEOUT,
        evaluate_identity(&mut socket, identity_expression(&id)?),
    )
    .await
    .map_err(|_| UNCONFIRMED.to_owned())?;
    // Drop the connection even on errors. No background adapter survives.
    drop(socket);
    verify_process_binding(expected_pid, expected_birth, port)?;
    result
}

#[derive(Clone)]
struct ProcessBinding {
    pid: u32,
    birth: u64,
    running: bool,
    family: String,
    application_id: String,
    listeners: Vec<(u32, u32, u16)>,
}

fn validate_process_binding(
    binding: &ProcessBinding,
    expected_pid: u32,
    expected_birth: u64,
    port: u16,
) -> Result<(), String> {
    let listeners: Vec<_> = binding
        .listeners
        .iter()
        .filter(|row| row.2 == port)
        .collect();
    if expected_pid == 0
        || expected_birth == 0
        || port == 0
        || binding.pid != expected_pid
        || binding.birth != expected_birth
        || !binding.running
        || !binding.family.starts_with("OpenAI.Codex_")
        || !binding.family.ends_with("_2p2nqsd0c76g0")
        || binding.application_id != "OpenAI.Codex_2p2nqsd0c76g0!App"
        || listeners.len() != 1
        || listeners[0].0 != expected_pid
        || listeners[0].1 != u32::from_ne_bytes([127, 0, 0, 1])
    {
        return Err(UNCONFIRMED.into());
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn verify_process_binding(_: u32, _: u64, _: u16) -> Result<(), String> {
    Err(UNCONFIRMED.into())
}

#[cfg(target_os = "windows")]
fn verify_process_binding(pid: u32, birth: u64, port: u16) -> Result<(), String> {
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE},
        NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
            TCP_TABLE_OWNER_PID_LISTENER,
        },
        Networking::WinSock::AF_INET,
        Storage::{
            FileSystem::SYNCHRONIZE,
            Packaging::Appx::{GetApplicationUserModelId, GetPackageFamilyName},
        },
        System::Threading::{
            GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    };
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if process.is_null() {
            return Err(UNCONFIRMED.into());
        }
        let process = Handle(process);
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) == 0 {
            return Err(UNCONFIRMED.into());
        }
        let read_package = |get: unsafe extern "system" fn(HANDLE, *mut u32, *mut u16) -> u32| {
            let mut size = 0;
            get(process.0, &mut size, null_mut());
            if size == 0 || size > 4096 {
                return None;
            }
            let mut buffer = vec![0u16; size as usize];
            if get(process.0, &mut size, buffer.as_mut_ptr()) != 0 {
                return None;
            }
            let end = buffer
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(buffer.len());
            String::from_utf16(&buffer[..end]).ok()
        };
        let family = read_package(GetPackageFamilyName).ok_or(UNCONFIRMED)?;
        let application_id = read_package(GetApplicationUserModelId).ok_or(UNCONFIRMED)?;
        let mut size = 0u32;
        let first = GetExtendedTcpTable(
            null_mut(),
            &mut size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
        if first != ERROR_INSUFFICIENT_BUFFER || size < 4 || size > 1024 * 1024 {
            return Err(UNCONFIRMED.into());
        }
        // u64 storage supplies alignment for the native TCP table.
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
            return Err(UNCONFIRMED.into());
        }
        let table = storage.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
        let count = (*table).dwNumEntries as usize;
        if count > (size as usize - 4) / std::mem::size_of::<MIB_TCPROW_OWNER_PID>() {
            return Err(UNCONFIRMED.into());
        }
        let rows = std::slice::from_raw_parts(std::ptr::addr_of!((*table).table[0]), count);
        let listeners = rows
            .iter()
            .map(|row| {
                (
                    row.dwOwningPid,
                    row.dwLocalAddr,
                    u16::from_be(row.dwLocalPort as u16),
                )
            })
            .collect();
        validate_process_binding(
            &ProcessBinding {
                pid,
                birth: ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64,
                running: WaitForSingleObject(process.0, 0) == 258,
                family,
                application_id,
                listeners,
            },
            pid,
            birth,
            port,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Value {
        json!({"sub":"subject-A","exp":1234567890,AUTH_NAMESPACE:{
            "chatgpt_account_id":"workspace-A","user_id":"user-A",
            "chatgpt_user_id":"secondary-user","account_id":"secondary-workspace"
        }})
    }

    fn token(value: &Value) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(value.to_string())
        )
    }

    #[test]
    fn expected_token_and_renderer_match_the_actual_desktop_principal() {
        let expected = identity_from_token(&token(&claims())).unwrap();
        let actual = parse_renderer_identity(&json!({"ok":true,
            "userId":"user-A","accountId":"workspace-A"}))
        .unwrap();
        assert_eq!(expected, actual);
        let mut same_principal = claims();
        same_principal["sub"] = json!("another-sub-for-the-same-chatgpt-user");
        assert_eq!(
            expected,
            identity_from_token(&token(&same_principal)).unwrap()
        );
        for changed in [
            json!({"ok":true,"userId":"user-B","accountId":"workspace-A"}),
            json!({"ok":true,"userId":"user-A","accountId":"workspace-B"}),
        ] {
            assert_ne!(expected, parse_renderer_identity(&changed).unwrap());
        }
        for missing in [
            json!({"ok":true,"accountId":"workspace-A"}),
            json!({"ok":true,"userId":"user-A"}),
            json!({"ok":true,"userId":"user-A","accountId":"workspace-A","authToken":"secret"}),
        ] {
            assert!(parse_renderer_identity(&missing).is_err());
        }
    }

    #[test]
    fn claim_priority_does_not_fallback_after_invalid_primary() {
        let mut value = claims();
        value[AUTH_NAMESPACE]["user_id"] = json!("");
        assert!(identity_from_token(&token(&value)).is_err());
        value[AUTH_NAMESPACE]["user_id"] = Value::Null;
        assert_eq!(
            identity_from_token(&token(&value)).unwrap().user_id,
            "secondary-user"
        );
        value["exp"] = json!(0);
        assert!(identity_from_token(&token(&value)).is_err());
        assert!(identity_from_token("not-a-token").is_err());
    }

    #[test]
    fn main_target_and_endpoint_are_exact_and_unique() {
        let good = json!({"type":"page","url":"app://-/index.html?initialRoute=%2F",
            "webSocketDebuggerUrl":"ws://127.0.0.1:45123/devtools/page/abc"});
        assert!(select_endpoint(&json!([good]), 45123).is_ok());
        assert!(select_endpoint(&json!([good, good]), 45123).is_err());
        let overlay = json!({"type":"page","url":"app://-/index.html?initialRoute=%2Favatar-overlay",
            "webSocketDebuggerUrl":"ws://127.0.0.1:45123/devtools/page/overlay"});
        let main = json!({"type":"page","url":"app://-/index.html",
            "webSocketDebuggerUrl":"ws://127.0.0.1:45123/devtools/page/main"});
        assert_eq!(
            select_endpoint(&json!([overlay, main]), 45123).unwrap(),
            "ws://127.0.0.1:45123/devtools/page/main"
        );
        for target in [
            "app://fs/index.html",
            "app://-/detached-window.html",
            "https://example.org/index.html",
        ] {
            assert!(!is_main_target(&json!({"type":"page","url":target})));
        }
        for endpoint in [
            "ws://localhost:45123/devtools/page/abc",
            "ws://127.0.0.1:45124/devtools/page/abc",
            "ws://127.0.0.1:45123/devtools/browser/abc",
            "ws://127.0.0.1:45123/devtools/page/abc?redirect=1",
            "ws://x@127.0.0.1:45123/devtools/page/abc",
        ] {
            assert!(validate_endpoint(endpoint, 45123).is_err());
        }
    }

    #[test]
    fn changed_process_or_foreign_wildcard_listener_is_rejected() {
        let binding = ProcessBinding {
            pid: 12,
            birth: 34,
            running: true,
            family: "OpenAI.Codex_2p2nqsd0c76g0".into(),
            application_id: "OpenAI.Codex_2p2nqsd0c76g0!App".into(),
            listeners: vec![(12, u32::from_ne_bytes([127, 0, 0, 1]), 45123)],
        };
        assert!(validate_process_binding(&binding, 12, 34, 45123).is_ok());
        assert!(validate_process_binding(&binding, 12, 35, 45123).is_err());
        assert!(validate_process_binding(&binding, 13, 34, 45123).is_err());
        for changed in 0..5 {
            let mut bad = binding.clone();
            match changed {
                0 => bad.running = false,
                1 => bad.family = "another-package".into(),
                2 => bad.listeners[0].0 = 13,
                3 => bad.listeners[0].1 = 0,
                _ => bad
                    .listeners
                    .push((13, u32::from_ne_bytes([127, 0, 0, 1]), 45123)),
            };
            assert!(validate_process_binding(&bad, 12, 34, 45123).is_err());
        }
    }
}
