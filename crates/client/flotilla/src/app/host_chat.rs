//! Chat surfaces: session tabs in the center strip, a virtualized transcript
//! with markdown/tool/decision rows, a multiline composer, the Chat panel,
//! the inbox and in-app notifications.
//!
//! Every control calls the real `Sync` chat API. A send is only cleared from
//! the composer after the daemon accepts it; approvals and questions are never
//! answered automatically, cards come from replayed durable events, and a
//! submission is refused locally while the same request is in flight.
use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;

use fl_model::chat::{
    ApprovalDecision, ChatHit, ChatSession, InboxItem, InboxKind, InboxState, MAX_CHAT_MESSAGES,
    PermissionMode, ProviderDiscovery, Question, SessionState, ToolStatus, TurnOutcome,
};
use flc_sync::ChatState;
use gpui::{ListAlignment, ListState, list};
use uuid::Uuid;

#[path = "host_chat/composer.rs"]
mod composer;
#[path = "host_chat/model.rs"]
mod model;

use self::composer::{Composer, ComposerEvent, ComposerOptions, composer, mono_options};
use self::model::{
    AnswerDraft, DiffKind, MarkdownBlock, RowKind, SubmitAction, Toast, ToastKind, ToolRow,
    TranscriptRow, approval_label, build_question_response, build_transcript, can_stop,
    collect_resolved_ids, composer_action, decision_ready, format_clock, format_duration,
    inline_spans, is_running, new_toasts, ordered_inbox, outcome_label, parse_markdown,
    parse_unified_diff, seen_sequences, sessions_newest_first, status_label, tool_icon,
    tool_kind_label, tool_status_label,
};
use super::*;

/// Toasts kept in memory; only the newest few are drawn.
const TOAST_LIMIT: usize = 20;
const TOASTS_SHOWN: usize = 4;
/// Rows rendered for an expanded diff; the rest stay in the durable text.
const MAX_RENDERED_DIFF_ROWS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum InFlight {
    Start(WorktreeId),
    Send(uuid::Uuid),
    Steer(uuid::Uuid),
    Stop(uuid::Uuid),
    Approval(uuid::Uuid, String),
    Answer(uuid::Uuid, String),
    Dismiss(uuid::Uuid),
    Close(uuid::Uuid),
}

struct ComposerBinding {
    input: Entity<Composer>,
    _events: Vec<Subscription>,
}

struct SetupForm {
    provider: Option<String>,
    model: Entity<TextInput>,
    thinking: Entity<TextInput>,
    permission: PermissionMode,
    error: Option<SharedString>,
    busy: bool,
    _events: Vec<Subscription>,
}

struct SearchState {
    input: Entity<TextInput>,
    hits: Vec<ChatHit>,
    busy: bool,
    error: Option<SharedString>,
    _events: Subscription,
}

struct QuestionForm {
    choices: BTreeMap<String, Vec<String>>,
    free: HashMap<String, Entity<Composer>>,
    structured: Option<Entity<Composer>>,
    error: Option<SharedString>,
    /// Embedded editors request a re-measure of their transcript row.
    _events: Vec<Subscription>,
}

pub(super) struct ChatUi {
    state: Arc<ChatState>,
    /// Open session tabs; the session id is the identity, never the label.
    tabs: Vec<Uuid>,
    active: Option<Uuid>,
    /// A composer removed by Escape until the user writes again.
    collapsed: HashSet<Uuid>,
    list: ListState,
    rows_key: Option<(Uuid, usize, u64)>,
    /// The list follows new events unless the user scrolled up.
    follow: Rc<Cell<bool>>,
    /// Row expansion state; keys are `kind:id` per session.
    expanded: HashSet<(Uuid, String)>,
    expansion_revision: u64,
    composers: HashMap<Uuid, ComposerBinding>,
    inflight: HashSet<InFlight>,
    errors: HashMap<Uuid, SharedString>,
    setup: Option<SetupForm>,
    providers: Option<Vec<ProviderDiscovery>>,
    providers_busy: bool,
    providers_error: Option<SharedString>,
    inbox_open: bool,
    inbox_busy: bool,
    inbox_error: Option<SharedString>,
    search: Option<SearchState>,
    question_forms: HashMap<(Uuid, String), QuestionForm>,
    toasts: Vec<Toast>,
    seen: HashMap<Uuid, u64>,
    seen_ready: bool,
    /// The composer to focus on the next frame.
    focus_pending: Option<Uuid>,
    /// A search hit to reveal and outline.
    jump: Option<(Uuid, u64)>,
    jump_applied: Option<(Uuid, u64)>,
    /// A session to open once the mirror lists it again.
    pending_open: Option<Uuid>,
    /// Two-step daemon session close.
    confirm_close: Option<Uuid>,
}

impl ChatUi {
    pub(super) fn new(state: Arc<ChatState>) -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, px(400.));
        let follow = Rc::new(Cell::new(true));
        {
            let follow = follow.clone();
            list.set_scroll_handler(move |event, _, _| {
                follow.set(!event.is_scrolled);
            });
        }
        Self {
            state,
            tabs: Vec::new(),
            active: None,
            collapsed: HashSet::new(),
            list,
            rows_key: None,
            follow,
            expanded: HashSet::new(),
            expansion_revision: 0,
            composers: HashMap::new(),
            inflight: HashSet::new(),
            errors: HashMap::new(),
            setup: None,
            providers: None,
            providers_busy: false,
            providers_error: None,
            inbox_open: false,
            inbox_busy: false,
            inbox_error: None,
            search: None,
            question_forms: HashMap::new(),
            toasts: Vec::new(),
            seen: HashMap::new(),
            seen_ready: false,
            focus_pending: None,
            jump: None,
            jump_applied: None,
            pending_open: None,
            confirm_close: None,
        }
    }

    /// Adopt a pushed mirror snapshot. The first snapshot seeds the seen
    /// cursors so replayed history does not fire notifications.
    pub(super) fn adopt(&mut self, state: Arc<ChatState>, cx: &mut Context<HostWorkspace>) {
        if !self.seen_ready {
            self.seen = seen_sequences(&state.events);
            self.seen_ready = true;
        } else {
            for toast in new_toasts(&self.seen, &state.events) {
                if !self
                    .toasts
                    .iter()
                    .any(|known| known.session == toast.session && known.seq == toast.seq)
                {
                    self.toasts.push(toast);
                }
            }
            self.seen = seen_sequences(&state.events);
            self.toasts
                .sort_by_key(|toast| std::cmp::Reverse(toast.seq));
            self.toasts.truncate(TOAST_LIMIT);
        }
        // Cards for sessions that the daemon forgot cannot stay actionable.
        self.question_forms
            .retain(|key, _| state.sessions.contains_key(&key.0));
        self.tabs
            .retain(|session| state.sessions.contains_key(session));
        if self
            .active
            .is_some_and(|active| !state.sessions.contains_key(&active))
        {
            self.active = None;
        }
        self.state = state;
        cx.notify();
    }

    pub(super) fn active_for(&self, scope: LayoutScope) -> Option<Uuid> {
        let active = self.active?;
        let worktree = self.state.session(active)?.worktree;
        match scope {
            LayoutScope::Worktree(id) if id == worktree => Some(active),
            _ => None,
        }
    }

    /// Tabs shown in the center strip for this scope: (session, label, active).
    pub(super) fn tab_labels(&self, scope: LayoutScope) -> Vec<(Uuid, SharedString, bool)> {
        self.tabs_for(scope)
            .into_iter()
            .map(|session| {
                let label = self.session_label(session);
                (session, label, self.active == Some(session))
            })
            .collect()
    }

    pub(super) fn clear_active(&mut self) {
        self.active = None;
    }

    /// Every conversation the mirror holds, newest first.
    pub(super) fn conversations(&self) -> Vec<ChatSession> {
        let mut sessions: Vec<_> = self.state.sessions.values().cloned().collect();
        sessions.sort_by_key(|session| (std::cmp::Reverse(session.updated_ms), session.id));
        sessions
    }

    /// Chat tabs currently open in the center strip.
    pub(super) fn open_tabs(&self) -> Vec<Uuid> {
        self.tabs.clone()
    }

    pub(super) fn session(&self, id: Uuid) -> Option<&ChatSession> {
        self.state.session(id)
    }

    fn tabs_for(&self, scope: LayoutScope) -> Vec<Uuid> {
        let LayoutScope::Worktree(worktree) = scope else {
            return Vec::new();
        };
        self.tabs
            .iter()
            .copied()
            .filter(|session| {
                self.state
                    .session(*session)
                    .is_some_and(|session| session.worktree == worktree)
            })
            .collect()
    }

    fn inbox_count(&self) -> usize {
        self.state.inbox.len()
    }

    fn session_label(&self, session: Uuid) -> SharedString {
        self.state
            .session(session)
            .map(|session| session.label.clone())
            .unwrap_or_else(|| "chat".into())
            .into()
    }

    fn resolved(&self, session: Uuid, request: &str) -> bool {
        collect_resolved_ids(self.state.session_events(session)).contains(request)
    }

    fn is_expanded(&self, session: Uuid, key: &str) -> bool {
        self.expanded.contains(&(session, key.to_owned()))
    }

    fn toggle_expanded(&mut self, session: Uuid, key: String) {
        let entry = (session, key);
        if !self.expanded.remove(&entry) {
            self.expanded.insert(entry);
        }
        self.expansion_revision = self.expansion_revision.wrapping_add(1);
    }

    fn sync_list(&mut self, session: Uuid, rows: &[TranscriptRow]) {
        let fingerprint = rows_fingerprint(rows) ^ self.expansion_revision;
        let key = (session, rows.len(), fingerprint);
        if self.rows_key == Some(key) {
            return;
        }
        let same_session = self.rows_key.map(|known| known.0) == Some(session);
        if same_session && !self.follow.get() {
            // Keep the reader's place; measured rows are recomputed lazily.
            self.list.splice(0..self.list.item_count(), rows.len());
        } else {
            self.follow.set(true);
            self.list.reset(rows.len());
        }
        self.rows_key = Some(key);
    }

    fn scroll_to_seq(&mut self, session: Uuid, seq: u64, rows: &[TranscriptRow]) {
        self.jump = Some((session, seq));
        if let Some(index) = row_index_for_seq(rows, seq) {
            self.follow.set(false);
            self.list.scroll_to_reveal_item(index);
            self.jump_applied = Some((session, seq));
        }
    }
}

/// Keeps an embedded answer editor's row measured as its text grows.
fn track_embedded_composer(
    cx: &mut Context<HostWorkspace>,
    input: &Entity<Composer>,
    events: &mut Vec<Subscription>,
) {
    events.push(cx.subscribe(input, |view, _, event: &ComposerEvent, cx| {
        if matches!(event, ComposerEvent::Changed) {
            view.chat.expansion_revision = view.chat.expansion_revision.wrapping_add(1);
            cx.notify();
        }
    }));
}

fn rows_fingerprint(rows: &[TranscriptRow]) -> u64 {
    let mut hasher = DefaultHasher::new();
    rows.len().hash(&mut hasher);
    for row in rows {
        row.seq.hash(&mut hasher);
        row_weight(&row.kind).hash(&mut hasher);
    }
    hasher.finish()
}

fn row_weight(kind: &RowKind) -> usize {
    match kind {
        RowKind::User { text, .. } => text.len(),
        RowKind::Assistant { text, complete, .. } => text.len() + usize::from(*complete),
        RowKind::Thinking { text, .. } => text.len(),
        RowKind::Tool(tool) => {
            tool.input.len()
                + tool.output.as_deref().map_or(0, str::len)
                + tool.diff.as_deref().map_or(0, str::len)
                + status_weight(&tool.status)
                + tool
                    .children
                    .iter()
                    .map(|row| row_weight(&row.kind))
                    .sum::<usize>()
        }
        RowKind::FileEdit {
            diff, truncated, ..
        } => diff.len() + usize::from(*truncated),
        RowKind::Approval {
            details,
            resolved,
            options,
            ..
        } => details.len() + usize::from(*resolved) + options.len(),
        RowKind::Question {
            questions,
            schema_json,
            resolved,
            ..
        } => questions.len() + schema_json.as_deref().map_or(0, str::len) + usize::from(*resolved),
        RowKind::TurnEnd { .. } => 1,
        RowKind::Structured { json } => json.len(),
        RowKind::Error { message, .. } => message.len(),
        RowKind::ProcessExited { .. } => 1,
    }
}

fn status_weight(status: &ToolStatus) -> usize {
    match status {
        ToolStatus::Running => 1,
        ToolStatus::Completed => 2,
        ToolStatus::Failed => 3,
        ToolStatus::Denied => 4,
    }
}

fn row_index_for_seq(rows: &[TranscriptRow], seq: u64) -> Option<usize> {
    // Rows carry the sequence that created them; a search hit belongs to the
    // last row created at or before that sequence.
    let mut index = None;
    for (position, row) in rows.iter().enumerate() {
        if row.seq <= seq {
            index = Some(position);
        } else {
            break;
        }
    }
    index
}

impl HostWorkspace {
    /// True only while the link is up and the daemon actually answers chat
    /// requests (protocol 0.4). Without it every chat control is disabled.
    fn chat_live(&self) -> bool {
        self.state.live && self.chat.state.live
    }

    // ---- session surfaces -------------------------------------------------

    fn ensure_composer(&mut self, session: Uuid, cx: &mut Context<Self>) -> Entity<Composer> {
        if let Some(binding) = self.chat.composers.get(&session) {
            return binding.input.clone();
        }
        let input = composer(cx, ComposerOptions::default());
        let events = cx.subscribe(
            &input,
            move |view, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Submitted(text) => {
                    view.submit_chat_text(session, text.to_string(), cx);
                }
                ComposerEvent::Cancelled => {
                    // Escape closes the composer rows: the input, its error and
                    // the selector strip all collapse to one "Write a reply" row.
                    view.chat.collapsed.insert(session);
                    view.chat.errors.remove(&session);
                    cx.notify();
                }
                ComposerEvent::Changed => {}
            },
        );
        self.chat.composers.insert(
            session,
            ComposerBinding {
                input: input.clone(),
                _events: vec![events],
            },
        );
        input
    }

    pub(super) fn open_chat_tab(
        &mut self,
        session: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.chat.state.session(session).is_none() {
            return;
        }
        if !self.chat.tabs.contains(&session) {
            self.chat.tabs.push(session);
        }
        self.chat.active = Some(session);
        self.chat.collapsed.remove(&session);
        if let Some(documents) = self.documents.get_mut(&self.scope) {
            documents.active = None;
        }
        let composer = self.ensure_composer(session, cx);
        let focus = composer.read(cx).focus_handle(cx);
        window.focus(&focus);
        self.fetch_chat_events(session, cx);
        cx.notify();
    }

    pub(super) fn select_chat_tab(&mut self, session: Uuid, cx: &mut Context<Self>) {
        if self.chat.state.session(session).is_none() {
            return;
        }
        self.chat.active = Some(session);
        self.chat.collapsed.remove(&session);
        if let Some(documents) = self.documents.get_mut(&self.scope) {
            documents.active = None;
        }
        self.ensure_composer(session, cx);
        self.chat.focus_pending = Some(session);
        self.fetch_chat_events(session, cx);
        cx.notify();
    }

    pub(super) fn close_chat_tab(&mut self, session: Uuid, cx: &mut Context<Self>) {
        self.chat.tabs.retain(|known| *known != session);
        if self.chat.active == Some(session) {
            self.chat.active = None;
        }
        self.chat.composers.remove(&session);
        self.chat.collapsed.remove(&session);
        self.chat.errors.remove(&session);
        self.chat.question_forms.retain(|key, _| key.0 != session);
        cx.notify();
    }

    /// Ask the daemon for everything the mirror is missing; the pushed mirror
    /// stays the source of truth.
    fn fetch_chat_events(&mut self, session: Uuid, cx: &mut Context<Self>) {
        let (from, behind) = match self.chat.state.session(session) {
            Some(known) => {
                let from = self.chat.state.missing_from(session);
                (from, known.seq >= from)
            }
            None => return,
        };
        if !behind {
            return;
        }
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .get_chat_events(session, from, MAX_CHAT_MESSAGES as u16)
                .await;
            let _ = this.update(cx, |view, cx| {
                if let Err(error) = result {
                    view.chat.errors.insert(session, error_text(&error));
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- setup and providers ---------------------------------------------

    fn open_chat_setup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.chat.setup.is_some() {
            self.chat.setup = None;
            cx.notify();
            return;
        }
        if !self.chat_live() {
            self.show_error(
                "Offline — starting a chat needs a live daemon on protocol 0.4.",
                cx,
            );
            return;
        }
        self.dialog = None;
        self.sections_ui.close();
        let model = cx.new(|cx| TextInput::new(cx, "Model (optional)"));
        let thinking = cx.new(|cx| TextInput::new(cx, "Thinking setting (optional)"));
        let mut events = Vec::new();
        for input in [&model, &thinking] {
            events.push(
                cx.subscribe(input, |view, _, event: &TextInputEvent, cx| match event {
                    TextInputEvent::Submitted(_) => view.start_chat_from_setup(cx),
                    TextInputEvent::Cancelled => {
                        view.chat.setup = None;
                        cx.notify();
                    }
                }),
            );
        }
        let focus = model.read(cx).focus_handle(cx);
        self.chat.setup = Some(SetupForm {
            provider: None,
            model,
            thinking,
            permission: PermissionMode::Ask,
            error: None,
            busy: false,
            _events: events,
        });
        self.refresh_chat_providers(cx);
        focus.focus(window);
        cx.notify();
    }

    fn refresh_chat_providers(&mut self, cx: &mut Context<Self>) {
        if self.chat.providers_busy {
            return;
        }
        self.chat.providers_busy = true;
        self.chat.providers_error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.chat_providers().await;
            let _ = this.update(cx, |view, cx| {
                view.chat.providers_busy = false;
                match result {
                    Ok(providers) => {
                        if let Some(form) = view.chat.setup.as_mut()
                            && form.provider.is_none()
                        {
                            form.provider = providers
                                .iter()
                                .find(|provider| {
                                    provider.unavailable_reason.is_none()
                                        && provider.capabilities.chat
                                })
                                .map(|provider| provider.provider.id.clone());
                        }
                        view.chat.providers = Some(providers);
                    }
                    Err(error) => view.chat.providers_error = Some(error_text(&error)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_chat_provider(&mut self, provider: String, cx: &mut Context<Self>) {
        if let Some(form) = self.chat.setup.as_mut() {
            form.provider = Some(provider);
            form.error = None;
        }
        cx.notify();
    }

    fn cycle_chat_permission(&mut self, cx: &mut Context<Self>) {
        if let Some(form) = self.chat.setup.as_mut() {
            form.permission = match form.permission {
                PermissionMode::Ask => PermissionMode::ReadOnly,
                PermissionMode::ReadOnly => PermissionMode::Ask,
            };
        }
        cx.notify();
    }

    fn start_chat_from_setup(&mut self, cx: &mut Context<Self>) {
        let Some(worktree) = self.selected_worktree().map(|worktree| worktree.id) else {
            if let Some(form) = self.chat.setup.as_mut() {
                form.error = Some("Select a worktree first.".into());
            }
            cx.notify();
            return;
        };
        if !self.chat_live() {
            if let Some(form) = self.chat.setup.as_mut() {
                form.error = Some("Offline — the daemon is not connected.".into());
            }
            cx.notify();
            return;
        }
        if self.chat.inflight.contains(&InFlight::Start(worktree)) {
            return;
        }
        let Some(form) = self.chat.setup.as_ref() else {
            return;
        };
        if form.busy {
            return;
        }
        let Some(provider) = form.provider.clone() else {
            if let Some(form) = self.chat.setup.as_mut() {
                form.error = Some("Choose a discovered provider.".into());
            }
            cx.notify();
            return;
        };
        let unavailable = self.chat.providers.as_ref().and_then(|providers| {
            providers
                .iter()
                .find(|candidate| candidate.provider.id == provider)
                .and_then(|candidate| {
                    candidate.unavailable_reason.clone().or_else(|| {
                        (!candidate.capabilities.chat).then(|| "no chat capability".to_owned())
                    })
                })
        });
        if let Some(reason) = unavailable {
            if let Some(form) = self.chat.setup.as_mut() {
                form.error = Some(format!("{provider} is unavailable: {reason}").into());
            }
            cx.notify();
            return;
        }
        if self.chat.providers.is_none() {
            if let Some(form) = self.chat.setup.as_mut() {
                form.error = Some("Providers are still being discovered.".into());
            }
            cx.notify();
            return;
        }
        let model = non_empty(form.model.read(cx).text());
        let thinking = non_empty(form.thinking.read(cx).text());
        let permission = form.permission;
        self.chat.inflight.insert(InFlight::Start(worktree));
        if let Some(form) = self.chat.setup.as_mut() {
            form.busy = true;
            form.error = None;
        }
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .start_chat(worktree, provider, model, thinking, permission, None)
                .await;
            let _ = this.update(cx, |view, cx| {
                view.chat.inflight.remove(&InFlight::Start(worktree));
                match result {
                    Ok(session) => {
                        view.chat.setup = None;
                        if !view.chat.tabs.contains(&session.id) {
                            view.chat.tabs.push(session.id);
                        }
                        view.chat.active = Some(session.id);
                        view.chat.focus_pending = Some(session.id);
                        view.chat.collapsed.remove(&session.id);
                        view.chat.errors.remove(&session.id);
                        view.ensure_composer(session.id, cx);
                        view.fetch_chat_events(session.id, cx);
                    }
                    Err(error) => {
                        if let Some(form) = view.chat.setup.as_mut() {
                            form.busy = false;
                            form.error = Some(error_text(&error));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- composer and decisions ------------------------------------------

    fn submit_chat_text(&mut self, session: Uuid, text: String, cx: &mut Context<Self>) {
        let text = text.trim_end().to_owned();
        if text.is_empty() {
            return;
        }
        if !self.chat_live() {
            self.chat
                .errors
                .insert(session, "Offline — the message was not sent.".into());
            cx.notify();
            return;
        }
        let Some(status) = self
            .chat
            .state
            .session(session)
            .map(|known| known.status.clone())
        else {
            self.chat
                .errors
                .insert(session, "This session is not in the mirror.".into());
            cx.notify();
            return;
        };
        let Some(action) = composer_action(&status) else {
            self.chat.errors.insert(
                session,
                "The session is not ready for a message; resolve the pending request first.".into(),
            );
            cx.notify();
            return;
        };
        let key = match action {
            SubmitAction::Send => InFlight::Send(session),
            SubmitAction::Steer => InFlight::Steer(session),
        };
        if !self.chat.inflight.insert(key.clone()) {
            return;
        }
        self.chat.errors.remove(&session);
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = match action {
                SubmitAction::Send => sync.send_chat(session, text.clone()).await,
                SubmitAction::Steer => sync.steer_chat(session, text.clone()).await,
            };
            let _ = this.update(cx, |view, cx| {
                view.chat.inflight.remove(&key);
                match result {
                    Ok(()) => {
                        // Only now is the composer cleared: the daemon accepted
                        // the message, so no send was claimed early.
                        if let Some(binding) = view.chat.composers.get(&session) {
                            binding.input.update(cx, |input, cx| input.clear(cx));
                        }
                        view.chat.errors.remove(&session);
                        view.chat.follow.set(true);
                    }
                    Err(error) => {
                        // Keep the draft so the user can fix and resend.
                        view.chat.errors.insert(session, error_text(&error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn stop_chat(&mut self, session: Uuid, cx: &mut Context<Self>) {
        if !self.chat_live() {
            self.chat
                .errors
                .insert(session, "Offline — the turn was not stopped.".into());
            cx.notify();
            return;
        }
        let Some(status) = self
            .chat
            .state
            .session(session)
            .map(|known| known.status.clone())
        else {
            return;
        };
        if !can_stop(&status) || !self.chat.inflight.insert(InFlight::Stop(session)) {
            return;
        }
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.cancel_chat(session).await;
            let _ = this.update(cx, |view, cx| {
                view.chat.inflight.remove(&InFlight::Stop(session));
                if let Err(error) = result {
                    view.chat.errors.insert(session, error_text(&error));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn approve_chat_request(
        &mut self,
        session: Uuid,
        request: String,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let key = InFlight::Approval(session, request.clone());
        let resolved = self.chat.resolved(session, &request);
        if !decision_ready(
            self.chat_live(),
            resolved,
            self.chat.inflight.contains(&key),
        ) {
            return;
        }
        self.chat.inflight.insert(key.clone());
        let sync = self.sync.clone();
        let id = request.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.approve_chat(session, id, decision).await;
            let _ = this.update(cx, |view, cx| {
                view.chat.inflight.remove(&key);
                match result {
                    Ok(()) => {
                        view.chat.errors.remove(&session);
                    }
                    Err(error) => {
                        view.chat.errors.insert(session, error_text(&error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn ensure_question_form(
        &mut self,
        session: Uuid,
        request: &str,
        questions: &[Question],
        schema: Option<&str>,
        cx: &mut Context<Self>,
    ) -> &mut QuestionForm {
        let key = (session, request.to_owned());
        if !self.chat.question_forms.contains_key(&key) {
            let mut free = HashMap::new();
            let mut events = Vec::new();
            for question in questions.iter().filter(|question| question.free_text) {
                let placeholder = if question.secret {
                    "Secret answer (visible while typing in this build)"
                } else {
                    "Type an answer…"
                };
                let input = composer(cx, mono_options(placeholder));
                track_embedded_composer(cx, &input, &mut events);
                free.insert(question.id.clone(), input);
            }
            let structured = schema.map(|_| {
                let input = composer(cx, mono_options("Structured JSON answer…"));
                track_embedded_composer(cx, &input, &mut events);
                input
            });
            self.chat.question_forms.insert(
                key.clone(),
                QuestionForm {
                    choices: BTreeMap::new(),
                    free,
                    structured,
                    error: None,
                    _events: events,
                },
            );
        }
        self.chat
            .question_forms
            .get_mut(&key)
            .expect("question form was just inserted")
    }

    fn toggle_question_choice(
        &mut self,
        session: Uuid,
        request: String,
        question: String,
        label: String,
        multi: bool,
        cx: &mut Context<Self>,
    ) {
        let mut cleared_error = false;
        if let Some(form) = self.chat.question_forms.get_mut(&(session, request)) {
            let choices = form.choices.entry(question).or_default();
            if multi {
                if let Some(index) = choices.iter().position(|known| known == &label) {
                    choices.remove(index);
                } else {
                    choices.push(label);
                }
            } else if choices.first() == Some(&label) {
                choices.clear();
            } else {
                *choices = vec![label];
            }
            cleared_error = form.error.take().is_some();
        }
        if cleared_error {
            self.chat.expansion_revision = self.chat.expansion_revision.wrapping_add(1);
        }
        cx.notify();
    }

    fn submit_question_answer(
        &mut self,
        session: Uuid,
        request: String,
        questions: Vec<Question>,
        schema: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let key = InFlight::Answer(session, request.clone());
        if !decision_ready(
            self.chat_live(),
            self.chat.resolved(session, &request),
            self.chat.inflight.contains(&key),
        ) {
            return;
        }
        let form_key = (session, request.clone());
        let response = {
            let Some(form) = self.chat.question_forms.get_mut(&form_key) else {
                return;
            };
            let mut draft = AnswerDraft {
                choices: form.choices.clone(),
                ..AnswerDraft::default()
            };
            for question in questions.iter().filter(|question| question.free_text) {
                if let Some(input) = form.free.get(&question.id) {
                    draft
                        .free
                        .insert(question.id.clone(), input.read(cx).text().to_owned());
                }
            }
            if let Some(input) = form.structured.as_ref() {
                draft.structured = input.read(cx).text().to_owned();
            }
            match build_question_response(&questions, schema.as_deref(), &draft) {
                Ok(response) => response,
                Err(error) => {
                    form.error = Some(error.to_string().into());
                    self.chat.expansion_revision = self.chat.expansion_revision.wrapping_add(1);
                    cx.notify();
                    return;
                }
            }
        };
        self.chat.inflight.insert(key.clone());
        let sync = self.sync.clone();
        let id = request.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.answer_chat(session, id, response).await;
            let _ = this.update(cx, |view, cx| {
                view.chat.inflight.remove(&key);
                match result {
                    Ok(()) => {
                        view.chat.question_forms.remove(&form_key);
                        view.chat.errors.remove(&session);
                    }
                    Err(error) => {
                        if let Some(form) = view.chat.question_forms.get_mut(&form_key) {
                            form.error = Some(error_text(&error));
                        } else {
                            view.chat.errors.insert(session, error_text(&error));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn close_chat_session(&mut self, session: Uuid, cx: &mut Context<Self>) {
        if !self.chat_live() || !self.chat.inflight.insert(InFlight::Close(session)) {
            return;
        }
        self.chat.confirm_close = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.close_chat(session).await;
            let _ = this.update(cx, |view, cx| {
                view.chat.inflight.remove(&InFlight::Close(session));
                match result {
                    Ok(()) => {
                        view.chat.errors.remove(&session);
                        // Keep the durable transcript in the mirror for
                        // inspection, but drop the live tab and composer.
                        view.chat.composers.remove(&session);
                        view.chat.collapsed.remove(&session);
                        view.chat.question_forms.retain(|key, _| key.0 != session);
                    }
                    Err(error) => {
                        view.chat.errors.insert(session, error_text(&error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- inbox, search, toasts -------------------------------------------

    fn open_chat_inbox(&mut self, cx: &mut Context<Self>) {
        self.dialog = None;
        self.sections_ui.close();
        self.routes_ui.close();
        self.chat.inbox_open = true;
        self.chat.inbox_error = None;
        self.refresh_chat_inbox(cx);
        cx.notify();
    }

    fn close_chat_inbox(&mut self, cx: &mut Context<Self>) {
        self.chat.inbox_open = false;
        cx.notify();
    }

    fn refresh_chat_inbox(&mut self, cx: &mut Context<Self>) {
        if self.chat.inbox_busy {
            return;
        }
        self.chat.inbox_busy = true;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.list_inbox().await;
            let _ = this.update(cx, |view, cx| {
                view.chat.inbox_busy = false;
                if let Err(error) = result {
                    view.chat.inbox_error = Some(error_text(&error));
                } else {
                    view.chat.inbox_error = None;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn dismiss_chat_inbox(&mut self, item: uuid::Uuid, cx: &mut Context<Self>) {
        if !self.chat_live() || !self.chat.inflight.insert(InFlight::Dismiss(item)) {
            return;
        }
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.dismiss_inbox(item).await;
            let _ = this.update(cx, |view, cx| {
                view.chat.inflight.remove(&InFlight::Dismiss(item));
                if let Err(error) = result {
                    view.chat.inbox_error = Some(error_text(&error));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_chat_inbox_item(
        &mut self,
        item: InboxItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.chat.inbox_open = false;
        self.select(LayoutScope::Worktree(item.worktree), cx);
        self.open_chat_tab(item.session, window, cx);
    }

    fn ensure_chat_search(&mut self, cx: &mut Context<Self>) -> Entity<TextInput> {
        if let Some(search) = self.chat.search.as_ref() {
            return search.input.clone();
        }
        let input = cx.new(|cx| TextInput::new(cx, "Search all chat transcripts"));
        let events = cx.subscribe(&input, |view, _, event: &TextInputEvent, cx| match event {
            TextInputEvent::Submitted(_) => view.run_chat_search(cx),
            TextInputEvent::Cancelled => {
                if let Some(search) = view.chat.search.as_mut() {
                    search.hits.clear();
                    search.error = None;
                }
                cx.notify();
            }
        });
        self.chat.search = Some(SearchState {
            input: input.clone(),
            hits: Vec::new(),
            busy: false,
            error: None,
            _events: events,
        });
        input
    }

    fn run_chat_search(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.chat.search.as_ref() else {
            return;
        };
        if search.busy {
            return;
        }
        let query = search.input.read(cx).text().trim().to_owned();
        if query.is_empty() {
            if let Some(search) = self.chat.search.as_mut() {
                search.hits.clear();
                search.error = None;
            }
            cx.notify();
            return;
        }
        if !self.chat_live() {
            if let Some(search) = self.chat.search.as_mut() {
                search.error = Some("Offline — search needs a live daemon.".into());
            }
            cx.notify();
            return;
        }
        if let Some(search) = self.chat.search.as_mut() {
            search.busy = true;
            search.error = None;
        }
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.search_chats(query, 50).await;
            let _ = this.update(cx, |view, cx| {
                if let Some(search) = view.chat.search.as_mut() {
                    search.busy = false;
                    match result {
                        Ok(hits) => search.hits = hits,
                        Err(error) => search.error = Some(error_text(&error)),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_chat_hit(&mut self, hit: ChatHit, window: &mut Window, cx: &mut Context<Self>) {
        self.select(LayoutScope::Worktree(hit.worktree), cx);
        if self.chat.state.session(hit.session).is_none() {
            // The mirror may not list the session yet; ask the daemon and open
            // the tab in the frame that first sees the session.
            self.chat.pending_open = Some(hit.session);
            self.chat.jump = Some((hit.session, hit.sequence));
            let sync = self.sync.clone();
            let worktree = hit.worktree;
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                let _ = sync.list_chat_sessions(worktree).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
            return;
        }
        self.open_chat_tab(hit.session, window, cx);
        let rows = build_transcript(self.chat.state.session_events(hit.session));
        self.chat.scroll_to_seq(hit.session, hit.sequence, &rows);
        cx.notify();
    }

    fn dismiss_toast(&mut self, session: Uuid, seq: u64, cx: &mut Context<Self>) {
        self.chat
            .toasts
            .retain(|toast| !(toast.session == session && toast.seq == seq));
        cx.notify();
    }

    fn open_toast(&mut self, session: Uuid, seq: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.chat
            .toasts
            .retain(|toast| !(toast.session == session && toast.seq == seq));
        if let Some(worktree) = self
            .chat
            .state
            .session(session)
            .map(|session| session.worktree)
        {
            self.select(LayoutScope::Worktree(worktree), cx);
        }
        self.open_chat_tab(session, window, cx);
        let rows = build_transcript(self.chat.state.session_events(session));
        self.chat.scroll_to_seq(session, seq, &rows);
        cx.notify();
    }

    pub(super) fn apply_pending_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.chat.pending_open.take()
            && self.chat.state.session(session).is_some()
        {
            self.open_chat_tab(session, window, cx);
        }
        let Some(session) = self.chat.focus_pending.take() else {
            return;
        };
        if let Some(binding) = self.chat.composers.get(&session) {
            let focus = binding.input.read(cx).focus_handle(cx);
            window.focus(&focus);
        }
    }

    // ---- rendering: surface ----------------------------------------------

    pub(super) fn render_chat_surface(
        &mut self,
        session: Uuid,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let Some(known) = self.chat.state.session(session).cloned() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(t.surface_bg)
                .text_color(t.text_secondary)
                .child("This chat is no longer in the mirror.")
                .into_any_element();
        };
        let events = self.chat.state.session_events(session).to_vec();
        let rows = Arc::new(build_transcript(&events));
        self.chat.sync_list(session, rows.as_slice());
        if let Some((jump_session, seq)) = self.chat.jump
            && jump_session == session
            && self.chat.jump_applied != Some((jump_session, seq))
            && let Some(index) = row_index_for_seq(rows.as_slice(), seq)
        {
            self.chat.follow.set(false);
            self.chat.list.scroll_to_reveal_item(index);
            self.chat.jump_applied = Some((jump_session, seq));
        }
        let header = self.render_chat_header(&known, &t, cx);
        let count = rows.len();
        let body: AnyElement = if count == 0 {
            div()
                .flex_1()
                .min_h(px(0.))
                .p(px(16.))
                .text_size(t.text_size_sm)
                .text_color(t.text_secondary)
                .child("No durable events yet. The daemon appends one page at a time.")
                .into_any_element()
        } else {
            let highlight = self
                .chat
                .jump
                .filter(|(known, _)| *known == session)
                .and_then(|(_, seq)| row_index_for_seq(rows.as_slice(), seq));
            let this = cx.entity();
            let theme = t.clone();
            let list_rows = rows.clone();
            let list_state = self.chat.list.clone();
            list(list_state, move |item, _, cx| {
                let Some(row) = list_rows.get(item).cloned() else {
                    return div().into_any_element();
                };
                this.update(cx, |view, cx| {
                    let element = view.render_transcript_row(session, &row, &theme, cx);
                    if highlight == Some(item) {
                        div()
                            .border_l_2()
                            .border_color(theme.accent)
                            .child(element)
                            .into_any_element()
                    } else {
                        element
                    }
                })
            })
            .flex_1()
            .min_h(px(0.))
            .into_any_element()
        };
        let composer = self.render_composer(session, &known, &t, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface_bg)
            .text_color(t.text_primary)
            .child(header)
            .child(
                div()
                    .id("chat-transcript-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(body),
            )
            .child(composer)
            .into_any_element()
    }

    fn render_chat_header(
        &self,
        session: &ChatSession,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = session.id;
        let status = status_label(&session.status);
        let source = match session.status_source {
            fl_model::chat::StatusSource::Structured => "structured",
            fl_model::chat::StatusSource::Hook => "hook",
            fl_model::chat::StatusSource::Heuristic => "heuristic",
        };
        let stop_pending = self.chat.inflight.contains(&InFlight::Stop(id));
        let close_pending = self.chat.inflight.contains(&InFlight::Close(id));
        let confirm = self.chat.confirm_close == Some(id);
        let stoppable = can_stop(&session.status);
        let mut row = div()
            .flex_shrink_0()
            .px(px(14.))
            .py(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(t.border)
            .bg(t.chrome_bg)
            .text_size(t.text_size_sm)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text_primary)
                    .child(SharedString::from(session.label.clone())),
            )
            .child(
                div()
                    .text_color(t.text_tertiary)
                    .child(SharedString::from(session.provider.clone())),
            )
            .child(ui_chip(
                status,
                if is_running(&session.status) {
                    t.warning
                } else if matches!(&session.status, SessionState::Failed { .. }) {
                    t.danger
                } else {
                    t.text_secondary
                },
                t,
            ))
            .child(ui_chip(format!("status: {source}"), t.text_tertiary, t));
        if let Some(model) = &session.model {
            row = row.child(ui_chip(model.clone(), t.text_tertiary, t));
        }
        row = row.child(ui_chip(
            format!("permission: {}", permission_label(session.permission_mode)),
            t.text_tertiary,
            t,
        ));
        row = row.child(div().flex_1());
        if stoppable {
            let enabled = self.chat_live() && !stop_pending;
            row = row.child(
                ui_button(
                    "chat-stop",
                    if stop_pending { "Stopping…" } else { "Stop" },
                    enabled,
                    t,
                )
                .when(enabled, |d| {
                    d.on_click(cx.listener(move |view, _, _, cx| view.stop_chat(id, cx)))
                }),
            );
        }
        if confirm {
            row = row
                .child(
                    ui_button("chat-close-confirm", "Close session", self.chat_live(), t).when(
                        self.chat_live(),
                        |d| {
                            d.on_click(
                                cx.listener(move |view, _, _, cx| view.close_chat_session(id, cx)),
                            )
                        },
                    ),
                )
                .child(
                    ui_button("chat-close-cancel", "Keep", true, t).on_click(cx.listener(
                        |view, _, _, cx| {
                            view.chat.confirm_close = None;
                            cx.notify();
                        },
                    )),
                );
        } else {
            let enabled = self.chat_live() && !close_pending;
            row = row.child(
                ui_button(
                    "chat-close",
                    if close_pending {
                        "Closing…"
                    } else {
                        "Close session"
                    },
                    enabled,
                    t,
                )
                .when(enabled, |d| {
                    d.on_click(cx.listener(move |view, _, _, cx| {
                        view.chat.confirm_close = Some(id);
                        cx.notify();
                    }))
                }),
            );
        }
        row.into_any_element()
    }
}

fn permission_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "ask",
        PermissionMode::ReadOnly => "read-only",
    }
}

fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

// ---- transcript rows -------------------------------------------------------

impl HostWorkspace {
    fn render_transcript_row(
        &mut self,
        session: Uuid,
        row: &TranscriptRow,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &row.kind {
            RowKind::User { text, .. } => div()
                .w_full()
                .px(px(16.))
                .py(px(6.))
                .flex()
                .flex_col()
                .items_end()
                .child(
                    div()
                        .max_w(px(720.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(t.radius_md)
                        .bg(t.control_bg)
                        .text_size(t.text_size)
                        .text_color(t.text_primary)
                        .child(SharedString::from(text.clone())),
                )
                .into_any_element(),
            RowKind::Assistant { text, complete, .. } => {
                let mut body = div()
                    .w_full()
                    .px(px(16.))
                    .py(px(6.))
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .text_size(t.text_size)
                    .text_color(t.text_primary);
                body = body.children(render_markdown(text, t));
                if !*complete {
                    body = body.child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_tertiary)
                            .child("▍ streaming"),
                    );
                }
                body.into_any_element()
            }
            RowKind::Thinking { id, text } => {
                let key = format!("thinking:{id}");
                let expanded = self.chat.is_expanded(session, &key);
                let id = id.clone();
                let mut block = div()
                    .w_full()
                    .px(px(16.))
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .id(SharedString::from(format!("think-{id}-{session}")))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .cursor_pointer()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_secondary)
                            .child(if expanded { "▾" } else { "▸" })
                            .child(SharedString::from(format!(
                                "Thinking · {} characters",
                                text.chars().count()
                            )))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.chat.toggle_expanded(session, key.clone());
                                cx.notify();
                            })),
                    );
                if expanded {
                    block = block.child(
                        div()
                            .pl(px(16.))
                            .text_size(t.text_size_sm)
                            .text_color(t.text_secondary)
                            .child(SharedString::from(text.clone())),
                    );
                }
                block.into_any_element()
            }
            RowKind::Tool(tool) => self.render_tool_row(session, tool, t, cx),
            RowKind::FileEdit {
                path,
                diff,
                truncated,
            } => div()
                .w_full()
                .px(px(16.))
                .py(px(5.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(t.text_size_sm)
                        .text_color(t.text_primary)
                        .child("✎")
                        .child(SharedString::from(format!("Edited {path}"))),
                )
                .child(diff_block(diff, *truncated, t))
                .into_any_element(),
            RowKind::Approval {
                id,
                tool,
                details,
                truncated,
                options,
                resolved,
            } => self.render_approval_row(
                session, id, tool, details, *truncated, options, *resolved, t, cx,
            ),
            RowKind::Question {
                id,
                questions,
                schema_json,
                resolved,
            } => self.render_question_row(
                session,
                id,
                questions,
                schema_json.as_deref(),
                *resolved,
                t,
                cx,
            ),
            RowKind::TurnEnd {
                duration_ms,
                turns,
                usage,
                model,
                outcome,
            } => {
                let mut parts = vec![format_clock(row.at_ms)];
                if let Some(duration) = duration_ms {
                    parts.push(format_duration(*duration));
                }
                parts.push(outcome_label(outcome).to_owned());
                if *turns > 1 {
                    parts.push(format!("{turns} turns"));
                }
                if let Some(model) = model {
                    parts.push(model.clone());
                }
                if let Some(tokens) = usage.input_tokens {
                    parts.push(format!("in {tokens}"));
                }
                if let Some(tokens) = usage.output_tokens {
                    parts.push(format!("out {tokens}"));
                }
                if let Some(tokens) = usage.cache_read_tokens {
                    parts.push(format!("cache read {tokens}"));
                }
                if let Some(tokens) = usage.cache_write_tokens {
                    parts.push(format!("cache write {tokens}"));
                }
                if let Some(cost) = &usage.cost_usd {
                    parts.push(format!("${cost}"));
                }
                div()
                    .w_full()
                    .px(px(16.))
                    .py(px(4.))
                    .text_size(t.text_size_sm)
                    .text_color(match outcome {
                        TurnOutcome::Completed => t.text_secondary,
                        TurnOutcome::Cancelled => t.warning,
                        TurnOutcome::Failed => t.danger,
                    })
                    .child(SharedString::from(parts.join(" · ")))
                    .into_any_element()
            }
            RowKind::Structured { json } => div()
                .w_full()
                .px(px(16.))
                .py(px(5.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(label("Structured output", t))
                .child(mono_block(json, t))
                .into_any_element(),
            RowKind::Error {
                code,
                message,
                fatal,
            } => div()
                .w_full()
                .px(px(16.))
                .py(px(6.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .text_size(t.text_size_sm)
                .text_color(t.danger)
                .child(SharedString::from(format!(
                    "{}{code}: {message}",
                    if *fatal { "fatal " } else { "" }
                )))
                .into_any_element(),
            RowKind::ProcessExited { code, expected } => div()
                .w_full()
                .px(px(16.))
                .py(px(4.))
                .text_size(t.text_size_sm)
                .text_color(t.text_tertiary)
                .child(SharedString::from(format!(
                    "Provider process exited{}{}.",
                    code.map(|code| format!(" with code {code}"))
                        .unwrap_or_default(),
                    if *expected { " (expected)" } else { "" }
                )))
                .into_any_element(),
        }
    }

    fn render_tool_row(
        &mut self,
        session: Uuid,
        tool: &ToolRow,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("tool:{}", tool.id);
        let expanded = self.chat.is_expanded(session, &key);
        let status_color = match &tool.status {
            ToolStatus::Running => t.warning,
            ToolStatus::Completed => t.success,
            ToolStatus::Failed => t.danger,
            ToolStatus::Denied => t.text_tertiary,
        };
        let id = tool.id.clone();
        let children = tool.children.clone();
        let mut block = div()
            .w_full()
            .px(px(16.))
            .py(px(4.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .id(SharedString::from(format!("tool-{id}-{session}")))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_size(t.text_size_sm)
                    .child(if expanded { "▾" } else { "▸" })
                    .child(
                        div()
                            .text_color(t.text_secondary)
                            .child(tool_icon(&tool.kind)),
                    )
                    .child(
                        div()
                            .text_color(t.text_tertiary)
                            .child(tool_kind_label(&tool.kind)),
                    )
                    .child(
                        div()
                            .text_color(t.text_primary)
                            .child(SharedString::from(tool.title.clone())),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_color(status_color)
                            .child(tool_status_label(&tool.status)),
                    )
                    .when(!children.is_empty(), |d| {
                        d.child(
                            div()
                                .text_color(t.text_tertiary)
                                .child(SharedString::from(format!("{} sub", children.len()))),
                        )
                    })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.chat.toggle_expanded(session, key.clone());
                        cx.notify();
                    })),
            );
        if expanded {
            if !tool.input.is_empty() {
                let mut section = div()
                    .pl(px(18.))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(label("input", t))
                    .child(mono_block(&tool.input, t));
                if tool.input_truncated {
                    section = section.child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_tertiary)
                            .child("(input truncated by the daemon)"),
                    );
                }
                block = block.child(section);
            }
            if let Some(output) = &tool.output {
                block = block.child(
                    div()
                        .pl(px(18.))
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(label("output", t))
                        .child(mono_block(output, t)),
                );
            }
            if let Some(diff) = &tool.diff {
                block = block.child(div().pl(px(18.)).child(diff_block(diff, false, t)));
            }
        }
        for child in &children {
            block = block.child(
                div()
                    .pl(px(18.))
                    .border_l_1()
                    .border_color(t.border)
                    .child(self.render_transcript_row(session, child, t, cx)),
            );
        }
        block.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_approval_row(
        &mut self,
        session: Uuid,
        id: &str,
        tool: &str,
        details: &str,
        truncated: bool,
        options: &[ApprovalDecision],
        resolved: bool,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pending = self
            .chat
            .inflight
            .contains(&InFlight::Approval(session, id.to_owned()));
        let live = self.chat_live();
        let mut card = div()
            .w_full()
            .px(px(16.))
            .py(px(6.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .p(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .rounded(t.radius_md)
                    .bg(t.surface_raised)
                    .border_1()
                    .border_color(if resolved { t.border } else { t.warning })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(t.text_size_sm)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if resolved {
                                t.text_secondary
                            } else {
                                t.warning
                            })
                            .child(SharedString::from(format!("Approval · {tool}")))
                            .when(resolved, |d| {
                                d.child(ui_chip("resolved", t.text_tertiary, t))
                            })
                            .when(pending && !resolved, |d| {
                                d.child(ui_chip("submitting…", t.text_secondary, t))
                            }),
                    )
                    .when(!details.is_empty(), |d| {
                        d.child(mono_block(details, t)).when(truncated, |d| {
                            d.child(
                                div()
                                    .text_size(t.text_size_sm)
                                    .text_color(t.text_tertiary)
                                    .child("(details truncated by the daemon)"),
                            )
                        })
                    }),
            );
        if !resolved {
            let mut buttons = div().flex().items_center().gap(px(6.));
            if options.is_empty() {
                card = card.child(
                    div()
                        .px(px(10.))
                        .text_size(t.text_size_sm)
                        .text_color(t.text_tertiary)
                        .child("The provider offered no approval decisions for this request."),
                );
            }
            for decision in options {
                let decision = *decision;
                let enabled = decision_ready(live, resolved, pending);
                let request = id.to_owned();
                let label = approval_label(decision);
                let button_id = format!("approve-{id}-{decision:?}");
                buttons =
                    buttons.child(ui_button(button_id, label, enabled, t).when(enabled, |d| {
                        d.on_click(cx.listener(move |view, _, _, cx| {
                            view.approve_chat_request(session, request.clone(), decision, cx)
                        }))
                    }));
            }
            card = card.child(div().px(px(10.)).child(buttons));
        }
        card.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_question_row(
        &mut self,
        session: Uuid,
        id: &str,
        questions: &[Question],
        schema_json: Option<&str>,
        resolved: bool,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pending = self
            .chat
            .inflight
            .contains(&InFlight::Answer(session, id.to_owned()));
        let live = self.chat_live();
        if resolved {
            self.chat.question_forms.remove(&(session, id.to_owned()));
        }
        let (choices, free, structured, error) = if resolved {
            (BTreeMap::new(), HashMap::new(), None, None)
        } else {
            let form = self.ensure_question_form(session, id, questions, schema_json, cx);
            (
                form.choices.clone(),
                form.free.clone(),
                form.structured.clone(),
                form.error.clone(),
            )
        };
        let mut card = div().w_full().px(px(16.)).py(px(6.)).child(
            div()
                .p(px(10.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .rounded(t.radius_md)
                .bg(t.surface_raised)
                .border_1()
                .border_color(if resolved { t.border } else { t.accent })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(t.text_size_sm)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if resolved { t.text_secondary } else { t.accent })
                        .child("Question from the provider")
                        .when(resolved, |d| {
                            d.child(ui_chip("resolved", t.text_tertiary, t))
                        })
                        .when(pending && !resolved, |d| {
                            d.child(ui_chip("submitting…", t.text_secondary, t))
                        }),
                ),
        );
        let mut body = div().flex().flex_col().gap(px(8.)).px(px(10.));
        for question in questions {
            let mut group = div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.text_primary)
                        .child(SharedString::from(question.text.clone())),
                )
                .child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.text_tertiary)
                        .child(if question.multi {
                            "Select all that apply."
                        } else if question.options.is_empty() {
                            "Type your answer."
                        } else {
                            "Choose one."
                        }),
                );
            let selected = choices.get(&question.id).cloned().unwrap_or_default();
            if schema_json.is_some() {
                body = body.child(group);
                continue;
            }
            for (index, option) in question.options.iter().enumerate() {
                if resolved {
                    group = group.child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_secondary)
                            .child(SharedString::from(format!(
                                "{} {}",
                                if selected.contains(&option.label) {
                                    "[x]"
                                } else {
                                    "[ ]"
                                },
                                option.label
                            ))),
                    );
                    continue;
                }
                let checked = selected.contains(&option.label);
                let request = id.to_owned();
                let question_id = question.id.clone();
                let label_text = option.label.clone();
                let multi = question.multi;
                let mut row = div()
                    .id(SharedString::from(format!(
                        "q-{id}-{}-{index}",
                        question.id
                    )))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(6.))
                    .py(px(3.))
                    .rounded(t.radius_sm)
                    .cursor_pointer()
                    .text_size(t.text_size_sm)
                    .when(checked, |d| d.bg(t.row_selected))
                    .child(if checked {
                        if multi { "☑" } else { "◉" }
                    } else if multi {
                        "☐"
                    } else {
                        "○"
                    })
                    .child(SharedString::from(option.label.clone()));
                if !option.description.is_empty() {
                    row = row.child(
                        div()
                            .text_color(t.text_tertiary)
                            .child(SharedString::from(option.description.clone())),
                    );
                }
                row = row.on_click(cx.listener(move |view, _, _, cx| {
                    view.toggle_question_choice(
                        session,
                        request.clone(),
                        question_id.clone(),
                        label_text.clone(),
                        multi,
                        cx,
                    )
                }));
                group = group.child(row);
            }
            if schema_json.is_none()
                && question.free_text
                && let Some(input) = free.get(&question.id).cloned()
            {
                group = group.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(
                            div()
                                .text_size(t.text_size_sm)
                                .text_color(t.text_tertiary)
                                .child(if question.secret {
                                    "Secret answer — visible while typing in this build."
                                } else {
                                    "Free text answer"
                                }),
                        )
                        .child(input),
                );
            }
            body = body.child(group);
        }
        if let Some(schema) = schema_json {
            let mut section = div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.text_primary)
                        .child("MCP form: supply the structured JSON answer."),
                )
                .child(mono_block(schema, t));
            if let Some(input) = structured {
                section = section.child(input);
            }
            body = body.child(section);
        }
        if let Some(error) = error {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error),
            );
        }
        if !resolved {
            let enabled = decision_ready(live, resolved, pending);
            let request = id.to_owned();
            let questions = questions.to_vec();
            let schema = schema_json.map(str::to_owned);
            body = body.child(ui_button("answer-question", "Answer", enabled, t).when(
                enabled,
                |d| {
                    d.on_click(cx.listener(move |view, _, _, cx| {
                        view.submit_question_answer(
                            session,
                            request.clone(),
                            questions.clone(),
                            schema.clone(),
                            cx,
                        )
                    }))
                },
            ));
        }
        card = card.child(body);
        card.into_any_element()
    }

    // ---- composer --------------------------------------------------------

    fn render_composer(
        &mut self,
        session: Uuid,
        chat: &ChatSession,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let offline = !self.chat_live();
        let collapsed = self.chat.collapsed.contains(&session);
        let note = generic_note(t);
        if collapsed {
            return div()
                .flex_shrink_0()
                .px(px(14.))
                .py(px(8.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .border_t_1()
                .border_color(t.border)
                .bg(t.chrome_bg)
                .child(
                    ui_button("chat-write", "Write a reply", !offline, t).when(!offline, |d| {
                        d.on_click(cx.listener(move |view, _, window, cx| {
                            view.chat.collapsed.remove(&session);
                            let composer = view.ensure_composer(session, cx);
                            let focus = composer.read(cx).focus_handle(cx);
                            window.focus(&focus);
                            cx.notify();
                        }))
                    }),
                )
                .child(note)
                .into_any_element();
        }
        let input = self.ensure_composer(session, cx);
        let action = composer_action(&chat.status);
        let send_pending = self.chat.inflight.contains(&InFlight::Send(session))
            || self.chat.inflight.contains(&InFlight::Steer(session));
        let enabled = action.is_some() && !offline && !send_pending;
        let button_label = match (action, send_pending) {
            (_, true) => "Sending…",
            (Some(SubmitAction::Send), _) => "Send",
            (Some(SubmitAction::Steer), _) => "Steer",
            (None, _) => "Send",
        };
        let stop_pending = self.chat.inflight.contains(&InFlight::Stop(session));
        let error = self.chat.errors.get(&session).cloned();
        let status_note: SharedString = if offline {
            "Offline — messages are not sent.".into()
        } else {
            match &chat.status {
                SessionState::Idle => "Idle. Enter sends; Shift+Enter adds a line.".into(),
                SessionState::Running => {
                    "Running. Enter steers the active turn; Shift+Enter adds a line.".into()
                }
                SessionState::AwaitingInput => {
                    "Awaiting input. Resolve the pending approval or question first.".into()
                }
                SessionState::Starting => "Starting…".into(),
                SessionState::Closed => "This session is closed.".into(),
                SessionState::Failed { .. } => "This session failed; no further messages.".into(),
            }
        };
        let mut row = div()
            .flex_shrink_0()
            .border_t_1()
            .border_color(t.border)
            .bg(t.chrome_bg)
            .px(px(14.))
            .py(px(8.))
            .flex()
            .flex_col()
            .gap(px(6.));
        if let Some(error) = error {
            row = row.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error),
            );
        }
        row = row.child(
            div()
                .flex()
                .items_end()
                .gap(px(6.))
                .child(div().flex_1().min_w(px(0.)).child(input))
                .child(
                    ui_button("chat-send", button_label, enabled, t).when(enabled, |d| {
                        d.on_click(cx.listener(move |view, _, window, cx| {
                            view.submit_composer(session, window, cx)
                        }))
                    }),
                )
                .when(can_stop(&chat.status), |d| {
                    let enabled = !offline && !stop_pending;
                    d.child(
                        ui_button(
                            "chat-stop-inline",
                            if stop_pending { "Stopping…" } else { "Stop" },
                            enabled,
                            t,
                        )
                        .when(enabled, |d| {
                            d.on_click(
                                cx.listener(move |view, _, _, cx| view.stop_chat(session, cx)),
                            )
                        }),
                    )
                }),
        );
        row = row
            .child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.text_tertiary)
                    .child(status_note),
            )
            .child(note);
        row.into_any_element()
    }

    fn submit_composer(&mut self, session: Uuid, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .chat
            .composers
            .get(&session)
            .map(|binding| binding.input.read(cx).text().to_owned());
        let Some(text) = text else {
            return;
        };
        self.submit_chat_text(session, text, cx);
    }
}

fn generic_note(t: &Theme) -> AnyElement {
    div()
        .text_size(t.text_size_sm)
        .text_color(t.text_tertiary)
        .child(
            "Plain text only: this daemon serves no file index, so @-file mentions are not offered.",
        )
        .into_any_element()
}

// ---- Chat panel -----------------------------------------------------------

impl HostWorkspace {
    pub(super) fn render_chat_panel(
        &mut self,
        worktree: WorktreeId,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let offline = !self.chat_live();
        let inbox = self.chat.inbox_count();
        let setup_open = self.chat.setup.is_some();
        let mut body = div().p(px(8.)).flex().flex_col().gap(px(8.));
        let mut actions = div().flex().items_center().gap(px(6.));
        actions = actions
            .child(
                ui_button(
                    "chat-new",
                    if setup_open { "Cancel" } else { "New chat" },
                    !offline,
                    t,
                )
                .when(!offline, |d| {
                    d.on_click(cx.listener(|view, _, window, cx| view.open_chat_setup(window, cx)))
                }),
            )
            .child(
                ui_button(
                    "chat-inbox",
                    if inbox > 0 {
                        format!("Inbox {inbox}")
                    } else {
                        "Inbox".to_owned()
                    },
                    !offline,
                    t,
                )
                .when(!offline, |d| {
                    d.on_click(cx.listener(|view, _, _, cx| view.open_chat_inbox(cx)))
                }),
            );
        let live_note = if offline {
            "Offline — chats need a live daemon on protocol 0.4."
        } else {
            "Sessions come from the daemon. Click a session to open its tab."
        };
        body = body.child(actions).child(
            div()
                .text_size(t.text_size_sm)
                .text_color(if offline { t.danger } else { t.text_secondary })
                .child(live_note),
        );
        if setup_open {
            body = body.child(self.render_chat_setup(t, cx));
        }
        let sessions: Vec<ChatSession> =
            sessions_newest_first(&self.chat.state.sessions_of(worktree))
                .into_iter()
                .cloned()
                .collect();
        if sessions.is_empty() {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.text_secondary)
                    .child("No chat sessions in this worktree yet."),
            );
        } else {
            body = body.child(label("Sessions", t));
            for session in sessions {
                body = body.child(self.render_chat_session_row(&session, t, cx));
            }
        }
        body = body.child(label("History", t));
        let search = self.ensure_chat_search(cx);
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(div().flex_1().min_w(px(0.)).child(search))
                .child(
                    ui_button("chat-search", "Search", !offline, t).when(!offline, |d| {
                        d.on_click(cx.listener(|view, _, _, cx| view.run_chat_search(cx)))
                    }),
                ),
        );
        if let Some(search) = self.chat.search.as_ref() {
            if search.busy {
                body = body.child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.text_secondary)
                        .child("Searching…"),
                );
            }
            if let Some(error) = search.error.clone() {
                body = body.child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.danger)
                        .child(error),
                );
            }
            for (index, hit) in search.hits.iter().enumerate() {
                let hit = hit.clone();
                let snippet = hit.snippet.clone();
                let session = hit.session;
                let seq = hit.sequence;
                body = body.child(
                    div()
                        .id(SharedString::from(format!("chat-hit-{index}")))
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .px(px(6.))
                        .py(px(4.))
                        .rounded(t.radius_sm)
                        .cursor_pointer()
                        .hover({
                            let hover = t.raised_hover;
                            move |s| s.bg(hover)
                        })
                        .text_size(t.text_size_sm)
                        .child(
                            div()
                                .text_color(t.text_primary)
                                .child(SharedString::from(snippet)),
                        )
                        .child(div().text_color(t.text_tertiary).child(SharedString::from(
                            format!("{} · event {}", self.chat.session_label(session), seq),
                        )))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.open_chat_hit(hit.clone(), window, cx)
                        })),
                );
            }
            if !search.busy && search.hits.is_empty() && search.error.is_none() {
                body = body.child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.text_tertiary)
                        .child("No archived transcript hits yet."),
                );
            }
        }
        body = body.child(generic_note(t));
        body.into_any_element()
    }

    fn render_chat_session_row(
        &mut self,
        session: &ChatSession,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = session.id;
        let offline = !self.chat_live();
        let close_pending = self.chat.inflight.contains(&InFlight::Close(id));
        let confirm = self.chat.confirm_close == Some(id);
        let active = self.chat.active == Some(id);
        let color = match &session.status {
            SessionState::Running => t.warning,
            SessionState::AwaitingInput => t.accent,
            SessionState::Failed { .. } => t.danger,
            _ => t.text_secondary,
        };
        let row = div()
            .id(SharedString::from(format!("chat-session-{id}")))
            .on_hover(super::host_hover::watch(
                cx.entity().downgrade(),
                super::host_hover::Target::Session(id),
            ))
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(6.))
            .py(px(5.))
            .rounded(t.radius_sm)
            .when(active, |d| d.bg(t.row_selected))
            .cursor_pointer()
            .hover({
                let hover = t.raised_hover;
                move |s| s.bg(hover)
            })
            .text_size(t.text_size_sm)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().text_color(color).child("●"))
                    .child(
                        div()
                            .text_color(t.text_primary)
                            .child(SharedString::from(session.label.clone())),
                    )
                    .child(
                        div()
                            .text_color(t.text_tertiary)
                            .child(SharedString::from(session.provider.clone())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_color(t.text_tertiary)
                    .child(status_label(&session.status))
                    .child(SharedString::from(format!("seq {}", session.seq))),
            )
            .on_click(cx.listener(move |view, _, window, cx| view.open_chat_tab(id, window, cx)));
        let mut controls = div().flex().items_center().gap(px(4.));
        if confirm {
            controls = controls
                .child(
                    ui_button("session-close-yes", "Close", !offline, t).when(!offline, |d| {
                        d.on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.close_chat_session(id, cx)
                        }))
                    }),
                )
                .child(
                    ui_button("session-close-no", "Keep", true, t).on_click(cx.listener(
                        |view, _, _, cx| {
                            view.chat.confirm_close = None;
                            cx.notify();
                        },
                    )),
                );
        } else {
            controls = controls.child(
                ui_button(
                    "session-close",
                    if close_pending { "Closing…" } else { "×" },
                    !offline && !close_pending,
                    t,
                )
                .when(!offline && !close_pending, |d| {
                    d.on_click(cx.listener(move |view, _, _, cx| {
                        cx.stop_propagation();
                        view.chat.confirm_close = Some(id);
                        cx.notify();
                    }))
                }),
            );
        }
        row.child(controls).into_any_element()
    }

    fn render_chat_setup(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.chat.setup.as_ref() else {
            return div().into_any_element();
        };
        let busy = form.busy;
        let enabled = !busy && self.chat_live();
        let mut card = div()
            .p(px(8.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .rounded(t.radius_md)
            .bg(t.surface_bg)
            .border_1()
            .border_color(t.border)
            .child(label("Start a chat", t));
        if self.chat.providers_busy {
            card = card.child(note("Discovering providers…", t));
        }
        if let Some(error) = self.chat.providers_error.clone() {
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.danger)
                            .child(error),
                    )
                    .child(ui_button("chat-providers-retry", "Retry", enabled, t).when(
                        enabled,
                        |d| {
                            d.on_click(
                                cx.listener(|view, _, _, cx| view.refresh_chat_providers(cx)),
                            )
                        },
                    )),
            );
        }
        if let Some(providers) = self.chat.providers.as_ref() {
            if providers.is_empty() {
                card = card.child(note(
                    "The daemon discovered no installable chat providers.",
                    t,
                ));
            }
            for (index, provider) in providers.iter().enumerate() {
                let id = provider.provider.id.clone();
                let selected = form.provider.as_deref() == Some(id.as_str());
                let unavailable = provider.unavailable_reason.clone().or_else(|| {
                    (!provider.capabilities.chat).then(|| "no chat capability".to_owned())
                });
                let version = provider
                    .version
                    .as_ref()
                    .map(|version| format!(" {version}"))
                    .unwrap_or_default();
                let mut row = div()
                    .id(SharedString::from(format!("chat-provider-{index}")))
                    .px(px(6.))
                    .py(px(4.))
                    .rounded(t.radius_sm)
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .when(selected, |d| d.bg(t.row_selected))
                    .text_size(t.text_size_sm)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(if selected { "●" } else { "○" })
                            .child(SharedString::from(format!(
                                "{}{version}",
                                provider.provider.name
                            )))
                            .child(
                                div()
                                    .text_color(t.text_tertiary)
                                    .child(SharedString::from(id.clone())),
                            ),
                    );
                row = match &unavailable {
                    Some(reason) => row.child(
                        div()
                            .text_color(t.warning)
                            .child(SharedString::from(format!("Unavailable: {reason}"))),
                    ),
                    None if enabled => row.cursor_pointer().hover({
                        let hover = t.raised_hover;
                        move |s| s.bg(hover)
                    }),
                    None => row,
                };
                if unavailable.is_none() && enabled {
                    row =
                        row.on_click(cx.listener(move |view, _, _, cx| {
                            view.choose_chat_provider(id.clone(), cx)
                        }));
                }
                card = card.child(row);
            }
        }
        card = card
            .child(label("Model", t))
            .child(form.model.clone())
            .child(label("Thinking", t))
            .child(form.thinking.clone())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(label("Permission", t))
                    .child(
                        ui_button(
                            "chat-permission",
                            permission_label(form.permission),
                            enabled,
                            t,
                        )
                        .when(enabled, |d| {
                            d.on_click(cx.listener(|view, _, _, cx| view.cycle_chat_permission(cx)))
                        }),
                    ),
            );
        if let Some(error) = form.error.clone() {
            card = card.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error),
            );
        }
        card = card.child(
            ui_button(
                "chat-start",
                if busy { "Starting…" } else { "Start chat" },
                enabled,
                t,
            )
            .when(enabled, |d| {
                d.on_click(cx.listener(|view, _, _, cx| view.start_chat_from_setup(cx)))
            }),
        );
        card.into_any_element()
    }
}

// ---- inbox dialog and toasts ---------------------------------------------

impl HostWorkspace {
    pub(super) fn render_chat_dialog(
        &self,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.chat.inbox_open {
            return None;
        }
        let items: Vec<InboxItem> = ordered_inbox(&self.chat.state.inbox)
            .into_iter()
            .cloned()
            .collect();
        let offline = !self.chat_live();
        let mut body = div().flex().flex_col().gap(px(6.));
        if self.chat.inbox_busy {
            body = body.child(note("Refreshing the inbox…", t));
        }
        if let Some(error) = self.chat.inbox_error.clone() {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error),
            );
        }
        if items.is_empty() {
            body = body.child(note("Nothing needs attention.", t));
        }
        for (index, item) in items.iter().enumerate() {
            let id = item.id;
            let session = item.session;
            let worktree = item.worktree;
            let dismiss_pending = self.chat.inflight.contains(&InFlight::Dismiss(id));
            let dismissable = !offline && !dismiss_pending;
            let item_for_open = item.clone();
            body = body.child(
                div()
                    .id(SharedString::from(format!("inbox-item-{index}")))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(t.radius_sm)
                    .bg(t.surface_bg)
                    .border_1()
                    .border_color(t.border)
                    .text_size(t.text_size_sm)
                    .child(ui_chip(
                        inbox_kind_label(item.kind),
                        inbox_kind_color(item.kind, t),
                        t,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .child(
                                div()
                                    .text_color(t.text_primary)
                                    .child(SharedString::from(item.title.clone())),
                            )
                            .child(div().text_color(t.text_tertiary).child(SharedString::from(
                                format!(
                                    "{} · {} · {}",
                                    self.chat.session_label(session),
                                    inbox_state_label(item.state),
                                    format_clock(item.created_ms)
                                ),
                            ))),
                    )
                    .child(div().flex_1())
                    .child(ui_chip(short_uuid(worktree), t.text_tertiary, t))
                    .child(
                        ui_button("inbox-open", "Open", true, t).on_click(cx.listener(
                            move |view, _, window, cx| {
                                view.open_chat_inbox_item(item_for_open.clone(), window, cx)
                            },
                        )),
                    )
                    .child(
                        ui_button(
                            "inbox-dismiss",
                            if dismiss_pending {
                                "Dismissing…"
                            } else {
                                "Dismiss"
                            },
                            dismissable,
                            t,
                        )
                        .when(dismissable, |d| {
                            d.on_click(
                                cx.listener(move |view, _, _, cx| view.dismiss_chat_inbox(id, cx)),
                            )
                        }),
                    ),
            );
        }
        let card = div()
            .id("chat-inbox-card")
            .w(px(680.))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .rounded(t.radius_lg)
            .bg(t.surface_raised)
            .border_1()
            .border_color(t.border)
            .shadow_lg()
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Inbox"),
            )
            .child(note(
                "Approvals, questions and completions the daemon recorded. Dismiss removes the item through the daemon; it never answers it.",
                t,
            ))
            .child(
                div()
                    .id("chat-inbox-body")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        ui_button("inbox-refresh", "Refresh", !offline, t).when(!offline, |d| {
                            d.on_click(cx.listener(|view, _, _, cx| view.refresh_chat_inbox(cx)))
                        }),
                    )
                    .child(div().flex_1())
                    .child(ui_button("inbox-close", "Close", true, t).on_click(cx.listener(
                        |view, _, _, cx| view.close_chat_inbox(cx),
                    ))),
            );
        Some(
            div()
                .id("chat-inbox-overlay")
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.45))
                .child(card)
                .into_any_element(),
        )
    }

    pub(super) fn render_chat_toasts(
        &self,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.chat.toasts.is_empty() {
            return None;
        }
        let mut stack = div()
            .absolute()
            .right(px(16.))
            .bottom(px(44.))
            .w(px(320.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .text_size(t.text_size_sm);
        for toast in self.chat.toasts.iter().take(TOASTS_SHOWN) {
            let session = toast.session;
            let seq = toast.seq;
            let color = toast_color(toast.kind, t);
            stack =
                stack.child(
                    div()
                        .id(SharedString::from(format!("toast-{session}-{seq}")))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .p(px(8.))
                        .rounded(t.radius_md)
                        .bg(t.surface_raised)
                        .border_1()
                        .border_color(color)
                        .shadow_lg()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .child(ui_chip("in-app", t.text_tertiary, t))
                                .child(
                                    div()
                                        .text_color(color)
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(SharedString::from(toast.title.clone())),
                                )
                                .child(div().flex_1())
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "toast-close-{session}-{seq}"
                                        )))
                                        .px(px(3.))
                                        .cursor_pointer()
                                        .text_color(t.text_tertiary)
                                        .child("×")
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            view.dismiss_toast(session, seq, cx)
                                        })),
                                ),
                        )
                        .child(div().text_color(t.text_secondary).child(SharedString::from(
                            format!("{} · event {}", self.chat.session_label(session), seq),
                        )))
                        .child(
                            div()
                                .id(SharedString::from(format!("toast-open-{session}-{seq}")))
                                .cursor_pointer()
                                .text_color(t.accent)
                                .child("Open chat")
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.open_toast(session, seq, window, cx)
                                })),
                        ),
                );
        }
        if self.chat.toasts.len() > 1 {
            stack = stack.child(div().flex().justify_end().child(
                ui_button("toasts-clear", "Clear all", true, t).on_click(cx.listener(
                    |view, _, _, cx| {
                        view.chat.toasts.clear();
                        cx.notify();
                    },
                )),
            ));
        }
        Some(stack.into_any_element())
    }
}

fn inbox_kind_label(kind: InboxKind) -> &'static str {
    match kind {
        InboxKind::Approval => "approval",
        InboxKind::Question => "question",
        InboxKind::Completed => "completed",
        InboxKind::Error => "error",
    }
}

fn inbox_kind_color(kind: InboxKind, t: &Theme) -> gpui::Hsla {
    match kind {
        InboxKind::Approval => t.warning,
        InboxKind::Question => t.accent,
        InboxKind::Completed => t.success,
        InboxKind::Error => t.danger,
    }
}

fn inbox_state_label(state: InboxState) -> &'static str {
    match state {
        InboxState::Unread => "unread",
        InboxState::Read => "read",
        InboxState::Dismissed => "dismissed",
    }
}

fn toast_color(kind: ToastKind, t: &Theme) -> gpui::Hsla {
    match kind {
        ToastKind::Approval => t.warning,
        ToastKind::Question => t.accent,
        ToastKind::Completed => t.success,
        ToastKind::Failed => t.danger,
    }
}

fn short_uuid(worktree: WorktreeId) -> String {
    let text = worktree.to_string();
    text.chars().take(8).collect()
}

// ---- markdown, diffs and small widgets -----------------------------------

fn render_markdown(text: &str, t: &Theme) -> Vec<AnyElement> {
    parse_markdown(text)
        .into_iter()
        .map(|block| match block {
            MarkdownBlock::Heading { level, text } => {
                let size = match level {
                    1 => px(18.),
                    2 => px(16.),
                    _ => px(14.),
                };
                div()
                    .pt(px(6.))
                    .text_size(size)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text_primary)
                    .child(SharedString::from(text))
                    .into_any_element()
            }
            MarkdownBlock::Paragraph(text) => paragraph(&text, t),
            MarkdownBlock::List { ordered, items } => {
                let mut list = div().flex().flex_col().gap(px(2.)).pl(px(4.));
                for (index, item) in items.into_iter().enumerate() {
                    let marker = if ordered {
                        format!("{}.", index + 1)
                    } else {
                        "•".to_owned()
                    };
                    list = list.child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_color(t.text_secondary)
                                    .child(SharedString::from(marker)),
                            )
                            .child(paragraph_element(&item, t)),
                    );
                }
                list.into_any_element()
            }
            MarkdownBlock::Code { language, text } => {
                let mut block = div()
                    .w_full()
                    .p(px(8.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .rounded(t.radius_sm)
                    .bg(t.chrome_bg)
                    .border_1()
                    .border_color(t.border)
                    .font_family(t.font_mono.clone())
                    .text_size(t.mono_size)
                    .line_height(px(18.));
                if let Some(language) = language {
                    block = block.child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_tertiary)
                            .child(SharedString::from(language)),
                    );
                }
                for line in text.split('\n') {
                    block = block.child(div().child(if line.is_empty() {
                        SharedString::from(" ")
                    } else {
                        SharedString::from(line.to_owned())
                    }));
                }
                block.into_any_element()
            }
        })
        .collect()
}

fn paragraph(text: &str, t: &Theme) -> AnyElement {
    div().child(paragraph_element(text, t)).into_any_element()
}

fn paragraph_element(text: &str, t: &Theme) -> AnyElement {
    let spans = inline_spans(text);
    if spans.len() == 1 && !spans[0].code {
        return div()
            .text_color(t.text_primary)
            .child(SharedString::from(spans[0].text.clone()))
            .into_any_element();
    }
    let mut row = div()
        .flex()
        .flex_wrap()
        .gap(px(2.))
        .items_center()
        .text_color(t.text_primary);
    for span in spans {
        if span.code {
            row = row.child(
                div()
                    .px(px(4.))
                    .rounded(px(3.))
                    .bg(t.control_bg)
                    .font_family(t.font_mono.clone())
                    .text_size(t.mono_size)
                    .text_color(t.text_primary)
                    .child(SharedString::from(span.text)),
            );
        } else {
            row = row.child(div().child(SharedString::from(span.text)));
        }
    }
    row.into_any_element()
}

fn mono_block(text: &str, t: &Theme) -> AnyElement {
    let mut block = div()
        .w_full()
        .p(px(6.))
        .flex()
        .flex_col()
        .rounded(t.radius_sm)
        .bg(t.chrome_bg)
        .border_1()
        .border_color(t.border)
        .font_family(t.font_mono.clone())
        .text_size(t.mono_size)
        .line_height(px(17.))
        .text_color(t.text_primary);
    for line in text.split('\n') {
        block = block.child(div().child(if line.is_empty() {
            SharedString::from(" ")
        } else {
            SharedString::from(line.to_owned())
        }));
    }
    block.into_any_element()
}

fn diff_block(source: &str, truncated: bool, t: &Theme) -> AnyElement {
    let parsed = parse_unified_diff(source);
    let total = parsed.rows.len();
    let mut block = div()
        .w_full()
        .rounded(t.radius_sm)
        .border_1()
        .border_color(t.border)
        .overflow_hidden()
        .font_family(t.font_mono.clone())
        .text_size(t.mono_size)
        .line_height(px(17.));
    for row in parsed.rows.iter().take(MAX_RENDERED_DIFF_ROWS) {
        let (background, color, marker) = match row.kind {
            DiffKind::Added => (t.success.opacity(0.12), t.success, "+"),
            DiffKind::Removed => (t.danger.opacity(0.12), t.danger, "-"),
            DiffKind::Hunk => (t.accent.opacity(0.10), t.accent, "@"),
            DiffKind::File => (t.control_bg, t.text_tertiary, ""),
            DiffKind::Note => (t.control_bg, t.text_tertiary, "\\"),
            DiffKind::Context => (t.surface_bg, t.text_secondary, " "),
        };
        let numbers = match (row.old, row.new) {
            (Some(old), Some(new)) => format!("{old:>4} {new:>4}"),
            (Some(old), None) => format!("{old:>4}     "),
            (None, Some(new)) => format!("     {new:>4}"),
            (None, None) => "          ".to_owned(),
        };
        block = block.child(
            div()
                .flex()
                .items_start()
                .gap(px(6.))
                .px(px(6.))
                .bg(background)
                .child(
                    div()
                        .text_color(t.text_tertiary)
                        .child(SharedString::from(numbers)),
                )
                .child(div().text_color(color).child(marker))
                .child(
                    div()
                        .text_color(color)
                        .child(SharedString::from(row.text.clone())),
                ),
        );
    }
    if total > MAX_RENDERED_DIFF_ROWS || truncated {
        block = block.child(
            div()
                .px(px(6.))
                .py(px(2.))
                .text_color(t.text_tertiary)
                .child(SharedString::from(format!(
                    "… {} more diff rows (bounded)",
                    total.saturating_sub(MAX_RENDERED_DIFF_ROWS)
                ))),
        );
    }
    block.into_any_element()
}

fn label(text: &str, t: &Theme) -> AnyElement {
    div()
        .text_size(t.text_size_sm)
        .font_weight(FontWeight::MEDIUM)
        .text_color(t.text_secondary)
        .child(SharedString::from(text.to_owned()))
        .into_any_element()
}

fn note(text: &str, t: &Theme) -> AnyElement {
    div()
        .text_size(t.text_size_sm)
        .text_color(t.text_secondary)
        .child(SharedString::from(text.to_owned()))
        .into_any_element()
}

fn ui_chip(label: impl Into<SharedString>, color: gpui::Hsla, t: &Theme) -> gpui::Div {
    let label: SharedString = label.into();
    div()
        .px(px(6.))
        .py(px(1.))
        .rounded(t.radius_sm)
        .bg(t.control_bg)
        .text_size(t.text_size_sm)
        .text_color(color)
        .child(label)
}

fn ui_button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let id: SharedString = id.into();
    let label: SharedString = label.into();
    let hover = t.raised_hover;
    div()
        .id(id)
        .px(px(8.))
        .py(px(3.))
        .rounded(t.radius_sm)
        .bg(if enabled { t.control_bg } else { t.surface_bg })
        .text_size(t.text_size_sm)
        .text_color(if enabled {
            t.text_primary
        } else {
            t.text_tertiary
        })
        .when(enabled, |d| d.cursor_pointer().hover(move |s| s.bg(hover)))
        .child(label)
}
