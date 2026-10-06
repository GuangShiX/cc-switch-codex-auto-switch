//! Codex desktop thread follower coordination, implemented by CC Switch.
//!
//! This is the desktop coordinator protocol, not the app-server API. It never
//! loads rollouts or requests complete history, answers approvals, or starts a
//! second app-server. Incoming snapshots are immediately reduced to metadata;
//! message/tool bodies are not retained or logged. A caller must provide a
//! complete candidate inventory (for example read-only local thread metadata)
//! and persist pause/resume intentions before invoking mutation methods.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const PIPE_NAME: &str = r"\\.\pipe\codex-ipc";
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MUTATION_TIMEOUT: Duration = Duration::from_secs(30);
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(3);
const REOPENED_CHAT_TIMEOUT: Duration = Duration::from_secs(30);
const OWNER_REFOLLOW_INTERVAL: Duration = Duration::from_secs(1);
const GUARDED_READ_SLICE: Duration = Duration::from_millis(500);
const MAX_CANDIDATES: usize = 5000;
const RESUME_TEXT: &str = "[CC Switch：恢复原任务] 本次换号前由 CC Switch 暂停了此任务，现已重开桌面。请检查中断现场和最后一次工具调用，继续此前尚未完成的原任务；保留原项目、模型和权限。结果不明确的操作先核对状态，不重复已完成的操作，原有审批仍需按规则处理。";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SessionErrorKind {
    /// No coordinator connection. This alone does not prove no desktop exists.
    Unavailable,
    Unknown,
    Incompatible,
    StateChanged,
    Blocked,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionError {
    pub kind: SessionErrorKind,
    pub message: String,
    pub mutation_may_have_been_sent: bool,
}

impl SessionError {
    fn new(kind: SessionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            mutation_may_have_been_sent: false,
        }
    }

    fn uncertain(message: impl Into<String>) -> Self {
        Self {
            kind: SessionErrorKind::OutcomeUnknown,
            message: message.into(),
            mutation_may_have_been_sent: true,
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SessionError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskContext {
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    /// Digests of settings and permissions, never configuration/message text.
    pub thread_settings: Value,
    pub current_permissions: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskSnapshot {
    pub thread_id: String,
    pub owner: String,
    pub turn_id: Option<String>,
    pub status: Option<String>,
    pub runtime_status: String,
    pub waiting: bool,
    pub is_child: bool,
    pub ephemeral: bool,
    pub client_user_message_id: Option<String>,
    pub context: TaskContext,
}

impl TaskSnapshot {
    pub fn safely_running(&self) -> bool {
        self.status.as_deref() == Some("inProgress")
            && self.runtime_status == "active"
            && self.turn_id.is_some()
            && !self.waiting
            && !self.is_child
            && !self.ephemeral
    }

    pub fn safely_idle(&self) -> bool {
        matches!(self.runtime_status.as_str(), "idle" | "notLoaded")
            && self.status.as_deref() != Some("inProgress")
            && !self.waiting
    }

    fn matches_paused(&self, paused: &PausedTask) -> bool {
        self.thread_id == paused.thread_id
            && self.turn_id.as_deref() == Some(paused.turn_id.as_str())
            && self.status.as_deref() == Some("interrupted")
            && self.safely_idle()
            && !self.is_child
            && !self.ephemeral
            && self.context == paused.context
            && paused.confirmed_by_cc_switch
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PausedTask {
    pub thread_id: String,
    pub turn_id: String,
    pub context: TaskContext,
    pub pause_operation_id: String,
    confirmed_by_cc_switch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_settings: Option<SavedTaskSettings>,
}

/// Bounded thread-local selections, never login data or conversation content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SavedTaskSettings {
    pub thread_settings: Value,
    pub current_permissions: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeConfirmation {
    pub thread_id: String,
    pub turn_id: String,
    pub operation_id: String,
    /// True when a previous uncertain request was reconciled from state.
    pub already_observed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskInventory {
    pub running: Vec<TaskSnapshot>,
    pub idle: Vec<TaskSnapshot>,
    pub blocked: Vec<TaskSnapshot>,
    pub unloaded_thread_ids: Vec<String>,
    pub unresolved_thread_ids: Vec<String>,
    /// Set only by the caller after proving candidate discovery coverage.
    pub candidate_coverage_complete: bool,
}

impl TaskInventory {
    pub fn safe_to_restart(&self) -> bool {
        self.candidate_coverage_complete
            && self.blocked.is_empty()
            && self.unresolved_thread_ids.is_empty()
    }
}

trait PipeIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> PipeIo for T {}

struct TrackedState {
    owner: String,
    revision: Value,
    metadata: Value,
}

pub struct DesktopSession {
    io: Box<dyn PipeIo>,
    client_id: String,
    receive_buffer: Vec<u8>,
    followed: HashSet<String>,
    states: HashMap<String, TrackedState>,
    resync_needed: HashSet<String>,
    mutation_attempts: HashSet<String>,
    reopened_baselines: HashMap<String, TaskSnapshot>,
    compatible: bool,
    broken: bool,
}

impl DesktopSession {
    pub async fn connect() -> Result<Self, SessionError> {
        #[cfg(target_os = "windows")]
        {
            let pipe = tokio::net::windows::named_pipe::ClientOptions::new()
                .open(PIPE_NAME)
                .map_err(|_| {
                    SessionError::new(
                        SessionErrorKind::Unavailable,
                        "Codex 桌面协调连接不可用；无法据此认定没有运行任务",
                    )
                })?;
            Self::initialize(Box::new(pipe)).await
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err(SessionError::new(
                SessionErrorKind::Unavailable,
                "当前平台尚未实现 Codex 桌面任务连接",
            ))
        }
    }

    async fn initialize(io: Box<dyn PipeIo>) -> Result<Self, SessionError> {
        let mut session = Self {
            io,
            client_id: "initializing-client".into(),
            receive_buffer: Vec::new(),
            followed: HashSet::new(),
            states: HashMap::new(),
            resync_needed: HashSet::new(),
            mutation_attempts: HashSet::new(),
            reopened_baselines: HashMap::new(),
            compatible: true,
            broken: false,
        };
        let reply = session
            .request(
                "initialize",
                json!({"clientType":"cc-switch-desktop-session"}),
                None,
                false,
            )
            .await?;
        session.client_id = reply
            .pointer("/result/clientId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                SessionError::new(
                    SessionErrorKind::Incompatible,
                    "Codex 桌面协调初始化未返回客户端身份",
                )
            })?
            .into();
        Ok(session)
    }

    /// Candidate IDs must come from metadata/hook registration, not history.
    /// Empty or incomplete candidate coverage is never automatically safe.
    pub async fn snapshot_safe_running_tasks(
        &mut self,
        thread_ids: &[String],
    ) -> Result<TaskInventory, SessionError> {
        if thread_ids.len() > MAX_CANDIDATES {
            return Err(SessionError::new(
                SessionErrorKind::Unknown,
                "候选聊天数量超出本次检查上限，不能确认全部运行任务",
            ));
        }
        let mut unique = HashSet::new();
        for id in thread_ids {
            validate_id(id)?;
            if unique.insert(id.clone()) {
                self.states.remove(id);
                self.follow(id, false).await?;
                self.follow(id, true).await?;
            }
        }
        let deadline = tokio::time::Instant::now() + SNAPSHOT_TIMEOUT;
        while unique.iter().any(|id| !self.states.contains_key(id))
            && tokio::time::Instant::now() < deadline
        {
            if self.pump_until(deadline).await?.is_none() {
                break;
            }
            self.flush_resyncs().await?;
        }
        // Owner discovery is batched on the same connection, with one global
        // deadline. Never wait several seconds independently for every old ID.
        let unresolved: Vec<_> = unique
            .iter()
            .filter(|id| !self.states.contains_key(*id))
            .cloned()
            .collect();
        let mut requests = HashMap::new();
        for id in unresolved {
            let request_id = uuid::Uuid::new_v4().to_string();
            self.send(&json!({"type":"request","requestId":request_id,"sourceClientId":self.client_id,"method":"thread-owner-discovery","version":1,"params":{"hostId":"local","conversationId":id},"timeoutMs":REQUEST_TIMEOUT.as_millis() as u64}), false).await?;
            requests.insert(request_id, id);
        }
        let mut unloaded = HashSet::new();
        let owner_deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        while !requests.is_empty() {
            let Some(message) = self.pump_until(owner_deadline).await? else {
                break;
            };
            if message["type"] == "response" {
                if let Some(id) = message["requestId"]
                    .as_str()
                    .and_then(|request_id| requests.remove(request_id))
                {
                    if message["resultType"] == "error" && message["error"] == "no-client-found" {
                        unloaded.insert(id);
                    }
                }
            }
            self.flush_resyncs().await?;
        }
        let mut inventory = TaskInventory::default();
        for id in unique {
            match self.current_snapshot(&id) {
                Some(task) if task.safely_running() => inventory.running.push(task),
                Some(task) if task.safely_idle() => inventory.idle.push(task),
                Some(task) => inventory.blocked.push(task),
                None if unloaded.contains(&id) => inventory.unloaded_thread_ids.push(id),
                None => inventory.unresolved_thread_ids.push(id),
            }
        }
        // Stable ordering avoids accidental priority differences from HashMap.
        inventory
            .running
            .sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
        inventory.idle.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
        inventory
            .blocked
            .sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
        inventory.unresolved_thread_ids.sort();
        inventory.unloaded_thread_ids.sort();
        Ok(inventory)
    }

    pub async fn fresh_snapshot(&mut self, thread_id: &str) -> Result<TaskSnapshot, SessionError> {
        validate_id(thread_id)?;
        self.states.remove(thread_id);
        // A false/true pair obtains a fresh owner snapshot even if already following.
        self.follow(thread_id, false).await?;
        self.follow(thread_id, true).await?;
        self.await_snapshot(thread_id, SNAPSHOT_TIMEOUT).await
    }

    pub async fn begin_follow(&mut self, thread_id: &str) -> Result<(), SessionError> {
        validate_id(thread_id)?;
        self.states.remove(thread_id);
        self.follow(thread_id, true).await
    }

    /// Check all live running snapshots with this method before pausing any.
    pub fn can_preserve_running_task(&self, task: &TaskSnapshot) -> Result<(), SessionError> {
        if !task.safely_running() || self.current_snapshot(&task.thread_id).as_ref() != Some(task) {
            return Err(SessionError::new(
                SessionErrorKind::StateChanged,
                "保存设置前原任务状态已改变；未发送暂停请求",
            ));
        }
        self.saved_settings_for(&task.thread_id).map(|_| ())
    }

    fn saved_settings_for(&self, thread_id: &str) -> Result<SavedTaskSettings, SessionError> {
        let metadata = &self
            .states
            .get(thread_id)
            .ok_or_else(|| SessionError::new(SessionErrorKind::Unknown, "原聊天实时设置不可用"))?
            .metadata;
        capture_saved_settings(metadata)
    }

    pub async fn wait_for_reopened_paused_chat<F>(
        &mut self,
        paused: &PausedTask,
        guard: F,
    ) -> Result<TaskSnapshot, SessionError>
    where
        F: Fn() -> bool,
    {
        let snapshot = self.wait_for_paused_chat(paused, guard, true).await?;
        self.reopened_baselines
            .insert(pause_key(paused), snapshot.clone());
        Ok(snapshot)
    }

    /// After opening an original chat on a new desktop, wait for its live owner
    /// to finish loading. Following a thread does not itself load that chat.
    /// This emits metadata subscriptions only; it cannot start or interrupt a
    /// turn. The caller must still persist the resume intent and recheck the
    /// account before invoking resume_and_confirm.
    pub async fn wait_for_owned_paused_chat<F>(
        &mut self,
        paused: &PausedTask,
        guard: F,
    ) -> Result<TaskSnapshot, SessionError>
    where
        F: Fn() -> bool,
    {
        self.wait_for_paused_chat(paused, guard, false).await
    }

    async fn wait_for_paused_chat<F>(
        &mut self,
        paused: &PausedTask,
        guard: F,
        reopened: bool,
    ) -> Result<TaskSnapshot, SessionError>
    where
        F: Fn() -> bool,
    {
        validate_id(&paused.thread_id)?;
        if !guard() {
            return Err(cancelled());
        }
        if !paused.confirmed_by_cc_switch {
            return Err(SessionError::new(
                SessionErrorKind::StateChanged,
                "无法确认原任务由本次换号暂停；未发送恢复请求",
            ));
        }
        self.states.remove(&paused.thread_id);
        let deadline = tokio::time::Instant::now() + REOPENED_CHAT_TIMEOUT;
        let mut next_follow = tokio::time::Instant::now();
        loop {
            if !guard() {
                return Err(cancelled());
            }
            if let Some(current) = self.current_snapshot(&paused.thread_id) {
                if current.matches_paused(paused)
                    || (reopened && matches_reopened_pause(&current, paused))
                {
                    return Ok(current);
                }
                return Err(SessionError::new(
                    SessionErrorKind::StateChanged,
                    "原聊天加载后轮次、模型、权限或等待状态已改变；未发送恢复请求",
                ));
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(SessionError::new(
                    SessionErrorKind::Unknown,
                    "新桌面未在 30 秒内加载原聊天的实时 owner；未发送恢复请求",
                ));
            }
            if now >= next_follow {
                // An earlier subscription can arrive before owner creation.
                // Re-subscribe to obtain a snapshot once loading completes.
                self.resync_needed.remove(&paused.thread_id);
                for following in [false, true] {
                    if !guard() {
                        return Err(cancelled());
                    }
                    let result = tokio::time::timeout_at(
                        deadline,
                        self.follow(&paused.thread_id, following),
                    )
                    .await;
                    if !guard() {
                        return Err(cancelled());
                    }
                    match result {
                        Ok(result) => result?,
                        Err(_) => {
                            // A timed-out write may have emitted a partial
                            // frame. Never reuse or retry that connection.
                            self.broken = true;
                            self.states.clear();
                            return Err(SessionError::new(
                                SessionErrorKind::Unavailable,
                                "等待原聊天 owner 时协调连接写入超时；未发送恢复请求",
                            ));
                        }
                    }
                }
                next_follow = tokio::time::Instant::now() + OWNER_REFOLLOW_INTERVAL;
            }
            let read_deadline = deadline
                .min(next_follow)
                .min(tokio::time::Instant::now() + GUARDED_READ_SLICE);
            let result = self.pump_until(read_deadline).await;
            if !guard() {
                return Err(cancelled());
            }
            result?;
        }
    }

    /// Caller journals intent first. The guard is checked again immediately
    /// before the interrupt request, after the awaited owner recheck.
    pub async fn pause_and_confirm<F>(
        &mut self,
        expected: &TaskSnapshot,
        operation_id: &str,
        guard: F,
    ) -> Result<PausedTask, SessionError>
    where
        F: Fn() -> bool,
    {
        validate_id(operation_id)?;
        if !guard() {
            return Err(cancelled());
        }
        let current = self.fresh_snapshot(&expected.thread_id).await?;
        if current != *expected || !current.safely_running() {
            return Err(SessionError::new(
                SessionErrorKind::StateChanged,
                "暂停前原聊天、轮次、模型、权限或运行状态已改变",
            ));
        }
        // Refuse a known un-restorable selection before touching the live turn.
        let saved_settings = self.saved_settings_for(&current.thread_id)?;
        self.confirm_owner(&current).await?;
        if !guard() {
            return Err(cancelled());
        }
        let key = format!("pause:{operation_id}:{}", current.thread_id);
        if !self.mutation_attempts.insert(key) {
            return Err(SessionError::uncertain(
                "本次暂停曾发送过，需核对结果，未重复发送",
            ));
        }
        let turn_id = current.turn_id.clone().unwrap();
        let paused = PausedTask {
            thread_id: current.thread_id.clone(),
            turn_id: turn_id.clone(),
            context: current.context.clone(),
            pause_operation_id: operation_id.into(),
            confirmed_by_cc_switch: true,
            saved_settings: Some(saved_settings),
        };
        let reply = self.request("thread-follower-interrupt-turn", json!({"conversationId":current.thread_id,"mode":"user-stop","expectedTurnId":turn_id}), Some(&current.owner), true).await;
        let acknowledged = reply
            .as_ref()
            .ok()
            .and_then(|r| r.pointer("/result/interruptedTurnId"))
            .and_then(Value::as_str)
            == Some(paused.turn_id.as_str());
        // Lost acknowledgments are reconciled from the original live turn. No replay.
        let deadline = tokio::time::Instant::now() + CONFIRM_TIMEOUT;
        loop {
            if let Some(observed) = self.current_snapshot(&paused.thread_id) {
                if observed.matches_paused(&paused) && acknowledged {
                    return Ok(paused);
                }
                if observed.matches_paused(&paused) && !acknowledged {
                    return Err(SessionError::uncertain("原轮次已中断，但未收到本次暂停的准确确认；不能认定由 CC Switch 暂停或自动恢复"));
                }
                if observed.turn_id.as_deref() != Some(paused.turn_id.as_str())
                    || observed.waiting
                    || observed.context != paused.context
                {
                    return Err(SessionError::uncertain(
                        "暂停请求后原任务已改变，未继续退出或恢复",
                    ));
                }
            }
            if self.broken || tokio::time::Instant::now() >= deadline {
                break;
            }
            if self
                .pump_until(deadline)
                .await
                .map_err(|error| SessionError::uncertain(error.message))?
                .is_none()
            {
                break;
            }
            self.flush_resyncs()
                .await
                .map_err(|error| SessionError::uncertain(error.message))?;
        }
        Err(SessionError::uncertain(if acknowledged {
            "暂停请求已确认，但未确认原轮次停止状态；未重复请求"
        } else {
            "暂停请求结果未确认；未重复请求或继续关闭桌面"
        }))
    }

    /// Restore only a pause owned by this flow. Caller persists operation_id
    /// before calling; after OutcomeUnknown it must call reconcile_resume,
    /// never blindly call this method again on a new connection.
    pub async fn resume_and_confirm<F>(
        &mut self,
        paused: &PausedTask,
        operation_id: &str,
        guard: F,
    ) -> Result<ResumeConfirmation, SessionError>
    where
        F: Fn() -> bool,
    {
        validate_id(operation_id)?;
        if !guard() {
            return Err(cancelled());
        }
        let current = self.fresh_snapshot(&paused.thread_id).await?;
        if let Some(confirmation) = self.resumed_confirmation(&current, paused, operation_id, true)
        {
            return Ok(confirmation);
        }
        let unchanged_pause = match self.reopened_baselines.get(&pause_key(paused)) {
            Some(baseline) => current == *baseline && matches_reopened_pause(&current, paused),
            // A pause restored in the same desktop still has its original,
            // strictly checked context. A reopened desktop must first establish
            // its baseline through wait_for_reopened_paused_chat.
            None => current.matches_paused(paused),
        };
        if !unchanged_pause {
            return Err(SessionError::new(
                SessionErrorKind::StateChanged,
                "恢复前原轮次、模型、权限或等待状态已改变；不会自动继续用户停止或已完成任务",
            ));
        }
        self.confirm_owner(&current).await?;
        if !guard() {
            return Err(cancelled());
        }
        let key = format!("resume:{operation_id}:{}", paused.thread_id);
        if !self.mutation_attempts.insert(key) {
            return Err(SessionError::uncertain(
                "本次恢复曾发送过，需核对新轮次，未重复发送",
            ));
        }
        let _reply = self
            .request(
                "thread-follower-start-turn",
                resume_params(paused, operation_id),
                Some(&current.owner),
                true,
            )
            .await;
        self.observe_resume(paused, operation_id, false, CONFIRM_TIMEOUT)
            .await
    }

    /// Read-only reconciliation: does not emit start/interrupt commands.
    pub async fn reconcile_resume(
        &mut self,
        paused: &PausedTask,
        operation_id: &str,
    ) -> Result<ResumeConfirmation, SessionError> {
        validate_id(operation_id)?;
        self.fresh_snapshot(&paused.thread_id).await?;
        self.observe_resume(paused, operation_id, true, SNAPSHOT_TIMEOUT)
            .await
    }

    async fn observe_resume(
        &mut self,
        paused: &PausedTask,
        operation_id: &str,
        already_observed: bool,
        wait: Duration,
    ) -> Result<ResumeConfirmation, SessionError> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(current) = self.current_snapshot(&paused.thread_id) {
                if let Some(confirmation) =
                    self.resumed_confirmation(&current, paused, operation_id, already_observed)
                {
                    return Ok(confirmation);
                }
                let belongs_to_this_start =
                    current.client_user_message_id.as_deref() == Some(operation_id);
                // The desktop first broadcasts this operation's optimistic turn
                // with a null id, then fills its id and effective settings in
                // subsequent patches. Read through those states without sending
                // start again; another user's turn must still stop this wait.
                if (current.turn_id.as_deref() != Some(paused.turn_id.as_str())
                    && !belongs_to_this_start)
                    || (current.waiting && !belongs_to_this_start)
                    || !same_saved_project_and_model(&current, paused)
                {
                    return Err(SessionError::uncertain(
                        "恢复结果与本次操作无法对应，需核对原聊天；未重复发送继续",
                    ));
                }
            }
            if self.broken || tokio::time::Instant::now() >= deadline {
                break;
            }
            if self
                .pump_until(deadline)
                .await
                .map_err(|error| SessionError::uncertain(error.message))?
                .is_none()
            {
                break;
            }
            self.flush_resyncs()
                .await
                .map_err(|error| SessionError::uncertain(error.message))?;
        }
        Err(SessionError::uncertain(
            "未确认本次恢复产生的新轮次；不会盲目重发继续",
        ))
    }

    fn resumed_confirmation(
        &self,
        current: &TaskSnapshot,
        paused: &PausedTask,
        operation_id: &str,
        already_observed: bool,
    ) -> Option<ResumeConfirmation> {
        if paused.saved_settings.is_none() {
            return resumed_by_operation(current, paused, operation_id, already_observed);
        }
        let saved = paused.saved_settings.as_ref()?;
        let metadata = &self.states.get(&paused.thread_id)?.metadata;
        (current.thread_id == paused.thread_id
            && current
                .turn_id
                .as_deref()
                .is_some_and(|turn| turn != paused.turn_id)
            && current.client_user_message_id.as_deref() == Some(operation_id)
            && same_saved_project_and_model(current, paused)
            && !current.is_child
            && !current.ephemeral
            && effective_permissions_match(&metadata["currentPermissions"], saved)
            && resumed_model_settings_match(metadata, saved))
        .then(|| ResumeConfirmation {
            thread_id: paused.thread_id.clone(),
            turn_id: current.turn_id.clone().unwrap(),
            operation_id: operation_id.into(),
            already_observed,
        })
    }

    fn current_snapshot(&self, thread_id: &str) -> Option<TaskSnapshot> {
        self.states
            .get(thread_id)
            .and_then(|tracked| summarize(&tracked.metadata, &tracked.owner))
    }

    async fn await_snapshot(
        &mut self,
        thread_id: &str,
        wait: Duration,
    ) -> Result<TaskSnapshot, SessionError> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(task) = self.current_snapshot(thread_id) {
                return Ok(task);
            }
            if self.pump_until(deadline).await?.is_none() {
                break;
            }
            self.flush_resyncs().await?;
        }
        Err(SessionError::new(
            SessionErrorKind::Unknown,
            "未获得原聊天的实时桌面状态；无 owner 不能当作没有运行任务",
        ))
    }

    async fn confirm_owner(&mut self, snapshot: &TaskSnapshot) -> Result<(), SessionError> {
        let reply = self
            .request(
                "thread-owner-discovery",
                json!({"hostId":"local","conversationId":snapshot.thread_id}),
                None,
                false,
            )
            .await?;
        if reply["handledByClientId"].as_str() != Some(snapshot.owner.as_str())
            || self.current_snapshot(&snapshot.thread_id).as_ref() != Some(snapshot)
        {
            return Err(SessionError::new(
                SessionErrorKind::StateChanged,
                "原聊天的桌面 owner 或任务状态已改变",
            ));
        }
        Ok(())
    }

    async fn follow(&mut self, thread_id: &str, following: bool) -> Result<(), SessionError> {
        if following {
            self.followed.insert(thread_id.into());
        } else {
            self.followed.remove(thread_id);
        }
        self.send(&json!({"type":"broadcast","sourceClientId":self.client_id,"method":"thread-stream-following-changed","version":1,"params":{"conversationId":thread_id,"hostId":"local","following":following}}), false).await
    }

    async fn flush_resyncs(&mut self) -> Result<(), SessionError> {
        let ids: Vec<_> = self.resync_needed.drain().collect();
        for id in ids {
            self.follow(&id, false).await?;
            self.follow(&id, true).await?;
        }
        Ok(())
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        target: Option<&str>,
        mutation: bool,
    ) -> Result<Value, SessionError> {
        if !self.compatible {
            return Err(SessionError::new(
                SessionErrorKind::Incompatible,
                "Codex 桌面协调协议版本已改变",
            ));
        }
        let timeout = if mutation {
            MUTATION_TIMEOUT
        } else {
            REQUEST_TIMEOUT
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut envelope = json!({"type":"request","requestId":request_id,"sourceClientId":self.client_id,"method":method,"params":params,"version":method_version(method)?,"timeoutMs":timeout.as_millis() as u64});
        if let Some(target) = target {
            envelope["targetClientId"] = json!(target);
        }
        self.send(&envelope, mutation).await?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let message = self.pump_until(deadline).await.map_err(|error| {
                if mutation {
                    SessionError::uncertain(error.message)
                } else {
                    error
                }
            })?;
            let Some(message) = message else {
                return Err(if mutation {
                    SessionError::uncertain("Codex 桌面请求超时，操作结果未知；未重放")
                } else {
                    SessionError::new(SessionErrorKind::Unknown, "Codex 桌面状态查询超时")
                });
            };
            self.flush_resyncs().await.map_err(|error| {
                if mutation {
                    SessionError::uncertain(error.message)
                } else {
                    error
                }
            })?;
            if message["type"] == "response" && message["requestId"] == request_id {
                if message["resultType"] == "success" {
                    return Ok(message);
                }
                let (kind, reason) = match message["error"].as_str() {
                    Some("no-client-found") => {
                        (SessionErrorKind::Unknown, "原聊天的桌面 owner 不可用")
                    }
                    Some("request-version-mismatch") => (
                        SessionErrorKind::Incompatible,
                        "Codex 桌面协调协议版本不匹配",
                    ),
                    Some("client-disconnected") => (SessionErrorKind::Unknown, "原聊天桌面已断开"),
                    _ => (SessionErrorKind::Unknown, "Codex 桌面协调请求未确认成功"),
                };
                return Err(if mutation {
                    SessionError::uncertain(reason)
                } else {
                    SessionError::new(kind, reason)
                });
            }
        }
    }

    async fn send(&mut self, message: &Value, mutation: bool) -> Result<(), SessionError> {
        if self.broken {
            return Err(SessionError::new(
                SessionErrorKind::Unavailable,
                "Codex 桌面连接已失效",
            ));
        }
        let body = serde_json::to_vec(message)
            .map_err(|_| SessionError::new(SessionErrorKind::Unknown, "无法编码桌面请求"))?;
        checked_length(body.len())?;
        let mut frame = Vec::with_capacity(body.len() + 4);
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend(body);
        match tokio::time::timeout(REQUEST_TIMEOUT, self.io.write_all(&frame)).await {
            Ok(Ok(())) => Ok(()),
            _ => {
                self.broken = true;
                self.states.clear();
                Err(if mutation {
                    SessionError::uncertain("桌面请求写入未确认，需核对结果；未重放")
                } else {
                    SessionError::new(SessionErrorKind::Unavailable, "Codex 桌面协调连接写入失败")
                })
            }
        }
    }

    async fn pump_until(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Result<Option<Value>, SessionError> {
        loop {
            if self.receive_buffer.len() >= 4 {
                let length =
                    u32::from_le_bytes(self.receive_buffer[..4].try_into().unwrap()) as usize;
                checked_length(length)?;
                if self.receive_buffer.len() >= length + 4 {
                    let message: Value = serde_json::from_slice(
                        &self.receive_buffer[4..length + 4],
                    )
                    .map_err(|_| {
                        SessionError::new(
                            SessionErrorKind::Incompatible,
                            "Codex 桌面返回无法识别的协议帧",
                        )
                    })?;
                    self.receive_buffer.drain(..length + 4);
                    if message["type"] == "client-discovery-request" {
                        self.send(&json!({"type":"client-discovery-response","requestId":message["requestId"],"response":{"canHandle":false}}), false).await?;
                    } else {
                        self.ingest(&message)?;
                    }
                    return Ok(Some(message));
                }
            }
            if self.broken {
                return Err(SessionError::new(
                    SessionErrorKind::Unavailable,
                    "Codex 桌面协调连接已失效",
                ));
            }
            let mut chunk = [0u8; 8192];
            match tokio::time::timeout_at(deadline, self.io.read(&mut chunk)).await {
                Err(_) => return Ok(None),
                Ok(Ok(0)) | Ok(Err(_)) => {
                    self.broken = true;
                    self.states.clear();
                    return Err(SessionError::new(
                        SessionErrorKind::Unavailable,
                        "Codex 桌面协调连接已断开",
                    ));
                }
                Ok(Ok(length)) => self.receive_buffer.extend_from_slice(&chunk[..length]),
            }
        }
    }

    fn ingest(&mut self, message: &Value) -> Result<(), SessionError> {
        if message["type"] != "broadcast" {
            return Ok(());
        }
        match message["method"].as_str() {
            Some("ipc-connection-reset") => {
                self.states.clear();
                self.resync_needed.extend(self.followed.iter().cloned());
            }
            Some("client-status-changed")
                if message.pointer("/params/status").and_then(Value::as_str)
                    == Some("disconnected") =>
            {
                let owner = message.pointer("/params/clientId").and_then(Value::as_str);
                self.states
                    .retain(|_, value| Some(value.owner.as_str()) != owner);
            }
            Some("thread-stream-state-changed") => {
                if message.pointer("/params/hostId").and_then(Value::as_str) != Some("local") {
                    return Ok(());
                }
                let Some(id) = message
                    .pointer("/params/conversationId")
                    .and_then(Value::as_str)
                else {
                    return Ok(());
                };
                if !self.followed.contains(id) {
                    return Ok(());
                }
                if message["version"] != 11 {
                    self.compatible = false;
                    self.states.clear();
                    return Err(SessionError::new(
                        SessionErrorKind::Incompatible,
                        "Codex 桌面聊天状态协议版本已改变",
                    ));
                }
                let Some(owner) = message["sourceClientId"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                else {
                    self.states.remove(id);
                    return Ok(());
                };
                let change = &message["params"]["change"];
                if change["type"] == "snapshot" {
                    let metadata =
                        project_at(&[], &change["conversationState"]).unwrap_or(Value::Null);
                    if metadata["id"].as_str() != Some(id) || change["revision"].is_null() {
                        self.states.remove(id);
                        return Ok(());
                    }
                    self.states.insert(
                        id.into(),
                        TrackedState {
                            owner: owner.into(),
                            revision: change["revision"].clone(),
                            metadata,
                        },
                    );
                } else if change["type"] == "patches" {
                    let valid = self.states.get_mut(id).filter(|tracked| {
                        tracked.owner == owner && tracked.revision == change["baseRevision"]
                    });
                    if let Some(tracked) = valid {
                        if apply_metadata_patches(&mut tracked.metadata, &change["patches"]).is_ok()
                        {
                            tracked.revision = change["revision"].clone();
                        } else {
                            self.states.remove(id);
                            self.resync_needed.insert(id.into());
                        }
                    } else {
                        self.states.remove(id);
                        self.resync_needed.insert(id.into());
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn cancelled() -> SessionError {
    SessionError::new(
        SessionErrorKind::Cancelled,
        "本次流程已取消或被新的用户选择取代",
    )
}

/// Read thread IDs only. Missing/unreadable databases and truncated discovery
/// are unknown inventory, never an empty list interpreted as no running work.
pub fn discover_thread_ids(codex_dir: &std::path::Path) -> Result<Vec<String>, SessionError> {
    use rusqlite::{Connection, OpenFlags};
    let path = codex_dir.join("state_5.sqlite");
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| {
        SessionError::new(
            SessionErrorKind::Unknown,
            "无法只读打开 Codex 聊天元数据，不能确认全部运行任务",
        )
    })?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|_| SessionError::new(SessionErrorKind::Unknown, "无法配置聊天元数据读取"))?;
    let columns: HashSet<String> = connection
        .prepare("PRAGMA table_info(threads)")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect()
        })
        .map_err(|_| {
            SessionError::new(SessionErrorKind::Unknown, "无法识别 Codex 聊天元数据结构")
        })?;
    if !columns.contains("id") || !columns.contains("archived") {
        return Err(SessionError::new(
            SessionErrorKind::Incompatible,
            "Codex 聊天元数据缺少必要字段",
        ));
    }
    // Include child/ephemeral threads in safety discovery. They are never
    // resumed independently, but excluding a running child would make an
    // incomplete root-only inventory falsely authorize closing the app.
    let order = if columns.contains("updated_at_ms") {
        "updated_at_ms DESC, id ASC"
    } else if columns.contains("updated_at") {
        "updated_at DESC, id ASC"
    } else {
        "id ASC"
    };
    let sql = format!(
        "SELECT id FROM threads ORDER BY {order} LIMIT {}",
        MAX_CANDIDATES + 1
    );
    let ids: Vec<String> = connection
        .prepare(&sql)
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect()
        })
        .map_err(|_| {
            SessionError::new(
                SessionErrorKind::Unknown,
                "读取 Codex 聊天标识失败，不能确认运行任务",
            )
        })?;
    if ids.len() > MAX_CANDIDATES {
        return Err(SessionError::new(
            SessionErrorKind::Unknown,
            "本机聊天元数据超过完整检查上限，未截断后当作安全",
        ));
    }
    for id in &ids {
        validate_id(id)?;
    }
    Ok(ids)
}

/// Open the same desktop chat through the Windows registered Codex protocol.
/// This only navigates; it never sends a prompt or starts terminal Codex.
pub fn open_original_chat(thread_id: &str) -> Result<(), SessionError> {
    validate_id(thread_id)?;
    #[cfg(target_os = "windows")]
    {
        // This standard Shell API does not require a new windows-sys feature
        // in the application's dependency manifest.
        #[link(name = "shell32")]
        extern "system" {
            fn ShellExecuteW(
                window: *mut std::ffi::c_void,
                operation: *const u16,
                file: *const u16,
                parameters: *const u16,
                directory: *const u16,
                show: i32,
            ) -> *mut std::ffi::c_void;
        }
        let wide = |value: &str| {
            value
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>()
        };
        let verb = wide("open");
        let url = wide(&format!("codex://threads/{thread_id}"));
        // SW_SHOWNORMAL. The caller has already confirmed the new app instance.
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                url.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
            )
        };
        if result as isize <= 32 {
            return Err(SessionError::new(
                SessionErrorKind::Unknown,
                "Windows 未确认打开原聊天；未重复导航或发送继续",
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(SessionError::new(
            SessionErrorKind::Unavailable,
            "当前平台尚未实现原桌面聊天导航",
        ))
    }
}

fn validate_id(id: &str) -> Result<(), SessionError> {
    uuid::Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| SessionError::new(SessionErrorKind::Unknown, "聊天或操作标识格式无效"))
}
fn checked_length(length: usize) -> Result<(), SessionError> {
    if length == 0 || length > MAX_FRAME_BYTES {
        Err(SessionError::new(
            SessionErrorKind::Incompatible,
            "Codex 桌面协议帧长度无效",
        ))
    } else {
        Ok(())
    }
}
fn method_version(method: &str) -> Result<u64, SessionError> {
    match method {
        "initialize" => Ok(0),
        "thread-owner-discovery" => Ok(1),
        "thread-follower-interrupt-turn" => Ok(4),
        "thread-follower-start-turn" => Ok(2),
        _ => Err(SessionError::new(
            SessionErrorKind::Blocked,
            "拒绝未知桌面协调请求",
        )),
    }
}

fn resume_params(paused: &PausedTask, operation_id: &str) -> Value {
    // The desktop resolver treats usePermissionSelection=true as a request to
    // delegate permissions to current app-server defaults. Explicit false on
    // both selectors instead inherits this original thread's permissions.
    let mut result = json!({"conversationId":paused.thread_id,"turnStart":{"request":{"threadId":paused.thread_id,"input":[{"type":"text","text":RESUME_TEXT,"text_elements":[]}],"clientUserMessageId":operation_id},"context":{"inheritThreadSettings":true,"usePermissionSelection":false,"useAppServerPermissionDefault":false}}});
    if let Some(saved) = &paused.saved_settings {
        for key in [
            "approvalPolicy",
            "approvalsReviewer",
            "sandboxPolicy",
            "permissions",
            "model",
            "effort",
            "collaborationMode",
            "cwd",
            "serviceTier",
            "multiAgentMode",
        ] {
            if let Some(value) = saved.thread_settings.get(key) {
                result["turnStart"]["request"][key] = value.clone();
            }
        }
        if let Some(roots) = saved.current_permissions.get("runtimeWorkspaceRoots") {
            result["turnStart"]["request"]["runtimeWorkspaceRoots"] = roots.clone();
        }
    }
    result
}

fn pause_key(paused: &PausedTask) -> String {
    format!("{}:{}", paused.pause_operation_id, paused.thread_id)
}

fn same_saved_project_and_model(current: &TaskSnapshot, paused: &PausedTask) -> bool {
    if paused.saved_settings.is_none() {
        return current.context == paused.context;
    }
    current.context.cwd == paused.context.cwd
        && current.context.model == paused.context.model
        && current.context.model_provider == paused.context.model_provider
}

fn matches_reopened_pause(current: &TaskSnapshot, paused: &PausedTask) -> bool {
    if paused.saved_settings.is_none() {
        return current.matches_paused(paused);
    }
    paused.confirmed_by_cc_switch
        && current.thread_id == paused.thread_id
        && current.turn_id.as_deref() == Some(paused.turn_id.as_str())
        && current.status.as_deref() == Some("interrupted")
        && current.safely_idle()
        && !current.is_child
        && !current.ephemeral
        && same_saved_project_and_model(current, paused)
}

fn save_settings_error() -> SessionError {
    SessionError::new(
        SessionErrorKind::Blocked,
        "原聊天模型或权限设置不完整、格式未知或超过保存上限；未暂停任务",
    )
}

fn bounded_text(value: &Value, max: usize, nullable: bool) -> bool {
    (nullable && value.is_null()) || value.as_str().is_some_and(|text| text.len() <= max)
}

fn bounded_roots(value: &Value) -> bool {
    value.as_array().is_some_and(|roots| {
        roots.len() <= 128 && roots.iter().all(|root| bounded_text(root, 4096, false))
    })
}

fn valid_policy_field(key: &str, value: &Value) -> bool {
    match key {
        "approvalPolicy" => {
            value.as_str().is_some_and(|policy| {
                ["never", "on-request", "on-failure", "untrusted"].contains(&policy)
            }) || value.as_object().is_some_and(|policy| {
                policy.len() == 1
                    && policy
                        .get("granular")
                        .and_then(Value::as_object)
                        .is_some_and(|granular| {
                            granular.len() == 5
                                && granular.iter().all(|(key, value)| {
                                    [
                                        "sandbox_approval",
                                        "rules",
                                        "skill_approval",
                                        "request_permissions",
                                        "mcp_elicitations",
                                    ]
                                    .contains(&key.as_str())
                                        && value.is_boolean()
                                })
                        })
            })
        }
        "approvalsReviewer" => bounded_text(value, 128, true),
        "runtimeWorkspaceRoots" => bounded_roots(value),
        "activePermissionProfile" => {
            value.is_null()
                || value.as_object().is_some_and(|profile| {
                    profile
                        .keys()
                        .all(|key| ["id", "extends"].contains(&key.as_str()))
                        && profile
                            .get("id")
                            .is_some_and(|id| bounded_text(id, 512, false))
                        && profile
                            .get("extends")
                            .is_none_or(|extends| bounded_text(extends, 512, true))
                })
        }
        "sandboxPolicy" => value.as_object().is_some_and(|policy| {
            policy
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    [
                        "dangerFullAccess",
                        "readOnly",
                        "workspaceWrite",
                        "externalSandbox",
                    ]
                    .contains(&kind)
                })
                && policy.iter().all(|(key, value)| match key.as_str() {
                    "type" => bounded_text(value, 64, false),
                    "writableRoots" => bounded_roots(value),
                    "networkAccess" | "excludeTmpdirEnvVar" | "excludeSlashTmp" => {
                        value.is_boolean()
                    }
                    _ => false,
                })
        }),
        _ => false,
    }
}

fn valid_thread_setting(key: &str, value: &Value) -> bool {
    match key {
        "approvalPolicy" | "approvalsReviewer" | "activePermissionProfile" | "sandboxPolicy" => {
            valid_policy_field(key, value)
        }
        "permissions" => bounded_text(value, 512, true),
        "model" => {
            bounded_text(value, 256, false) && value.as_str().is_some_and(|model| !model.is_empty())
        }
        "cwd" => bounded_text(value, 4096, true),
        "effort" | "serviceTier" | "summary" | "personality" | "multiAgentMode" => {
            bounded_text(value, 128, true)
        }
        "disabledPluginIds" => {
            value.is_null()
                || value.as_array().is_some_and(|ids| {
                    ids.len() <= 128 && ids.iter().all(|id| bounded_text(id, 512, false))
                })
        }
        "collaborationMode" => {
            value.is_null()
                || value.as_object().is_some_and(|mode| {
                    mode.keys()
                        .all(|key| ["mode", "settings"].contains(&key.as_str()))
                        && mode
                            .get("mode")
                            .is_some_and(|value| bounded_text(value, 128, false))
                        && mode
                            .get("settings")
                            .and_then(Value::as_object)
                            .is_some_and(|settings| {
                                settings.keys().all(|key| {
                                    ["model", "reasoning_effort", "developer_instructions"]
                                        .contains(&key.as_str())
                                }) && settings
                                    .get("model")
                                    .is_some_and(|value| bounded_text(value, 256, false))
                                    && settings
                                        .get("reasoning_effort")
                                        .is_none_or(|value| bounded_text(value, 128, true))
                                    && settings
                                        .get("developer_instructions")
                                        .is_none_or(|value| bounded_text(value, 16384, true))
                            })
                })
        }
        _ => false,
    }
}

fn validate_saved_settings(saved: &SavedTaskSettings) -> Result<(), SessionError> {
    let settings = saved
        .thread_settings
        .as_object()
        .ok_or_else(save_settings_error)?;
    let permissions = saved
        .current_permissions
        .as_object()
        .ok_or_else(save_settings_error)?;
    for (section, values) in [
        ("latestThreadSettings", settings),
        ("currentPermissions", permissions),
    ] {
        for (key, value) in values {
            let valid = if section == "latestThreadSettings" {
                valid_thread_setting(key, value)
            } else {
                valid_policy_field(key, value)
            };
            if !valid {
                let kind = match value {
                    Value::Null => "null",
                    Value::Bool(_) => "boolean",
                    Value::Number(_) => "number",
                    Value::String(_) => "string",
                    Value::Array(_) => "array",
                    Value::Object(_) => "object",
                };
                return Err(SessionError::new(SessionErrorKind::Blocked,
                    format!("无法保存原聊天设置字段 {section}.{key}（{kind}）：格式未知或超出白名单上限；未暂停任务")));
            }
        }
    }
    if !["model", "approvalPolicy", "sandboxPolicy"]
        .iter()
        .all(|key| settings.contains_key(*key))
        || !["approvalPolicy", "sandboxPolicy"]
            .iter()
            .all(|key| permissions.contains_key(*key))
        || serde_json::to_vec(saved).map_or(true, |bytes| bytes.len() > 32768)
    {
        return Err(save_settings_error());
    }
    Ok(())
}

fn capture_saved_settings(metadata: &Value) -> Result<SavedTaskSettings, SessionError> {
    let mut settings = metadata["latestThreadSettings"]
        .as_object()
        .cloned()
        .ok_or_else(save_settings_error)?;
    // modelProvider is read-only on ThreadSettings; preserve it through the
    // exact task-context checks instead of sending a write to that field.
    if let Some(provider) = settings.remove("modelProvider") {
        if provider != metadata["modelProvider"] {
            return Err(save_settings_error());
        }
    }
    let permissions = metadata["currentPermissions"]
        .as_object()
        .cloned()
        .ok_or_else(save_settings_error)?;
    settings
        .entry("model")
        .or_insert_with(|| metadata["latestModel"].clone());
    if settings["model"] != metadata["latestModel"] {
        return Err(save_settings_error());
    }
    settings
        .entry("cwd")
        .or_insert_with(|| metadata["cwd"].clone());
    if settings["cwd"] != metadata["cwd"] {
        return Err(save_settings_error());
    }
    settings
        .entry("effort")
        .or_insert_with(|| metadata["latestReasoningEffort"].clone());
    if !settings.contains_key("collaborationMode") && !metadata["latestCollaborationMode"].is_null()
    {
        settings.insert(
            "collaborationMode".into(),
            metadata["latestCollaborationMode"].clone(),
        );
    }
    for key in [
        "approvalPolicy",
        "approvalsReviewer",
        "sandboxPolicy",
        "activePermissionProfile",
    ] {
        if let Some(value) = permissions.get(key) {
            settings.entry(key).or_insert_with(|| value.clone());
        }
    }
    settings
        .entry("activePermissionProfile")
        .or_insert(Value::Null);
    let profile = settings
        .get("activePermissionProfile")
        .and_then(|value| value.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    settings.insert("permissions".into(), profile);
    let saved = SavedTaskSettings {
        thread_settings: Value::Object(settings),
        current_permissions: Value::Object(permissions),
    };
    validate_saved_settings(&saved)?;
    Ok(saved)
}

fn resumed_model_settings_match(metadata: &Value, saved: &SavedTaskSettings) -> bool {
    // The desktop clears latestReasoningEffort when collaborationMode supplies
    // the effective model/effort. Compare that effective value, not the cleared
    // flat field, or a successfully restored turn would be reported as failed.
    let expected_mode = &saved.thread_settings["collaborationMode"];
    let actual_mode = &metadata["latestCollaborationMode"];
    let expected_effort = if expected_mode.is_object() {
        &expected_mode["settings"]["reasoning_effort"]
    } else {
        &saved.thread_settings["effort"]
    };
    let actual_effort = if actual_mode.is_object() {
        &actual_mode["settings"]["reasoning_effort"]
    } else {
        &metadata["latestReasoningEffort"]
    };
    let effort_ok = actual_effort == expected_effort;
    let mode_ok = saved
        .thread_settings
        .get("collaborationMode")
        .is_none_or(|expected| {
            metadata.get("latestCollaborationMode") == Some(expected)
                || (expected.is_null() && metadata.get("latestCollaborationMode").is_none())
        });
    effort_ok && mode_ok
}

fn effective_permissions_match(current: &Value, saved: &SavedTaskSettings) -> bool {
    ["approvalPolicy", "approvalsReviewer", "sandboxPolicy"]
        .iter()
        .all(|key| {
            saved.thread_settings.get(*key).is_none_or(|expected| {
                current.get(*key) == Some(expected)
                    || (expected.is_null() && current.get(*key).is_none())
            })
        })
        && current["activePermissionProfile"]["id"]
            == saved.thread_settings["activePermissionProfile"]["id"]
        && (saved.thread_settings["activePermissionProfile"].is_null()
            || saved
                .current_permissions
                .get("runtimeWorkspaceRoots")
                .is_none_or(|expected| {
                    workspace_roots_match(&current["runtimeWorkspaceRoots"], expected)
                }))
}

fn workspace_roots_match(actual: &Value, expected: &Value) -> bool {
    let roots = |value: &Value| {
        value.as_array().and_then(|items| {
            items
                .iter()
                .map(|item| {
                    item.as_str().map(|path| {
                        let normalized = path.replace('\\', "/");
                        let normalized = normalized.trim_end_matches('/');
                        if cfg!(target_os = "windows") {
                            normalized.to_lowercase()
                        } else {
                            normalized.to_string()
                        }
                    })
                })
                .collect::<Option<HashSet<_>>>()
        })
    };
    roots(actual).is_some_and(|actual| roots(expected).is_some_and(|expected| actual == expected))
}

fn resumed_by_operation(
    current: &TaskSnapshot,
    paused: &PausedTask,
    operation_id: &str,
    already_observed: bool,
) -> Option<ResumeConfirmation> {
    let turn = current.turn_id.as_ref()?;
    (current.thread_id == paused.thread_id
        && turn != &paused.turn_id
        && current.client_user_message_id.as_deref() == Some(operation_id)
        && current.context == paused.context)
        .then(|| ResumeConfirmation {
            thread_id: paused.thread_id.clone(),
            turn_id: turn.clone(),
            operation_id: operation_id.into(),
            already_observed,
        })
}

fn context_digest(value: &Value) -> Value {
    use sha2::{Digest, Sha256};
    if value.is_null() {
        return Value::Null;
    }
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    json!(format!("{:x}", Sha256::digest(bytes)))
}

/// Project a known metadata subtree; None means a transcript/body path.
fn project_at(path: &[String], value: &Value) -> Option<Value> {
    if path.is_empty() {
        let object = value.as_object()?;
        let mut result = Map::new();
        for (key, value) in object {
            if let Some(projected) = project_at(&[key.clone()], value) {
                result.insert(key.clone(), projected);
            }
        }
        return Some(Value::Object(result));
    }
    let first = path[0].as_str();
    match first {
        "id"
        | "cwd"
        | "latestModel"
        | "latestReasoningEffort"
        | "latestCollaborationMode"
        | "modelProvider"
        | "agentNickname"
        | "threadSource"
        | "ephemeral"
        | "sideConversation" => Some(value.clone()),
        "latestThreadSettings" | "currentPermissions" => Some(value.clone()),
        "source" => {
            if path.len() == 1 {
                Some(
                    json!({"subAgent":value.get("subAgent").map(|v| !v.is_null()).unwrap_or(false)}),
                )
            } else if path.get(1).map(String::as_str) == Some("subAgent") {
                Some(json!(!value.is_null()))
            } else {
                None
            }
        }
        "requests" | "unconfirmedTurnSubmissions" => Some(if path.len() == 1 {
            Value::Array(
                value
                    .as_array()
                    .map(|items| items.iter().map(|_| json!({})).collect())
                    .unwrap_or_default(),
            )
        } else {
            json!({})
        }),
        "threadRuntimeStatus" => {
            if path.len() == 1 {
                Some(
                    json!({"type":value["type"],"activeFlags":value["activeFlags"].as_array().map(|items| items.iter().map(|_| json!({})).collect::<Vec<_>>()).unwrap_or_default()}),
                )
            } else if path.get(1).map(String::as_str) == Some("type") {
                Some(value.clone())
            } else if path.get(1).map(String::as_str) == Some("activeFlags") {
                Some(if path.len() == 2 {
                    Value::Array(
                        value
                            .as_array()
                            .map(|items| items.iter().map(|_| json!({})).collect())
                            .unwrap_or_default(),
                    )
                } else {
                    json!({})
                })
            } else {
                None
            }
        }
        "turns" => project_turn_path(&path[1..], value, true),
        "turnHistory" => project_history_path(&path[1..], value),
        _ => None,
    }
}

fn project_turn_path(path: &[String], value: &Value, is_array: bool) -> Option<Value> {
    if is_array && path.is_empty() {
        return Some(Value::Array(
            value
                .as_array()?
                .iter()
                .filter_map(|turn| project_turn_path(&[], turn, false))
                .collect(),
        ));
    }
    if is_array {
        return project_turn_path(&path[1..], value, false);
    }
    if path.is_empty() {
        return Some(
            json!({"turnId":value["turnId"],"status":value["status"],"turnStartedAtMs":value["turnStartedAtMs"],"params":{"clientUserMessageId":value.pointer("/params/clientUserMessageId").unwrap_or(&Value::Null)}}),
        );
    }
    match path[0].as_str() {
        "turnId" | "status" | "turnStartedAtMs" => Some(value.clone()),
        "params" if path.len() == 1 => {
            Some(json!({"clientUserMessageId":value["clientUserMessageId"]}))
        }
        "params" if path.get(1).map(String::as_str) == Some("clientUserMessageId") => {
            Some(value.clone())
        }
        _ => None,
    }
}

fn project_history_path(path: &[String], value: &Value) -> Option<Value> {
    if path.is_empty() {
        return Some(
            json!({"kind":value["kind"],"history":{"entitiesByKey":project_history_path(&["history".into(),"entitiesByKey".into()], &value["history"]["entitiesByKey"]),"islands":project_history_path(&["history".into(),"islands".into()], &value["history"]["islands"])}}),
        );
    }
    if path[0] == "kind" {
        return Some(value.clone());
    }
    if path[0] != "history" {
        return None;
    }
    if path.len() == 1 {
        return Some(
            json!({"entitiesByKey":project_history_path(&["history".into(),"entitiesByKey".into()], &value["entitiesByKey"]),"islands":project_history_path(&["history".into(),"islands".into()], &value["islands"])}),
        );
    }
    match path[1].as_str() {
        "entitiesByKey" if path.len() == 2 => {
            let mut projected = Map::new();
            for (key, turn) in value.as_object()? {
                projected.insert(key.clone(), project_turn_path(&[], turn, false)?);
            }
            Some(Value::Object(projected))
        }
        "entitiesByKey" => project_turn_path(&path[3..], value, false),
        "islands" => {
            let suffix = &path[2..];
            if suffix.is_empty() {
                return Some(Value::Array(value.as_array()?.iter().map(|island| json!({"entries":island["entries"].as_array().map(|entries| entries.iter().map(|entry| json!({"value":entry["value"]})).collect::<Vec<_>>()).unwrap_or_default()})).collect()));
            }
            if suffix.len() == 1 {
                return Some(
                    json!({"entries":value["entries"].as_array().map(|entries| entries.iter().map(|entry| json!({"value":entry["value"]})).collect::<Vec<_>>()).unwrap_or_default()}),
                );
            }
            if suffix[1] != "entries" {
                return None;
            }
            if suffix.len() == 2 {
                return Some(Value::Array(
                    value
                        .as_array()?
                        .iter()
                        .map(|entry| json!({"value":entry["value"]}))
                        .collect(),
                ));
            }
            if suffix.len() == 3 {
                return Some(json!({"value":value["value"]}));
            }
            (suffix.get(3).map(String::as_str) == Some("value")).then(|| value.clone())
        }
        _ => None,
    }
}

fn apply_metadata_patches(root: &mut Value, patches: &Value) -> Result<(), ()> {
    for patch in patches.as_array().ok_or(())? {
        let path: Vec<String> = patch["path"]
            .as_array()
            .ok_or(())?
            .iter()
            .map(|segment| {
                segment
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| segment.as_u64().map(|value| value.to_string()))
                    .ok_or(())
            })
            .collect::<Result<_, _>>()?;
        if path
            .iter()
            .any(|segment| matches!(segment.as_str(), "__proto__" | "constructor" | "prototype"))
        {
            return Err(());
        }
        let op = patch["op"].as_str().ok_or(())?;
        let projection = project_at(&path, &patch["value"]);
        if projection.is_none() && op != "remove" {
            continue;
        }
        // For removals use the existing metadata value to decide relevance.
        if op == "remove" && !metadata_path_exists(root, &path) {
            continue;
        }
        if path.is_empty() {
            if op == "remove" {
                return Err(());
            }
            *root = projection.ok_or(())?;
            continue;
        }
        let (last, parents) = path.split_last().ok_or(())?;
        let mut parent = &mut *root;
        for segment in parents {
            parent = match parent {
                Value::Object(object) => object.get_mut(segment).ok_or(())?,
                Value::Array(array) => array
                    .get_mut(segment.parse::<usize>().map_err(|_| ())?)
                    .ok_or(())?,
                _ => return Err(()),
            };
        }
        match parent {
            Value::Object(object) => match op {
                "remove" => {
                    object.remove(last).ok_or(())?;
                }
                "add" | "replace" => {
                    object.insert(last.clone(), projection.ok_or(())?);
                }
                _ => return Err(()),
            },
            Value::Array(array) => {
                let index: usize = last.parse().map_err(|_| ())?;
                match op {
                    "remove" if index < array.len() => {
                        array.remove(index);
                    }
                    "add" if index <= array.len() => array.insert(index, projection.ok_or(())?),
                    "replace" if index < array.len() => array[index] = projection.ok_or(())?,
                    _ => return Err(()),
                }
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

fn metadata_path_exists(root: &Value, path: &[String]) -> bool {
    let mut current = root;
    for segment in path {
        current = match current {
            Value::Object(object) => match object.get(segment) {
                Some(value) => value,
                None => return false,
            },
            Value::Array(array) => match segment.parse::<usize>().ok().and_then(|i| array.get(i)) {
                Some(value) => value,
                None => return false,
            },
            _ => return false,
        };
    }
    true
}

fn summarize(state: &Value, owner: &str) -> Option<TaskSnapshot> {
    let last = if state.pointer("/turnHistory/kind").and_then(Value::as_str) == Some("canonical") {
        let history = &state["turnHistory"]["history"];
        let keys: Vec<&Value> = history["islands"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|island| island["entries"].as_array().into_iter().flatten())
            .map(|entry| &entry["value"])
            .collect();
        keys.iter()
            .rev()
            .find_map(|key| {
                key.as_str()
                    .and_then(|key| history["entitiesByKey"].get(key))
            })
            .or_else(|| {
                history["entitiesByKey"].as_object().and_then(|entities| {
                    entities
                        .values()
                        .max_by_key(|turn| turn["turnStartedAtMs"].as_i64().unwrap_or(0))
                })
            })
    } else {
        state["turns"].as_array().and_then(|turns| turns.last())
    };
    let empty = Value::Null;
    let last = last.unwrap_or(&empty);
    let nonempty = |value: &Value| {
        value
            .as_array()
            .map(|items| !items.is_empty())
            .unwrap_or(false)
    };
    Some(TaskSnapshot {
        thread_id: state["id"].as_str()?.into(),
        owner: owner.into(),
        turn_id: last["turnId"].as_str().map(str::to_string),
        status: last["status"].as_str().map(str::to_string),
        runtime_status: state
            .pointer("/threadRuntimeStatus/type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .into(),
        waiting: nonempty(&state["requests"])
            || nonempty(&state["threadRuntimeStatus"]["activeFlags"])
            || nonempty(&state["unconfirmedTurnSubmissions"]),
        is_child: state["agentNickname"]
            .as_str()
            .map(|value| !value.is_empty())
            .unwrap_or(false)
            || state["threadSource"]
                .as_str()
                .map(|value| value.to_lowercase().contains("subagent"))
                .unwrap_or(false)
            || state
                .pointer("/source/subAgent")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ephemeral: state["ephemeral"].as_bool().unwrap_or(false)
            || state["sideConversation"].as_bool().unwrap_or(false),
        client_user_message_id: last
            .pointer("/params/clientUserMessageId")
            .and_then(Value::as_str)
            .map(str::to_string),
        context: TaskContext {
            cwd: state["cwd"].as_str().map(str::to_string),
            model: state["latestModel"].as_str().map(str::to_string),
            model_provider: state["modelProvider"].as_str().map(str::to_string),
            thread_settings: context_digest(&state["latestThreadSettings"]),
            current_permissions: context_digest(&state["currentPermissions"]),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const THREAD: &str = "11111111-1111-4111-8111-111111111111";
    const OP: &str = "22222222-2222-4222-8222-222222222222";

    fn state(status: &str) -> Value {
        let permissions = json!({"approvalPolicy":"on-request","approvalsReviewer":"user","sandboxPolicy":{"type":"workspaceWrite","writableRoots":["C:/project"],"networkAccess":false},"activePermissionProfile":null,"runtimeWorkspaceRoots":["C:/project"]});
        let mut settings = permissions.clone();
        settings
            .as_object_mut()
            .unwrap()
            .remove("runtimeWorkspaceRoots");
        settings["model"] = json!("model");
        settings["effort"] = json!("high");
        settings["cwd"] = json!("C:/project");
        settings["permissions"] = Value::Null;
        settings["collaborationMode"] = json!({"mode":"default","settings":{"model":"model","reasoning_effort":"high","developer_instructions":null}});
        json!({"id":THREAD,"cwd":"C:/project","latestModel":"model","latestReasoningEffort":"high","latestCollaborationMode":settings["collaborationMode"],"modelProvider":"openai","latestThreadSettings":settings,"currentPermissions":permissions,"threadRuntimeStatus":{"type":if status=="inProgress" {"active"} else {"idle"},"activeFlags":[]},"requests":[],"unconfirmedTurnSubmissions":[],"turns":[{"turnId":"original-turn","status":status,"params":{"clientUserMessageId":"old-message","input":[{"text":"private user text"}]},"items":[{"text":"private tool output"}]}]})
    }
    fn projected(status: &str) -> Value {
        project_at(&[], &state(status)).unwrap()
    }
    fn paused() -> PausedTask {
        let task = summarize(&projected("interrupted"), "owner").unwrap();
        PausedTask {
            thread_id: THREAD.into(),
            turn_id: "original-turn".into(),
            context: task.context,
            pause_operation_id: OP.into(),
            confirmed_by_cc_switch: true,
            saved_settings: None,
        }
    }
    fn fake_session() -> DesktopSession {
        let (client, _) = tokio::io::duplex(8192);
        DesktopSession {
            io: Box::new(client),
            client_id: "client".into(),
            receive_buffer: Vec::new(),
            followed: HashSet::from([THREAD.into()]),
            states: HashMap::new(),
            resync_needed: HashSet::new(),
            mutation_attempts: HashSet::new(),
            reopened_baselines: HashMap::new(),
            compatible: true,
            broken: false,
        }
    }

    #[test]
    fn snapshot_projection_discards_conversation_bodies_and_preserves_context() {
        let metadata = projected("inProgress");
        let encoded = serde_json::to_string(&metadata).unwrap();
        assert!(!encoded.contains("private"));
        assert!(!encoded.contains("input"));
        assert!(!encoded.contains("items"));
        let task = summarize(&metadata, "owner").unwrap();
        assert!(task.safely_running());
        assert_eq!(
            task.context.thread_settings,
            context_digest(&state("inProgress")["latestThreadSettings"])
        );
        assert!(!serde_json::to_string(&task)
            .unwrap()
            .contains("approvalPolicy"));
    }

    #[test]
    fn canonical_history_and_patches_preserve_last_turn_and_drop_text() {
        let original = state("inProgress")["turns"][0].clone();
        let mut metadata=project_at(&[],&json!({"id":THREAD,"threadRuntimeStatus":{"type":"active"},"turnHistory":{"kind":"canonical","history":{"entitiesByKey":{"last":original},"islands":[{"entries":[{"value":"last"}]}]}}})).unwrap();
        assert_eq!(
            summarize(&metadata, "owner").unwrap().turn_id.as_deref(),
            Some("original-turn")
        );
        apply_metadata_patches(&mut metadata,&json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","last","status"],"value":"interrupted"},{"op":"add","path":["turnHistory","history","entitiesByKey","last","items",0],"value":{"text":"secret"}},{"op":"replace","path":["threadRuntimeStatus","type"],"value":"idle"}])).unwrap();
        let task = summarize(&metadata, "owner").unwrap();
        assert!(task.safely_idle());
        assert_eq!(task.status.as_deref(), Some("interrupted"));
        assert!(!serde_json::to_string(&metadata).unwrap().contains("secret"));
    }

    #[test]
    fn approvals_unconfirmed_turns_children_and_user_stops_are_not_resumed() {
        for waiting_key in ["requests", "unconfirmedTurnSubmissions"] {
            let mut value = state("inProgress");
            value[waiting_key] = json!([{"text":"secret"}]);
            assert!(!summarize(&project_at(&[], &value).unwrap(), "owner")
                .unwrap()
                .safely_running());
        }
        let mut value = state("interrupted");
        value["threadRuntimeStatus"]["activeFlags"] = json!(["waitingOnApproval"]);
        assert!(!summarize(&project_at(&[], &value).unwrap(), "owner")
            .unwrap()
            .matches_paused(&paused()));
        let mut owned = paused();
        owned.confirmed_by_cc_switch = false;
        assert!(!summarize(&projected("interrupted"), "owner")
            .unwrap()
            .matches_paused(&owned));
        assert!(!summarize(&projected("completed"), "owner")
            .unwrap()
            .matches_paused(&paused()));
    }

    #[test]
    fn resume_request_uses_actual_follower_protocol_and_desktop_permission_selection() {
        let params = resume_params(&paused(), OP);
        assert_eq!(method_version("thread-follower-start-turn").unwrap(), 2);
        assert_eq!(method_version("thread-follower-interrupt-turn").unwrap(), 4);
        assert_eq!(
            params
                .pointer("/turnStart/request/clientUserMessageId")
                .unwrap(),
            OP
        );
        assert_eq!(
            params
                .pointer("/turnStart/context/inheritThreadSettings")
                .unwrap(),
            true
        );
        assert_eq!(
            params
                .pointer("/turnStart/context/useAppServerPermissionDefault")
                .unwrap(),
            false
        );
        assert_eq!(
            params
                .pointer("/turnStart/context/usePermissionSelection")
                .unwrap(),
            false
        );
        assert!(params
            .pointer("/turnStart/request/approvalPolicy")
            .is_none());
        assert!(params.pointer("/turnStart/request/model").is_none());
        assert!(method_version("account/read").is_err());
    }

    #[test]
    fn revision_gap_owner_disconnect_and_protocol_change_invalidate_state() {
        let mut session = fake_session();
        session.ingest(&json!({"type":"broadcast","method":"thread-stream-state-changed","version":11,"sourceClientId":"owner","params":{"hostId":"local","conversationId":THREAD,"change":{"type":"snapshot","revision":1,"conversationState":state("inProgress")}}})).unwrap();
        assert!(session.current_snapshot(THREAD).is_some());
        session.ingest(&json!({"type":"broadcast","method":"thread-stream-state-changed","version":11,"sourceClientId":"owner","params":{"hostId":"local","conversationId":THREAD,"change":{"type":"patches","baseRevision":0,"revision":2,"patches":[]}}})).unwrap();
        assert!(session.current_snapshot(THREAD).is_none());
        assert!(session.resync_needed.contains(THREAD));
        session.states.insert(
            THREAD.into(),
            TrackedState {
                owner: "owner".into(),
                revision: json!(1),
                metadata: projected("inProgress"),
            },
        );
        session.ingest(&json!({"type":"broadcast","method":"client-status-changed","params":{"clientId":"owner","status":"disconnected"}})).unwrap();
        assert!(session.current_snapshot(THREAD).is_none());
        assert!(session.ingest(&json!({"type":"broadcast","method":"thread-stream-state-changed","version":12,"sourceClientId":"owner","params":{"hostId":"local","conversationId":THREAD}})).is_err());
    }

    #[test]
    fn unknown_candidate_coverage_and_empty_inventory_cannot_authorize_restart() {
        assert!(!TaskInventory::default().safe_to_restart());
        let mut inventory = TaskInventory {
            candidate_coverage_complete: true,
            ..Default::default()
        };
        assert!(inventory.safe_to_restart());
        inventory.unresolved_thread_ids.push(THREAD.into());
        assert!(!inventory.safe_to_restart());
    }

    #[tokio::test]
    async fn fragmented_frame_and_unrelated_broadcast_are_processed_before_response() {
        let (client, mut server) = tokio::io::duplex(65536);
        let server_task = tokio::spawn(async move {
            let mut header = [0; 4];
            server.read_exact(&mut header).await.unwrap();
            let mut request = vec![0; u32::from_le_bytes(header) as usize];
            server.read_exact(&mut request).await.unwrap();
            let request: Value = serde_json::from_slice(&request).unwrap();
            let reply = json!({"type":"response","requestId":request["requestId"],"resultType":"success","result":{"clientId":"assigned"}});
            let body = serde_json::to_vec(&reply).unwrap();
            let mut frame = (body.len() as u32).to_le_bytes().to_vec();
            frame.extend(body);
            for chunk in frame.chunks(3) {
                server.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let session = DesktopSession::initialize(Box::new(client)).await.unwrap();
        assert_eq!(session.client_id, "assigned");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn uncertain_resume_is_reconciled_by_operation_id_without_resending() {
        let paused = paused();
        let mut new = projected("inProgress");
        new["turns"][0]["turnId"] = json!("new-turn");
        new["turns"][0]["params"]["clientUserMessageId"] = json!(OP);
        let confirmation =
            resumed_by_operation(&summarize(&new, "new-owner").unwrap(), &paused, OP, true)
                .unwrap();
        assert_eq!(confirmation.turn_id, "new-turn");
        assert!(confirmation.already_observed);
        assert!(resumed_by_operation(
            &summarize(&new, "owner").unwrap(),
            &paused,
            "different",
            true
        )
        .is_none());
        let mut changed = new;
        changed["latestModel"] = json!("changed");
        assert!(
            resumed_by_operation(&summarize(&changed, "owner").unwrap(), &paused, OP, true)
                .is_none()
        );
    }

    #[test]
    fn invalid_frames_are_rejected_before_large_allocation() {
        assert!(checked_length(0).is_err());
        assert!(checked_length(MAX_FRAME_BYTES + 1).is_err());
        assert!(checked_length(100).is_ok());
    }

    async fn read_frame(io: &mut tokio::io::DuplexStream) -> Option<Value> {
        let mut header = [0; 4];
        io.read_exact(&mut header).await.ok()?;
        let mut body = vec![0; u32::from_le_bytes(header) as usize];
        io.read_exact(&mut body).await.ok()?;
        serde_json::from_slice(&body).ok()
    }

    async fn write_frame(io: &mut tokio::io::DuplexStream, value: Value) {
        let body = serde_json::to_vec(&value).unwrap();
        io.write_all(&(body.len() as u32).to_le_bytes())
            .await
            .unwrap();
        io.write_all(&body).await.unwrap();
    }

    fn subscription_owner(
        value: Value,
        delay: Duration,
        cancel_after_follow: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> (DesktopSession, tokio::task::JoinHandle<Vec<String>>) {
        let (client, mut server) = tokio::io::duplex(65536);
        let session = DesktopSession {
            io: Box::new(client),
            client_id: "client".into(),
            receive_buffer: Vec::new(),
            followed: HashSet::new(),
            states: HashMap::new(),
            resync_needed: HashSet::new(),
            mutation_attempts: HashSet::new(),
            reopened_baselines: HashMap::new(),
            compatible: true,
            broken: false,
        };
        let owner = tokio::spawn(async move {
            let loaded_at = tokio::time::Instant::now() + delay;
            let mut methods = Vec::new();
            let mut revision = 0;
            while let Some(message) = read_frame(&mut server).await {
                let method = message["method"].as_str().unwrap_or("");
                methods.push(method.to_owned());
                if method == "thread-stream-following-changed"
                    && message["params"]["following"] == true
                {
                    if let Some(cancel) = &cancel_after_follow {
                        cancel.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                    if tokio::time::Instant::now() >= loaded_at {
                        revision += 1;
                        write_frame(
                            &mut server,
                            json!({
                                "type":"broadcast", "sourceClientId":"new-owner",
                                "method":"thread-stream-state-changed", "version":11,
                                "params":{"hostId":"local","conversationId":THREAD,
                                    "change":{"type":"snapshot","revision":revision,
                                        "conversationState":value}}
                            }),
                        )
                        .await;
                    }
                }
            }
            methods
        });
        (session, owner)
    }

    #[tokio::test]
    async fn reopened_original_chat_can_load_after_the_normal_snapshot_timeout() {
        let delay = SNAPSHOT_TIMEOUT + Duration::from_millis(200);
        let (mut session, owner) = subscription_owner(state("interrupted"), delay, None);
        let started = tokio::time::Instant::now();
        let observed = session
            .wait_for_owned_paused_chat(&paused(), || true)
            .await
            .unwrap();
        assert!(started.elapsed() >= delay);
        assert!(observed.matches_paused(&paused()));
        assert_eq!(observed.owner, "new-owner");
        assert!(session.mutation_attempts.is_empty());
        drop(session);
        let methods = owner.await.unwrap();
        assert!(
            methods.len() >= 8,
            "owner needs subscriptions after loading"
        );
        assert!(methods
            .iter()
            .all(|method| method == "thread-stream-following-changed"));
    }

    #[tokio::test]
    async fn cancelled_owner_wait_sends_no_further_subscription_or_turn_mutation() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let allowed = Arc::new(AtomicBool::new(true));
        let (mut session, owner) = subscription_owner(
            state("interrupted"),
            REOPENED_CHAT_TIMEOUT,
            Some(allowed.clone()),
        );
        let started = tokio::time::Instant::now();
        let error = session
            .wait_for_owned_paused_chat(&paused(), || allowed.load(Ordering::SeqCst))
            .await
            .unwrap_err();
        assert_eq!(error.kind, SessionErrorKind::Cancelled);
        assert!(!error.mutation_may_have_been_sent);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(session.mutation_attempts.is_empty());
        drop(session);
        assert_eq!(
            owner.await.unwrap(),
            vec![
                "thread-stream-following-changed",
                "thread-stream-following-changed"
            ]
        );

        let (mut session, owner) = subscription_owner(state("interrupted"), Duration::ZERO, None);
        assert_eq!(
            session
                .wait_for_owned_paused_chat(&paused(), || false)
                .await
                .unwrap_err()
                .kind,
            SessionErrorKind::Cancelled
        );
        drop(session);
        assert!(owner.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn owner_wait_rejects_changed_turn_model_permissions_approval_or_completion() {
        let mut variants = Vec::new();
        let mut changed = state("interrupted");
        changed["turns"][0]["turnId"] = json!("user-started-a-new-turn");
        variants.push(changed);
        let mut changed = state("interrupted");
        changed["latestModel"] = json!("user-selected-model");
        variants.push(changed);
        let mut changed = state("interrupted");
        changed["currentPermissions"] = json!({"approvalPolicy":"never"});
        variants.push(changed);
        let mut changed = state("interrupted");
        changed["requests"] = json!([{"method":"item/commandExecution/requestApproval"}]);
        variants.push(changed);
        variants.push(state("completed"));
        variants.push(state("inProgress"));
        for changed in variants {
            let (mut session, owner) = subscription_owner(changed, Duration::ZERO, None);
            let error = session
                .wait_for_owned_paused_chat(&paused(), || true)
                .await
                .unwrap_err();
            assert_eq!(error.kind, SessionErrorKind::StateChanged);
            assert!(!error.mutation_may_have_been_sent);
            assert!(session.mutation_attempts.is_empty());
            drop(session);
            assert!(owner
                .await
                .unwrap()
                .iter()
                .all(|method| method == "thread-stream-following-changed"));
        }
    }

    fn saved_paused() -> PausedTask {
        let mut original = state("interrupted");
        original["currentPermissions"]["approvalPolicy"] = json!("never");
        original["currentPermissions"]["sandboxPolicy"] = json!({"type":"dangerFullAccess"});
        original["latestThreadSettings"]["approvalPolicy"] = json!("never");
        original["latestThreadSettings"]["sandboxPolicy"] = json!({"type":"dangerFullAccess"});
        let task = summarize(&project_at(&[], &original).unwrap(), "original-owner").unwrap();
        PausedTask {
            thread_id: THREAD.into(),
            turn_id: "original-turn".into(),
            context: task.context,
            pause_operation_id: OP.into(),
            confirmed_by_cc_switch: true,
            saved_settings: Some(capture_saved_settings(&original).unwrap()),
        }
    }

    fn restoring_owner(
        behavior: &str,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> (DesktopSession, tokio::task::JoinHandle<(usize, usize)>) {
        let (client, mut server) = tokio::io::duplex(65536);
        let mut session = fake_session();
        session.io = Box::new(client);
        session.followed.clear();
        let behavior = behavior.to_owned();
        let task = tokio::spawn(async move {
            let mut current = state("interrupted");
            if behavior == "null-plugins" || behavior == "changed-plugins-on-start" {
                current["latestThreadSettings"]["disabledPluginIds"] = json!([]);
            }
            let mut revision = 0;
            let mut follows = 0;
            let settings_count = 0;
            let mut starts = 0;
            while let Some(message) = read_frame(&mut server).await {
                let method = message["method"].as_str().unwrap_or("");
                match method {
                    "thread-stream-following-changed" if message["params"]["following"] == true => {
                        follows += 1;
                        if behavior == "changed-baseline" && follows == 2 {
                            current["latestReasoningEffort"] = json!("low");
                            current["latestThreadSettings"]["effort"] = json!("low");
                        }
                    }
                    "thread-owner-discovery" => {
                        if let Some(cancel) = &cancel {
                            cancel.store(false, std::sync::atomic::Ordering::SeqCst);
                        }
                        write_frame(
                            &mut server,
                            json!({"type":"response","requestId":message["requestId"],
                            "resultType":"success","handledByClientId":"new-owner","result":{}}),
                        )
                        .await;
                        continue;
                    }
                    "thread-follower-start-turn" => {
                        starts += 1;
                        let request = &message["params"]["turnStart"]["request"];
                        if behavior == "optimistic-start" {
                            current["turns"][0]["turnId"] = Value::Null;
                            current["turns"][0]["status"] = json!("inProgress");
                            current["turns"][0]["params"]["clientUserMessageId"] =
                                request["clientUserMessageId"].clone();
                            current["unconfirmedTurnSubmissions"] =
                                json!([{"clientUserMessageId":OP}]);
                            revision += 1;
                            write_frame(&mut server, json!({"type":"broadcast","sourceClientId":"new-owner",
                                "method":"thread-stream-state-changed","version":11,"params":{"hostId":"local",
                                "conversationId":THREAD,"change":{"type":"snapshot","revision":revision,"conversationState":current}}})).await;
                            write_frame(
                                &mut server,
                                json!({"type":"response","requestId":message["requestId"],
                                "resultType":"success","result":{"turn":{"id":"restored-turn"}}}),
                            )
                            .await;
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            current["turns"][0]["turnId"] = json!("restored-turn");
                            revision += 1;
                            write_frame(&mut server, json!({"type":"broadcast","sourceClientId":"new-owner",
                                "method":"thread-stream-state-changed","version":11,"params":{"hostId":"local",
                                "conversationId":THREAD,"change":{"type":"snapshot","revision":revision,"conversationState":current}}})).await;
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            current["unconfirmedTurnSubmissions"] = json!([]);
                        }
                        for key in [
                            "model",
                            "effort",
                            "collaborationMode",
                            "approvalPolicy",
                            "approvalsReviewer",
                            "sandboxPolicy",
                            "permissions",
                            "cwd",
                        ] {
                            if let Some(value) = request.get(key) {
                                current["latestThreadSettings"][key] = value.clone();
                            }
                        }
                        current["latestReasoningEffort"] =
                            if request["collaborationMode"].is_object() {
                                Value::Null
                            } else {
                                request["effort"].clone()
                            };
                        if let Some(mode) = request.get("collaborationMode") {
                            current["latestCollaborationMode"] = mode.clone();
                        }
                        assert_eq!(
                            message["params"]["turnStart"]["request"]["approvalPolicy"],
                            "never"
                        );
                        assert_eq!(
                            message["params"]["turnStart"]["request"]["sandboxPolicy"],
                            json!({"type":"dangerFullAccess"})
                        );
                        assert_eq!(
                            message["params"]["turnStart"]["context"]["usePermissionSelection"],
                            false
                        );
                        if behavior != "wrong-start-policy" {
                            current["currentPermissions"] = serde_json::to_value(
                                saved_paused().saved_settings.unwrap().current_permissions,
                            )
                            .unwrap();
                        }
                        if behavior == "changed-plugins-on-start" {
                            current["latestThreadSettings"]["disabledPluginIds"] =
                                json!(["user-selected-disabled-plugin"]);
                        }
                        current["turns"][0]["turnId"] = json!("restored-turn");
                        current["turns"][0]["status"] = json!("inProgress");
                        current["turns"][0]["params"]["clientUserMessageId"] = message["params"]
                            ["turnStart"]["request"]["clientUserMessageId"]
                            .clone();
                        current["threadRuntimeStatus"]["type"] = json!("active");
                        if behavior != "optimistic-start" {
                            write_frame(
                                &mut server,
                                json!({"type":"response","requestId":message["requestId"],
                                "resultType":"success","result":{"turn":{"id":"restored-turn"}}}),
                            )
                            .await;
                        }
                    }
                    "thread-stream-following-changed" => continue,
                    _ => panic!("unexpected method {method}"),
                }
                revision += 1;
                write_frame(
                    &mut server,
                    json!({"type":"broadcast","sourceClientId":"new-owner",
                    "method":"thread-stream-state-changed","version":11,"params":{"hostId":"local",
                        "conversationId":THREAD,"change":{"type":"snapshot","revision":revision,
                            "conversationState":current}}}),
                )
                .await;
            }
            (settings_count, starts)
        });
        (session, task)
    }

    #[tokio::test]
    async fn original_helper_single_start_preserves_policy_without_settings_rpc() {
        let saved = saved_paused();
        let (mut session, owner) = restoring_owner("normal", None);
        session.begin_follow(THREAD).await.unwrap();
        session
            .wait_for_reopened_paused_chat(&saved, || true)
            .await
            .unwrap();
        let confirmation = session
            .resume_and_confirm(&saved, OP, || true)
            .await
            .unwrap();
        assert_eq!(confirmation.turn_id, "restored-turn");
        assert!(!confirmation.already_observed);
        assert!(
            session
                .resume_and_confirm(&saved, OP, || true)
                .await
                .unwrap()
                .already_observed
        );
        drop(session);
        assert_eq!(owner.await.unwrap(), (0, 1));
    }

    #[tokio::test]
    async fn single_resume_waits_for_own_optimistic_turn_and_later_effective_settings() {
        let saved = saved_paused();
        let (mut session, owner) = restoring_owner("optimistic-start", None);
        session
            .wait_for_reopened_paused_chat(&saved, || true)
            .await
            .unwrap();
        let confirmation = session
            .resume_and_confirm(&saved, OP, || true)
            .await
            .unwrap();
        assert_eq!(confirmation.turn_id, "restored-turn");
        drop(session);
        assert_eq!(owner.await.unwrap(), (0, 1));
    }

    #[test]
    fn effective_permissions_ignore_unused_roots_and_profile_inheritance_metadata() {
        let saved = saved_paused().saved_settings.unwrap();
        let mut actual = saved.current_permissions.clone();
        actual
            .as_object_mut()
            .unwrap()
            .remove("runtimeWorkspaceRoots");
        assert!(effective_permissions_match(&actual, &saved));
        actual["approvalPolicy"] = json!("on-request");
        assert!(!effective_permissions_match(&actual, &saved));

        let mut saved = saved;
        saved.thread_settings["activePermissionProfile"] =
            json!({"id":"original","extends":"base"});
        actual = saved.current_permissions.clone();
        actual["activePermissionProfile"] = json!({"id":"original","extends":null});
        actual["runtimeWorkspaceRoots"] = json!(["C:\\project\\"]);
        assert!(effective_permissions_match(&actual, &saved));
        actual["activePermissionProfile"]["id"] = json!("other");
        assert!(!effective_permissions_match(&actual, &saved));
        actual["activePermissionProfile"]["id"] = json!("original");
        actual["runtimeWorkspaceRoots"] = json!(["C:/other"]);
        assert!(!effective_permissions_match(&actual, &saved));
    }

    #[tokio::test]
    async fn single_resume_rejects_user_baseline_change_and_cancellation() {
        let saved = saved_paused();
        let (mut session, owner) = restoring_owner("changed-baseline", None);
        session
            .wait_for_reopened_paused_chat(&saved, || true)
            .await
            .unwrap();
        assert_eq!(
            session
                .resume_and_confirm(&saved, OP, || true)
                .await
                .unwrap_err()
                .kind,
            SessionErrorKind::StateChanged
        );
        drop(session);
        assert_eq!(owner.await.unwrap(), (0, 0));

        use std::sync::atomic::{AtomicBool, Ordering};
        let allowed = std::sync::Arc::new(AtomicBool::new(true));
        let (mut session, owner) = restoring_owner("normal", Some(allowed.clone()));
        session
            .wait_for_reopened_paused_chat(&saved, || true)
            .await
            .unwrap();
        assert_eq!(
            session
                .resume_and_confirm(&saved, OP, || allowed.load(Ordering::SeqCst))
                .await
                .unwrap_err()
                .kind,
            SessionErrorKind::Cancelled
        );
        drop(session);
        assert_eq!(owner.await.unwrap(), (0, 0));
    }

    #[tokio::test]
    async fn single_resumed_turn_with_wrong_permissions_is_not_confirmed_or_replayed() {
        let saved = saved_paused();
        let (mut session, owner) = restoring_owner("wrong-start-policy", None);
        session
            .wait_for_reopened_paused_chat(&saved, || true)
            .await
            .unwrap();
        assert_eq!(
            session
                .resume_and_confirm(&saved, OP, || true)
                .await
                .unwrap_err()
                .kind,
            SessionErrorKind::OutcomeUnknown
        );
        assert!(session.reconcile_resume(&saved, OP).await.is_err());
        drop(session);
        assert_eq!(owner.await.unwrap(), (0, 1));
    }

    #[test]
    fn saved_setting_capture_is_bounded_whitelisted_and_never_contains_history_or_auth() {
        let captured = capture_saved_settings(&state("inProgress")).unwrap();
        let encoded = serde_json::to_string(&captured).unwrap();
        assert!(!encoded.contains("private"));
        assert!(!encoded.contains("clientUserMessageId"));
        let mut unknown = state("inProgress");
        unknown["latestThreadSettings"]["unsupportedSetting"] = json!({"private":"unprinted"});
        let error = capture_saved_settings(&unknown).unwrap_err();
        assert!(error
            .message
            .contains("latestThreadSettings.unsupportedSetting（object）"));
        assert!(!error.message.contains("unprinted"));
        unknown["latestThreadSettings"] = json!({"effort":"x".repeat(40000)});
        assert!(capture_saved_settings(&unknown).is_err());
        let mut incomplete = state("inProgress");
        incomplete["currentPermissions"]
            .as_object_mut()
            .unwrap()
            .remove("sandboxPolicy");
        assert!(capture_saved_settings(&incomplete).is_err());
        let granular = json!({"granular":{"sandbox_approval":false,"rules":false,
            "skill_approval":false,"request_permissions":true,"mcp_elicitations":true}});
        let mut supported = state("inProgress");
        supported["currentPermissions"]["approvalPolicy"] = granular.clone();
        supported["latestThreadSettings"]["approvalPolicy"] = granular;
        assert!(capture_saved_settings(&supported).is_ok());
    }

    fn mock_owner(
        initial_status: &str,
        acknowledge_pause: bool,
    ) -> (DesktopSession, tokio::task::JoinHandle<usize>) {
        let (client, mut server) = tokio::io::duplex(65536);
        let session = DesktopSession {
            io: Box::new(client),
            client_id: "client".into(),
            receive_buffer: Vec::new(),
            followed: HashSet::new(),
            states: HashMap::new(),
            resync_needed: HashSet::new(),
            mutation_attempts: HashSet::new(),
            reopened_baselines: HashMap::new(),
            compatible: true,
            broken: false,
        };
        let mut current = state(initial_status);
        let handle = tokio::spawn(async move {
            let mut start_count = 0;
            let mut revision = 0;
            while let Some(request) = read_frame(&mut server).await {
                let method = request["method"].as_str().unwrap_or("");
                if method == "thread-stream-following-changed"
                    && request["params"]["following"] == true
                {
                    revision += 1;
                    write_frame(&mut server, json!({"type":"broadcast","sourceClientId":"owner","method":"thread-stream-state-changed","version":11,"params":{"hostId":"local","conversationId":THREAD,"change":{"type":"snapshot","revision":revision,"conversationState":current}}})).await;
                } else if method == "thread-owner-discovery" {
                    write_frame(&mut server,json!({"type":"response","requestId":request["requestId"],"resultType":"success","handledByClientId":"owner","result":{}})).await;
                } else if method == "thread-follower-interrupt-turn" {
                    assert_eq!(request["version"], 4);
                    assert_eq!(request["targetClientId"], "owner");
                    assert_eq!(request["params"]["expectedTurnId"], "original-turn");
                    current["turns"][0]["status"] = json!("interrupted");
                    current["threadRuntimeStatus"]["type"] = json!("idle");
                    revision += 1;
                    write_frame(&mut server,json!({"type":"broadcast","sourceClientId":"owner","method":"thread-stream-state-changed","version":11,"params":{"hostId":"local","conversationId":THREAD,"change":{"type":"snapshot","revision":revision,"conversationState":current}}})).await;
                    let result = if acknowledge_pause {
                        json!({"interruptedTurnId":"original-turn"})
                    } else {
                        json!({})
                    };
                    write_frame(&mut server,json!({"type":"response","requestId":request["requestId"],"resultType":"success","result":result})).await;
                } else if method == "thread-follower-start-turn" {
                    start_count += 1;
                    assert_eq!(request["version"], 2);
                    assert_eq!(request["targetClientId"], "owner");
                    assert_eq!(
                        request["params"]["turnStart"]["context"]["inheritThreadSettings"],
                        true
                    );
                    assert_eq!(
                        request["params"]["turnStart"]["context"]["usePermissionSelection"],
                        false
                    );
                    assert_eq!(
                        request["params"]["turnStart"]["context"]["useAppServerPermissionDefault"],
                        false
                    );
                    let turn_request = &request["params"]["turnStart"]["request"];
                    current["latestCollaborationMode"] = turn_request["collaborationMode"].clone();
                    current["latestReasoningEffort"] =
                        if turn_request["collaborationMode"].is_object() {
                            Value::Null
                        } else {
                            turn_request["effort"].clone()
                        };
                    current["turns"][0]["status"] = json!("inProgress");
                    current["turns"][0]["turnId"] = json!("restored-turn");
                    current["turns"][0]["params"]["clientUserMessageId"] =
                        request["params"]["turnStart"]["request"]["clientUserMessageId"].clone();
                    current["threadRuntimeStatus"]["type"] = json!("active");
                    revision += 1;
                    write_frame(&mut server,json!({"type":"broadcast","sourceClientId":"owner","method":"thread-stream-state-changed","version":11,"params":{"hostId":"local","conversationId":THREAD,"change":{"type":"snapshot","revision":revision,"conversationState":current}}})).await;
                    write_frame(&mut server,json!({"type":"response","requestId":request["requestId"],"resultType":"success","result":{"turn":{"id":"restored-turn"}}})).await;
                }
            }
            start_count
        });
        (session, handle)
    }

    #[tokio::test]
    async fn actual_frames_confirm_owned_pause_and_restore_exact_chat_once() {
        let (mut session, owner) = mock_owner("inProgress", true);
        let expected = summarize(&projected("inProgress"), "owner").unwrap();
        let pause = session
            .pause_and_confirm(&expected, OP, || true)
            .await
            .unwrap();
        let resume = session
            .resume_and_confirm(&pause, OP, || true)
            .await
            .unwrap();
        assert_eq!(resume.turn_id, "restored-turn");
        assert!(!resume.already_observed);
        let reconciled = session
            .resume_and_confirm(&pause, OP, || true)
            .await
            .unwrap();
        assert!(reconciled.already_observed);
        drop(session);
        assert_eq!(owner.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn interrupted_state_without_exact_ack_never_claims_pause_ownership() {
        let (mut session, owner) = mock_owner("inProgress", false);
        let expected = summarize(&projected("inProgress"), "owner").unwrap();
        let error = session
            .pause_and_confirm(&expected, OP, || true)
            .await
            .unwrap_err();
        assert_eq!(error.kind, SessionErrorKind::OutcomeUnknown);
        assert!(error.mutation_may_have_been_sent);
        drop(session);
        assert_eq!(owner.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn cancelled_guard_does_not_send_interrupt_or_resume() {
        let (mut session, owner) = mock_owner("inProgress", true);
        let expected = summarize(&projected("inProgress"), "owner").unwrap();
        assert_eq!(
            session
                .pause_and_confirm(&expected, OP, || false)
                .await
                .unwrap_err()
                .kind,
            SessionErrorKind::Cancelled
        );
        assert_eq!(
            session
                .resume_and_confirm(&paused(), OP, || false)
                .await
                .unwrap_err()
                .kind,
            SessionErrorKind::Cancelled
        );
        drop(session);
        assert_eq!(owner.await.unwrap(), 0);
    }

    #[test]
    fn metadata_discovery_includes_children_and_ephemeral_for_safety_without_reading_text() {
        let temp = tempfile::tempdir().unwrap();
        let connection = rusqlite::Connection::open(temp.path().join("state_5.sqlite")).unwrap();
        connection.execute_batch("CREATE TABLE threads(id TEXT, archived INTEGER,agent_path TEXT,thread_source TEXT,originator TEXT,ephemeral INTEGER,updated_at_ms INTEGER,private_transcript TEXT);").unwrap();
        for (id, archived, path, source, ephemeral) in [
            (THREAD, 0, "/root", "cli", 0),
            ("33333333-3333-4333-8333-333333333333", 1, "/root", "cli", 0),
            (
                "44444444-4444-4444-8444-444444444444",
                0,
                "/root/child",
                "subagent",
                0,
            ),
            ("55555555-5555-4555-8555-555555555555", 0, "/root", "cli", 1),
        ] {
            connection
                .execute(
                    "INSERT INTO threads VALUES(?1,?2,?3,?4,NULL,?5,1,'never read this body')",
                    rusqlite::params![id, archived, path, source, ephemeral],
                )
                .unwrap();
        }
        assert_eq!(
            discover_thread_ids(temp.path()).unwrap(),
            vec![
                THREAD.to_string(),
                "33333333-3333-4333-8333-333333333333".into(),
                "44444444-4444-4444-8444-444444444444".into(),
                "55555555-5555-4555-8555-555555555555".into(),
            ]
        );
        assert!(discover_thread_ids(&temp.path().join("missing")).is_err());
    }
}
