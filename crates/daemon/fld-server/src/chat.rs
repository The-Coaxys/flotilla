//! Authoritative daemon chat service (M4).
//!
//! One `fld_agent::Adapter` per live session. A supervisor task drains the
//! adapter's event receiver, assigns the durable per-session sequence, commits
//! the event with its status/pending/inbox effects, and only then publishes
//! `ChatEventAppended`. A client that reconnects pages from its last durable
//! sequence; a daemon restart clears unanswerable pending decisions and
//! reattaches lazily with the stored resume target.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use fl_model::WorktreeId;
use fl_model::chat::{
    ApprovalDecision, BoundedText, ChatEvent, ChatSession, EventRecord, InboxKind, PermissionMode,
    QuestionResponse, ResumeTarget, SessionConfig, SessionState, StatusSource, TurnOutcome,
    CHAT_SCHEMA_VERSION, MAX_CHAT_TEXT,
};
use fl_model::routing::GitTask;
use fl_model::workspace::DaemonEvent;
use fl_proto::{RpcError, RpcErrorCode};
use fld_agent::{Adapter, AdapterError, ProviderRegistry};
use fld_store::StoreError;
use fld_store::database::{
    ChatEventEffects, NewInboxItem, PendingKind, PendingRequest,
};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Semaphore, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::events::EventBus;
use crate::terminal::{TerminalService, bounded, store_rpc, stopped_rpc};

/// Live provider processes the daemon will supervise at once.
pub(crate) const MAX_LIVE_CHATS: usize = 8;
const START_TIMEOUT: Duration = Duration::from_secs(30);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const STRUCTURED_TIMEOUT: Duration = Duration::from_secs(180);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const NOTICE_CAPACITY: usize = 256;
/// Concurrent routed one-shot provider calls.
const MAX_STRUCTURED_CALLS: usize = 2;
/// Provider-native identities are bounded by the wire contract.
const MAX_CHAT_ID_BYTES: usize = fl_proto::validation::MAX_CHAT_ID_BYTES;
/// Failed session text and inbox titles share this bound.
const MAX_CHAT_FAILURE_BYTES: usize = fl_proto::validation::MAX_CHAT_INBOX_BYTES;
const MAX_CHAT_MODEL_BYTES: usize = fl_proto::validation::MAX_CHAT_MODEL_BYTES;
const MAX_CHAT_PROVIDER_BYTES: usize = fl_proto::validation::MAX_CHAT_PROVIDER_BYTES;
const MAX_CHAT_PATH_BYTES: usize = fl_proto::validation::MAX_PATH_BYTES;
const MAX_CHAT_OPTIONS: usize = 8;
const MAX_CHAT_QUESTIONS: usize = 64;
const MAX_CHAT_QUESTION_OPTIONS: usize = 64;
const MAX_CHAT_SUBAGENT_EVENTS: usize = 256;
const MAX_CHAT_EVENT_DEPTH: usize = 8;
const TRUNCATION_MARKER: &str = " …[truncated]";

pub(crate) struct ChatService {
    terminals: Arc<TerminalService>,
    events: Arc<EventBus>,
    registry: ProviderRegistry,
    live: Mutex<HashMap<uuid::Uuid, LiveChat>>,
    /// (provider, native session) owned by exactly one live session. A
    /// synchronous lock keeps claim release cancellation-safe.
    claims: std::sync::Mutex<HashMap<(String, String), uuid::Uuid>>,
    /// Serializes launches so a cold restore cannot race a user start.
    launch: Mutex<()>,
    /// Bounds concurrent routed one-shot provider calls.
    structured: Semaphore,
    notices: broadcast::Sender<ChatNotice>,
    shutdown_task: Mutex<Option<JoinHandle<()>>>,
}

/// NUL-free, bounded text with an explicit truncation marker. Ingest uses it
/// so no adapter event can fail wire validation after it was accepted.
pub(crate) fn bounded_with_marker(value: &str, max: usize) -> String {
    let value = value.replace('\0', "");
    if value.len() <= max {
        return value;
    }
    let limit = max.saturating_sub(TRUNCATION_MARKER.len());
    let mut end = limit.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = String::with_capacity(end + TRUNCATION_MARKER.len());
    bounded.push_str(&value[..end]);
    bounded.push_str(TRUNCATION_MARKER);
    bounded
}

struct LiveChat {
    adapter: Arc<Adapter>,
    drain: Option<JoinHandle<()>>,
}

/// Releases a provider claim even when the launching future is cancelled.
struct ClaimGuard<'a> {
    service: &'a ChatService,
    id: uuid::Uuid,
}
impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        self.service.release_claims_for(self.id);
    }
}

fn sanitize_bounded(value: &mut BoundedText, max: usize) {
    let before = value.text.len();
    let text = bounded_with_marker(&value.text, max);
    if text.len() != before || text != value.text {
        value.truncated = true;
    }
    value.text = text;
}

/// Apply the wire caps to one event tree before it can be stored or published.
/// Counts are truncated and text is made NUL-free, so an adapter bug that
/// produces an oversized or poisonous payload cannot kill client connections.
fn sanitize_event(event: &mut ChatEvent, depth: usize) {
    match event {
        ChatEvent::SessionReady(info) => {
            info.resume.provider_id =
                bounded_with_marker(&info.resume.provider_id, MAX_CHAT_PROVIDER_BYTES);
            info.resume.native_id =
                bounded_with_marker(&info.resume.native_id, MAX_CHAT_ID_BYTES);
            if info.resume.cwd.to_str().is_none() {
                info.resume.cwd = PathBuf::from(info.resume.cwd.to_string_lossy().as_ref());
            }
            let cwd = bounded_with_marker(&info.resume.cwd.to_string_lossy(), MAX_CHAT_PATH_BYTES);
            info.resume.cwd = PathBuf::from(cwd);
            if let Some(model) = &mut info.model {
                *model = bounded_with_marker(model, MAX_CHAT_MODEL_BYTES);
            }
        }
        ChatEvent::StateChanged(SessionState::Failed { message }) => {
            *message = bounded_with_marker(message, MAX_CHAT_FAILURE_BYTES);
        }
        ChatEvent::StateChanged(_) => {}
        ChatEvent::UserMessage { id, text } | ChatEvent::AssistantText { id, text, .. } => {
            *id = bounded_with_marker(id, MAX_CHAT_TEXT);
            *text = bounded_with_marker(text, MAX_CHAT_TEXT);
        }
        ChatEvent::StructuredOutput { json } => {
            *json = bounded_with_marker(json, MAX_CHAT_TEXT);
        }
        ChatEvent::Thinking { id, delta } => {
            *id = bounded_with_marker(id, MAX_CHAT_TEXT);
            *delta = bounded_with_marker(delta, MAX_CHAT_TEXT);
        }
        ChatEvent::ToolCall(call) => {
            call.id = bounded_with_marker(&call.id, MAX_CHAT_TEXT);
            call.title = bounded_with_marker(&call.title, MAX_CHAT_TEXT);
            sanitize_bounded(&mut call.input_summary, MAX_CHAT_TEXT);
            if let Some(output) = &mut call.output {
                sanitize_bounded(output, MAX_CHAT_TEXT);
            }
            if let Some(diff) = &mut call.diff {
                sanitize_bounded(diff, MAX_CHAT_TEXT);
            }
        }
        ChatEvent::ToolOutput { id, delta } => {
            *id = bounded_with_marker(id, MAX_CHAT_TEXT);
            sanitize_bounded(delta, MAX_CHAT_TEXT);
        }
        ChatEvent::FileEdit {
            path,
            unified_diff,
        } => {
            let rendered = path.to_string_lossy();
            if rendered.contains('\0') || rendered.len() > MAX_CHAT_PATH_BYTES {
                *path = PathBuf::from(bounded_with_marker(&rendered, MAX_CHAT_PATH_BYTES));
            }
            sanitize_bounded(unified_diff, MAX_CHAT_TEXT);
        }
        ChatEvent::ApprovalRequest {
            id,
            tool,
            details,
            options,
        } => {
            *id = bounded_with_marker(id, MAX_CHAT_TEXT);
            *tool = bounded_with_marker(tool, MAX_CHAT_TEXT);
            sanitize_bounded(details, MAX_CHAT_TEXT);
            options.truncate(MAX_CHAT_OPTIONS);
        }
        ChatEvent::QuestionRequest {
            id,
            questions,
            schema_json,
        } => {
            *id = bounded_with_marker(id, MAX_CHAT_TEXT);
            if let Some(schema) = schema_json {
                *schema = bounded_with_marker(schema, MAX_CHAT_TEXT);
            }
            questions.truncate(MAX_CHAT_QUESTIONS);
            for question in questions {
                question.id = bounded_with_marker(&question.id, MAX_CHAT_TEXT);
                question.text = bounded_with_marker(&question.text, MAX_CHAT_TEXT);
                question.options.truncate(MAX_CHAT_QUESTION_OPTIONS);
                for option in &mut question.options {
                    option.label = bounded_with_marker(&option.label, MAX_CHAT_TEXT);
                    option.description =
                        bounded_with_marker(&option.description, MAX_CHAT_TEXT);
                }
            }
        }
        ChatEvent::RequestResolved { id } => {
            *id = bounded_with_marker(id, MAX_CHAT_TEXT);
        }
        ChatEvent::Subagent {
            parent_tool_id,
            events,
        } => {
            *parent_tool_id = bounded_with_marker(parent_tool_id, MAX_CHAT_TEXT);
            if depth >= MAX_CHAT_EVENT_DEPTH {
                events.clear();
            } else {
                events.truncate(MAX_CHAT_SUBAGENT_EVENTS);
                for child in events {
                    sanitize_event(child, depth + 1);
                }
            }
        }
        ChatEvent::TurnEnd {
            model, usage, ..
        } => {
            if let Some(model) = model {
                *model = bounded_with_marker(model, MAX_CHAT_MODEL_BYTES);
            }
            if let Some(cost) = &mut usage.cost_usd {
                *cost = bounded_with_marker(cost, 64);
            }
        }
        ChatEvent::Error { code, message, .. } => {
            *code = bounded_with_marker(code, MAX_CHAT_TEXT);
            *message = bounded_with_marker(message, MAX_CHAT_TEXT);
        }
        ChatEvent::Diagnostic { stream, output } => {
            *stream = bounded_with_marker(stream, MAX_CHAT_TEXT);
            sanitize_bounded(output, MAX_CHAT_TEXT);
        }
        ChatEvent::ProcessExited { .. } => {}
    }
}

fn sanitize_record(record: &mut EventRecord) {
    record.schema_version = CHAT_SCHEMA_VERSION;
    record.native_session_id = record
        .native_session_id
        .as_deref()
        .map(|value| bounded_with_marker(value, MAX_CHAT_ID_BYTES))
        .filter(|value| !value.is_empty());
    record.turn_id = record
        .turn_id
        .as_deref()
        .map(|value| bounded_with_marker(value, MAX_CHAT_ID_BYTES))
        .filter(|value| !value.is_empty());
    sanitize_event(&mut record.event, 0);
}

/// In-process notifications for the routed Git dispatcher. Durable state and
/// wire events remain the source of truth; this is a supervised turn signal.
#[derive(Debug, Clone)]
pub(crate) enum ChatNotice {
    Event { session: uuid::Uuid, record: EventRecord },
    Session(ChatSession),
    Inbox,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn rpc(code: RpcErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: bounded(message.into()),
    }
}

fn invalid(message: impl Into<String>) -> RpcError {
    rpc(RpcErrorCode::InvalidArgument, message)
}

fn internal(message: impl Into<String>) -> RpcError {
    rpc(RpcErrorCode::Internal, message)
}

fn adapter_rpc(error: AdapterError) -> RpcError {
    let code = match &error {
        AdapterError::Unsupported(_) => RpcErrorCode::Unsupported,
        AdapterError::InvalidConfig(_)
        | AdapterError::InvalidResume(_)
        | AdapterError::Busy
        | AdapterError::NoActiveTurn
        | AdapterError::UnknownRequest(_)
        | AdapterError::Rejected(_) => RpcErrorCode::InvalidArgument,
        _ => RpcErrorCode::Internal,
    };
    rpc(code, error.to_string())
}

/// `session:kind:hash(key)`; global inbox dedupe identity. Hashing keeps the
/// key bounded even when a provider request ID is large.
fn inbox_dedupe(session: uuid::Uuid, kind: &str, key: &str) -> String {
    format!(
        "{session}:{kind}:{:x}",
        Sha256::digest(key.as_bytes())
    )
}

impl ChatService {
    pub(crate) async fn start(
        terminals: Arc<TerminalService>,
        events: Arc<EventBus>,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, RpcError> {
        Self::start_with_registry(terminals, events, cancel, ProviderRegistry::default()).await
    }

    async fn start_with_registry(
        terminals: Arc<TerminalService>,
        events: Arc<EventBus>,
        cancel: CancellationToken,
        registry: ProviderRegistry,
    ) -> Result<Arc<Self>, RpcError> {
        let (notices, _) = broadcast::channel(NOTICE_CAPACITY);
        let service = Arc::new(Self {
            terminals,
            events,
            registry,
            live: Mutex::new(HashMap::new()),
            claims: std::sync::Mutex::new(HashMap::new()),
            launch: Mutex::new(()),
            structured: Semaphore::new(MAX_STRUCTURED_CALLS),
            notices,
            shutdown_task: Mutex::new(None),
        });
        // No adapter survives a daemon restart: pending decisions are gone and
        // sessions read idle until an explicit reconnect restores them.
        let sessions = service
            .store(|store| store.database.reset_chats_after_restart(now_ms()))
            .await?;
        for session in sessions {
            service.publish_session(session);
        }
        let weak = Arc::downgrade(&service);
        let watcher = tokio::spawn(async move {
            cancel.cancelled().await;
            if let Some(service) = Weak::upgrade(&weak) {
                service.shutdown().await;
            }
        });
        service.shutdown_task.lock().await.replace(watcher);
        Ok(service)
    }

    async fn store<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut fld_store::Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, RpcError> {
        self.terminals
            .shared
            .store
            .call(job)
            .await
            .map_err(|_| stopped_rpc())?
            .map_err(store_rpc)
    }

    fn publish(&self, event: DaemonEvent) {
        self.events.publish(event);
    }

    fn publish_session(&self, session: ChatSession) {
        self.publish(DaemonEvent::ChatSessionChanged(session.clone()));
        let _ = self.notices.send(ChatNotice::Session(session));
    }

    fn publish_inbox(&self) {
        self.publish(DaemonEvent::InboxChanged);
        let _ = self.notices.send(ChatNotice::Inbox);
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ChatNotice> {
        self.notices.subscribe()
    }

    // --- Queries -----------------------------------------------------------

    pub(crate) async fn list_sessions(
        &self,
        worktree: WorktreeId,
    ) -> Result<Vec<ChatSession>, RpcError> {
        self.store(move |store| store.database.chat_sessions(worktree))
            .await
    }

    pub(crate) async fn event_page(
        &self,
        session: uuid::Uuid,
        from: u64,
        limit: u16,
    ) -> Result<fl_model::chat::ChatEventPage, RpcError> {
        self.store(move |store| store.database.chat_event_page(session, from, limit))
            .await
    }

    pub(crate) async fn search(
        &self,
        query: String,
        limit: u16,
    ) -> Result<Vec<fl_model::chat::ChatHit>, RpcError> {
        self.store(move |store| store.database.search_chats(&query, limit))
            .await
    }

    pub(crate) async fn inbox(&self) -> Result<Vec<fl_model::chat::InboxItem>, RpcError> {
        self.store(|store| store.database.chat_inbox()).await
    }

    pub(crate) async fn dismiss_inbox(&self, item: uuid::Uuid) -> Result<(), RpcError> {
        self.store(move |store| store.database.dismiss_chat_inbox(item).map(|_| ()))
            .await?;
        self.publish_inbox();
        Ok(())
    }

    pub(crate) async fn session(&self, id: uuid::Uuid) -> Result<ChatSession, RpcError> {
        self.store(move |store| store.database.chat_session(id))
            .await
    }

    // --- Session lifecycle -------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_chat(
        self: &Arc<Self>,
        worktree: WorktreeId,
        provider: String,
        model: Option<String>,
        thinking: Option<String>,
        permission_mode: PermissionMode,
        resume: Option<ResumeTarget>,
    ) -> Result<ChatSession, RpcError> {
        if thinking
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(rpc(
                RpcErrorCode::Unsupported,
                "extended thinking is not supported by these provider adapters yet; omit the setting instead of silently ignoring it",
            ));
        }
        let _launch = self.launch.lock().await;
        if self.live.lock().await.len() >= MAX_LIVE_CHATS {
            return Err(rpc(
                RpcErrorCode::Unsupported,
                format!("{MAX_LIVE_CHATS} chats are already running; close one first"),
            ));
        }
        let spec = self
            .registry
            .get(&provider)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown chat provider {provider:?}")))?;
        let (tree_worktree, _project) = self
            .store(move |store| store.database.worktree(worktree))
            .await?;
        let id = uuid::Uuid::new_v4();
        if let Some(target) = &resume {
            self.claim_provider(&spec.id, &target.native_id, id)?;
        }
        let config = SessionConfig {
            cwd: PathBuf::from(&tree_worktree.path),
            model: model.clone(),
            permission_mode,
            instructions: None,
            session_id: Some(id),
            provider_config_json: None,
            output_schema_json: None,
            resume,
        };
        let launched = tokio::time::timeout(START_TIMEOUT, Adapter::launch(spec.clone(), config))
            .await
            .map_err(|_| internal("provider start timed out"))
            .and_then(|result| result.map_err(adapter_rpc));
        let mut adapter = match launched {
            Ok(adapter) => adapter,
            Err(error) => {
                self.release_claims_for(id);
                return Err(error);
            }
        };
        let info = adapter.info.clone();
        if let Err(error) = self
            .claim_provider(&spec.id, &info.resume.native_id, id)
        {
            let _ = adapter.shutdown().await;
            self.release_claims_for(id);
            return Err(error);
        }
        let events = adapter.take_events();
        let now = now_ms();
        let session = ChatSession {
            id: info.id,
            worktree: tree_worktree.id,
            provider: spec.id.clone(),
            label: format!("{} {}", spec.name, &info.id.to_string()[..8]),
            status: SessionState::Starting,
            status_source: StatusSource::Structured,
            model,
            thinking,
            permission_mode,
            resume: Some(info.resume.clone()),
            seq: 0,
            created_ms: now,
            updated_ms: now,
        };
        if let Err(error) = self
            .store({
                let session = session.clone();
                move |store| store.database.create_chat_session(&session)
            })
            .await
        {
            let _ = adapter.shutdown().await;
            self.release_claims_for(id);
            return Err(error);
        }
        let adapter = Arc::new(adapter);
        self.live.lock().await.insert(
            info.id,
            LiveChat {
                adapter,
                drain: None,
            },
        );
        let drain = self.spawn_drain(info.id, events);
        if let Some(live) = self.live.lock().await.get_mut(&info.id) {
            live.drain = Some(drain);
        } else {
            drain.abort();
        }
        self.publish_session(session.clone());
        Ok(session)
    }

    pub(crate) async fn send(self: &Arc<Self>, id: uuid::Uuid, text: String) -> Result<(), RpcError> {
        let adapter = self.ensure_live(id).await?;
        let result = tokio::time::timeout(SEND_TIMEOUT, adapter.send(text))
            .await
            .map_err(|_| internal("provider send timed out"))?;
        result.map_err(adapter_rpc)
    }

    pub(crate) async fn steer(self: &Arc<Self>, id: uuid::Uuid, text: String) -> Result<(), RpcError> {
        let adapter = self.ensure_live(id).await?;
        let result = tokio::time::timeout(SEND_TIMEOUT, adapter.steer(text))
            .await
            .map_err(|_| internal("provider steer timed out"))?;
        result.map_err(adapter_rpc)
    }

    pub(crate) async fn cancel(&self, id: uuid::Uuid) -> Result<(), RpcError> {
        if let Some(adapter) = self.adapter(id).await {
            let result = tokio::time::timeout(CONTROL_TIMEOUT, adapter.cancel())
                .await
                .map_err(|_| internal("provider cancel timed out"))?;
            return result.map_err(adapter_rpc);
        }
        // A session with no live adapter has no turn to cancel; normalize it
        // instead of pretending something was interrupted.
        let session = self.session(id).await?;
        if matches!(
            session.status,
            SessionState::Starting | SessionState::Running | SessionState::AwaitingInput
        ) {
            let closed = self
                .store(move |store| store.database.close_chat_session(id, now_ms()))
                .await?;
            self.publish_session(closed);
            self.publish_inbox();
        }
        Ok(())
    }

    pub(crate) async fn approve(
        self: &Arc<Self>,
        id: uuid::Uuid,
        request: String,
        decision: ApprovalDecision,
    ) -> Result<(), RpcError> {
        self.require_pending(id, &request, PendingKind::Approval)
            .await?;
        let adapter = self.ensure_live(id).await?;
        let result = tokio::time::timeout(
            CONTROL_TIMEOUT,
            adapter.approve(request.clone(), decision),
        )
        .await
        .map_err(|_| internal("provider approval timed out"))?;
        result.map_err(adapter_rpc)?;
        self.resolve_pending(id, &request, PendingKind::Approval)
            .await?;
        Ok(())
    }

    pub(crate) async fn answer(
        self: &Arc<Self>,
        id: uuid::Uuid,
        request: String,
        response: QuestionResponse,
    ) -> Result<(), RpcError> {
        self.require_pending(id, &request, PendingKind::Question)
            .await?;
        let adapter = self.ensure_live(id).await?;
        let result = tokio::time::timeout(
            CONTROL_TIMEOUT,
            adapter.respond_question(request.clone(), response),
        )
        .await
        .map_err(|_| internal("provider question timed out"))?;
        result.map_err(adapter_rpc)?;
        self.resolve_pending(id, &request, PendingKind::Question)
            .await?;
        Ok(())
    }

    pub(crate) async fn close(&self, id: uuid::Uuid) -> Result<(), RpcError> {
        if let Some(mut live) = self.live.lock().await.remove(&id) {
            // Stop the supervisor before the adapter closes its channel, so an
            // intentional close is never raced by a channel-failure path.
            if let Some(drain) = live.drain.take() {
                drain.abort();
            }
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, live.adapter.shutdown()).await;
        }
        self.release_claims_for(id);
        let session = self
            .store(move |store| store.database.close_chat_session(id, now_ms()))
            .await?;
        self.publish_session(session);
        self.publish_inbox();
        Ok(())
    }

    /// Stop every live adapter and close every session of a worktree that is
    /// about to be removed. A provider must never outlive its worktree.
    pub(crate) async fn close_worktree(&self, worktree: WorktreeId) -> Result<(), RpcError> {
        let mut ids: Vec<uuid::Uuid> = self
            .list_sessions(worktree)
            .await?
            .into_iter()
            .map(|session| session.id)
            .collect();
        ids.extend(self.live.lock().await.keys().copied());
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            let Ok(session) = self.session(id).await else {
                continue;
            };
            if session.worktree != worktree || matches!(session.status, SessionState::Closed) {
                continue;
            }
            self.close(id).await?;
        }
        Ok(())
    }

    async fn require_pending(
        &self,
        id: uuid::Uuid,
        request: &str,
        kind: PendingKind,
    ) -> Result<(), RpcError> {
        let request = request.to_owned();
        let pending = self
            .store(move |store| store.database.chat_pending(id))
            .await?;
        if pending
            .iter()
            .any(|pending| pending.request == request && pending.kind == kind)
        {
            Ok(())
        } else {
            Err(invalid(
                "no such pending request; it may already be resolved or belong to an earlier daemon",
            ))
        }
    }

    async fn resolve_pending(
        &self,
        id: uuid::Uuid,
        request: &str,
        kind: PendingKind,
    ) -> Result<(), RpcError> {
        let dedupe = inbox_dedupe(
            id,
            match kind {
                PendingKind::Approval => "approval",
                PendingKind::Question => "question",
            },
            request,
        );
        let request = request.to_owned();
        self.store(move |store| {
            store.database.resolve_chat_pending(id, &request, &dedupe)
        })
        .await?;
        self.publish_inbox();
        Ok(())
    }

    /// One live session owns one provider-native identity. Reusing another
    /// session's identity would multiplex two transcripts into one provider
    /// thread, so it is rejected instead of silently resumed.
    fn claim_provider(
        &self,
        provider: &str,
        native_id: &str,
        id: uuid::Uuid,
    ) -> Result<(), RpcError> {
        if native_id.is_empty() {
            return Ok(());
        }
        let key = (provider.to_owned(), native_id.to_owned());
        let mut claims = self.claims.lock().expect("claims poisoned");
        match claims.get(&key) {
            Some(owner) if *owner != id => Err(invalid(format!(
                "provider session {native_id:?} is already attached to live chat {owner}"
            ))),
            _ => {
                claims.insert(key, id);
                Ok(())
            }
        }
    }

    fn release_claims_for(&self, id: uuid::Uuid) {
        self.claims
            .lock()
            .expect("claims poisoned")
            .retain(|_, owner| *owner != id);
    }

    async fn adapter(&self, id: uuid::Uuid) -> Option<Arc<Adapter>> {
        self.live
            .lock()
            .await
            .get(&id)
            .map(|live| live.adapter.clone())
    }

    /// Warm reattach is a no-op; after a daemon restart this validates the
    /// stored resume target and relaunches. A rejected target archives the old
    /// transcript read-only instead of silently creating a new identity.
    async fn ensure_live(self: &Arc<Self>, id: uuid::Uuid) -> Result<Arc<Adapter>, RpcError> {
        if let Some(adapter) = self.adapter(id).await {
            return Ok(adapter);
        }
        let _launch = self.launch.lock().await;
        if let Some(adapter) = self.adapter(id).await {
            return Ok(adapter);
        }
        let session = self.session(id).await?;
        if self
            .store(move |store| store.database.chat_archived(id))
            .await?
        {
            return Err(invalid(
                "this transcript is archived read-only; its provider session could not be restored",
            ));
        }
        if self.live.lock().await.len() >= MAX_LIVE_CHATS {
            return Err(rpc(
                RpcErrorCode::Unsupported,
                format!("{MAX_LIVE_CHATS} chats are already running; close one first"),
            ));
        }
        let spec = self.registry.get(&session.provider).cloned().ok_or_else(|| {
            invalid(format!(
                "provider {:?} is no longer configured",
                session.provider
            ))
        })?;
        let Some(resume) = session.resume.clone() else {
            self.archive_unrestorable(
                id,
                "no provider resume target was stored; the transcript is archived read-only",
            )
            .await;
            return Err(invalid(
                "this chat has no resumable provider session; its transcript is archived",
            ));
        };
        // Restore in the worktree's CURRENT location. A stored target for a
        // different path is a semantic mismatch, not a transient failure.
        let (tree, _project) = self
            .store({
                let worktree = session.worktree;
                move |store| store.database.worktree(worktree)
            })
            .await?;
        let current = match tokio::fs::canonicalize(&tree.path).await {
            Ok(path) => path,
            Err(error) => {
                return Err(internal(format!(
                    "the worktree path is not accessible; the transcript stays restorable ({error})"
                )));
            }
        };
        if resume.cwd != current {
            self.archive_unrestorable(
                id,
                &format!(
                    "the stored conversation belongs to {} but this worktree is now {}",
                    resume.cwd.display(),
                    current.display()
                ),
            )
            .await;
            return Err(invalid(
                "the stored provider session belongs to a different worktree path; its transcript is archived",
            ));
        }
        self.claim_provider(&session.provider, &resume.native_id, id)?;
        let config = SessionConfig {
            cwd: current,
            model: session.model.clone(),
            permission_mode: session.permission_mode,
            instructions: None,
            session_id: Some(session.id),
            provider_config_json: None,
            output_schema_json: None,
            resume: Some(resume),
        };
        let launched = tokio::time::timeout(START_TIMEOUT, Adapter::launch(spec.clone(), config))
            .await
            .map_err(|_| internal("provider restore timed out"))
            .and_then(|result| result.map_err(adapter_rpc));
        let mut adapter = match launched {
            Ok(adapter) => adapter,
            Err(error) => {
                self.release_claims_for(id);
                if let AdapterError::InvalidResume(message) = error {
                    self.archive_unrestorable(
                        id,
                        &format!("the stored conversation could not be resumed ({message})"),
                    )
                    .await;
                    return Err(invalid(
                        "the stored provider session is no longer resumable; its transcript is archived",
                    ));
                }
                return Err(error);
            }
        };
        let info = adapter.info.clone();
        if let Err(error) = self
            .claim_provider(&session.provider, &info.resume.native_id, id)
        {
            let _ = adapter.shutdown().await;
            self.release_claims_for(id);
            return Err(error);
        }
        let events = adapter.take_events();
        let adapter = Arc::new(adapter);
        self.live.lock().await.insert(
            id,
            LiveChat {
                adapter: adapter.clone(),
                drain: None,
            },
        );
        let drain = self.spawn_drain(id, events);
        if let Some(live) = self.live.lock().await.get_mut(&id) {
            live.drain = Some(drain);
        } else {
            drain.abort();
        }
        // Record the validated target before any new turn is accepted.
        if session.resume.as_ref() != Some(&info.resume) {
            let resume = info.resume.clone();
            let stored = self
                .store(move |store| store.database.set_chat_resume(id, &resume))
                .await?;
            self.publish_session(stored);
        }
        Ok(adapter)
    }

    /// Archive a transcript whose stored provider identity cannot be restored.
    /// Nothing is relaunched under a new identity.
    async fn archive_unrestorable(&self, id: uuid::Uuid, reason: &str) {
        let title = bounded(format!(
            "Transcript archived read-only: {reason}. Start a new chat to continue."
        ));
        match self
            .store(move |store| {
                let archived = store.database.archive_chat(id, now_ms())?;
                store.database.add_chat_inbox(
                    id,
                    NewInboxItem {
                        kind: InboxKind::Error,
                        title,
                        dedupe: inbox_dedupe(id, "archive", &id.to_string()),
                        created_ms: now_ms(),
                    },
                )?;
                Ok(archived)
            })
            .await
        {
            Ok(archived) => {
                self.publish_session(archived);
                self.publish_inbox();
            }
            Err(error) => tracing::warn!(message = %error.message, "chat archive failed"),
        }
    }

    fn spawn_drain(
        self: &Arc<Self>,
        id: uuid::Uuid,
        events: tokio::sync::mpsc::Receiver<EventRecord>,
    ) -> JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            Self::drain(weak, id, events).await;
        })
    }

    async fn drain(
        weak: Weak<ChatService>,
        id: uuid::Uuid,
        mut events: tokio::sync::mpsc::Receiver<EventRecord>,
    ) {
        while let Some(record) = events.recv().await {
            let Some(service) = Weak::upgrade(&weak) else {
                return;
            };
            if let Err(error) = service.record_event(id, record).await {
                service.fail_session(id, error.message.clone()).await;
                return;
            }
        }
        let Some(service) = Weak::upgrade(&weak) else {
            return;
        };
        service.retire(id).await;
    }

    /// The adapter event channel closed. Its authoritative state decides the
    /// outcome: only an expected end is a clean close. A failed adapter is
    /// recorded as Failed with its own message, never laundered into Closed.
    async fn retire(&self, id: uuid::Uuid) {
        let live = self.live.lock().await.remove(&id);
        self.release_claims_for(id);
        let state = live.as_ref().map(|live| live.adapter.state());
        let Ok(session) = self.session(id).await else {
            return;
        };
        if matches!(
            session.status,
            SessionState::Closed | SessionState::Failed { .. }
        ) {
            return;
        }
        match state {
            Some(SessionState::Closed) => {
                let closed = self
                    .store(move |store| store.database.close_chat_session(id, now_ms()))
                    .await;
                match closed {
                    Ok(closed) => {
                        self.publish_session(closed);
                        self.publish_inbox();
                    }
                    Err(error) => {
                        tracing::warn!(message = %error.message, "chat retirement failed")
                    }
                }
            }
            Some(SessionState::Failed { message }) => {
                self.record_channel_failure(id, message).await;
            }
            _ => {
                self.record_channel_failure(
                    id,
                    "the provider event stream closed before the session ended".into(),
                )
                .await;
            }
        }
    }

    async fn record_channel_failure(&self, id: uuid::Uuid, message: String) {
        let title = bounded_with_marker(&message, MAX_CHAT_FAILURE_BYTES);
        let dedupe = inbox_dedupe(id, "error", "adapter-closed");
        let result = self
            .store(move |store| {
                store.database.fail_chat_session_with_inbox(
                    id,
                    &title,
                    NewInboxItem {
                        kind: InboxKind::Error,
                        title: title.clone(),
                        dedupe,
                        created_ms: now_ms(),
                    },
                    now_ms(),
                )
            })
            .await;
        match result {
            Ok(session) => {
                self.publish_session(session);
                self.publish_inbox();
            }
            Err(error) => tracing::warn!(message = %error.message, "chat failure was not recorded"),
        }
    }

    async fn fail_session(&self, id: uuid::Uuid, message: String) {
        self.live.lock().await.remove(&id);
        self.release_claims_for(id);
        let result = self
            .store({
                let message = bounded(message);
                move |store| store.database.fail_chat_session(id, &message, now_ms())
            })
            .await;
        match result {
            Ok(session) => {
                self.publish_session(session);
                self.publish_inbox();
            }
            Err(error) => tracing::warn!(message = %error.message, "chat failure was not recorded"),
        }
    }

    // --- Event persistence -------------------------------------------------

    async fn record_event(&self, id: uuid::Uuid, record: EventRecord) -> Result<(), RpcError> {
        let session = self.session(id).await?;
        let next = session
            .seq
            .checked_add(1)
            .ok_or_else(|| internal("chat sequence overflow"))?;
        let mut durable = record;
        durable.session_id = id;
        durable.sequence = next;
        sanitize_record(&mut durable);
        // Ingest gate: an event that still cannot pass the wire contract is
        // never stored or published. It is dropped and logged instead of
        // tearing down every connected client.
        if let Err(error) = fl_proto::validation::chat_event(&durable) {
            tracing::warn!(
                session = %id,
                %error,
                "dropping a provider chat event that cannot pass wire validation"
            );
            return Ok(());
        }
        let effects = self.effects_for(id, &durable, &session).await?;
        let dirty = effects_dirty(&effects);
        let appended = self
            .store({
                let record = durable.clone();
                move |store| store.database.append_chat_event(id, &record, effects)
            })
            .await?;
        // The store may absorb a byte-identical replay; nothing is published.
        if !appended.inserted {
            return Ok(());
        }
        self.publish(DaemonEvent::ChatEventAppended {
            session: id,
            worktree: appended.session.worktree,
            incarnation: durable.incarnation,
            sequence: durable.sequence,
        });
        let _ = self.notices.send(ChatNotice::Event {
            session: id,
            record: durable,
        });
        let changed = appended.session.status != session.status
            || appended.session.resume != session.resume
            || appended.session.status_source != session.status_source;
        if changed {
            self.publish_session(appended.session);
        }
        if dirty {
            self.publish_inbox();
        }
        Ok(())
    }

    async fn effects_for(
        &self,
        id: uuid::Uuid,
        record: &EventRecord,
        session: &ChatSession,
    ) -> Result<ChatEventEffects, RpcError> {
        let mut effects = event_effects(
            &record.event,
            session,
            id,
            record.sequence,
            record.timestamp_ms,
        )?;
        if needs_pending_clear(&record.event) {
            self.clear_pending_effects(id, &mut effects).await?;
        }
        Ok(effects)
    }

    async fn clear_pending_effects(
        &self,
        id: uuid::Uuid,
        effects: &mut ChatEventEffects,
    ) -> Result<(), RpcError> {
        let pending = self
            .store(move |store| store.database.chat_pending(id))
            .await?;
        for item in pending {
            let kind = match item.kind {
                PendingKind::Approval => "approval",
                PendingKind::Question => "question",
            };
            effects.dismiss_inbox.push(inbox_dedupe(id, kind, &item.request));
            effects.resolve_pending.push(item.request);
        }
        Ok(())
    }

    // --- Routed one-shot structured calls ----------------------------------

    /// Launch a throwaway adapter for a structured one-shot answer. Approval
    /// and question requests terminate it: a routed action never grants a
    /// permission on the user's behalf.
    pub(crate) async fn structured_call(
        &self,
        provider: &str,
        model: Option<String>,
        cwd: PathBuf,
        prompt: String,
        schema: String,
    ) -> Result<String, RpcError> {
        let _permit = self
            .structured
            .acquire()
            .await
            .map_err(|_| internal("chat service is shutting down"))?;
        let spec = self
            .registry
            .get(provider)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown chat provider {provider:?}")))?;
        let config = SessionConfig {
            cwd,
            model,
            // A routed one-shot answer may read but never edit: the action is
            // applied by the daemon only after its answer is validated.
            permission_mode: PermissionMode::ReadOnly,
            instructions: None,
            session_id: None,
            provider_config_json: None,
            output_schema_json: Some(schema),
            resume: None,
        };
        let mut adapter = match tokio::time::timeout(START_TIMEOUT, Adapter::launch(spec.clone(), config))
            .await
            .map_err(|_| internal("provider start timed out"))
            .and_then(|result| result.map_err(adapter_rpc))
        {
            Ok(adapter) => adapter,
            Err(error) => return Err(error),
        };
        let info = adapter.info.clone();
        if let Err(error) = self
            .claim_provider(&spec.id, &info.resume.native_id, info.id)
        {
            let _ = adapter.shutdown().await;
            return Err(error);
        }
        let _claim = ClaimGuard {
            service: self,
            id: info.id,
        };
        let mut events = adapter.take_events();
        let result = match adapter.send(prompt).await.map_err(adapter_rpc) {
            Ok(()) => tokio::time::timeout(STRUCTURED_TIMEOUT, async {
            let mut output: Option<String> = None;
            loop {
                let Some(record) = events.recv().await else {
                    return Err(internal(
                        "provider exited before producing a structured answer",
                    ));
                };
                match record.event {
                    ChatEvent::StructuredOutput { json } => output = Some(json),
                    ChatEvent::TurnEnd {
                        outcome: TurnOutcome::Completed,
                        ..
                    } => {
                        return output.ok_or_else(|| {
                            internal("provider completed without a structured answer")
                        });
                    }
                    ChatEvent::TurnEnd { outcome, .. } => {
                        return Err(internal(format!(
                            "provider turn ended with {outcome:?} before a structured answer"
                        )));
                    }
                    ChatEvent::ApprovalRequest { .. } => {
                        return Err(invalid(
                            "provider requested approval; routed one-shot actions never approve automatically",
                        ));
                    }
                    ChatEvent::QuestionRequest { .. } => {
                        return Err(invalid(
                            "provider requested input; a routed one-shot action cannot answer it",
                        ));
                    }
                    ChatEvent::Error { message, fatal: true, .. } => {
                        return Err(internal(format!("provider error: {message}")));
                    }
                    ChatEvent::ProcessExited { expected: false, .. } => {
                        return Err(internal("provider exited before completing"));
                    }
                    _ => {}
                }
            }
            })
            .await
            .unwrap_or_else(|_| Err(internal("provider structured call timed out"))),
            Err(error) => Err(error),
        };
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, adapter.shutdown()).await;
        self.release_claims_for(info.id);
        result
    }

    /// Whether this task can be answered by one bounded structured call.
    pub(crate) fn is_structured(task: GitTask) -> bool {
        matches!(
            task,
            GitTask::NameWorktree
                | GitTask::Commit
                | GitTask::CommitPush
                | GitTask::CreatePr
                | GitTask::EnhancePrompt
        )
    }

    /// The provider-native JSON schema for a routed structured task.
    pub(crate) fn output_schema(task: GitTask) -> &'static str {
        match task {
            GitTask::NameWorktree => {
                r#"{"type":"object","additionalProperties":false,"required":["branch","label"],
                    "properties":{"branch":{"type":"string","minLength":1,"maxLength":200},
                    "label":{"type":"string","minLength":1,"maxLength":120}}}"#
            }
            GitTask::Commit | GitTask::CommitPush => {
                r#"{"type":"object","additionalProperties":false,"required":["message"],
                    "properties":{"message":{"type":"string","minLength":1,"maxLength":60000}}}"#
            }
            GitTask::CreatePr => {
                r#"{"type":"object","additionalProperties":false,"required":["title","body"],
                    "properties":{"title":{"type":"string","minLength":1,"maxLength":200},
                    "body":{"type":"string","maxLength":60000},
                    "draft":{"type":"boolean"}}}"#
            }
            GitTask::EnhancePrompt => {
                r#"{"type":"object","additionalProperties":false,"required":["prompt"],
                    "properties":{"prompt":{"type":"string","minLength":1,"maxLength":60000}}}"#
            }
            _ => "{}",
        }
    }

    // --- Shutdown ----------------------------------------------------------

    async fn shutdown(&self) {
        let live = std::mem::take(&mut *self.live.lock().await);
        for (id, mut chat) in live {
            // Abort supervisors first: no channel-close path may race the
            // daemon's own expected shutdown and record a failure.
            if let Some(drain) = chat.drain.take() {
                drain.abort();
            }
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, chat.adapter.shutdown()).await;
            let active = self
                .session(id)
                .await
                .map(|session| {
                    matches!(
                        session.status,
                        SessionState::Starting
                            | SessionState::Running
                            | SessionState::AwaitingInput
                    )
                })
                .unwrap_or(false);
            if active
                && let Ok(session) = self
                    .store(move |store| store.database.close_chat_session(id, now_ms()))
                    .await
            {
                self.publish_session(session);
            }
        }
        self.claims.lock().expect("claims poisoned").clear();
        self.publish_inbox();
        let _ = self.shutdown_task.lock().await.take();
    }
}

/// Whether an event changed pending decisions or inbox cards.
fn effects_dirty(effects: &ChatEventEffects) -> bool {
    !effects.inbox.is_empty()
        || !effects.resolve_pending.is_empty()
        || !effects.add_pending.is_empty()
        || !effects.dismiss_inbox.is_empty()
}

fn merge_effects(into: &mut ChatEventEffects, from: ChatEventEffects) {
    if from.status.is_some() {
        into.status = from.status;
    }
    if from.resume.is_some() {
        into.resume = from.resume;
    }
    into.add_pending.extend(from.add_pending);
    into.resolve_pending.extend(from.resolve_pending);
    into.dismiss_inbox.extend(from.dismiss_inbox);
    into.inbox.extend(from.inbox);
    into.archive |= from.archive;
}

/// Terminal events clear every unanswerable pending decision.
fn needs_pending_clear(event: &ChatEvent) -> bool {
    match event {
        ChatEvent::StateChanged(SessionState::Closed | SessionState::Failed { .. }) => true,
        ChatEvent::TurnEnd { .. }
        | ChatEvent::Error { fatal: true, .. }
        | ChatEvent::ProcessExited { .. } => true,
        _ => false,
    }
}

/// Durable effects of one event. Subagent children contribute their own
/// approval/question cards so a nested request can be answered by its child ID.
fn event_effects(
    event: &ChatEvent,
    session: &ChatSession,
    id: uuid::Uuid,
    sequence: u64,
    timestamp: u64,
) -> Result<ChatEventEffects, RpcError> {
    let mut effects = ChatEventEffects::default();
    match event {
        ChatEvent::SessionReady(info) => {
            effects.resume = Some(info.resume.clone());
        }
        ChatEvent::StateChanged(state) => {
            let state = match state {
                SessionState::Failed { message } => SessionState::Failed {
                    message: bounded(message.clone()),
                },
                other => other.clone(),
            };
            effects.status = Some((state, StatusSource::Structured));
        }
        ChatEvent::ApprovalRequest {
            id: request,
            tool,
            details,
            ..
        } => {
            effects.status = Some((SessionState::AwaitingInput, StatusSource::Structured));
            effects.add_pending.push(PendingRequest {
                request: request.clone(),
                kind: PendingKind::Approval,
                payload: postcard::to_stdvec(event)
                    .map_err(|error| internal(error.to_string()))?,
                created_ms: timestamp,
            });
            effects.inbox.push(NewInboxItem {
                kind: InboxKind::Approval,
                title: bounded(format!(
                    "Approval needed: {tool} — {}",
                    details.text.lines().next().unwrap_or_default()
                )),
                dedupe: inbox_dedupe(id, "approval", request),
                created_ms: timestamp,
            });
        }
        ChatEvent::QuestionRequest {
            id: request,
            questions,
            ..
        } => {
            effects.status = Some((SessionState::AwaitingInput, StatusSource::Structured));
            effects.add_pending.push(PendingRequest {
                request: request.clone(),
                kind: PendingKind::Question,
                payload: postcard::to_stdvec(event)
                    .map_err(|error| internal(error.to_string()))?,
                created_ms: timestamp,
            });
            let prompt = questions
                .first()
                .map(|question| question.text.as_str())
                .unwrap_or("Input needed");
            effects.inbox.push(NewInboxItem {
                kind: InboxKind::Question,
                title: bounded(format!("Input needed: {prompt}")),
                dedupe: inbox_dedupe(id, "question", request),
                created_ms: timestamp,
            });
        }
        ChatEvent::RequestResolved { id: request } => {
            effects.resolve_pending.push(request.clone());
            effects
                .dismiss_inbox
                .push(inbox_dedupe(id, "approval", request));
            effects
                .dismiss_inbox
                .push(inbox_dedupe(id, "question", request));
        }
        ChatEvent::Subagent { events, .. } => {
            for child in events {
                // Only decisions are session-wide. A child's turn end or
                // completion must not close or idle the parent session.
                if matches!(
                    child,
                    ChatEvent::ApprovalRequest { .. }
                        | ChatEvent::QuestionRequest { .. }
                        | ChatEvent::RequestResolved { .. }
                ) {
                    merge_effects(
                        &mut effects,
                        event_effects(child, session, id, sequence, timestamp)?,
                    );
                }
            }
        }
        ChatEvent::TurnEnd { outcome, .. } => match outcome {
            TurnOutcome::Completed => {
                effects.status = Some((SessionState::Idle, StatusSource::Structured));
                effects.inbox.push(NewInboxItem {
                    kind: InboxKind::Completed,
                    title: bounded(format!("{} completed a turn", session.label)),
                    dedupe: inbox_dedupe(id, "turn", &sequence.to_string()),
                    created_ms: timestamp,
                });
            }
            TurnOutcome::Cancelled => {
                effects.status = Some((SessionState::Idle, StatusSource::Structured));
            }
            TurnOutcome::Failed => {
                effects.status = Some((
                    SessionState::Failed {
                        message: "the provider reported a failed turn".into(),
                    },
                    StatusSource::Structured,
                ));
                effects.inbox.push(NewInboxItem {
                    kind: InboxKind::Error,
                    title: bounded(format!("{} failed a turn", session.label)),
                    dedupe: inbox_dedupe(id, "error", &sequence.to_string()),
                    created_ms: timestamp,
                });
            }
        },
        ChatEvent::Error {
            message, fatal: true, ..
        } => {
            effects.status = Some((
                SessionState::Failed {
                    message: bounded(message.clone()),
                },
                StatusSource::Structured,
            ));
            effects.inbox.push(NewInboxItem {
                kind: InboxKind::Error,
                title: bounded(message.clone()),
                dedupe: inbox_dedupe(id, "error", &sequence.to_string()),
                created_ms: timestamp,
            });
        }
        ChatEvent::ProcessExited { expected, .. } => {
            effects.status = Some((
                if *expected {
                    SessionState::Closed
                } else {
                    SessionState::Failed {
                        message: "the provider process exited unexpectedly".into(),
                    }
                },
                StatusSource::Structured,
            ));
            if !*expected {
                effects.inbox.push(NewInboxItem {
                    kind: InboxKind::Error,
                    title: bounded(format!("{} exited unexpectedly", session.label)),
                    dedupe: inbox_dedupe(id, "error", &sequence.to_string()),
                    created_ms: timestamp,
                });
            }
        }
        _ => {}
    }
    Ok(effects)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::default_launch;
    use crate::FlotillaHome;
    use fl_model::ProjectId;
    use fl_model::workspace::{Project, ProjectKind, Worktree};
    use std::path::Path;
    use std::time::Instant;

    fn session_fixture(worktree: WorktreeId) -> ChatSession {
        ChatSession {
            id: uuid::Uuid::new_v4(),
            worktree,
            provider: "claude".into(),
            label: "Claude Code".into(),
            status: SessionState::Idle,
            status_source: StatusSource::Structured,
            model: Some("sonnet".into()),
            thinking: None,
            permission_mode: PermissionMode::Ask,
            resume: None,
            seq: 0,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    async fn harness(dir: &Path, script: &Path) -> (Arc<ChatService>, WorktreeId) {
        let home = FlotillaHome::new(dir.join(".flotilla"));
        home.ensure_dirs().unwrap();
        std::fs::create_dir_all(home.root().join("run")).unwrap();
        tokio::spawn(fld_prochost::serve(home.root().to_path_buf()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while fld_prochost::Client::connect(home.root()).await.is_err() {
            assert!(Instant::now() < deadline, "process host did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let terminals = TerminalService::start(&home, None, default_launch())
            .await
            .unwrap();
        let worktree_path = dir.join("wt");
        std::fs::create_dir_all(&worktree_path).unwrap();
        let rendered = worktree_path.to_str().unwrap().to_owned();
        let worktree = terminals
            .shared
            .store
            .call({
                let path = rendered.clone();
                move |store| {
                    let workspace = store.database.workspace_tree()?.workspaces[0].id;
                    let project = Project {
                        id: ProjectId::new(),
                        workspace,
                        name: "repo".into(),
                        path: path.clone(),
                        kind: ProjectKind::Git,
                        default_branch: Some("main".into()),
                        worktree_root: format!("{path}-worktrees"),
                        order: 0,
                    };
                    let primary = Worktree {
                        id: WorktreeId::new(),
                        project: project.id,
                        path,
                        branch: Some("main".into()),
                        target: None,
                        label: "main".into(),
                        is_primary: true,
                        created_ms: 1,
                        last_active_ms: 1,
                    };
                    let id = primary.id;
                    store.database.add_project(project, &[primary])?;
                    Ok(id)
                }
            })
            .await
            .unwrap()
            .unwrap();
        let mut provider = ProviderRegistry::default().get("claude").unwrap().clone();
        provider.command = "/bin/sh".into();
        provider.args = vec![script.to_str().unwrap().to_owned()];
        let mut registry = ProviderRegistry::default();
        registry.override_provider(provider).unwrap();
        let service = ChatService::start_with_registry(
            terminals,
            Arc::new(EventBus::default()),
            CancellationToken::new(),
            registry,
        )
        .await
        .unwrap();
        (service, worktree)
    }

    async fn wait_for_terminal(service: &ChatService, id: uuid::Uuid) -> ChatSession {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let session = service.session(id).await.unwrap();
            if matches!(
                session.status,
                SessionState::Failed { .. } | SessionState::Closed
            ) {
                return session;
            }
            assert!(
                Instant::now() < deadline,
                "session never reached a terminal state: {:?}",
                session.status
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[test]
    fn ingest_sanitizer_strips_nuls_and_truncates_to_wire_caps() {
        let mut record = EventRecord {
            schema_version: CHAT_SCHEMA_VERSION,
            session_id: uuid::Uuid::new_v4(),
            incarnation: uuid::Uuid::new_v4(),
            sequence: 1,
            timestamp_ms: 1,
            native_session_id: Some(format!("native\0{}", "x".repeat(MAX_CHAT_ID_BYTES))),
            turn_id: Some("t".repeat(MAX_CHAT_ID_BYTES + 32)),
            event: ChatEvent::AssistantText {
                id: "m1".into(),
                update: fl_model::chat::TextUpdate::Final,
                text: format!("hello\0{}", "x".repeat(MAX_CHAT_TEXT + 32)),
            },
        };
        sanitize_record(&mut record);
        match &record.event {
            ChatEvent::AssistantText { text, .. } => {
                assert!(!text.contains('\0'));
                assert!(text.len() <= MAX_CHAT_TEXT);
                assert!(text.ends_with(TRUNCATION_MARKER));
            }
            other => panic!("unexpected event {other:?}"),
        }
        let native = record.native_session_id.as_ref().unwrap();
        assert!(!native.contains('\0'));
        assert!(native.len() <= MAX_CHAT_ID_BYTES);
        let turn = record.turn_id.as_ref().unwrap();
        assert!(turn.len() <= MAX_CHAT_ID_BYTES);
        assert!(turn.ends_with(TRUNCATION_MARKER));
        assert!(fl_proto::validation::chat_event(&record).is_ok());
    }

    #[test]
    fn nested_decisions_produce_pending_inbox_and_awaiting_status() {
        let worktree = WorktreeId::new();
        let session = session_fixture(worktree);
        let event = ChatEvent::Subagent {
            parent_tool_id: "tool".into(),
            events: vec![ChatEvent::QuestionRequest {
                id: "child-question".into(),
                questions: vec![fl_model::chat::Question {
                    id: "q1".into(),
                    text: "Pick one".into(),
                    options: Vec::new(),
                    multi: false,
                    free_text: true,
                    secret: false,
                }],
                schema_json: None,
            }],
        };
        let effects =
            event_effects(&event, &session, session.id, 4, 9).expect("effects are representable");
        assert_eq!(
            effects.status,
            Some((SessionState::AwaitingInput, StatusSource::Structured))
        );
        assert_eq!(effects.add_pending.len(), 1);
        assert_eq!(effects.add_pending[0].request, "child-question");
        assert_eq!(effects.add_pending[0].kind, PendingKind::Question);
        assert_eq!(effects.inbox.len(), 1);
        assert_eq!(effects.inbox[0].kind, InboxKind::Question);
        assert!(effects_dirty(&effects));
    }

    #[test]
    fn failed_turn_declares_failed_rather_than_idle() {
        let effects = event_effects(
            &ChatEvent::TurnEnd {
                duration_ms: None,
                turns: 1,
                usage: fl_model::chat::Usage::default(),
                model: None,
                outcome: TurnOutcome::Failed,
            },
            &session_fixture(WorktreeId::new()),
            uuid::Uuid::new_v4(),
            8,
            10,
        )
        .unwrap();
        assert!(matches!(effects.status, Some((SessionState::Failed { .. }, _))));
        assert!(effects.inbox.iter().any(|item| item.kind == InboxKind::Error));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overflowing_provider_ends_failed_not_closed() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("peer.sh");
        tokio::fs::write(
            &script,
            "IFS= read -r init\n\
             printf '%s\\n' '{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"1\",\"response\":{}}}'\n\
             IFS= read -r prompt\n\
             printf '%s\\n' '{\"type\":\"control_request\",\"request_id\":\"perm-1\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"Bash\",\"input\":{\"command\":\"pwd\"},\"tool_use_id\":\"tool-1\"}}'\n\
             i=0\n\
             while [ \"$i\" -lt 5000 ]; do printf '%s\\n' '{\"type\":\"unknown_for_backpressure_test\"}'; i=$((i+1)); done\n\
             while IFS= read -r line; do :; done\n",
        )
        .await
        .unwrap();
        let (service, worktree) = harness(dir.path(), &script).await;
        let session = service
            .start_chat(
                worktree,
                "claude".into(),
                None,
                None,
                PermissionMode::Ask,
                None,
            )
            .await
            .unwrap();
        service.send(session.id, "hello".into()).await.unwrap();
        let ended = wait_for_terminal(&service, session.id).await;
        match &ended.status {
            SessionState::Failed { message } => {
                assert!(
                    message.contains("overflow")
                        || message.contains("backpressure")
                        || message.contains("event stream"),
                    "unexpected failure message: {message}"
                );
            }
            other => panic!("an overflowed provider was reported as {other:?}"),
        }
        let inbox = service.inbox().await.unwrap();
        assert!(inbox.iter().any(|item| item.kind == InboxKind::Error));
        // The dead adapter's stale decision must not remain answerable.
        assert!(!inbox
            .iter()
            .any(|item| matches!(item.kind, InboxKind::Approval | InboxKind::Question)));
        let session_id = session.id;
        let pending = service
            .terminals
            .shared
            .store
            .call(move |store| store.database.chat_pending(session_id))
            .await
            .unwrap()
            .unwrap();
        assert!(pending.is_empty(), "stale pending decisions: {pending:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removing_a_worktree_closes_its_live_chat() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("peer.sh");
        tokio::fs::write(
            &script,
            "IFS= read -r init\n\
             printf '%s\\n' '{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"1\",\"response\":{}}}'\n\
             while IFS= read -r line; do :; done\n",
        )
        .await
        .unwrap();
        let (service, worktree) = harness(dir.path(), &script).await;
        let session = service
            .start_chat(
                worktree,
                "claude".into(),
                None,
                None,
                PermissionMode::Ask,
                None,
            )
            .await
            .unwrap();
        service.close_worktree(worktree).await.unwrap();
        let closed = service.session(session.id).await.unwrap();
        assert_eq!(closed.status, SessionState::Closed);
        assert!(!service.live.lock().await.contains_key(&session.id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_provider_identity_cannot_be_resumed_twice() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("peer.sh");
        tokio::fs::write(
            &script,
            "IFS= read -r init\n\
             printf '%s\\n' '{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"1\",\"response\":{}}}'\n\
             while IFS= read -r line; do :; done\n",
        )
        .await
        .unwrap();
        let (service, worktree) = harness(dir.path(), &script).await;
        let first = service
            .start_chat(
                worktree,
                "claude".into(),
                None,
                None,
                PermissionMode::Ask,
                None,
            )
            .await
            .unwrap();
        let resume = first.resume.clone().unwrap();
        let error = service
            .start_chat(
                worktree,
                "claude".into(),
                None,
                None,
                PermissionMode::Ask,
                Some(resume),
            )
            .await
            .expect_err("a live provider identity must not be attached twice");
        assert_eq!(error.code, RpcErrorCode::InvalidArgument);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsupported_thinking_is_rejected_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("peer.sh");
        tokio::fs::write(&script, "exit 1\n").await.unwrap();
        let (service, worktree) = harness(dir.path(), &script).await;
        let error = service
            .start_chat(
                worktree,
                "claude".into(),
                None,
                Some("high".into()),
                PermissionMode::Ask,
                None,
            )
            .await
            .expect_err("thinking is not supported by either adapter");
        assert_eq!(error.code, RpcErrorCode::Unsupported);
    }
}
