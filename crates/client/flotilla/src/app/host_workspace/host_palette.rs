//! The command palette overlay for one host: modes, rows, keys and actions.
//!
//! Data comes from the mirrors this client already holds — the workspace tree
//! and statuses, the open layout's tabs, the open documents, cached directory
//! listings and the chat session mirror. Nothing here makes a new wire call:
//! every row ends in an existing `HostWorkspace` method.
use fl_model::chat::{ChatSession, SessionState};
use fl_model::layout::LayoutOp;
use fl_model::workspace::{
    EntryKind, LayoutScope, ProjectKind, ScriptKind, ScriptRunState, Worktree,
};
use fl_model::{PaneId, Size, TabId, TerminalId, WorkspaceId};
use flc_sync::WorkspacesState;
use flc_ui::input::{TextInput, TextInputEvent};
use flc_ui::theme::Theme;
use gpui::prelude::*;
use gpui::{
    AnyElement, AsyncApp, Context, Entity, Focusable, KeyDownEvent, MouseButton, SharedString,
    Subscription, WeakEntity, Window, div, px,
};

use super::{DocumentKind, HostWorkspace, Panel, join};
use crate::app::git_actions;
use crate::app::palette::{
    self, Action, Listing, MAX_VISIBLE, MODES, Mode, Panel as PalettePanel, Source,
};

/// Hard cap on rows kept for a query; the overlay shows at most 9.
const MAX_ENTRIES: usize = 200;

/// Open palette state for this host.
pub(super) struct Ui {
    pub(super) mode: Mode,
    input: Entity<TextInput>,
    pub(super) selected: usize,
    _events: Subscription,
    _changes: Subscription,
}

impl HostWorkspace {
    /// Ask the next render to open the palette (the app-level keybinding has
    /// no window here).
    pub(crate) fn request_palette(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.pending_palette = Some(mode);
        cx.notify();
    }

    pub(super) fn open_palette(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| TextInput::new(cx, mode.placeholder()));
        let events = cx.subscribe_in(
            &input,
            window,
            |host, _input, event: &TextInputEvent, window, cx| match event {
                TextInputEvent::Submitted(text) => host.palette_accept(text.clone(), window, cx),
                TextInputEvent::Cancelled => host.close_palette(window, cx),
            },
        );
        // TextInput only reports submit/cancel; typing updates the query, so
        // watch its notifications to re-rank while the user types.
        let changes = cx.observe(&input, |host, _input, cx| {
            if host.palette.is_some() {
                cx.notify();
            }
        });
        input.read(cx).focus_handle(cx).focus(window);
        self.palette = Some(Ui {
            mode,
            input,
            selected: 0,
            _events: events,
            _changes: changes,
        });
        cx.notify();
    }

    pub(super) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            self.refocus(window, cx);
        }
        cx.notify();
    }

    /// Return focus to whatever the palette covered.
    pub(crate) fn refocus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(document) = self
            .documents
            .get(&self.scope)
            .and_then(|documents| documents.active.and_then(|index| documents.open.get(index)))
        {
            let handle = match &document.kind {
                DocumentKind::File { editor, .. } => editor.read(cx).focus_handle(cx),
                DocumentKind::Diff(diff) => diff.read(cx).focus_handle(cx),
            };
            window.focus(&handle);
            return;
        }
        match self.shells.get(&self.scope) {
            Some((shell, _)) => {
                let handle = shell.read(cx).focus_handle();
                window.focus(&handle);
            }
            None => window.focus(&self.focus),
        }
    }

    /// Down/Up and Ctrl+1…6; Enter and Escape arrive through the text input.
    pub(super) fn palette_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(ui) = self.palette.as_mut() else {
            return;
        };
        let keystroke = &event.keystroke;
        if keystroke.modifiers.control
            && !keystroke.modifiers.shift
            && !keystroke.modifiers.alt
            && !keystroke.modifiers.platform
        {
            if let Some(mode) = Mode::from_chord(&keystroke.key) {
                ui.mode = mode;
                ui.selected = 0;
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        if keystroke.key == "tab" && !keystroke.modifiers.control && !keystroke.modifiers.alt {
            ui.mode = if keystroke.modifiers.shift {
                ui.mode.prev()
            } else {
                ui.mode.next()
            };
            ui.selected = 0;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        let handled = match keystroke.key.as_str() {
            "up" => {
                ui.selected = ui.selected.saturating_sub(1);
                true
            }
            "down" => {
                ui.selected = (ui.selected + 1).min(MAX_VISIBLE - 1);
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn palette_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if let Some(ui) = self.palette.as_mut() {
            ui.mode = mode;
            ui.selected = 0;
            cx.notify();
        }
    }

    fn palette_pick(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ui) = self.palette.as_mut() {
            ui.selected = index;
        }
        let query = self
            .palette
            .as_ref()
            .map(|ui| ui.input.read(cx).text().to_string())
            .unwrap_or_default();
        self.palette_accept(query.into(), window, cx);
    }

    fn palette_accept(&mut self, query: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ui) = self.palette.take() else {
            return;
        };
        let listing = self.palette_entries(ui.mode, &query);
        let selected = ui.selected.min(listing.entries.len().saturating_sub(1));
        let action = listing
            .entries
            .get(selected)
            .map(|entry| entry.action.clone());
        self.refocus(window, cx);
        match action {
            Some(action) => self.run_palette_action(action, window, cx),
            None => self.show_error("Nothing matches that search.", cx),
        }
        cx.notify();
    }

    // ---- sources -------------------------------------------------------------

    pub(super) fn palette_entries(&self, mode: Mode, query: &str) -> Listing {
        let mut listing = Listing::default();
        match mode {
            Mode::Global => {
                self.push_commands(&mut listing);
                self.push_worktrees(&mut listing);
                self.push_tabs(&mut listing);
                self.push_conversations(&mut listing);
                self.push_files(&mut listing);
            }
            Mode::Commands => self.push_commands(&mut listing),
            Mode::Files => self.push_files(&mut listing),
            Mode::Worktrees => self.push_worktrees(&mut listing),
            Mode::Tabs => self.push_tabs(&mut listing),
            Mode::Conversations => self.push_conversations(&mut listing),
        }
        if mode == Mode::Files {
            listing.note = Some(files_note(self.listings.len()));
        }
        let entries = palette::rank(query, listing.entries)
            .into_iter()
            .take(MAX_ENTRIES)
            .collect();
        Listing {
            entries,
            note: listing.note,
        }
    }

    fn push_commands(&self, out: &mut Listing) {
        out.push(
            "Add project",
            "Open a folder or clone a repository",
            Source::Command,
            Action::AddProject,
        );
        out.push(
            "New terminal",
            "Start a shell in the current scope",
            Source::Command,
            Action::NewTerminal,
        );
        out.push(
            "Home terminals",
            "The host-wide layout outside every project",
            Source::Command,
            Action::SelectScope(LayoutScope::Home),
        );
        out.push(
            if self.panel_open {
                "Hide right panel"
            } else {
                "Show right panel"
            },
            "Files, changes, checks and chat",
            Source::Command,
            Action::TogglePanel,
        );
        out.push(
            "Commands and prompt templates",
            "Edit scoped commands and routed action prompts",
            Source::Command,
            Action::OpenCommands,
        );
        out.push(
            "Branch group diff",
            "Shared context and per-child changes",
            Source::Command,
            Action::OpenGroupDiff,
        );
        for (panel, title, detail) in [
            (
                PalettePanel::Files,
                "Open Files panel",
                "Browse the worktree's files",
            ),
            (
                PalettePanel::Changes,
                "Open Changes panel",
                "The worktree's pending changes",
            ),
            (
                PalettePanel::Checks,
                "Open Checks panel",
                "Pull request checks",
            ),
            (PalettePanel::Review, "Open Review panel", "Review threads"),
            (
                PalettePanel::Chat,
                "Open Chat panel",
                "Conversations and history",
            ),
        ] {
            out.push(title, detail, Source::Command, Action::OpenPanel(panel));
        }
        if let Some(tree) = self.state.tree.as_ref() {
            for project in tree
                .projects
                .iter()
                .filter(|project| project.kind == ProjectKind::Git)
            {
                out.push(
                    format!("New worktree in {}", project.name),
                    "Create a branch and its worktree",
                    Source::Command,
                    Action::NewWorktree(project.id),
                );
            }
        }
        let Some(worktree) = self.selected_worktree() else {
            return;
        };
        let id = worktree.id;
        let label = worktree.label.clone();
        if !self.is_folder(worktree)
            && let Some(context) = self.git_context(worktree)
        {
            let action = git_actions::primary(context);
            if action.actionable() {
                out.push(
                    action.label(),
                    format!("{label} · git"),
                    Source::Command,
                    Action::Git {
                        worktree: id,
                        action,
                    },
                );
            }
        }
        let state = self.sync.layout(self.scope).workspace();
        if let Some(layout) = state.projected.as_ref()
            && let Some(view) = layout.view(layout.active)
            && let Some(tab) = view
                .tabs
                .iter()
                .find(|tab| Some(tab.id) == view.active)
                .or_else(|| view.tabs.first())
        {
            let label = tab
                .name
                .clone()
                .unwrap_or_else(|| "the active tab".to_string());
            out.push(
                "Close tab",
                format!("{label} · {}", self.scope_name(self.scope)),
                Source::Command,
                Action::CloseTab {
                    view: view.id,
                    tab: tab.id,
                },
            );
        }
        for script in self.state.scripts.get(&id).into_iter().flatten() {
            let title = match &script.kind {
                ScriptKind::Setup => "Run setup script".to_string(),
                ScriptKind::Run(None) => "Run the run script".to_string(),
                ScriptKind::Run(Some(name)) => format!("Run {name}"),
                ScriptKind::Teardown => "Run teardown script".to_string(),
            };
            out.push(
                title,
                format!("{label} · {}", script_run_label(&script.state)),
                Source::Command,
                Action::RunScript {
                    worktree: id,
                    kind: script.kind.clone(),
                },
            );
        }
    }

    fn push_worktrees(&self, out: &mut Listing) {
        let Some(tree) = self.state.tree.as_ref() else {
            return;
        };
        for worktree in &tree.worktrees {
            out.push(
                worktree.label.clone(),
                worktree_detail(worktree, self.state.as_ref()),
                Source::Worktree,
                Action::SelectScope(LayoutScope::Worktree(worktree.id)),
            );
        }
    }

    fn push_tabs(&self, out: &mut Listing) {
        for (scope, documents) in &self.documents {
            for document in &documents.open {
                let diff = matches!(document.kind, DocumentKind::Diff(_));
                out.push(
                    document.title.clone(),
                    format!("{} · {}", self.scope_name(*scope), document.path),
                    Source::Tab,
                    Action::ActivateDocument {
                        scope: *scope,
                        path: document.path.clone(),
                        diff,
                    },
                );
            }
        }
        for session in self.chat.open_tabs() {
            if let Some(session) = self.chat.session(session) {
                out.push(
                    session.label.clone(),
                    format!("{} · chat · seq {}", session.provider, session.seq),
                    Source::Tab,
                    Action::OpenChat(session.id),
                );
            }
        }
        let state = self.sync.layout(self.scope).workspace();
        let Some(layout) = state.projected.as_ref() else {
            return;
        };
        let scope_name = self.scope_name(self.scope);
        for view in &layout.views {
            for (index, tab) in view.tabs.iter().enumerate() {
                let title = tab
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("Terminal {}", index + 1));
                out.push(
                    title,
                    format!("{scope_name} · terminal"),
                    Source::Tab,
                    Action::SelectTab {
                        view: view.id,
                        tab: tab.id,
                    },
                );
            }
        }
    }

    fn push_conversations(&self, out: &mut Listing) {
        for session in self.chat.conversations() {
            out.push(
                session.label.clone(),
                session_detail(&session, self.state.as_ref()),
                Source::Conversation,
                Action::OpenChat(session.id),
            );
        }
    }

    fn push_files(&self, out: &mut Listing) {
        for listing in self.listings.values() {
            for entry in &listing.entries {
                if entry.kind != EntryKind::File {
                    continue;
                }
                if out.entries.len() >= MAX_ENTRIES {
                    return;
                }
                out.push(
                    entry.name.clone(),
                    listing.path.clone(),
                    Source::File,
                    Action::OpenFile(join(&listing.path, &entry.name)),
                );
            }
        }
    }

    fn scope_name(&self, scope: LayoutScope) -> SharedString {
        match scope {
            LayoutScope::Home => "Home".into(),
            LayoutScope::Worktree(id) => self
                .state
                .worktree(id)
                .map(|worktree| worktree.label.clone())
                .unwrap_or_else(|| "worktree".into())
                .into(),
        }
    }

    // ---- actions -------------------------------------------------------------

    pub(super) fn run_palette_action(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            Action::AddProject => self.open_add_project(window, cx),
            Action::NewWorktree(project) => self.open_create_worktree(project, window, cx),
            Action::NewTerminal => self.new_terminal(cx),
            Action::TogglePanel => {
                self.panel_open = !self.panel_open;
                cx.notify();
            }
            Action::OpenPanel(panel) => {
                self.panel_open = true;
                self.panel = match panel {
                    PalettePanel::Files => Panel::Files,
                    PalettePanel::Changes => Panel::Changes,
                    PalettePanel::Checks => Panel::Checks,
                    PalettePanel::Review => Panel::Review,
                    PalettePanel::Chat => Panel::Chat,
                };
                cx.notify();
            }
            Action::SelectScope(scope) => self.select(scope, cx),
            Action::OpenFile(path) => self.open_file(path, cx),
            Action::ActivateDocument { scope, path, diff } => {
                self.select(scope, cx);
                if !self.activate_existing(&path, diff) {
                    if diff {
                        self.show_error("That diff is no longer open.", cx);
                    } else {
                        self.open_file(path, cx);
                    }
                }
                cx.notify();
            }
            Action::OpenChat(session) => {
                // A conversation lives in its worktree's layout scope.
                match self.chat.session(session).map(|known| known.worktree) {
                    Some(worktree) if self.state.worktree(worktree).is_some() => {
                        if self.scope != LayoutScope::Worktree(worktree) {
                            self.select(LayoutScope::Worktree(worktree), cx);
                        }
                        self.open_chat_tab(session, window, cx);
                    }
                    Some(_) => self.show_error("That conversation's worktree is gone.", cx),
                    None => self.show_error("That conversation is no longer known.", cx),
                }
            }
            Action::SelectTab { view, tab } => {
                self.apply_layout(LayoutOp::SelectTab { view, tab }, cx)
            }
            Action::CloseTab { view, tab } => {
                self.apply_layout(LayoutOp::CloseTab { view, tab }, cx)
            }
            Action::RunScript { worktree, kind } => {
                self.select(LayoutScope::Worktree(worktree), cx);
                self.run_script(worktree, kind, false, cx);
            }
            Action::OpenCommands => {
                let workspace = self
                    .selected_worktree()
                    .and_then(|worktree| {
                        self.state.tree.as_ref().and_then(|tree| {
                            tree.projects
                                .iter()
                                .find(|project| project.id == worktree.project)
                                .map(|project| project.workspace)
                        })
                    })
                    .or_else(|| Self::tree_workspace(&self.state));
                match workspace {
                    Some(workspace) => self.open_commands(workspace, cx),
                    None => self.show_error("No workspace is loaded yet.", cx),
                }
            }
            Action::OpenGroupDiff => match self.selected_worktree().map(|worktree| worktree.id) {
                Some(worktree) => self.open_group_diff(worktree, cx),
                None => self.show_error("Select a worktree to show its branch group diff.", cx),
            },
            Action::Git { worktree, action } => {
                self.select(LayoutScope::Worktree(worktree), cx);
                self.run_git_action(action, false, window, cx);
            }
        }
    }

    fn tree_workspace(state: &WorkspacesState) -> Option<WorkspaceId> {
        state
            .tree
            .as_ref()
            .and_then(|tree| tree.workspaces.iter().min_by_key(|w| w.order))
            .map(|workspace| workspace.id)
    }

    /// Add a terminal tab through the layout model (the same operation the
    /// shell's own shortcut submits).
    fn new_terminal(&mut self, cx: &mut Context<Self>) {
        let state = self.sync.layout(self.scope).workspace();
        let Some(layout) = state.projected.as_ref() else {
            self.show_error("The daemon has not sent a terminal layout yet.", cx);
            return;
        };
        let op = LayoutOp::NewTab {
            view: layout.active,
            tab: TabId::new(),
            pane: PaneId::new(),
            terminal: TerminalId::new(),
            name: None,
            size: Size::new(80, 24),
        };
        self.apply_layout(op, cx);
    }

    fn apply_layout(&mut self, op: LayoutOp, cx: &mut Context<Self>) {
        let ticket = self.sync.layout(self.scope).apply(op);
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            if let Err(error) = ticket.wait().await {
                let _ = this.update(cx, |view, cx| {
                    view.show_error(format!("Layout change failed: {error}"), cx)
                });
            }
        })
        .detach();
    }

    // ---- rendering -----------------------------------------------------------

    pub(super) fn render_palette(
        &mut self,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (mode, selected, input) = {
            let ui = self.palette.as_ref()?;
            (ui.mode, ui.selected, ui.input.clone())
        };
        let query = input.read(cx).text().to_string();
        let listing = self.palette_entries(mode, &query);
        let total = listing.entries.len();
        let selected = selected.min(total.saturating_sub(1));
        let rows: Vec<AnyElement> = if listing.entries.is_empty() {
            vec![
                div()
                    .px(px(10.))
                    .py(px(8.))
                    .text_size(t.text_size_sm)
                    .text_color(t.text_tertiary)
                    .child("No matches in this mode.")
                    .into_any_element(),
            ]
        } else {
            listing
                .entries
                .iter()
                .take(MAX_VISIBLE)
                .enumerate()
                .map(|(index, entry)| {
                    let active = index == selected;
                    div()
                        .id(("palette-row", index))
                        .h(px(30.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .rounded(t.radius_sm)
                        .cursor_pointer()
                        .when(active, |d| {
                            d.bg(t.row_selected)
                                .border_1()
                                .border_color(t.row_selected_border)
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .child(entry.title.clone()),
                        )
                        .child(
                            div()
                                .max_w(px(220.))
                                .truncate()
                                .text_size(t.text_size_sm)
                                .text_color(t.text_secondary)
                                .child(entry.detail.clone()),
                        )
                        .child(
                            div()
                                .text_size(t.text_size_sm)
                                .text_color(t.text_tertiary)
                                .child(entry.source.label()),
                        )
                        .on_click(cx.listener(move |host, _, window, cx| {
                            host.palette_pick(index, window, cx)
                        }))
                        .into_any_element()
                })
                .collect()
        };
        let modes: Vec<AnyElement> = MODES
            .iter()
            .map(|candidate| {
                let candidate = *candidate;
                let active = candidate == mode;
                div()
                    .id(SharedString::from(format!(
                        "palette-mode-{}",
                        candidate.index()
                    )))
                    .h(px(22.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .rounded(t.radius_sm)
                    .cursor_pointer()
                    .text_size(t.text_size_sm)
                    .text_color(if active {
                        t.text_primary
                    } else {
                        t.text_secondary
                    })
                    .when(active, |d| d.bg(t.control_bg))
                    .child(candidate.title())
                    .child(div().text_color(t.text_tertiary).child(candidate.chord()))
                    .on_click(cx.listener(move |host, _, _, cx| host.palette_mode(candidate, cx)))
                    .into_any_element()
            })
            .collect();
        let count = if total == 1 {
            "1 result".to_string()
        } else {
            format!("{total} results")
        };
        let note = listing.note.unwrap_or_else(|| SharedString::from(count));
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .justify_center()
                .bg(gpui::black().opacity(0.4))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|host, _, window, cx| host.close_palette(window, cx)),
                )
                .child(
                    div()
                        .id("palette")
                        .mt(px(64.))
                        .w(px(620.))
                        .max_h(px(440.))
                        .flex()
                        .flex_col()
                        .rounded(t.radius_lg)
                        .bg(t.surface_raised)
                        .border_1()
                        .border_color(t.border)
                        .shadow_lg()
                        .key_context("Palette")
                        .on_key_down(cx.listener(|host, event: &KeyDownEvent, _, cx| {
                            host.palette_key(event, cx)
                        }))
                        .on_mouse_down(MouseButton::Left, |_, _, cx: &mut gpui::App| {
                            cx.stop_propagation()
                        })
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap(px(4.))
                                .p(px(8.))
                                .children(modes),
                        )
                        .child(div().px(px(10.)).pb(px(8.)).child(input))
                        .child(
                            div()
                                .id("palette-rows")
                                .px(px(6.))
                                .pb(px(6.))
                                .flex()
                                .flex_col()
                                .children(rows),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .px(px(12.))
                                .py(px(8.))
                                .border_t_1()
                                .border_color(t.border)
                                .text_size(t.text_size_sm)
                                .text_color(t.text_tertiary)
                                .child(note)
                                .child(div().flex_1())
                                .child("↑↓ move · Tab next mode · Enter run · Esc close"),
                        ),
                )
                .into_any_element(),
        )
    }
}

fn files_note(directories: usize) -> SharedString {
    match directories {
        0 => "Files: no directory is cached yet — open a folder in the Files panel first.".into(),
        1 => "Files: 1 cached directory — files in folders not opened yet are missing.".into(),
        count => format!(
            "Files: {count} cached directories — files in folders not opened yet are missing."
        )
        .into(),
    }
}

fn script_run_label(state: &ScriptRunState) -> String {
    match state {
        ScriptRunState::Idle => "idle".into(),
        ScriptRunState::Running => "running".into(),
        ScriptRunState::Exited { code: Some(code) } => format!("exited {code}"),
        ScriptRunState::Exited { code: None } => "exited".into(),
    }
}

fn worktree_detail(worktree: &Worktree, state: &flc_sync::WorkspacesState) -> SharedString {
    let mut detail = worktree.branch.clone().unwrap_or_else(|| "detached".into());
    if let Some(status) = state.statuses.get(&worktree.id) {
        let summary = &status.summary;
        detail.push_str(&format!(" · ↑{} ↓{}", summary.ahead, summary.behind));
        if summary.dirty {
            detail.push_str(" · dirty");
        }
    }
    detail.into()
}

fn session_detail(session: &ChatSession, state: &flc_sync::WorkspacesState) -> SharedString {
    let status = match &session.status {
        SessionState::Starting => "starting".to_string(),
        SessionState::Idle => "idle".to_string(),
        SessionState::Running => "running".to_string(),
        SessionState::AwaitingInput => "awaiting input".to_string(),
        SessionState::Closed => "closed".to_string(),
        SessionState::Failed { message } => format!("failed: {message}"),
    };
    let worktree = state
        .worktree(session.worktree)
        .map(|worktree| worktree.label.clone())
        .unwrap_or_default();
    if worktree.is_empty() {
        format!("{} · {status} · seq {}", session.provider, session.seq).into()
    } else {
        format!(
            "{} · {status} · seq {} · {worktree}",
            session.provider, session.seq
        )
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::workspace::DirListing;

    fn listing(path: &str) -> DirListing {
        DirListing {
            path: path.into(),
            entries: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn files_note_admits_an_incomplete_source() {
        assert!(files_note(0).contains("no directory is cached"));
        assert!(files_note(1).contains("1 cached directory"));
        assert!(files_note(3).contains("3 cached directories"));
    }

    #[test]
    fn script_labels_are_explicit() {
        assert_eq!(script_run_label(&ScriptRunState::Idle), "idle");
        assert_eq!(script_run_label(&ScriptRunState::Running), "running");
        assert_eq!(
            script_run_label(&ScriptRunState::Exited { code: Some(2) }),
            "exited 2"
        );
    }

    #[test]
    fn listings_keep_directories_apart_from_files() {
        let listing = listing("/srv/repo");
        assert_eq!(listing.path, "/srv/repo");
        assert!(listing.entries.is_empty());
    }
}
