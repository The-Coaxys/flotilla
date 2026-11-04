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
    ApprovalDecision, ChatEvent, ChatSession, EventRecord, InboxKind, PermissionMode,
    QuestionResponse, ResumeTarget, SessionConfig, SessionState, StatusSource, TurnOutcome,
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
use tokio::sync::{Mutex, broadcast};
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

pub(crate) struct ChatService {
    terminals: Arc<TerminalService>,
    events: Arc<EventBus>,
    registry: ProviderRegistry,
    live: Mutex<HashMap<uuid::Uuid, LiveChat>>,
    /// Serializes launches so a cold restore cannot race a user start.
    launch: Mutex<()>,
    notices: broadcast::Sender<ChatNotice>,
    shutdown_task: Mutex<Option<JoinHandle<()>>>,
}

struct LiveChat {
    adapter: Arc<Adapter>,
    drain: Option<JoinHandle<()>>,
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
        let (notices, _) = broadcast::channel(NOTICE_CAPACITY);
        let service = Arc::new(Self {
            terminals,
            events,
            registry: ProviderRegistry::default(),
            live: Mutex::new(HashMap::new()),
            launch: Mutex::new(()),
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
        let mut adapter = tokio::time::timeout(START_TIMEOUT, Adapter::launch(spec.clone(), config))
            .await
            .map_err(|_| internal("provider start timed out"))?
            .map_err(adapter_rpc)?;
        let info = adapter.info.clone();
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
        self.store({
            let session = session.clone();
            move |store| store.database.create_chat_session(&session)
        })
        .await?;
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
        if let Some(live) = self.live.lock().await.remove(&id) {
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, live.adapter.shutdown()).await;
            if let Some(drain) = live.drain {
                drain.abort();
            }
        }
        let session = self
            .store(move |store| store.database.close_chat_session(id, now_ms()))
            .await?;
        self.publish_session(session);
        self.publish_inbox();
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
        let config = SessionConfig {
            cwd: resume.cwd.clone(),
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
            .map_err(|_| internal("provider restore timed out"))?;
        let mut adapter = match launched {
            Ok(adapter) => adapter,
            Err(AdapterError::InvalidResume(message)) => {
                self.archive_unrestorable(
                    id,
                    &format!("the stored conversation could not be resumed ({message})"),
                )
                .await;
                return Err(invalid(
                    "the stored provider session is no longer resumable; its transcript is archived",
                ));
            }
            Err(error) => return Err(adapter_rpc(error)),
        };
        let info = adapter.info.clone();
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

    async fn retire(&self, id: uuid::Uuid) {
        self.live.lock().await.remove(&id);
        let Ok(session) = self.session(id).await else {
            return;
        };
        if matches!(
            session.status,
            SessionState::Starting | SessionState::Running | SessionState::AwaitingInput
        ) {
            let closed = self
                .store(move |store| store.database.close_chat_session(id, now_ms()))
                .await;
            match closed {
                Ok(closed) => {
                    self.publish_session(closed);
                    self.publish_inbox();
                }
                Err(error) => tracing::warn!(message = %error.message, "chat retirement failed"),
            }
        }
    }

    async fn fail_session(&self, id: uuid::Uuid, message: String) {
        self.live.lock().await.remove(&id);
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
        let mut effects = ChatEventEffects::default();
        let sequence = record.sequence;
        let timestamp = record.timestamp_ms;
        match &record.event {
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
                let terminal = matches!(
                    state,
                    SessionState::Closed | SessionState::Failed { .. }
                );
                effects.status = Some((state, StatusSource::Structured));
                if terminal {
                    self.clear_pending_effects(id, &mut effects).await?;
                }
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
                    payload: postcard::to_stdvec(&record.event)
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
                    payload: postcard::to_stdvec(&record.event)
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
            ChatEvent::TurnEnd { outcome, .. } => {
                self.clear_pending_effects(id, &mut effects).await?;
                match outcome {
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
                }
            }
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
                self.clear_pending_effects(id, &mut effects).await?;
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
                self.clear_pending_effects(id, &mut effects).await?;
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
        let spec = self
            .registry
            .get(provider)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown chat provider {provider:?}")))?;
        let config = SessionConfig {
            cwd,
            model,
            permission_mode: PermissionMode::Ask,
            instructions: None,
            session_id: None,
            provider_config_json: None,
            output_schema_json: Some(schema),
            resume: None,
        };
        let mut adapter = tokio::time::timeout(START_TIMEOUT, Adapter::launch(spec, config))
            .await
            .map_err(|_| internal("provider start timed out"))?
            .map_err(adapter_rpc)?;
        let mut events = adapter.take_events();
        adapter.send(prompt).await.map_err(adapter_rpc)?;
        let result = tokio::time::timeout(STRUCTURED_TIMEOUT, async {
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
        .unwrap_or_else(|_| Err(internal("provider structured call timed out")));
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, adapter.shutdown()).await;
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
        for (id, chat) in live {
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, chat.adapter.shutdown()).await;
            if let Some(drain) = chat.drain {
                drain.abort();
            }
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
