//! Branch group diff: the daemon-computed repository summary and per-child
//! detail, plus lazy child creation and the shared-context report.
//!
//! The numbers and file lists are the daemon's; the client only renders them.
//! `Ensure child` registers the selected worktree as a group root when needed
//! and returns the existing child when the group already has one.
use super::*;
use fl_model::group::{GroupDiff, WorktreeChild};
use fl_proto::{Request, Response};

#[derive(Default)]
pub(super) struct GroupDiffUi {
    dialog: Option<GroupDiffState>,
    pub(super) busy: bool,
}

impl GroupDiffUi {
    pub(super) fn close(&mut self) {
        if self.busy {
            return;
        }
        self.dialog = None;
    }
}

struct GroupDiffState {
    worktree: WorktreeId,
    /// The loaded diff; `None` while loading or after a refusal.
    diff: Option<GroupDiff>,
    /// The child this dialog created or reused, with its shared context.
    child: Option<WorktreeChild>,
    error: Option<SharedString>,
}

fn note(text: impl Into<SharedString>, t: &Theme) -> gpui::Div {
    div()
        .text_size(t.text_size_sm)
        .text_color(t.text_secondary)
        .child(text.into())
}

fn heading(text: impl Into<SharedString>, t: &Theme) -> gpui::Div {
    div()
        .mt(px(4.))
        .text_size(t.text_size_sm)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text_secondary)
        .child(text.into())
}

fn button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let hover = t.raised_hover;
    div()
        .id(id.into())
        .px(px(8.))
        .py(px(3.))
        .rounded(t.radius_sm)
        .text_size(t.text_size_sm)
        .bg(t.control_bg)
        .text_color(if enabled {
            t.text_primary
        } else {
            t.text_tertiary
        })
        .when(enabled, |d| d.cursor_pointer().hover(move |s| s.bg(hover)))
        .child(label.into())
}

impl HostWorkspace {
    pub(super) fn open_group_diff(&mut self, worktree: WorktreeId, cx: &mut Context<Self>) {
        if !self.state.live {
            self.show_error("Offline — the branch group diff needs the daemon.", cx);
            return;
        }
        self.dialog = None;
        self.sections_ui.close();
        self.routes_ui.close();
        self.commands_ui.close();
        self.git_menu = false;
        self.group_diff_ui.dialog = Some(GroupDiffState {
            worktree,
            diff: None,
            child: None,
            error: None,
        });
        self.refresh_group_diff(cx);
        cx.notify();
    }

    pub(super) fn refresh_group_diff(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.group_diff_ui.dialog.as_ref() else {
            return;
        };
        let worktree = state.worktree;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.call(Request::WorktreeGroupDiff { worktree }).await;
            let _ = this.update(cx, |view, cx| {
                let Some(state) = view.group_diff_ui.dialog.as_mut() else {
                    return;
                };
                match result {
                    Ok(Response::GroupDiff(diff)) => {
                        state.diff = Some(diff);
                        state.error = None;
                    }
                    Ok(_) => {
                        state.diff = None;
                        state.error = Some("unexpected daemon answer to a group diff".into());
                    }
                    Err(error) => {
                        state.diff = None;
                        state.error = Some(error_text(&error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Create the group's child lazily (or return the existing one) and show
    /// the shared context it holds.
    pub(super) fn ensure_group_child(&mut self, cx: &mut Context<Self>) {
        if self.group_diff_ui.busy {
            return;
        }
        let Some(state) = self.group_diff_ui.dialog.as_ref() else {
            return;
        };
        let worktree = state.worktree;
        self.group_diff_ui.busy = true;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.call(Request::EnsureWorktreeChild { worktree }).await;
            let _ = this.update(cx, |view, cx| {
                view.group_diff_ui.busy = false;
                if let Some(state) = view.group_diff_ui.dialog.as_mut() {
                    match result {
                        Ok(Response::WorktreeChild(child)) => {
                            state.child = Some(child);
                            state.error = None;
                        }
                        Ok(_) => {
                            state.error = Some("unexpected daemon answer to ensure-child".into());
                        }
                        Err(error) => state.error = Some(error_text(&error)),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        self.refresh_group_diff(cx);
        cx.notify();
    }

    pub(super) fn render_group_diff_dialog(
        &self,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.group_diff_ui.dialog.as_ref()?;
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Branch group diff"),
            )
            .child(note(
                "Every member against the group base (the root's target, else the project default). A child inherits FEATURE.md, AGENTS.md/CLAUDE.md and context/.",
                t,
            ));
        if let Some(error) = &state.error {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error.clone()),
            );
        }
        if let Some(diff) = &state.diff {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(heading(
                        format!(
                            "{} against {}",
                            diff.group.label,
                            diff.group.base.as_deref().unwrap_or("(no base)")
                        ),
                        t,
                    ))
                    .child(note(
                        format!(
                            "+{} −{}  {} file(s)",
                            diff.added, diff.removed, diff.changed_files
                        ),
                        t,
                    )),
            );
            for (index, child) in diff.children.iter().enumerate() {
                let status = if child.conflicted {
                    "  conflicted"
                } else if child.dirty {
                    "  dirty"
                } else {
                    ""
                };
                let mut row = div()
                    .id(("group-child", index))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(8.))
                    .py(px(5.))
                    .rounded(t.radius_sm)
                    .bg(t.surface_bg)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(t.text_size_sm)
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!(
                                        "{}{} [{}]",
                                        if child.is_root { "★ " } else { "" },
                                        child.label,
                                        child.branch.as_deref().unwrap_or("detached")
                                    )),
                            )
                            .child(note(
                                format!("+{} −{}{status}", child.added, child.removed),
                                t,
                            )),
                    );
                for file in &child.files {
                    let counts = match (file.added, file.removed) {
                        (Some(added), Some(removed)) => format!("+{added} −{removed}"),
                        _ => "binary".to_owned(),
                    };
                    row = row.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(t.text_size_sm)
                                    .text_color(t.text_secondary)
                                    .child(file.path.clone()),
                            )
                            .child(note(counts, t)),
                    );
                }
                body = body.child(row);
            }
        } else if state.error.is_none() {
            body = body.child(note("Loading the group diff…", t));
        }
        if let Some(child) = &state.child {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p(px(8.))
                    .rounded(t.radius_sm)
                    .bg(t.surface_bg)
                    .child(heading(
                        if child.created {
                            format!(
                                "Created {} at {}",
                                child.worktree.label, child.worktree.path
                            )
                        } else {
                            format!("Reused {} at {}", child.worktree.label, child.worktree.path)
                        },
                        t,
                    ))
                    .child(note(
                        format!(
                            "shared context: {} present, {} missing",
                            child.shared.present.len(),
                            child.shared.missing.len()
                        ),
                        t,
                    )),
            );
        }
        let busy = self.group_diff_ui.busy;
        let body = body.child(
            div()
                .flex()
                .justify_end()
                .gap(px(6.))
                .child(
                    button("group-refresh", "Refresh", !busy, t).when(!busy, |d| {
                        d.on_click(cx.listener(|view, _, _, cx| view.refresh_group_diff(cx)))
                    }),
                )
                .child(
                    button("group-ensure-child", "Ensure child", !busy, t).when(!busy, |d| {
                        d.on_click(cx.listener(|view, _, _, cx| view.ensure_group_child(cx)))
                    }),
                )
                .child(button("group-close", "Close", !busy, t).when(!busy, |d| {
                    d.on_click(cx.listener(|view, _, window, cx| {
                        view.group_diff_ui.close();
                        view.close_dialog(window, cx);
                    }))
                })),
        );
        Some(
            div()
                .id("group-diff-overlay")
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.45))
                .child(
                    div()
                        .id("group-diff-card")
                        .w(px(760.))
                        .max_h_full()
                        .overflow_y_scroll()
                        .p(px(20.))
                        .rounded(t.radius_lg)
                        .bg(t.surface_raised)
                        .border_1()
                        .border_color(t.border)
                        .shadow_lg()
                        .child(body),
                )
                .into_any_element(),
        )
    }
}
