//! The daemon's chats as the client sees them: sessions per worktree, durable
//! transcript pages, pending decisions in the inbox, kept current from pushed
//! events and rebuilt from durable pages on every reconnect.
//!
//! Every page is appended by durable sequence; a gap never advances the
//! mirror, so the next fetch starts at the last sequence the UI actually has.
use std::collections::HashMap;
use std::sync::Arc;

use fl_model::WorktreeId;
use fl_model::chat::{
    ChatEventPage, ChatSession, EventRecord, InboxItem, InboxState, MAX_CHAT_HISTORY,
    MAX_CHAT_MESSAGES,
};
use fl_model::workspace::DaemonEvent;
use fl_proto::{Request, Response};
use flc_conn::broker::ControlLink;
use futures::StreamExt;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use crate::workspaces::WorkspacesState;

/// Events retained per session in memory; the daemon page bound is smaller,
/// and older events stay fetchable from the daemon.
pub const MAX_MIRRORED_EVENTS: usize = MAX_CHAT_HISTORY;
/// Worktrees whose session lists are fetched concurrently after a reconnect.
const FETCH_CONCURRENCY: usize = 8;
/// Pages fetched for one push hint before leaving the rest for the next hint.
const MAX_HINT_PAGES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MergeError {
    #[error("chat event gap: expected {expected}, got {got}")]
    Gap { expected: u64, got: u64 },
    #[error("chat event belongs to another session")]
    ForeignSession,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatState {
    /// Sessions by id; kept while offline.
    pub sessions: HashMap<uuid::Uuid, ChatSession>,
    /// Durable events per session, ordered and gap-free.
    pub events: HashMap<uuid::Uuid, Vec<EventRecord>>,
    /// Non-dismissed inbox items, newest first.
    pub inbox: Vec<InboxItem>,
    /// Whether the current daemon answered chat requests.
    pub live: bool,
}

impl ChatState {
    pub fn session(&self, id: uuid::Uuid) -> Option<&ChatSession> {
        self.sessions.get(&id)
    }

    pub fn session_events(&self, id: uuid::Uuid) -> &[EventRecord] {
        self.events.get(&id).map(Vec::as_slice).unwrap_or_default()
    }

    /// The next durable sequence this mirror is missing.
    pub fn missing_from(&self, session: uuid::Uuid) -> u64 {
        self.session_events(session)
            .last()
            .map(|event| event.sequence + 1)
            .unwrap_or(1)
    }

    pub fn sessions_of(&self, worktree: WorktreeId) -> Vec<&ChatSession> {
        let mut sessions: Vec<_> = self
            .sessions
            .values()
            .filter(|session| session.worktree == worktree)
            .collect();
        sessions.sort_by_key(|session| (session.created_ms, session.id));
        sessions
    }

    /// Adopt the authoritative session list of one worktree, forgetting
    /// sessions (and their events) that the daemon no longer has.
    pub fn replace_worktree_sessions(&mut self, worktree: WorktreeId, incoming: Vec<ChatSession>) {
        let live: Vec<uuid::Uuid> = incoming.iter().map(|session| session.id).collect();
        self.sessions
            .retain(|id, session| session.worktree != worktree || live.contains(id));
        self.events.retain(|id, _| self.sessions.contains_key(id));
        for session in incoming {
            self.merge_session(session);
        }
    }

    /// Insert a session snapshot unless the mirror already knows a newer one.
    pub fn merge_session(&mut self, session: ChatSession) {
        match self.sessions.get(&session.id) {
            Some(known)
                if known.seq > session.seq
                    || (known.seq == session.seq && known.updated_ms > session.updated_ms) => {}
            _ => {
                self.sessions.insert(session.id, session);
            }
        }
    }

    /// Append one durable page. Either the whole page is contiguous with the
    /// mirror, or nothing changes and the caller refetches from `missing_from`.
    pub fn append_page(&mut self, page: &ChatEventPage) -> Result<bool, MergeError> {
        for record in &page.events {
            if record.session_id != page.session {
                return Err(MergeError::ForeignSession);
            }
        }
        let mut list = self.events.remove(&page.session).unwrap_or_default();
        let last = list.last().map(|event| event.sequence).unwrap_or(0);
        let mut appended = false;
        for record in &page.events {
            if record.sequence <= last
                || list
                    .last()
                    .is_some_and(|known| known.sequence == record.sequence)
            {
                continue;
            }
            let expected = list.last().map(|event| event.sequence + 1).unwrap_or(1);
            if record.sequence != expected {
                self.events.insert(page.session, list);
                return Err(MergeError::Gap {
                    expected,
                    got: record.sequence,
                });
            }
            list.push(record.clone());
            appended = true;
        }
        // Never grow beyond the bound; the oldest events stay on the daemon.
        if list.len() > MAX_MIRRORED_EVENTS {
            list.drain(..list.len() - MAX_MIRRORED_EVENTS);
        }
        if appended && let Some(session) = self.sessions.get_mut(&page.session) {
            let newest = list.last().map(|event| event.sequence).unwrap_or(0);
            if newest > session.seq {
                session.seq = newest;
            }
        }
        self.events.insert(page.session, list);
        Ok(appended)
    }

    /// Replace the inbox snapshot; dismissed items disappear.
    pub fn set_inbox(&mut self, items: Vec<InboxItem>) {
        self.inbox = items
            .into_iter()
            .filter(|item| item.state != InboxState::Dismissed)
            .collect();
    }

    pub fn dismiss(&mut self, item: uuid::Uuid) {
        self.inbox.retain(|known| known.id != item);
    }

    /// Sessions whose mirror is behind the daemon's cursor, with the page
    /// start for the first missing event.
    pub fn behind(&self) -> Vec<(uuid::Uuid, u64, u64)> {
        let mut behind: Vec<_> = self
            .sessions
            .values()
            .filter_map(|session| {
                let from = self.missing_from(session.id);
                (session.seq >= from).then_some((session.id, from, session.seq))
            })
            .collect();
        behind.sort_by_key(|(id, ..)| *id);
        behind
    }
}

pub(crate) fn update(state: &watch::Sender<Arc<ChatState>>, f: impl FnOnce(&mut ChatState)) {
    state.send_if_modified(|current| {
        let mut next = (**current).clone();
        f(&mut next);
        if next == **current {
            false
        } else {
            *current = Arc::new(next);
            true
        }
    });
}

pub(crate) fn supports_chat(proto: fl_proto::ProtoVersion) -> bool {
    proto.major > 0 || proto.minor >= 4
}

/// Every request that only protocol 0.4 daemons can decode.
pub(crate) fn is_chat_request(request: &Request) -> bool {
    matches!(
        request,
        Request::ListChatSessions { .. }
            | Request::GetChatEvents { .. }
            | Request::StartChat { .. }
            | Request::SendChat { .. }
            | Request::SteerChat { .. }
            | Request::CancelChat { .. }
            | Request::ApproveChat { .. }
            | Request::AnswerChat { .. }
            | Request::CloseChat { .. }
            | Request::SearchChats { .. }
            | Request::ListInbox
            | Request::DismissInbox { .. }
    )
}

pub(crate) async fn run(
    mut links: watch::Receiver<Option<ControlLink>>,
    workspaces: watch::Receiver<Arc<WorkspacesState>>,
    state: Arc<watch::Sender<Arc<ChatState>>>,
    shutdown: CancellationToken,
) {
    loop {
        let link = links
            .borrow_and_update()
            .clone()
            .filter(|link| !link.is_closed());
        if let Some(link) = link {
            let mut events = link.events();
            if supports_chat(link.daemon_proto()) {
                let snapshot = workspaces.borrow().clone();
                resync(&link, &snapshot, &state).await;
                let mut workspace_changes = workspaces.clone();
                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => return,
                        () = link.closed() => break,
                        event = events.recv() => match event {
                            Ok(event) => follow_event(&link, &state, event).await,
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let snapshot = workspace_changes.borrow().clone();
                                resync(&link, &snapshot, &state).await;
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        },
                        changed = workspace_changes.changed() => {
                            if changed.is_err() { return }
                            // A new worktree has its own session list.
                            let snapshot = workspace_changes.borrow().clone();
                            resync(&link, &snapshot, &state).await;
                        }
                    }
                }
            } else {
                update(&state, |state| {
                    state.sessions.clear();
                    state.events.clear();
                    state.inbox.clear();
                });
                link.closed().await;
            }
            update(&state, |state| state.live = false);
        } else {
            update(&state, |state| state.live = false);
            tokio::select! {
                _ = shutdown.cancelled() => return,
                changed = links.changed() => if changed.is_err() { return },
            }
        }
        if shutdown.is_cancelled() {
            return;
        }
    }
}

async fn follow_event(
    link: &ControlLink,
    state: &watch::Sender<Arc<ChatState>>,
    event: DaemonEvent,
) {
    match event {
        DaemonEvent::ChatSessionChanged(session) => {
            let id = session.id;
            update(state, |state| state.merge_session(session));
            fetch_missing(link, state, id).await;
        }
        DaemonEvent::ChatEventAppended {
            session, worktree, ..
        } => {
            let known = state.borrow().sessions.contains_key(&session);
            if !known {
                fetch_sessions(link, state, worktree).await;
            }
            fetch_missing(link, state, session).await;
        }
        DaemonEvent::InboxChanged => fetch_inbox(link, state).await,
        _ => {}
    }
}

async fn fetch_sessions(
    link: &ControlLink,
    state: &watch::Sender<Arc<ChatState>>,
    worktree: WorktreeId,
) {
    if let Ok(Response::ChatSessions(sessions)) =
        link.rpc(Request::ListChatSessions { worktree }).await
    {
        update(state, |state| {
            state.replace_worktree_sessions(worktree, sessions)
        });
    }
}

async fn fetch_inbox(link: &ControlLink, state: &watch::Sender<Arc<ChatState>>) {
    if let Ok(Response::Inbox(items)) = link.rpc(Request::ListInbox).await {
        update(state, |state| state.set_inbox(items));
    }
}

/// Fetch pages from the mirror's first missing sequence until the daemon has
/// nothing more (bounded per call so one hint cannot monopolize the task).
async fn fetch_missing(
    link: &ControlLink,
    state: &watch::Sender<Arc<ChatState>>,
    session: uuid::Uuid,
) {
    for _ in 0..MAX_HINT_PAGES {
        let (from, behind) = {
            let state = state.borrow();
            let Some(known) = state.session(session) else {
                return;
            };
            let from = state.missing_from(session);
            (from, known.seq >= from)
        };
        if !behind {
            return;
        }
        let page = match link
            .rpc(Request::GetChatEvents {
                session,
                from,
                limit: MAX_CHAT_MESSAGES as u16,
            })
            .await
        {
            Ok(Response::ChatEvents(page)) => page,
            Err(error) if error.code() == Some(fl_proto::RpcErrorCode::NotFound) => return,
            _ => return,
        };
        let mut merged = Ok(false);
        update(state, |state| {
            merged = state.append_page(&page);
        });
        if merged.is_err() {
            return;
        }
        if !page.more || page.events.is_empty() {
            return;
        }
    }
}

async fn resync(
    link: &ControlLink,
    workspaces: &WorkspacesState,
    state: &watch::Sender<Arc<ChatState>>,
) {
    let Some(tree) = &workspaces.tree else {
        update(state, |state| state.live = true);
        return;
    };
    let worktrees: Vec<WorktreeId> = tree.worktrees.iter().map(|worktree| worktree.id).collect();
    let snapshots: Vec<(WorktreeId, Option<Vec<ChatSession>>)> =
        futures::stream::iter(worktrees.iter().copied())
            .map(|worktree| async move {
                match link.rpc(Request::ListChatSessions { worktree }).await {
                    // A failed fetch keeps what the mirror already has.
                    Ok(Response::ChatSessions(sessions)) => (worktree, Some(sessions)),
                    _ => (worktree, None),
                }
            })
            .buffer_unordered(FETCH_CONCURRENCY)
            .collect()
            .await;
    update(state, |state| {
        for (worktree, sessions) in snapshots {
            if let Some(sessions) = sessions {
                state.replace_worktree_sessions(worktree, sessions);
            }
        }
        state.live = true;
    });
    fetch_inbox(link, state).await;
    let behind = state.borrow().behind();
    for (session, ..) in behind {
        fetch_missing(link, state, session).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::chat::{ChatEvent, PermissionMode, SessionState, StatusSource, TextUpdate};
    use uuid::Uuid;

    fn session(worktree: WorktreeId, seq: u64) -> ChatSession {
        ChatSession {
            id: Uuid::new_v4(),
            worktree,
            provider: "claude".into(),
            label: "Claude Code".into(),
            status: SessionState::Idle,
            status_source: StatusSource::Structured,
            model: None,
            thinking: None,
            permission_mode: PermissionMode::Ask,
            resume: None,
            seq,
            created_ms: 1,
            updated_ms: seq,
        }
    }

    fn record(session: Uuid, sequence: u64) -> EventRecord {
        EventRecord {
            schema_version: fl_model::chat::CHAT_SCHEMA_VERSION,
            session_id: session,
            incarnation: Uuid::new_v4(),
            sequence,
            timestamp_ms: sequence,
            native_session_id: None,
            turn_id: None,
            event: ChatEvent::AssistantText {
                id: format!("m{sequence}"),
                update: TextUpdate::Final,
                text: format!("event {sequence}"),
            },
        }
    }

    fn page(session: Uuid, sequences: std::ops::RangeInclusive<u64>, more: bool) -> ChatEventPage {
        ChatEventPage {
            session,
            incarnation: Uuid::new_v4(),
            events: sequences
                .map(|sequence| record(session, sequence))
                .collect(),
            more,
        }
    }

    #[test]
    fn pages_append_in_order_and_duplicates_are_ignored() {
        let worktree = WorktreeId::new();
        let mut state = ChatState::default();
        let session = session(worktree, 0);
        let id = session.id;
        state.merge_session(session);
        assert!(state.append_page(&page(id, 1..=2, true)).unwrap());
        assert!(!state.append_page(&page(id, 1..=2, false)).unwrap());
        assert_eq!(
            state
                .session_events(id)
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(state.missing_from(id), 3);
        assert_eq!(state.session(id).unwrap().seq, 2);
        assert!(state.append_page(&page(id, 3..=4, false)).unwrap());
        assert_eq!(state.session(id).unwrap().seq, 4);
    }

    #[test]
    fn a_gap_changes_nothing_and_reports_the_expected_sequence() {
        let worktree = WorktreeId::new();
        let mut state = ChatState::default();
        let session = session(worktree, 0);
        let id = session.id;
        state.merge_session(session);
        state.append_page(&page(id, 1..=1, true)).unwrap();
        let error = state.append_page(&page(id, 3..=4, false)).unwrap_err();
        assert_eq!(
            error,
            MergeError::Gap {
                expected: 2,
                got: 3
            }
        );
        assert_eq!(state.session_events(id).len(), 1);
        assert_eq!(state.missing_from(id), 2);
    }

    #[test]
    fn reconnect_replaces_stale_worktree_sessions_and_keeps_other_worktrees() {
        let gone = WorktreeId::new();
        let kept = WorktreeId::new();
        let mut state = ChatState::default();
        let stale = session(gone, 3);
        let stale_id = stale.id;
        let live = session(kept, 2);
        let live_id = live.id;
        state.merge_session(stale);
        state.merge_session(live);
        state.append_page(&page(stale_id, 1..=1, false)).unwrap();
        assert_eq!(state.session_events(stale_id).len(), 1);
        state.replace_worktree_sessions(gone, Vec::new());
        assert!(state.session(stale_id).is_none());
        assert!(state.session_events(stale_id).is_empty());
        assert_eq!(state.session(live_id).unwrap().seq, 2);
    }

    #[test]
    fn newer_snapshots_win_and_behind_lists_missing_pages() {
        let worktree = WorktreeId::new();
        let mut state = ChatState::default();
        let mut current = session(worktree, 4);
        let id = current.id;
        state.merge_session(current.clone());
        state.append_page(&page(id, 1..=2, true)).unwrap();
        assert_eq!(state.behind(), vec![(id, 3, 4)]);
        current.seq = 2;
        state.merge_session(current);
        assert_eq!(state.session(id).unwrap().seq, 4);
        state.append_page(&page(id, 3..=4, false)).unwrap();
        assert!(state.behind().is_empty());
    }

    #[test]
    fn mirrored_events_are_bounded_and_foreign_pages_are_rejected() {
        let worktree = WorktreeId::new();
        let mut state = ChatState::default();
        let session = session(worktree, 0);
        let id = session.id;
        state.merge_session(session);
        let mut list = Vec::new();
        for sequence in 1..=(MAX_MIRRORED_EVENTS as u64) {
            list.push(record(id, sequence));
        }
        state
            .append_page(&ChatEventPage {
                session: id,
                incarnation: Uuid::new_v4(),
                events: list,
                more: false,
            })
            .unwrap();
        assert_eq!(state.session_events(id).len(), MAX_MIRRORED_EVENTS);
        let mut foreign = page(id, 1..=1, false);
        foreign.events[0].session_id = Uuid::new_v4();
        assert_eq!(
            state.append_page(&foreign).unwrap_err(),
            MergeError::ForeignSession
        );
    }

    #[test]
    fn inbox_snapshots_hide_dismissed_and_dismissal_is_local() {
        let mut state = ChatState::default();
        let session = Uuid::new_v4();
        let id = Uuid::new_v4();
        let worktree = WorktreeId::new();
        let item = InboxItem {
            id,
            session,
            worktree,
            kind: fl_model::chat::InboxKind::Approval,
            state: InboxState::Unread,
            title: "Approval needed".into(),
            created_ms: 1,
        };
        state.set_inbox(vec![item.clone()]);
        assert_eq!(state.inbox.len(), 1);
        state.set_inbox(vec![InboxItem {
            state: InboxState::Dismissed,
            ..item
        }]);
        assert!(state.inbox.is_empty());
        state.set_inbox(vec![InboxItem {
            id,
            session,
            worktree,
            kind: fl_model::chat::InboxKind::Approval,
            state: InboxState::Unread,
            title: "Approval needed".into(),
            created_ms: 1,
        }]);
        state.dismiss(id);
        assert!(state.inbox.is_empty());
    }
}
