//! Bounded hover cards for project, worktree and conversation rows.
//!
//! The card is a data snapshot built from the host's real mirrors, so a row
//! shows exactly the fields the client already knows: paths, branches, the
//! working-tree summary, and the provider/status/sequence of a chat session.
use fl_model::chat::{ChatSession, SessionState};
use fl_model::workspace::{Project, ProjectKind, Worktree, WorktreeStatus};
use flc_ui::theme::{ActiveTheme, Theme};
use gpui::prelude::*;
use gpui::{AnyElement, Context, FontWeight, Pixels, SharedString, div, px};

use super::Workspace;

/// Most fields a card shows; extra detail is dropped rather than growing.
pub(crate) const MAX_LINES: usize = 8;

/// A card positioned at the pointer's window y when the row was entered.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Card {
    pub(crate) y: Pixels,
    pub(crate) title: SharedString,
    pub(crate) subtitle: Option<SharedString>,
    pub(crate) lines: Vec<(SharedString, SharedString)>,
}

impl Card {
    fn new(title: impl Into<SharedString>, y: Pixels) -> Self {
        Self {
            y,
            title: title.into(),
            subtitle: None,
            lines: Vec::new(),
        }
    }

    fn subtitle(mut self, subtitle: impl Into<SharedString>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    fn line(mut self, label: &'static str, value: impl Into<SharedString>) -> Self {
        if self.lines.len() < MAX_LINES {
            self.lines.push((label.into(), value.into()));
        }
        self
    }
}

/// A worktree's card: path, branch, target and the last status summary.
pub(crate) fn worktree_card(
    worktree: &Worktree,
    project: Option<&Project>,
    status: Option<&WorktreeStatus>,
    y: Pixels,
) -> Card {
    let mut card = Card::new(worktree.label.clone(), y);
    if let Some(project) = project {
        card = card.subtitle(project.name.clone());
    }
    card = card.line("Path", worktree.path.clone());
    card = card.line(
        "Branch",
        worktree.branch.clone().unwrap_or_else(|| "detached".into()),
    );
    if let Some(target) = &worktree.target {
        card = card.line("Target", target.clone());
    }
    if let Some(summary) = status.map(|status| &status.summary) {
        card = card.line(
            "Changes",
            format!(
                "{}{}",
                if summary.dirty { "dirty" } else { "clean" },
                if summary.conflicted {
                    ", conflicted"
                } else {
                    ""
                }
            ),
        );
        card = card.line(
            "Ahead/behind",
            format!("↑{} ↓{}", summary.ahead, summary.behind),
        );
        card = card.line("Lines", format!("+{} −{}", summary.added, summary.removed));
    } else {
        card = card.line("Status", "not reported yet");
    }
    card = card.line(
        "Kind",
        if worktree.is_primary {
            "primary checkout"
        } else {
            "linked worktree"
        },
    );
    card
}

/// A project's card: primary checkout, repository kind and defaults.
pub(crate) fn project_card(project: &Project, worktrees: usize, y: Pixels) -> Card {
    let kind = match project.kind {
        ProjectKind::Git => "git repository",
        ProjectKind::Folder => "plain folder",
    };
    Card::new(project.name.clone(), y)
        .subtitle(kind)
        .line("Path", project.path.clone())
        .line(
            "Default branch",
            project.default_branch.clone().unwrap_or_else(|| "—".into()),
        )
        .line("Worktree root", project.worktree_root.clone())
        .line("Worktrees", worktrees.to_string())
}

/// A conversation's card: provider, status and the durable sequence.
pub(crate) fn session_card(session: &ChatSession, worktree: Option<&Worktree>, y: Pixels) -> Card {
    let status = match &session.status {
        SessionState::Starting => "starting".to_string(),
        SessionState::Idle => "idle".to_string(),
        SessionState::Running => "running".to_string(),
        SessionState::AwaitingInput => "awaiting input".to_string(),
        SessionState::Closed => "closed".to_string(),
        SessionState::Failed { message } => format!("failed: {message}"),
    };
    let mut card = Card::new(session.label.clone(), y)
        .subtitle(session.provider.clone())
        .line("Status", status)
        .line("Sequence", format!("seq {}", session.seq))
        .line(
            "Permissions",
            match session.permission_mode {
                fl_model::chat::PermissionMode::Ask => "ask",
                fl_model::chat::PermissionMode::ReadOnly => "read-only",
            },
        );
    if let Some(model) = &session.model {
        card = card.line("Model", model.clone());
    }
    if let Some(worktree) = worktree {
        card = card.line("Worktree", worktree.label.clone());
    }
    card.line(
        "Source",
        match session.status_source {
            fl_model::chat::StatusSource::Structured => "structured",
            fl_model::chat::StatusSource::Hook => "hook",
            fl_model::chat::StatusSource::Heuristic => "heuristic",
        },
    )
}

/// The card element, in window coordinates next to the hovered row.
pub(crate) fn element(card: &Card, t: &Theme) -> AnyElement {
    let mut body = div().flex().flex_col().gap(px(3.));
    for (label, value) in &card.lines {
        body = body.child(
            div()
                .flex()
                .gap(px(8.))
                .text_size(t.text_size_sm)
                .child(
                    div()
                        .w(px(72.))
                        .flex_none()
                        .text_color(t.text_tertiary)
                        .child(label.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(t.text_primary)
                        .child(value.clone()),
                ),
        );
    }
    div()
        .id("hover-card")
        .absolute()
        .left(px(266.))
        .top(card.y + px(14.))
        .w(px(390.))
        .p(px(10.))
        .flex()
        .flex_col()
        .gap(px(5.))
        .rounded(t.radius_md)
        .bg(t.surface_raised)
        .border_1()
        .border_color(t.border)
        .shadow_lg()
        .text_size(t.text_size_sm)
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(t.text_size)
                .child(card.title.clone()),
        )
        .when_some(card.subtitle.clone(), |d, subtitle| {
            d.child(div().text_color(t.text_secondary).child(subtitle))
        })
        .child(body)
        .into_any_element()
}

impl Workspace {
    /// The selected host's hover card, if one is showing.
    pub(super) fn render_hover_card(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let host = self
            .selected
            .and_then(|index| self.hosts.get(index))
            .and_then(|host| host.workspace.as_ref())?;
        let card = host.read(cx).hover_card()?;
        Some(element(&card, cx.theme()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::chat::{PermissionMode, StatusSource};
    use fl_model::workspace::GitSummary;
    use fl_model::{ProjectId, WorktreeId};

    fn worktree() -> Worktree {
        Worktree {
            id: WorktreeId::new(),
            project: ProjectId::new(),
            path: "/srv/repo-wt".into(),
            branch: Some("dev".into()),
            target: Some("main".into()),
            label: "dev".into(),
            is_primary: false,
            created_ms: 0,
            last_active_ms: 0,
        }
    }

    fn summary() -> GitSummary {
        GitSummary {
            branch: Some("dev".into()),
            head: None,
            upstream: Some("origin/dev".into()),
            ahead: 2,
            behind: 1,
            added: 12,
            removed: 3,
            dirty: true,
            conflicted: false,
        }
    }

    fn line<'a>(card: &'a Card, label: &str) -> Option<&'a str> {
        card.lines
            .iter()
            .find(|(name, _)| name == label)
            .map(|(_, value)| value.as_ref())
    }

    #[test]
    fn worktree_card_reports_real_git_fields() {
        let worktree = worktree();
        let status = WorktreeStatus {
            worktree: worktree.id,
            summary: summary(),
            files: Vec::new(),
            at_ms: 1,
        };
        let card = worktree_card(&worktree, None, Some(&status), px(10.));
        assert_eq!(card.title.to_string(), "dev");
        assert_eq!(line(&card, "Path"), Some("/srv/repo-wt"));
        assert_eq!(line(&card, "Branch"), Some("dev"));
        assert_eq!(line(&card, "Target"), Some("main"));
        assert_eq!(line(&card, "Changes"), Some("dirty"));
        assert_eq!(line(&card, "Ahead/behind"), Some("↑2 ↓1"));
        assert_eq!(line(&card, "Lines"), Some("+12 −3"));
        assert_eq!(line(&card, "Kind"), Some("linked worktree"));
        assert!(card.lines.len() <= MAX_LINES);
    }

    #[test]
    fn worktree_card_marks_missing_status_instead_of_zeroes() {
        let card = worktree_card(&worktree(), None, None, px(0.));
        assert_eq!(line(&card, "Status"), Some("not reported yet"));
    }

    #[test]
    fn session_card_reports_provider_status_and_sequence() {
        let session = ChatSession {
            id: uuid::Uuid::new_v4(),
            worktree: WorktreeId::new(),
            provider: "claude".into(),
            label: "fix the flaky test".into(),
            status: SessionState::AwaitingInput,
            status_source: StatusSource::Structured,
            model: Some("opus".into()),
            thinking: None,
            permission_mode: PermissionMode::Ask,
            resume: None,
            seq: 42,
            created_ms: 0,
            updated_ms: 0,
        };
        let card = session_card(&session, None, px(0.));
        assert_eq!(card.title.to_string(), "fix the flaky test");
        assert_eq!(
            card.subtitle.clone().map(|s| s.to_string()),
            Some("claude".to_string())
        );
        assert_eq!(line(&card, "Status"), Some("awaiting input"));
        assert_eq!(line(&card, "Sequence"), Some("seq 42"));
        assert_eq!(line(&card, "Model"), Some("opus"));
        assert_eq!(line(&card, "Source"), Some("structured"));
    }

    #[test]
    fn cards_stop_at_the_line_bound() {
        let mut card = Card::new("x", px(0.));
        for _ in 0..MAX_LINES + 4 {
            card = card.line("Field", "value");
        }
        assert_eq!(card.lines.len(), MAX_LINES);
    }
}
