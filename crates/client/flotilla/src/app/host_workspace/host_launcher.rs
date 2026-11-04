//! Empty-view launcher: tool cards plus a Recent list built from real state.
//!
//! Shown in the center when the current scope has no open documents, no chat
//! tab and no terminal tabs, instead of a blank terminal surface.
use fl_model::chat::ChatSession;
use fl_model::workspace::{LayoutScope, Worktree};
use flc_ui::theme::Theme;
use gpui::prelude::*;
use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, px};

use super::HostWorkspace;
use crate::app::palette::{Action, Panel};

/// Recent rows the launcher lists.
pub(super) const RECENT_LIMIT: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tool {
    AddProject,
    NewTerminal,
    Chat,
    Files,
}

pub(super) const TOOLS: [Tool; 4] = [Tool::AddProject, Tool::NewTerminal, Tool::Chat, Tool::Files];

impl Tool {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::AddProject => "Add project",
            Self::NewTerminal => "New terminal",
            Self::Chat => "Chat",
            Self::Files => "Files",
        }
    }

    pub(super) fn detail(self) -> &'static str {
        match self {
            Self::AddProject => "Open a folder or clone a repository",
            Self::NewTerminal => "Start a shell in this worktree",
            Self::Chat => "Talk to an agent about this worktree",
            Self::Files => "Browse this worktree's files and changes",
        }
    }

    pub(super) fn action(self) -> Action {
        match self {
            Self::AddProject => Action::AddProject,
            Self::NewTerminal => Action::NewTerminal,
            Self::Chat => Action::OpenPanel(Panel::Chat),
            Self::Files => Action::OpenPanel(Panel::Files),
        }
    }
}

/// One row of the launcher's Recent list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Recent {
    pub(super) title: String,
    pub(super) detail: String,
    pub(super) at_ms: u64,
    pub(super) action: Action,
}

/// Merge worktrees and conversations newest-first, deterministically.
///
/// Worktrees order by `last_active_ms`, conversations by `updated_ms`; equal
/// stamps fall back to the title so the list never reshuffles between draws.
pub(super) fn recent_entries(
    worktrees: &[Worktree],
    sessions: &[ChatSession],
    limit: usize,
) -> Vec<Recent> {
    let mut recent: Vec<Recent> = worktrees
        .iter()
        .map(|worktree| Recent {
            title: worktree.label.clone(),
            detail: format!(
                "worktree · {}",
                worktree.branch.clone().unwrap_or_else(|| "detached".into())
            ),
            at_ms: worktree.last_active_ms,
            action: Action::SelectScope(LayoutScope::Worktree(worktree.id)),
        })
        .chain(sessions.iter().map(|session| Recent {
            title: session.label.clone(),
            detail: format!("chat · {}", session.provider),
            at_ms: session.updated_ms,
            action: Action::OpenChat(session.id),
        }))
        .collect();
    recent.sort_by(|a, b| {
        b.at_ms
            .cmp(&a.at_ms)
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    recent.truncate(limit);
    recent
}

impl HostWorkspace {
    /// True when the center should show the launcher instead of the shell.
    pub(super) fn launcher_visible(&self) -> bool {
        if self
            .documents
            .get(&self.scope)
            .is_some_and(|documents| !documents.open.is_empty())
        {
            return false;
        }
        if self.chat.active_for(self.scope).is_some() {
            return false;
        }
        let state = self.sync.layout(self.scope).workspace();
        state
            .projected
            .as_ref()
            .is_some_and(|layout| layout.views.iter().all(|view| view.tabs.is_empty()))
    }

    fn recent_rows(&self) -> Vec<Recent> {
        let worktrees = self
            .state
            .tree
            .as_ref()
            .map(|tree| tree.worktrees.as_slice())
            .unwrap_or(&[]);
        let sessions = self.chat.conversations();
        recent_entries(worktrees, &sessions, RECENT_LIMIT)
    }

    pub(super) fn render_launcher(
        &mut self,
        _window: &mut Window,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scope = self.scope;
        let where_ = match scope {
            LayoutScope::Home => "the host".to_string(),
            LayoutScope::Worktree(id) => self
                .state
                .worktree(id)
                .map(|worktree| worktree.label.clone())
                .unwrap_or_else(|| "this worktree".into()),
        };
        let cards: Vec<AnyElement> = TOOLS
            .iter()
            .map(|tool| {
                let tool = *tool;
                let hover = t.raised_hover;
                div()
                    .id(SharedString::from(format!("launcher-{}", tool.title())))
                    .w(px(300.))
                    .h(px(74.))
                    .p(px(14.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(4.))
                    .rounded(t.radius_md)
                    .bg(t.surface_raised)
                    .border_1()
                    .border_color(t.border)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(
                        div()
                            .text_size(t.text_size)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tool.title()),
                    )
                    .child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_secondary)
                            .child(tool.detail()),
                    )
                    .on_click(cx.listener(move |host, _, window, cx| {
                        host.run_palette_action(tool.action(), window, cx)
                    }))
                    .into_any_element()
            })
            .collect();
        let recent = self.recent_rows();
        let mut recent_column = div().flex().flex_col().gap(px(2.)).w(px(624.));
        if recent.is_empty() {
            recent_column = recent_column.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.text_tertiary)
                    .child("Nothing recent yet."),
            );
        }
        for (index, row) in recent.into_iter().enumerate() {
            let hover = t.raised_hover;
            let action = row.action.clone();
            recent_column = recent_column.child(
                div()
                    .id(("launcher-recent", index))
                    .h(px(28.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .rounded(t.radius_sm)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(div().flex_1().min_w(px(0.)).truncate().child(row.title))
                    .child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_tertiary)
                            .child(row.detail),
                    )
                    .on_click(cx.listener(move |host, _, window, cx| {
                        host.run_palette_action(action.clone(), window, cx)
                    })),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(format!("Start something in {where_}"))),
                    )
                    .child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.text_secondary)
                            .child("No tabs are open here yet."),
                    ),
            )
            .child(
                div()
                    .w(px(624.))
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .children(cards),
            )
            .child(
                div()
                    .w(px(624.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .px(px(10.))
                            .text_size(t.text_size_sm)
                            .text_color(t.text_tertiary)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("RECENT"),
                    )
                    .child(recent_column),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::WorktreeId;
    use uuid::Uuid;

    fn worktree(label: &str, active: u64) -> Worktree {
        Worktree {
            id: WorktreeId::new(),
            project: fl_model::ProjectId::new(),
            path: format!("/srv/{label}"),
            branch: Some(label.into()),
            target: None,
            label: label.into(),
            is_primary: false,
            created_ms: 0,
            last_active_ms: active,
        }
    }

    fn session(label: &str, updated: u64) -> ChatSession {
        ChatSession {
            id: Uuid::new_v4(),
            worktree: WorktreeId::new(),
            provider: "codex".into(),
            label: label.into(),
            status: fl_model::chat::SessionState::Idle,
            status_source: fl_model::chat::StatusSource::Structured,
            model: None,
            thinking: None,
            permission_mode: fl_model::chat::PermissionMode::Ask,
            resume: None,
            seq: 0,
            created_ms: 0,
            updated_ms: updated,
        }
    }

    #[test]
    fn recent_merges_worktrees_and_conversations_newest_first() {
        let worktrees = [worktree("old", 10), worktree("new", 300)];
        let sessions = [session("talk", 200), session("fresh", 400)];
        let recent = recent_entries(&worktrees, &sessions, 6);
        let titles: Vec<_> = recent.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, ["fresh", "new", "talk", "old"]);
        assert!(matches!(recent[0].action, Action::OpenChat(_)));
        assert!(matches!(
            recent[1].action,
            Action::SelectScope(LayoutScope::Worktree(_))
        ));
    }

    #[test]
    fn recent_is_bounded_and_stable_on_ties() {
        let worktrees = [worktree("b", 5), worktree("a", 5)];
        let sessions = [session("c", 5)];
        let recent = recent_entries(&worktrees, &sessions, 2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].title, "a");
        assert_eq!(recent[1].title, "b");
        // Same inputs keep the same order.
        assert_eq!(
            recent_entries(&worktrees, &sessions, 2),
            recent_entries(&worktrees, &sessions, 2)
        );
    }

    #[test]
    fn tool_cards_route_to_real_actions() {
        assert_eq!(Tool::NewTerminal.action(), Action::NewTerminal);
        assert_eq!(Tool::Chat.action(), Action::OpenPanel(Panel::Chat));
        assert_eq!(Tool::Files.action(), Action::OpenPanel(Panel::Files));
        // The documented card order: project, terminal, chat, files.
        let titles: Vec<_> = TOOLS.iter().map(|tool| tool.title()).collect();
        assert_eq!(titles, ["Add project", "New terminal", "Chat", "Files"]);
    }
}
