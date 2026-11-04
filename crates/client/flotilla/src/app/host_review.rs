//! App-managed review interaction, isolated from forge/merge actions.
use super::*;
use fl_model::ReviewThreadId;
use fl_model::review::{ReviewAnchor, ReviewMutation, ReviewOrigin, ReviewStatus, ReviewTriage};

#[derive(Default)]
pub(super) struct ReviewUi {
    composer: Option<Composer>,
    busy: bool,
    error: Option<SharedString>,
    navigation: Option<(String, u32)>,
}
struct Composer {
    anchor: ReviewAnchor,
    reply: Option<ReviewThreadId>,
    input: Entity<TextInput>,
    _events: Subscription,
}

impl HostWorkspace {
    pub(super) fn compose_review(
        &mut self,
        anchor: ReviewAnchor,
        reply: Option<ReviewThreadId>,
        cx: &mut Context<Self>,
    ) {
        if self.review.busy {
            return;
        }
        if let Err(error) = anchor.validate() {
            self.show_error(error, cx);
            return;
        }
        let input = cx.new(|cx| {
            TextInput::new(
                cx,
                if reply.is_some() {
                    "Reply to thread"
                } else {
                    "Describe the finding"
                },
            )
        });
        let events = cx.subscribe(&input, |view, _, event: &TextInputEvent, cx| match event {
            TextInputEvent::Submitted(_) => view.submit_review(cx),
            TextInputEvent::Cancelled => {
                view.review.composer = None;
                cx.notify();
            }
        });
        self.review.composer = Some(Composer {
            anchor,
            reply,
            input,
            _events: events,
        });
        self.review.error = None;
        self.panel = Panel::Review;
        self.panel_open = true;
        cx.notify();
    }

    pub(super) fn review_active_file(&mut self, cx: &mut Context<Self>) {
        let LayoutScope::Worktree(worktree) = self.scope else {
            self.show_error("Select a worktree before commenting", cx);
            return;
        };
        let Some(root) = self
            .state
            .worktree(worktree)
            .map(|w| w.path.trim_end_matches('/').to_owned())
        else {
            return;
        };
        let Some(doc) = self
            .documents
            .get(&self.scope)
            .and_then(|d| d.active.and_then(|i| d.open.get(i)))
        else {
            self.show_error("Open a file or diff before adding a file comment", cx);
            return;
        };
        let Some(path) = doc
            .path
            .strip_prefix(&format!("{root}/"))
            .map(str::to_owned)
        else {
            return;
        };
        self.compose_review(
            ReviewAnchor {
                worktree,
                path,
                lines: None,
            },
            None,
            cx,
        );
    }

    fn submit_review(&mut self, cx: &mut Context<Self>) {
        let Some(composer) = &self.review.composer else {
            return;
        };
        let body = composer.input.read(cx).text().to_owned();
        if let Err(error) = fl_model::review::validate_body(&body) {
            self.review.error = Some(error.into());
            cx.notify();
            return;
        }
        let worktree = composer.anchor.worktree;
        let mutation = match composer.reply {
            Some(thread) => ReviewMutation::Reply {
                thread,
                origin: ReviewOrigin::User,
                body,
            },
            None => ReviewMutation::Add {
                anchor: composer.anchor.clone(),
                origin: ReviewOrigin::User,
                body,
            },
        };
        self.change_review(worktree, mutation, true, cx);
    }

    fn change_review(
        &mut self,
        worktree: WorktreeId,
        mutation: ReviewMutation,
        close_composer: bool,
        cx: &mut Context<Self>,
    ) {
        if self.review.busy {
            return;
        }
        let Some(base_rev) = self.state.reviews.get(&worktree).map(|s| s.rev) else {
            self.refresh_reviews(worktree, cx);
            return;
        };
        self.review.busy = true;
        self.review.error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.mutate_review(worktree, base_rev, mutation).await;
            if result.as_ref().is_err_and(is_conflict) { let _ = sync.list_reviews(worktree).await; }
            let _ = this.update(cx, |view, cx| {
                view.review.busy = false;
                match result {
                    Ok(_) => { if close_composer { view.review.composer = None; } }
                    Err(error) => view.review.error = Some(if is_conflict(&error) {
                        "Reviews changed on another client. The latest state is loaded; your draft is kept. Review it and retry.".into()
                    } else { error_text(&error) }),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn refresh_reviews(&mut self, worktree: WorktreeId, cx: &mut Context<Self>) {
        if self.review.busy {
            return;
        }
        self.review.busy = true;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.list_reviews(worktree).await;
            let _ = this.update(cx, |view, cx| {
                view.review.busy = false;
                view.review.error = result.err().map(|e| error_text(&e));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn copy_pending_reviews(&mut self, worktree: WorktreeId, cx: &mut Context<Self>) {
        if self.review.busy {
            return;
        }
        self.review.busy = true;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.pending_review_bundle(worktree).await;
            let _ = this.update(cx, |view, cx| {
                view.review.busy = false;
                match result {
                    Ok(bundle) if !bundle.threads.is_empty() => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(bundle.prompt));
                        view.banner =
                            Some("Pending review bundle copied. No message has been sent.".into());
                    }
                    Ok(_) => {
                        view.review.error = Some(
                            "No pending threads. Mark an agent finding Should fix to include it."
                                .into(),
                        )
                    }
                    Err(e) => view.review.error = Some(error_text(&e)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn navigate_review(&mut self, anchor: &ReviewAnchor, cx: &mut Context<Self>) {
        let Some(root) = self.state.worktree(anchor.worktree).map(|w| w.path.clone()) else {
            return;
        };
        let path = join(&root, &anchor.path);
        self.review.navigation = Some((path.clone(), anchor.lines.map_or(1, |r| r.start)));
        self.open_file(path, cx);
    }
    pub(super) fn reveal_review_line(&mut self, cx: &mut Context<Self>) {
        let editor = self
            .documents
            .get(&self.scope)
            .and_then(|d| d.active.and_then(|i| d.open.get(i)))
            .and_then(|doc| match &doc.kind {
                DocumentKind::File { editor, .. } => Some((doc.path.clone(), editor.clone())),
                _ => None,
            });
        if let Some((path, editor)) = editor {
            self.reveal_review_editor(&path, &editor, cx);
        }
    }
    pub(super) fn reveal_review_editor(
        &mut self,
        path: &str,
        editor: &Entity<EditorView>,
        cx: &mut Context<Self>,
    ) {
        if let Some((target, line)) = &self.review.navigation
            && target == path
        {
            let line = *line;
            editor.update(cx, |view, cx| {
                view.go_to_line(line.saturating_sub(1) as usize, cx)
            });
            self.review.navigation = None;
        }
    }

    pub(super) fn render_reviews(
        &self,
        worktree: WorktreeId,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let state = self.state.reviews.get(&worktree);
        let pending = state.map_or(0, |s| s.threads.iter().filter(|t| t.is_pending()).count());
        let mut body = div().p(px(10.)).flex().flex_col().gap(px(12.))
            .child(div().text_size(t.text_size_sm).text_color(t.text_secondary)
                .child("App-managed threads. Select new-side diff lines or comment on the open file. Nothing is posted to the forge."))
            .child(div().flex().gap(px(6.))
                .child(secondary_button("review-refresh", "Refresh", t).on_click(cx.listener(move |v, _, _, cx| v.refresh_reviews(worktree, cx))))
                .child(secondary_button("review-copy", "Copy pending", t).on_click(cx.listener(move |v, _, _, cx| v.copy_pending_reviews(worktree, cx)))))
            .child(SharedString::from(format!("{pending} pending{}", if self.review.busy { " · Saving…" } else { "" })));
        if let Some(error) = &self.review.error {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error.clone()),
            );
        }
        if let Some(composer) = &self.review.composer
            && composer.anchor.worktree == worktree
        {
            let location = anchor_label(&composer.anchor);
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from(location)),
                    )
                    .child(composer.input.clone())
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(
                                primary_button(
                                    "review-submit",
                                    if composer.reply.is_some() {
                                        "Reply"
                                    } else {
                                        "Add thread"
                                    },
                                    t,
                                )
                                .on_click(cx.listener(|v, _, _, cx| v.submit_review(cx))),
                            )
                            .child(secondary_button("review-cancel", "Cancel", t).on_click(
                                cx.listener(|v, _, _, cx| {
                                    if !v.review.busy {
                                        v.review.composer = None;
                                        cx.notify();
                                    }
                                }),
                            )),
                    ),
            );
        }
        if state.is_none_or(|s| s.threads.is_empty()) {
            body = body.child(
                div()
                    .text_color(t.text_secondary)
                    .child(if state.is_none() {
                        "Reviews are not loaded. Refresh to retry."
                    } else {
                        "No review threads yet."
                    }),
            );
        }
        for thread in state.into_iter().flat_map(|s| &s.threads) {
            let id = thread.id;
            let anchor = thread.anchor.clone();
            let reply_anchor = anchor.clone();
            let mut row = div()
                .pt(px(12.))
                .border_t_1()
                .border_color(t.border)
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .id(SharedString::from(format!("review-anchor-{id}")))
                        .cursor_pointer()
                        .text_color(t.accent)
                        .child(SharedString::from(anchor_label(&anchor)))
                        .on_click(cx.listener(move |v, _, _, cx| v.navigate_review(&anchor, cx))),
                )
                .child(
                    div()
                        .text_size(t.text_size_sm)
                        .text_color(t.text_secondary)
                        .child(SharedString::from(format!(
                            "{:?} · {:?} · {:?}",
                            thread.origin, thread.triage, thread.status
                        ))),
                );
            for comment in &thread.comments {
                row = row.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(
                            div()
                                .text_size(t.text_size_sm)
                                .text_color(t.text_tertiary)
                                .child(SharedString::from(format!("{:?}", comment.origin))),
                        )
                        .child(SharedString::from(comment.body.clone())),
                );
            }
            let action = |label: &'static str, mutation: ReviewMutation, cx: &mut Context<Self>| {
                let hover = t.raised_hover;
                div()
                    .id(SharedString::from(format!("review-{id}-{label}")))
                    .px(px(6.))
                    .py(px(4.))
                    .rounded(t.radius_sm)
                    .cursor_pointer()
                    .bg(t.control_bg)
                    .hover(move |s| s.bg(hover))
                    .text_size(t.text_size_sm)
                    .child(label)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.change_review(worktree, mutation.clone(), false, cx)
                    }))
            };
            let mut actions = div().flex().flex_wrap().gap(px(5.)).child(
                div()
                    .id(SharedString::from(format!("review-reply-{id}")))
                    .cursor_pointer()
                    .px(px(6.))
                    .py(px(4.))
                    .bg(t.control_bg)
                    .text_size(t.text_size_sm)
                    .child("Reply")
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.compose_review(reply_anchor.clone(), Some(id), cx)
                    })),
            );
            if thread.status == ReviewStatus::Resolved {
                actions =
                    actions.child(action("Reopen", ReviewMutation::Reopen { thread: id }, cx));
            } else {
                actions = actions
                    .child(action(
                        "Should fix",
                        ReviewMutation::ShouldFix { thread: id },
                        cx,
                    ))
                    .child(action(
                        "Dismiss",
                        ReviewMutation::Dismiss { thread: id },
                        cx,
                    ));
                if thread.triage != ReviewTriage::Dismissed {
                    actions = actions
                        .child(action(
                            "Fixing",
                            ReviewMutation::SetStatus {
                                thread: id,
                                status: ReviewStatus::Fix,
                            },
                            cx,
                        ))
                        .child(action(
                            "Resolve",
                            ReviewMutation::SetStatus {
                                thread: id,
                                status: ReviewStatus::Resolved,
                            },
                            cx,
                        ));
                }
            }
            body = body.child(row.child(actions));
        }
        body
    }
}
fn anchor_label(anchor: &ReviewAnchor) -> String {
    match anchor.lines {
        Some(r) if r.start == r.end => format!("{}:{}", anchor.path, r.start),
        Some(r) => format!("{}:{}–{}", anchor.path, r.start, r.end),
        None => anchor.path.clone(),
    }
}
