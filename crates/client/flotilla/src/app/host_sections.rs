//! Workspace-local section management and sidebar projection. Never derives agent
//! observations from terminal output; classification uses the shared pure model.
//!
//! Each workspace exposes Sections for definition editing and each real worktree
//! exposes Move / Pinned for membership. Display order and filter priority are
//! independent; pins and manual assignments precede filters. Definitions retain
//! their opening revision so concurrent edits fail visibly rather than overwrite.
//! Offline snapshots remain browsable, but mutations require a live 0.4 daemon.
//! Agent predicates can be authored for future adapters, but always receive an
//! unknown observation here; unavailable observations never satisfy a predicate.
use super::*;
use fl_model::sections::{self, AgentState, Section, SectionFilter, SectionKind, SectionMutation, SectionPlacement, RunState, MAX_SECTIONS};
use fl_model::workspace::{Project, WorkspaceTree};
use fl_model::{SectionId, WorkspaceId};

#[derive(Default)]
pub(super) struct SectionsUi {
    dialog: Option<SectionsDialog>,
    pub(super) busy: bool,
    error: Option<SharedString>,
}

impl SectionsUi {
    pub(super) fn close(&mut self) {
        self.dialog = None;
        self.error = None;
    }
}

enum SectionsDialog {
    Manage(WorkspaceId),
    Edit(SectionDraft),
    Delete { section: Section, rev: u64 },
    Assign(WorktreeId),
}

struct SectionDraft {
    section: Section,
    rev: u64,
    name: Entity<TextInput>,
    order: Entity<TextInput>,
    priority: Entity<TextInput>,
    branch: Entity<TextInput>,
    filter: SectionFilter,
    _events: Vec<Subscription>,
}

fn action(id: impl Into<SharedString>, label: impl Into<SharedString>, enabled: bool, t: &Theme) -> gpui::Stateful<gpui::Div> {
    let hover = t.raised_hover;
    div().id(id.into()).px(px(8.)).py(px(4.)).rounded(t.radius_sm)
        .text_size(t.text_size_sm)
        .text_color(if enabled { t.text_secondary } else { t.text_tertiary })
        .when(enabled, |d| d.cursor_pointer().hover(move |s| s.bg(hover)))
        .child(label.into())
}

fn note(text: impl Into<SharedString>, t: &Theme) -> gpui::Div {
    div().text_size(t.text_size_sm).text_color(t.text_secondary).child(text.into())
}

fn cycle<T: Copy + PartialEq>(value: &mut Option<T>, choices: &[T]) {
    *value = match *value {
        None => choices.first().copied(),
        Some(current) => choices.iter().position(|v| *v == current).and_then(|i| choices.get(i + 1)).copied(),
    };
}

impl HostWorkspace {
    fn sections_enabled(&self) -> bool {
        self.state.live && self.state.sections.is_some() && !self.sections_ui.busy
    }

    fn section_dialog(&mut self, dialog: SectionsDialog, cx: &mut Context<Self>) {
        if !self.sections_enabled() { return; }
        self.dialog = None;
        self.sections_ui.dialog = Some(dialog);
        self.sections_ui.error = None;
        cx.notify();
    }

    fn edit_section(&mut self, workspace: WorkspaceId, section: Option<Section>, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sections_enabled() { return; }
        let Some(state) = self.state.sections.as_ref() else { return; };
        if section.is_none() && state.sections.len() >= MAX_SECTIONS {
            self.sections_ui.error = Some("Section limit reached (256). Delete a section before creating another.".into());
            cx.notify();
            return;
        }
        // Resolve a clicked row against the same snapshot as the opening CAS
        // revision: an event can arrive between rendering that row and clicking.
        let section = match section {
            Some(clicked) => {
                let Some(current) = state.sections.iter().find(|s| s.id == clicked.id && s.workspace == workspace) else {
                    self.sections_ui.error = Some("This section was removed by another client. Reopen Sections to use the current list.".into());
                    cx.notify();
                    return;
                };
                Some(current.clone())
            }
            None => None,
        };
        let rev = state.rev;
        let section = section.unwrap_or_else(|| Section {
            id: SectionId::new(), workspace, name: String::new(),
            order: state.sections.iter().filter(|s| s.workspace == workspace).map(|s| s.order).max().map_or(0, |n| n.saturating_add(1)),
            priority: 0, kind: SectionKind::Manual,
        });
        let name = cx.new(|cx| TextInput::new(cx, "Section name (up to 128 bytes)"));
        name.update(cx, |input, cx| input.set_text(section.name.clone(), cx));
        let order = cx.new(|cx| TextInput::new(cx, "Display order: 0–4294967295"));
        order.update(cx, |input, cx| input.set_text(section.order.to_string(), cx));
        let priority = cx.new(|cx| TextInput::new(cx, "Filter priority: signed integer"));
        priority.update(cx, |input, cx| input.set_text(section.priority.to_string(), cx));
        let filter = match &section.kind { SectionKind::Manual => SectionFilter::default(), SectionKind::Filter(f) => f.clone() };
        let branch = cx.new(|cx| TextInput::new(cx, "Any branch (or feature/*)"));
        branch.update(cx, |input, cx| input.set_text(filter.branch_glob.clone().unwrap_or_default(), cx));
        let mut events = Vec::new();
        for input in [&name, &order, &priority, &branch] {
            events.push(cx.subscribe_in(input, window, |view, _, event, window, cx| match event {
                TextInputEvent::Submitted(_) => view.save_section(cx),
                TextInputEvent::Cancelled => view.close_dialog(window, cx),
            }));
        }
        name.read(cx).focus_handle(cx).focus(window);
        self.section_dialog(SectionsDialog::Edit(SectionDraft { section, rev, name, order, priority, branch, filter, _events: events }), cx);
    }

    fn save_section(&mut self, cx: &mut Context<Self>) {
        if !self.sections_enabled() { return; }
        let Some(SectionsDialog::Edit(draft)) = self.sections_ui.dialog.as_ref() else { return; };
        let mut section = draft.section.clone();
        section.name = draft.name.read(cx).text().trim().to_string();
        let parsed = draft.order.read(cx).text().parse::<u32>().ok().zip(draft.priority.read(cx).text().parse::<i32>().ok());
        let Some((order, priority)) = parsed else {
            self.sections_ui.error = Some("Display order must be 0–4294967295; priority must be a signed 32-bit integer.".into());
            cx.notify();
            return;
        };
        section.order = order;
        section.priority = priority;
        if matches!(section.kind, SectionKind::Filter(_)) {
            let mut filter = draft.filter.clone();
            let glob = draft.branch.read(cx).text();
            filter.branch_glob = (!glob.is_empty()).then(|| glob.to_string());
            section.kind = SectionKind::Filter(filter);
        }
        if let Err(message) = section.validate() {
            self.sections_ui.error = Some(message.into());
            cx.notify();
            return;
        }
        self.section_mutation(draft.rev, SectionMutation::Upsert(section), cx);
    }

    fn section_mutation(&mut self, rev: u64, mutation: SectionMutation, cx: &mut Context<Self>) {
        if !self.sections_enabled() { return; }
        let after = match self.sections_ui.dialog.as_ref() {
            Some(SectionsDialog::Edit(draft)) => SectionsDialog::Manage(draft.section.workspace),
            Some(SectionsDialog::Delete { section, .. }) => SectionsDialog::Manage(section.workspace),
            Some(SectionsDialog::Assign(id)) => SectionsDialog::Assign(*id),
            _ => return,
        };
        self.sections_ui.busy = true;
        self.sections_ui.error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.mutate_sections(rev, mutation).await;
            let _ = this.update(cx, |view, cx| {
                view.sections_ui.busy = false;
                view.state = view.sync.workspaces();
                match result {
                    Ok(_) => view.sections_ui.dialog = Some(after),
                    Err(error) => view.sections_ui.error = Some(if is_conflict(&error) {
                        "Sections changed on another client. Your changes were not saved. Cancel and reopen this editor to load the latest revision, then try again.".into()
                    } else { error_text(&error) }),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    pub(super) fn organize_worktree_button(&self, id: WorktreeId, state: &WorkspacesState, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let enabled = self.sections_enabled();
        let pinned = state.sections.as_ref().is_some_and(|s| s.memberships.iter().any(|m| m.worktree == id && m.pinned));
        action(format!("organize-{id}"), if pinned { "Pinned" } else { "Move" }, enabled, t)
            .when(enabled, |d| d.on_click(cx.listener(move |view, _, _, cx| {
                cx.stop_propagation();
                view.section_dialog(SectionsDialog::Assign(id), cx);
            }))).into_any_element()
    }

    fn project_header(&self, project: &Project, suffix: &str, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let id = project.id;
        let letter: String = project.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
        let hover = t.raised_hover;
        div().id(SharedString::from(format!("project-{id}-{suffix}")))
            .mt(px(6.)).h(px(28.)).px(px(8.)).flex().items_center().gap(px(8.)).rounded(t.radius_sm)
            .child(div().size(px(18.)).rounded(px(4.)).bg(t.control_bg).flex().items_center().justify_center().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(letter)))
            .child(div().flex_1().min_w(px(0.)).truncate().font_weight(FontWeight::MEDIUM).child(project.name.clone()))
            .when(project.kind == ProjectKind::Git, |d| d.child(
                div().id(SharedString::from(format!("new-worktree-{id}-{suffix}"))).px(px(6.)).rounded(t.radius_sm).text_color(t.text_secondary).cursor_pointer().hover(move |s| s.bg(hover)).child("+")
                    .on_click(cx.listener(move |view, _, window, cx| view.open_create_worktree(id, window, cx)))
            )).into_any_element()
    }

    pub(super) fn sections_sidebar(&self, tree: &WorkspaceTree, state: &WorkspacesState, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut column = div().flex().flex_col();
        let mut workspaces: Vec<_> = tree.workspaces.iter().collect();
        workspaces.sort_by_key(|w| (w.order, w.id));
        for workspace in workspaces {
            let workspace_id = workspace.id;
            let enabled = self.sections_enabled();
            column = column.child(div().mt(px(8.)).px(px(8.)).flex().items_center().gap(px(4.))
                .child(div().flex_1().min_w(px(0.)).truncate().text_size(t.text_size_sm).font_weight(FontWeight::SEMIBOLD).child(workspace.name.clone()))
                .child(action(format!("sections-{workspace_id}"), "Sections", enabled, t).when(enabled, |d| d.on_click(cx.listener(move |view, _, _, cx| view.section_dialog(SectionsDialog::Manage(workspace_id), cx))))));
            let mut projects: Vec<_> = tree.projects.iter().filter(|p| p.workspace == workspace_id).collect();
            projects.sort_by_key(|p| (p.order, p.id));
            let Some(sections) = state.sections.as_ref() else {
                column = column.child(note(if state.live { "Sections need daemon protocol 0.4 (or are still loading)." } else { "Offline — section changes unavailable." }, t).px(px(8.)));
                for project in &projects {
                    column = column.child(self.project_header(project, "legacy", t, cx));
                    let mut worktrees: Vec<_> = tree.worktrees.iter().filter(|w| w.project == project.id).collect();
                    worktrees.sort_by(|a, b| (!a.is_primary, &a.label, a.id).cmp(&(!b.is_primary, &b.label, b.id)));
                    for worktree in worktrees { column = column.child(self.worktree_row(worktree, state, t, cx)); }
                }
                continue;
            };
            // Rebuild from every pushed snapshot: git, forge, scripts, memberships
            // and section definitions all flow through this render projection.
            let placements: HashMap<_, _> = tree.worktrees.iter().filter(|w| projects.iter().any(|p| p.id == w.project)).map(|w| {
                (w.id, sections::classify(sections, workspace_id, w,
                    state.statuses.get(&w.id).map(|s| &s.summary), state.forges.get(&w.id),
                    state.scripts.get(&w.id).map(Vec::as_slice), None))
            }).collect();
            let mut ordered: Vec<_> = sections.sections.iter().filter(|s| s.workspace == workspace_id).collect();
            ordered.sort_by_key(|s| (s.order, s.id));
            let mut groups = vec![(SectionPlacement::Pinned, "Pinned".to_string(), "pinned".to_string())];
            groups.extend(ordered.iter().map(|s| (SectionPlacement::Section(s.id), format!("{} · {}", s.name, if matches!(s.kind, SectionKind::Manual) { "manual" } else { "filter" }), s.id.to_string())));
            groups.push((SectionPlacement::Unassigned, "Unassigned".into(), "unassigned".into()));
            for (placement, label, suffix) in groups {
                let count = placements.values().filter(|p| **p == placement).count();
                if placement == SectionPlacement::Pinned && count == 0 { continue; }
                column = column.child(div().mt(px(8.)).px(px(8.)).text_size(t.text_size_sm).text_color(t.text_secondary).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(format!("{label} ({count})"))));
                for project in &projects {
                    let mut worktrees: Vec<_> = tree.worktrees.iter().filter(|w| w.project == project.id && placements.get(&w.id) == Some(&placement)).collect();
                    worktrees.sort_by(|a, b| (!a.is_primary, &a.label, a.id).cmp(&(!b.is_primary, &b.label, b.id)));
                    let empty_project = placement == SectionPlacement::Unassigned && !tree.worktrees.iter().any(|w| w.project == project.id);
                    if worktrees.is_empty() && !empty_project { continue; }
                    column = column.child(self.project_header(project, &suffix, t, cx));
                    for worktree in worktrees { column = column.child(self.worktree_row(worktree, state, t, cx)); }
                }
                if count == 0 && placement != SectionPlacement::Unassigned { column = column.child(note("No worktrees in this section", t).px(px(12.))); }
            }
        }
        column.into_any_element()
    }

    pub(super) fn render_sections_dialog(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.sections_ui.dialog.as_ref()?;
        let enabled = self.sections_enabled();
        let mut body = div().flex().flex_col().gap(px(12.));
        match dialog {
            SectionsDialog::Manage(workspace) => {
                let workspace = *workspace;
                let name = self.state.tree.as_ref().and_then(|tree| tree.workspaces.iter().find(|w| w.id == workspace)).map(|w| w.name.as_str()).unwrap_or("Workspace");
                body = body.child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(format!("Sections · {name}"))))
                    .child(note("Pinned first, then sections in ascending display order, then Unassigned. Manual assignments override filters; higher filter priority wins. Ties use display order, then stable section identity.", t));
                if let Some(state) = self.state.sections.as_ref() {
                    let mut sections: Vec<_> = state.sections.iter().filter(|s| s.workspace == workspace).collect();
                    sections.sort_by_key(|s| (s.order, s.id));
                    if sections.is_empty() { body = body.child(note("No sections yet. Create a manual list or a live filter.", t)); }
                    for section in sections {
                        let edit = section.clone();
                        let delete = section.clone();
                        let rev = state.rev;
                        body = body.child(div().flex().items_center().gap(px(8.))
                            .child(div().flex_1().min_w(px(0.)).child(section.name.clone()).child(note(format!("{} · order {} · priority {}", if matches!(section.kind, SectionKind::Manual) { "Manual" } else { "Filter" }, section.order, section.priority), t)))
                            .child(action(format!("edit-section-{}", section.id), "Edit", enabled, t).when(enabled, |d| d.on_click(cx.listener(move |view, _, window, cx| view.edit_section(workspace, Some(edit.clone()), window, cx)))))
                            .child(action(format!("delete-section-{}", section.id), "Delete", enabled, t).when(enabled, |d| d.on_click(cx.listener(move |view, _, _, cx| view.section_dialog(SectionsDialog::Delete { section: delete.clone(), rev }, cx))))));
                    }
                    let can_create = enabled && state.sections.len() < MAX_SECTIONS;
                    body = body.child(action("new-section", "New section", can_create, t).when(can_create, |d| d.on_click(cx.listener(move |view, _, window, cx| view.edit_section(workspace, None, window, cx)))));
                    if state.sections.len() >= MAX_SECTIONS { body = body.child(note("Section limit reached (256).", t)); }
                }
            }
            SectionsDialog::Edit(draft) => {
                let is_filter = matches!(draft.section.kind, SectionKind::Filter(_));
                body = body.child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Edit section"))
                    .child(note("Name", t)).child(draft.name.clone())
                    .child(div().flex().gap(px(12.))
                        .child(div().flex_1().min_w(px(0.)).child(note("Display order (lower first)", t)).child(draft.order.clone()))
                        .child(div().flex_1().min_w(px(0.)).child(note("Filter priority (higher wins)", t)).child(draft.priority.clone())))
                    .child(action("section-kind", if is_filter { "Kind: Filter — change to Manual" } else { "Kind: Manual — change to Filter" }, enabled, t).when(enabled, |d| d.on_click(cx.listener(|view, _, _, cx| {
                        if let Some(SectionsDialog::Edit(draft)) = view.sections_ui.dialog.as_mut() {
                            draft.section.kind = if matches!(draft.section.kind, SectionKind::Manual) { SectionKind::Filter(draft.filter.clone()) } else { SectionKind::Manual };
                        }
                        cx.notify();
                    }))));
                if is_filter {
                    body = body.child(note("All selected conditions must match. Click a condition to cycle through values and back to Any. Unknown or unavailable observations never match a selected condition; Any imposes no requirement.", t));
                    for (index, label) in [
                        format!("PR: {}", match draft.filter.pr { None => "Any", Some(PrState::Open) => "Open", Some(PrState::Closed) => "Closed", Some(PrState::Merged) => "Merged" }),
                        format!("Review: {}", match draft.filter.review { None => "Any", Some(ReviewDecision::Approved) => "Approved", Some(ReviewDecision::ChangesRequested) => "Changes requested", Some(ReviewDecision::ReviewRequired) => "Review required" }),
                        format!("Checks: {}", match draft.filter.checks { None => "Any", Some(CheckStatus::Pending) => "Pending", Some(CheckStatus::Success) => "Success", Some(CheckStatus::Failure) => "Failure", Some(CheckStatus::Neutral) => "Neutral / skipped" }),
                        format!("Run: {}", match draft.filter.run { None => "Any", Some(RunState::Idle) => "Idle", Some(RunState::Running) => "Running", Some(RunState::Succeeded) => "Succeeded", Some(RunState::Failed) => "Failed", Some(RunState::ExitedUnknown) => "Exited (unknown code)" }),
                        format!("Agent state (unknown until adapter support): {}", match draft.filter.agent { None => "Any", Some(AgentState::Running) => "Running", Some(AgentState::Waiting) => "Waiting", Some(AgentState::Idle) => "Idle", Some(AgentState::Exited) => "Exited", Some(AgentState::Failed) => "Failed" }),
                    ].into_iter().enumerate() {
                        body = body.child(action(format!("section-predicate-{index}"), label, enabled, t).when(enabled, |d| d.on_click(cx.listener(move |view, _, _, cx| {
                            if let Some(SectionsDialog::Edit(draft)) = view.sections_ui.dialog.as_mut() {
                                match index {
                                    0 => cycle(&mut draft.filter.pr, &[PrState::Open, PrState::Closed, PrState::Merged]),
                                    1 => cycle(&mut draft.filter.review, &[ReviewDecision::Approved, ReviewDecision::ChangesRequested, ReviewDecision::ReviewRequired]),
                                    2 => cycle(&mut draft.filter.checks, &[CheckStatus::Pending, CheckStatus::Success, CheckStatus::Failure, CheckStatus::Neutral]),
                                    3 => cycle(&mut draft.filter.run, &[RunState::Idle, RunState::Running, RunState::Succeeded, RunState::Failed, RunState::ExitedUnknown]),
                                    _ => cycle(&mut draft.filter.agent, &[AgentState::Running, AgentState::Waiting, AgentState::Idle, AgentState::Exited, AgentState::Failed]),
                                }
                            }
                            cx.notify();
                        }))));
                    }
                    body = body.child(note("Agent state is configurable for adapter support, but is currently unknown and no selected agent condition can match. Terminal text is never inferred. Cycle back to Any to remove this requirement.", t));
                    body = body.child(note("Branch glob (empty = Any; whole branch; * = any sequence, ? = one character; other characters literal; up to 256 bytes)", t)).child(draft.branch.clone())
                        .child(note("PR / review / checks require available forge data. Run uses named run targets only, not setup or teardown. Missing run targets are unknown, not idle. Checks roll up failure, pending, success, then neutral; run rolls up running, failed, unknown exit, idle, then succeeded.", t));
                } else {
                    body = body.child(note("Assign real worktrees with their Move button. Changing a manual section to a filter requires clearing its manual assignments first.", t));
                }
                body = body.child(action("save-section", if self.sections_ui.busy { "Saving…" } else { "Save section" }, enabled, t).when(enabled, |d| d.on_click(cx.listener(|view, _, _, cx| view.save_section(cx)))));
            }
            SectionsDialog::Delete { section, rev } => {
                let assigned = self.state.sections.as_ref().map_or(0, |s| s.memberships.iter().filter(|m| m.manual == Some(section.id)).count());
                body = body.child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Delete section"))
                    .child(note(format!("Delete “{}”? {assigned} manual assignments will be cleared. Worktrees and their pins are kept; remaining filters may place them elsewhere.", section.name), t));
                let id = section.id;
                let rev = *rev;
                body = body.child(action("confirm-delete-section", "Delete section", enabled, t).text_color(t.danger).when(enabled, |d| d.on_click(cx.listener(move |view, _, _, cx| view.section_mutation(rev, SectionMutation::Delete(id), cx)))));
            }
            SectionsDialog::Assign(id) => {
                let id = *id;
                let worktree = self.state.worktree(id);
                let workspace = worktree.and_then(|w| self.state.tree.as_ref()?.projects.iter().find(|p| p.id == w.project).map(|p| p.workspace));
                body = body.child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child("Organize worktree"))
                    .child(note(worktree.map_or("This worktree was removed.", |w| w.path.as_str()), t));
                if let (Some(state), Some(workspace)) = (self.state.sections.as_ref(), workspace) {
                    let rev = state.rev;
                    let membership = state.memberships.iter().find(|m| m.worktree == id);
                    let pinned = membership.is_some_and(|m| m.pinned);
                    let manual = membership.and_then(|m| m.manual);
                    body = body.child(action("pin-worktree", if pinned { "Unpin worktree" } else { "Pin worktree" }, enabled, t).when(enabled, |d| d.on_click(cx.listener(move |view, _, _, cx| view.section_mutation(rev, SectionMutation::Pin { worktree: id, pinned: !pinned }, cx)))))
                        .child(note("Pinning takes precedence without removing a manual assignment. Unpinning restores manual / filter placement.", t))
                        .child(note("Manual section", t))
                        .child(action("unassign-worktree", if manual.is_none() { "Automatic (current)" } else { "Automatic — clear manual assignment" }, enabled && manual.is_some(), t).when(enabled && manual.is_some(), |d| d.on_click(cx.listener(move |view, _, _, cx| view.section_mutation(rev, SectionMutation::Assign { worktree: id, section: None }, cx)))));
                    let mut sections: Vec<_> = state.sections.iter().filter(|s| s.workspace == workspace && matches!(s.kind, SectionKind::Manual)).collect();
                    sections.sort_by_key(|s| (s.order, s.id));
                    if sections.is_empty() { body = body.child(note("No manual sections in this workspace. Use Sections → New section to create one.", t)); }
                    for section in sections {
                        let target = section.id;
                        let can_assign = enabled && manual != Some(target);
                        body = body.child(action(format!("assign-{target}"), format!("{}{}", section.name, if manual == Some(target) { " (current)" } else { "" }), can_assign, t).when(can_assign, |d| d.on_click(cx.listener(move |view, _, _, cx| view.section_mutation(rev, SectionMutation::Assign { worktree: id, section: Some(target) }, cx)))));
                    }
                }
            }
        }
        if !self.state.live { body = body.child(note("Offline — reconnect before changing sections. Your draft is kept.", t)); }
        else if self.state.sections.is_none() { body = body.child(note("Sections unavailable. Reconnect to a daemon supporting protocol 0.4.", t)); }
        if let Some(error) = self.sections_ui.error.clone() { body = body.child(div().text_size(t.text_size_sm).text_color(t.danger).child(error)); }
        let can_close = !self.sections_ui.busy;
        body = body.child(div().flex().justify_end().child(action("close-sections", if matches!(dialog, SectionsDialog::Edit(_) | SectionsDialog::Delete { .. }) { "Cancel" } else { "Done" }, can_close, t).when(can_close, |d| d.on_click(cx.listener(|view, _, window, cx| view.close_dialog(window, cx))))));
        Some(div().id("sections-overlay").absolute().inset_0().occlude().flex().items_center().justify_center().bg(gpui::black().opacity(0.45))
            .child(div().id("sections-card").w(px(520.)).max_h_full().overflow_y_scroll().p(px(20.)).rounded(t.radius_lg).bg(t.surface_raised).border_1().border_color(t.border).shadow_lg().child(body)).into_any_element())
    }
}
