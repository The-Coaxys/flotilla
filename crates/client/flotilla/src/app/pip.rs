//! Picture-in-picture: an always-on-top window for one pinned surface
//! (spec §8.1).
//!
//! The main window decides which surface is pinned; this module owns the
//! second window's content. Terminals and run targets are live views over the
//! same mirror source the main window uses, chat is a bounded transcript of
//! the pinned session kept current from the mirror, and a review is a bounded
//! snapshot of the selected worktree's pull request and review threads. The
//! pinned surface never follows the main window: switching tabs there leaves
//! the PiP content alone.
use std::sync::Arc;

use fl_model::chat::{ChatEvent, EventRecord, TextUpdate, TurnOutcome};
use fl_model::forge::{ForgeState, PrState};
use fl_model::review::ReviewState;
use fl_model::workspace::LayoutScope;
use flc_sync::ChatState;
use flc_terminal::{TerminalSource, TerminalView, TerminalViewOptions};
use flc_ui::theme::{ActiveTheme, Theme};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, SharedString, Subscription, Task,
    Window, div, px,
};
use uuid::Uuid;

/// Which surface the pinned window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipKind {
    Terminal,
    Chat,
    Run,
    Review,
}

impl PipKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            PipKind::Terminal => "Terminal",
            PipKind::Chat => "Chat",
            PipKind::Run => "Run",
            PipKind::Review => "Review",
        }
    }
}

/// Identity of the pinned surface, kept in the app so the Pin control can
/// show its state and the same surface does not open twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipPin {
    pub host: String,
    pub scope: LayoutScope,
    pub kind: PipKind,
    /// Chat sessions are identified by id; other surfaces leave this `None`.
    pub session: Option<Uuid>,
}

/// Everything the second window needs to render its surface.
pub(crate) enum PipContent {
    Terminal {
        kind: PipKind,
        title: String,
        subtitle: String,
        source: Arc<dyn TerminalSource>,
        options: TerminalViewOptions,
    },
    Chat {
        title: String,
        subtitle: String,
        session: Uuid,
        state: Arc<ChatState>,
        changes: tokio::sync::watch::Receiver<Arc<ChatState>>,
    },
    Review {
        title: String,
        subtitle: String,
        lines: Vec<String>,
    },
}

impl PipContent {
    pub(crate) fn kind(&self) -> PipKind {
        match self {
            PipContent::Terminal { kind, .. } => *kind,
            PipContent::Chat { .. } => PipKind::Chat,
            PipContent::Review { .. } => PipKind::Review,
        }
    }

    pub(crate) fn session(&self) -> Option<Uuid> {
        match self {
            PipContent::Chat { session, .. } => Some(*session),
            _ => None,
        }
    }

    pub(crate) fn title(&self) -> &str {
        match self {
            PipContent::Terminal { title, .. }
            | PipContent::Chat { title, .. }
            | PipContent::Review { title, .. } => title,
        }
    }
}

/// Build the review lines shown in a PiP: the pull request's state and the
/// open review threads, bounded so a noisy worktree cannot grow the window.
pub(crate) fn review_lines(
    forge: Option<&ForgeState>,
    review: Option<&ReviewState>,
    limit: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    match forge {
        None => lines.push("No forge state yet".to_string()),
        Some(state) => {
            if let Some(reason) = &state.unavailable {
                lines.push(format!("Forge unavailable: {reason}"));
            }
            match &state.pr {
                None => lines.push("No pull request for this branch".to_string()),
                Some(pr) => {
                    lines.push(format!("#{} {} ({:?})", pr.number, pr.title, pr.state));
                    if pr.state == PrState::Open {
                        if let Some(review) = review {
                            lines.push(format!("{} review threads", review.threads.len()));
                            for thread in review
                                .threads
                                .iter()
                                .take(limit.saturating_sub(lines.len()))
                            {
                                let first = thread
                                    .comments
                                    .first()
                                    .map(|comment| comment.body.as_str())
                                    .unwrap_or("");
                                let path = thread.anchor.path.rsplit('/').next().unwrap_or("");
                                lines.push(format!(
                                    "{}:{:?} {:?} — {}",
                                    path,
                                    thread.anchor.lines,
                                    thread.status,
                                    truncate(first, 96)
                                ));
                            }
                        } else {
                            lines.push("Review threads have not loaded".to_string());
                        }
                    }
                }
            }
        }
    }
    lines.truncate(limit);
    lines
}

fn truncate(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= limit {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// Bounded plain-text transcript of one session for the PiP chat surface.
pub(crate) fn chat_lines(state: &ChatState, session: Uuid, limit: usize) -> Vec<String> {
    let records = state.session_events(session);
    // Keep the newest `limit` renderable lines, in chronological order.
    let mut lines: Vec<String> = records
        .iter()
        .rev()
        .filter_map(chat_line)
        .take(limit)
        .collect();
    lines.reverse();
    lines
}

fn chat_line(record: &EventRecord) -> Option<String> {
    let line = match &record.event {
        ChatEvent::UserMessage { text, .. } => format!("You: {}", truncate(text, 160)),
        ChatEvent::AssistantText { text, update, .. } => match update {
            TextUpdate::Delta => return None,
            _ => format!("Assistant: {}", truncate(text, 160)),
        },
        ChatEvent::ApprovalRequest { tool, .. } => format!("Approval needed: {tool}"),
        ChatEvent::QuestionRequest { questions, .. } => {
            format!("Question: {}", questions.len())
        }
        ChatEvent::TurnEnd { outcome, .. } => match outcome {
            TurnOutcome::Completed => "Turn completed".to_string(),
            TurnOutcome::Cancelled => "Turn cancelled".to_string(),
            TurnOutcome::Failed => "Turn failed".to_string(),
        },
        ChatEvent::Error { message, .. } => format!("Error: {}", truncate(message, 160)),
        ChatEvent::ProcessExited { code, .. } => format!("Process exited: {code:?}"),
        _ => return None,
    };
    Some(line)
}

/// The second window's root view.
pub struct PipView {
    title: SharedString,
    kind: PipKind,
    subtitle: SharedString,
    terminal: Option<Entity<TerminalView>>,
    chat: Option<(Uuid, Arc<ChatState>)>,
    review: Vec<String>,
    focus: FocusHandle,
    _changes: Option<Task<()>>,
    _settings: Subscription,
}

impl PipView {
    pub(crate) fn new(
        content: PipContent,
        settings: Entity<crate::app::settings::Settings>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = content.kind();
        let title: SharedString = content.title().to_string().into();
        let subtitle: SharedString = match &content {
            PipContent::Terminal { subtitle, .. }
            | PipContent::Chat { subtitle, .. }
            | PipContent::Review { subtitle, .. } => subtitle.clone(),
        }
        .into();
        let mut terminal = None;
        let mut chat = None;
        let mut review = Vec::new();
        let mut changes = None;
        match content {
            PipContent::Terminal {
                source, options, ..
            } => {
                terminal = Some(cx.new(|cx| TerminalView::new(source, options, window, cx)));
            }
            PipContent::Chat {
                session,
                state,
                changes: rx,
                ..
            } => {
                chat = Some((session, state));
                changes = Some(cx.spawn(async move |this, cx| {
                    let mut rx = rx;
                    loop {
                        let state = rx.borrow_and_update().clone();
                        if this
                            .update(cx, |view, cx| {
                                view.chat = Some((session, state));
                                cx.notify();
                            })
                            .is_err()
                            || rx.changed().await.is_err()
                        {
                            break;
                        }
                    }
                }));
            }
            PipContent::Review { lines, .. } => review = lines,
        }
        let settings_watch = cx.observe(&settings, |_, _, cx| cx.notify());
        Self {
            title,
            kind,
            subtitle,
            terminal,
            chat,
            review,
            focus: cx.focus_handle(),
            _changes: changes,
            _settings: settings_watch,
        }
    }

    fn render_header(
        &self,
        t: &Theme,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .h(px(30.))
            .flex_none()
            .bg(t.chrome_bg)
            .border_b_1()
            .border_color(t.border)
            .child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.accent)
                    .child(self.kind.label()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(t.text_size_sm)
                    .child(self.title.clone()),
            )
            .child(
                div()
                    .px(px(6.))
                    .rounded(t.radius_sm)
                    .bg(t.control_bg)
                    .text_size(t.text_size_sm)
                    .text_color(t.text_secondary)
                    .child("Pinned"),
            )
            .child(
                div()
                    .id("pip-close")
                    .px(px(6.))
                    .rounded(t.radius_sm)
                    .cursor_pointer()
                    .text_color(t.text_tertiary)
                    .hover(move |s| s.bg(t.raised_hover))
                    .child("×")
                    .on_click(move |_, window: &mut Window, _| window.remove_window()),
            )
            .into_any_element()
    }

    fn render_body(&self, t: &Theme, _cx: &mut Context<Self>) -> AnyElement {
        if let Some(terminal) = &self.terminal {
            return terminal.clone().into_any_element();
        }
        let lines: Vec<String> = match &self.chat {
            Some((session, state)) => chat_lines(state, *session, 300),
            None => self.review.clone(),
        };
        let mut body = div()
            .id("pip-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(8.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .bg(t.surface_bg);
        if lines.is_empty() {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.text_secondary)
                    .child("Nothing to show yet"),
            );
        }
        for (index, line) in lines.into_iter().enumerate() {
            body = body.child(
                div()
                    .id(SharedString::from(format!("pip-line-{index}")))
                    .text_size(t.text_size_sm)
                    .text_color(t.text_primary)
                    .child(SharedString::from(line)),
            );
        }
        body.into_any_element()
    }
}

impl Focusable for PipView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PipView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface_bg)
            .text_color(t.text_primary)
            .font_family(t.font_ui.clone())
            .text_size(t.text_size)
            .track_focus(&self.focus)
            .child(self.render_header(&t, window, cx))
            .child(self.render_body(&t, cx))
            .child(
                div()
                    .flex_none()
                    .px(px(10.))
                    .py(px(4.))
                    .border_t_1()
                    .border_color(t.border)
                    .text_size(t.text_size_sm)
                    .text_color(t.text_tertiary)
                    .child(self.subtitle.clone()),
            )
    }
}

/// On Windows gpui maps every window kind to a normal application window, so
/// the always-on-top flag is applied to the native handle after creation.
#[cfg(windows)]
pub(crate) fn make_topmost(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
    };
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
    if let Err(error) = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    } {
        tracing::warn!("cannot keep the picture-in-picture window on top: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::ReviewThreadId;
    use fl_model::chat::{ChatSession, PermissionMode, SessionState, StatusSource};
    use fl_model::forge::{MergeMethods, PullRequest};
    use fl_model::review::{LineRange, ReviewAnchor, ReviewOrigin, ReviewStatus, ReviewThread};
    use fl_model::{WorktreeId, review::ReviewTriage};

    fn session() -> ChatSession {
        ChatSession {
            id: Uuid::from_u128(1),
            worktree: WorktreeId::from_bytes([2; 16]),
            provider: "claude".into(),
            label: "Claude".into(),
            status: SessionState::Idle,
            status_source: StatusSource::Structured,
            model: None,
            thinking: None,
            permission_mode: PermissionMode::Ask,
            resume: None,
            seq: 0,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    fn record(sequence: u64, event: ChatEvent) -> EventRecord {
        EventRecord {
            schema_version: 1,
            session_id: Uuid::from_u128(1),
            incarnation: Uuid::nil(),
            sequence,
            timestamp_ms: sequence,
            native_session_id: None,
            turn_id: None,
            event,
        }
    }

    #[test]
    fn chat_lines_are_bounded_and_keep_the_newest_events() {
        let mut state = ChatState {
            sessions: std::collections::HashMap::new(),
            events: std::collections::HashMap::new(),
            inbox: Vec::new(),
            live: true,
        };
        state.sessions.insert(Uuid::from_u128(1), session());
        let records = (0..50)
            .map(|n| {
                record(
                    n + 1,
                    ChatEvent::UserMessage {
                        id: format!("m{n}"),
                        text: format!("message {n}"),
                    },
                )
            })
            .collect();
        state.events.insert(Uuid::from_u128(1), records);
        let lines = chat_lines(&state, Uuid::from_u128(1), 5);
        assert_eq!(lines.len(), 5);
        assert!(lines[4].contains("message 49"));
        assert!(lines[0].contains("message 45"));
        // Deltas never produce duplicate PiP lines.
        state
            .events
            .get_mut(&Uuid::from_u128(1))
            .unwrap()
            .push(record(
                51,
                ChatEvent::AssistantText {
                    id: "a".into(),
                    update: TextUpdate::Delta,
                    text: "partial".into(),
                },
            ));
        assert_eq!(chat_lines(&state, Uuid::from_u128(1), 2).len(), 2);
    }

    #[test]
    fn review_lines_name_the_pull_request_and_bound_threads() {
        let worktree = WorktreeId::from_bytes([2; 16]);
        let pr = PullRequest {
            number: 12,
            url: String::new(),
            title: "A change".into(),
            state: PrState::Open,
            draft: false,
            base: "main".into(),
            head: "feature".into(),
            mergeable: fl_model::forge::Mergeable::Unknown,
            review: None,
            checks: Vec::new(),
            check_rollup: None,
            unresolved_threads: 0,
        };
        let forge = ForgeState {
            worktree,
            unavailable: None,
            pr: Some(pr),
            methods: MergeMethods {
                squash: true,
                merge: true,
                rebase: false,
            },
            fetched_ms: 1,
        };
        let thread = ReviewThread {
            id: ReviewThreadId::new(),
            anchor: ReviewAnchor {
                worktree,
                path: "src/main.rs".into(),
                lines: Some(LineRange { start: 1, end: 2 }),
            },
            origin: ReviewOrigin::User,
            triage: ReviewTriage::Undecided,
            status: ReviewStatus::Open,
            comments: Vec::new(),
        };
        let review = ReviewState {
            worktree,
            rev: 1,
            threads: vec![thread.clone(), thread],
        };
        let lines = review_lines(Some(&forge), Some(&review), 3);
        assert_eq!(lines[0], "#12 A change (Open)");
        assert!(lines.iter().any(|line| line.contains("2 review threads")));
        assert_eq!(lines.len(), 3);
        assert_eq!(review_lines(None, None, 8), vec!["No forge state yet"]);
    }

    #[test]
    fn a_truncated_line_stays_short() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("", 3), "");
    }
}
