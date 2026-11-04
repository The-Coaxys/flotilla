//! Hover-card state for the sidebar and conversation rows.
//!
//! A row records its identity on hover; a single timer (no polling) turns the
//! pending target into a card after a short delay. Leaving the row or entering
//! another one cancels the pending card by bumping the epoch.
use std::time::Duration;

use fl_model::ProjectId;
use fl_model::WorktreeId;
use gpui::{App, Context, Pixels, WeakEntity, Window};
use uuid::Uuid;

use super::HostWorkspace;
use crate::app::hover_card::{self, Card};

/// How long the pointer rests on a row before its card appears.
const HOVER_DELAY: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    Project(ProjectId),
    Worktree(WorktreeId),
    Session(Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Shown {
    pub(super) target: Target,
    /// The pointer's window y when the row was entered.
    pub(super) y: Pixels,
}

/// A row's hover listener: arms the delay on enter, cancels it on leave.
pub(super) fn watch(
    view: WeakEntity<HostWorkspace>,
    target: Target,
) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    move |hovered: &bool, window: &mut Window, cx: &mut App| {
        let y = window.mouse_position().y;
        let _ = view.update(cx, |host, cx| {
            if *hovered {
                host.hover_enter(target, y, cx);
            } else {
                host.hover_leave(target, cx);
            }
        });
    }
}

impl HostWorkspace {
    fn hover_enter(&mut self, target: Target, y: Pixels, cx: &mut Context<Self>) {
        if self
            .hover
            .as_ref()
            .is_some_and(|card| card.target == target)
        {
            if let Some(card) = self.hover.as_mut() {
                card.y = y;
            }
            return;
        }
        if self.hover_pending == Some(target) {
            return;
        }
        self.hover_epoch = self.hover_epoch.wrapping_add(1);
        let epoch = self.hover_epoch;
        self.hover_pending = Some(target);
        self.hover = None;
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            cx.background_executor().timer(HOVER_DELAY).await;
            let _ = this.update(cx, |view, cx| {
                if view.hover_epoch == epoch && view.hover_pending == Some(target) {
                    view.hover_pending = None;
                    view.hover = Some(Shown { target, y });
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn hover_leave(&mut self, target: Target, cx: &mut Context<Self>) {
        let pending = self.hover_pending == Some(target);
        let shown = self
            .hover
            .as_ref()
            .is_some_and(|card| card.target == target);
        if !pending && !shown {
            return;
        }
        self.hover_epoch = self.hover_epoch.wrapping_add(1);
        self.hover_pending = None;
        self.hover = None;
        cx.notify();
    }

    /// Snapshot for the window-level card element.
    pub(crate) fn hover_card(&self) -> Option<Card> {
        let shown = self.hover?;
        let y = shown.y;
        match shown.target {
            Target::Worktree(id) => {
                let tree = self.state.tree.as_ref()?;
                let worktree = tree.worktrees.iter().find(|worktree| worktree.id == id)?;
                let project = tree
                    .projects
                    .iter()
                    .find(|project| project.id == worktree.project);
                let status = self.state.statuses.get(&id);
                Some(hover_card::worktree_card(worktree, project, status, y))
            }
            Target::Project(id) => {
                let tree = self.state.tree.as_ref()?;
                let project = tree.projects.iter().find(|project| project.id == id)?;
                let worktrees = tree
                    .worktrees
                    .iter()
                    .filter(|worktree| worktree.project == id)
                    .count();
                Some(hover_card::project_card(project, worktrees, y))
            }
            Target::Session(id) => {
                let session = self.chat.session(id)?;
                let worktree = self.state.worktree(session.worktree);
                Some(hover_card::session_card(session, worktree, y))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_compare_by_identity() {
        let id = WorktreeId::new();
        assert_eq!(Target::Worktree(id), Target::Worktree(id));
        assert_ne!(Target::Worktree(id), Target::Worktree(WorktreeId::new()));
        assert_ne!(
            Target::Project(ProjectId::new()),
            Target::Worktree(WorktreeId::new())
        );
    }
}
