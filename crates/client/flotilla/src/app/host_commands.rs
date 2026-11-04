//! Custom commands and prompt templates: scoped create/edit/delete plus a
//! prepare-mode dispatch, all through the daemon.
//!
//! Resolution and refusals belong to the daemon: a duplicate name, a missing
//! definition, an invalid body or a stale revision comes back with a reason
//! and is shown in place without losing the draft. Dispatch only ever reports
//! the daemon's `TaskDispatch`; a prepared prompt is not claimed as sent.
use super::*;
use fl_model::WorkspaceId;
use fl_model::command::{
    CommandMutation, CommandState, CustomCommand, MAX_COMMAND_BODY_BYTES, MAX_COMMAND_NAME_BYTES,
};
use fl_model::routing::{
    DispatchMode, GitTask, MAX_TEMPLATE_BYTES, PromptTemplate, RoutingMutation, RoutingState,
    Scope, TaskDispatch, TaskDispatchState, default_template,
};
use fl_proto::{Request, Response};

/// Which editor list the manage dialog shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Commands,
    Templates,
}

#[derive(Default)]
pub(super) struct CommandsUi {
    dialog: Option<CommandsDialog>,
    pub(super) busy: bool,
    error: Option<SharedString>,
    /// The newest command snapshot this client learned from the daemon.
    state: Option<CommandState>,
    /// The newest routing snapshot this editor learned (for templates).
    routing: Option<RoutingState>,
    /// The last dispatch this editor performed, rendered below the list.
    dispatch: Option<TaskDispatch>,
    tab: Tab,
}

impl CommandsUi {
    pub(super) fn close(&mut self) {
        if self.busy {
            return;
        }
        self.dialog = None;
        self.error = None;
    }
}

enum CommandsDialog {
    Manage {
        workspace: WorkspaceId,
        scope: Scope,
    },
    EditCommand(CommandDraft),
    EditTemplate(TemplateDraft),
}

struct CommandDraft {
    workspace: WorkspaceId,
    scope: Scope,
    /// False for a new command, whose name is editable.
    existing: bool,
    rev: u64,
    name: Entity<TextInput>,
    description: Entity<TextInput>,
    body: Entity<EditorView>,
    error: Option<SharedString>,
    busy: bool,
}

struct TemplateDraft {
    workspace: WorkspaceId,
    scope: Scope,
    action: GitTask,
    rev: u64,
    body: Entity<EditorView>,
    configured: bool,
    error: Option<SharedString>,
    busy: bool,
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

fn field(label: &'static str, input: Entity<TextInput>, t: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .text_size(t.text_size_sm)
                .text_color(t.text_secondary)
                .child(label),
        )
        .child(input)
}

impl HostWorkspace {
    // ---- opening and refreshing ------------------------------------------------

    pub(super) fn commands_enabled(&self) -> bool {
        self.state.live && !self.commands_ui.busy
    }

    /// The newest command snapshot this client knows.
    pub(super) fn commands(&self) -> Option<&CommandState> {
        self.commands_ui.state.as_ref()
    }

    /// The newest routing snapshot this editor knows, falling back to the
    /// routes editor's snapshot and the pushed mirror.
    pub(super) fn commands_routing(&self) -> Option<&RoutingState> {
        match (self.commands_ui.routing.as_ref(), self.routing()) {
            (Some(ours), Some(theirs)) if ours.rev >= theirs.rev => Some(ours),
            (Some(ours), None) => Some(ours),
            (Some(_), Some(theirs)) => Some(theirs),
            (None, theirs) => theirs,
        }
    }

    pub(super) fn open_commands(&mut self, workspace: WorkspaceId, cx: &mut Context<Self>) {
        if !self.state.live {
            self.show_error("Offline — commands unavailable.", cx);
            return;
        }
        self.dialog = None;
        self.sections_ui.close();
        self.routes_ui.close();
        self.git_menu = false;
        self.commands_ui.error = None;
        self.commands_ui.dispatch = None;
        self.commands_ui.dialog = Some(CommandsDialog::Manage {
            workspace,
            scope: Scope::Workspace(workspace),
        });
        self.refresh_commands(cx);
        cx.notify();
    }

    pub(super) fn refresh_commands(&mut self, cx: &mut Context<Self>) {
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let commands = sync.call(Request::ListCommands).await;
            // Templates live in the routing snapshot; fetching both keeps this
            // editor usable without opening the routes editor first.
            let routing = sync.routing().await;
            let _ = this.update(cx, |view, cx| {
                match commands {
                    Ok(Response::CommandState(state)) => {
                        view.commands_ui.state = Some(state);
                        view.commands_ui.error = None;
                    }
                    Ok(_) => {
                        view.commands_ui.error =
                            Some("unexpected daemon answer to a command query".into())
                    }
                    Err(error) => view.commands_ui.error = Some(error_text(&error)),
                }
                if let Ok(state) = routing {
                    let pushed = view
                        .state
                        .routing
                        .as_ref()
                        .map(|pushed| pushed.rev)
                        .unwrap_or(0);
                    if state.rev >= pushed {
                        view.commands_ui.routing = Some(state);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn commands_scope_label(&self, scope: Scope) -> String {
        match scope {
            Scope::Global => "Global".into(),
            Scope::Workspace(id) => self
                .state
                .tree
                .as_ref()
                .and_then(|tree| tree.workspaces.iter().find(|w| w.id == id))
                .map(|w| w.name.clone())
                .unwrap_or_else(|| "Workspace".into()),
            Scope::Project(id) => self
                .state
                .tree
                .as_ref()
                .and_then(|tree| tree.projects.iter().find(|p| p.id == id))
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Project".into()),
        }
    }

    fn commands_scope_choices(&self, workspace: WorkspaceId) -> Vec<(Scope, String)> {
        let mut choices = vec![(Scope::Global, self.commands_scope_label(Scope::Global))];
        choices.push((
            Scope::Workspace(workspace),
            self.commands_scope_label(Scope::Workspace(workspace)),
        ));
        if let Some(tree) = self.state.tree.as_ref() {
            let mut projects: Vec<_> = tree
                .projects
                .iter()
                .filter(|project| project.workspace == workspace)
                .collect();
            projects.sort_by_key(|project| (project.order, project.id));
            for project in projects {
                choices.push((Scope::Project(project.id), project.name.clone()));
            }
        }
        choices
    }

    fn commands_for_scope(&self, scope: Scope) -> Vec<CustomCommand> {
        let mut commands: Vec<CustomCommand> = self
            .commands()
            .map(|state| {
                state
                    .commands
                    .iter()
                    .filter(|command| command.scope == scope)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        commands.sort_by(|left, right| left.name.cmp(&right.name));
        commands
    }

    fn set_commands_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.commands_ui.tab = tab;
        cx.notify();
    }

    fn set_commands_scope(&mut self, workspace: WorkspaceId, scope: Scope, cx: &mut Context<Self>) {
        if self.commands_ui.busy {
            return;
        }
        self.commands_ui.error = None;
        self.commands_ui.dialog = Some(CommandsDialog::Manage { workspace, scope });
        cx.notify();
    }

    pub(super) fn back_to_commands(
        &mut self,
        workspace: WorkspaceId,
        scope: Scope,
        cx: &mut Context<Self>,
    ) {
        if self.commands_ui.busy {
            return;
        }
        self.commands_ui.error = None;
        self.commands_ui.dialog = Some(CommandsDialog::Manage { workspace, scope });
        cx.notify();
    }

    // ---- command editing -------------------------------------------------------

    fn open_command_editor(
        &mut self,
        workspace: WorkspaceId,
        scope: Scope,
        existing: Option<CustomCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.commands_enabled() {
            return;
        }
        let Some(state) = self.commands().cloned() else {
            self.commands_ui.error = Some("Commands are still loading.".into());
            cx.notify();
            return;
        };
        let (name_text, description_text, body_text, existing) = match &existing {
            Some(command) => (
                command.name.clone(),
                command.description.clone().unwrap_or_default(),
                command.body.clone(),
                true,
            ),
            None => (String::new(), String::new(), String::new(), false),
        };
        let name = cx.new(|cx| TextInput::new(cx, "command-name"));
        name.update(cx, |input, cx| input.set_text(name_text, cx));
        let description = cx.new(|cx| TextInput::new(cx, "Description (optional)"));
        description.update(cx, |input, cx| input.set_text(description_text, cx));
        let body = cx.new(|cx| EditorView::new("Command.md", body_text.as_bytes(), None, cx));
        self.dialog = None;
        self.commands_ui.error = None;
        self.commands_ui.dialog = Some(CommandsDialog::EditCommand(CommandDraft {
            workspace,
            scope,
            existing,
            rev: state.rev,
            name: name.clone(),
            description,
            body: body.clone(),
            error: None,
            busy: false,
        }));
        name.read(cx).focus_handle(cx).focus(window);
        body.read(cx).focus_handle(cx);
        cx.notify();
    }

    fn command_draft_command(
        &self,
        draft: &CommandDraft,
        cx: &Context<Self>,
    ) -> Result<CustomCommand, SharedString> {
        let name = draft.name.read(cx).text().trim().to_owned();
        let description = draft.description.read(cx).text().trim().to_owned();
        let body = draft.body.read(cx).buffer().text().trim_end().to_owned();
        if name.len() > MAX_COMMAND_NAME_BYTES {
            return Err(format!("a command name is at most {MAX_COMMAND_NAME_BYTES} bytes").into());
        }
        if body.trim().is_empty() {
            return Err("the command body is empty".into());
        }
        if body.len() > MAX_COMMAND_BODY_BYTES {
            return Err(format!("the command body exceeds {MAX_COMMAND_BODY_BYTES} bytes").into());
        }
        let command = CustomCommand {
            scope: draft.scope,
            name,
            description: (!description.is_empty()).then_some(description),
            body,
        };
        command.validate().map_err(SharedString::from)?;
        Ok(command)
    }

    fn save_command(&mut self, cx: &mut Context<Self>) {
        if self.commands_ui.busy {
            return;
        }
        let Some(CommandsDialog::EditCommand(draft)) = self.commands_ui.dialog.as_ref() else {
            return;
        };
        let command = match self.command_draft_command(draft, cx) {
            Ok(command) => command,
            Err(error) => {
                if let Some(CommandsDialog::EditCommand(draft)) = self.commands_ui.dialog.as_mut() {
                    draft.error = Some(error);
                    cx.notify();
                }
                return;
            }
        };
        let (workspace, scope, rev, existing) =
            (draft.workspace, draft.scope, draft.rev, draft.existing);
        let mutation = if existing {
            CommandMutation::Update(command)
        } else {
            CommandMutation::Create(command)
        };
        if let Some(CommandsDialog::EditCommand(draft)) = self.commands_ui.dialog.as_mut() {
            draft.busy = true;
            draft.error = None;
        }
        self.commands_ui.busy = true;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .call(Request::MutateCommand {
                    expected_rev: rev,
                    mutation,
                })
                .await;
            let fresh = if matches!(&result, Err(error) if is_conflict(error)) {
                match sync.call(Request::ListCommands).await {
                    Ok(Response::CommandState(state)) => Some(state),
                    _ => None,
                }
            } else {
                None
            };
            let _ = this.update(cx, |view, cx| {
                view.commands_ui.busy = false;
                match result {
                    Ok(Response::CommandState(state)) => {
                        view.commands_ui.state = Some(state);
                        view.commands_ui.error = None;
                        view.commands_ui.dialog = Some(CommandsDialog::Manage { workspace, scope });
                    }
                    Ok(_) => {
                        view.commands_ui.error =
                            Some("unexpected daemon answer to a command edit".into());
                    }
                    Err(error) => {
                        if let Some(state) = fresh {
                            view.commands_ui.state = Some(state);
                        }
                        if let Some(CommandsDialog::EditCommand(draft)) =
                            view.commands_ui.dialog.as_mut()
                        {
                            draft.busy = false;
                            if is_conflict(&error)
                                && let Some(state) = view.commands_ui.state.as_ref()
                            {
                                draft.rev = state.rev;
                            }
                            draft.error = Some(error_text(&error));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn delete_command(&mut self, scope: Scope, name: String, cx: &mut Context<Self>) {
        if self.commands_ui.busy {
            return;
        }
        let Some(state) = self.commands().cloned() else {
            self.commands_ui.error = Some("Commands are still loading.".into());
            cx.notify();
            return;
        };
        self.commands_ui.busy = true;
        self.commands_ui.error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .call(Request::MutateCommand {
                    expected_rev: state.rev,
                    mutation: CommandMutation::Delete { scope, name },
                })
                .await;
            let fresh = if matches!(&result, Err(error) if is_conflict(error)) {
                match sync.call(Request::ListCommands).await {
                    Ok(Response::CommandState(state)) => Some(state),
                    _ => None,
                }
            } else {
                None
            };
            let _ = this.update(cx, |view, cx| {
                view.commands_ui.busy = false;
                match result {
                    Ok(Response::CommandState(state)) => {
                        view.commands_ui.state = Some(state);
                        view.commands_ui.error = None;
                    }
                    Ok(_) => {
                        view.commands_ui.error =
                            Some("unexpected daemon answer to a command delete".into())
                    }
                    Err(error) => {
                        if let Some(state) = fresh {
                            view.commands_ui.state = Some(state);
                        }
                        view.commands_ui.error = Some(error_text(&error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Dispatch one stored command in prepare mode through the daemon's
    /// custom-command route. Nothing is submitted: the prompt is shown.
    fn run_command(&mut self, name: String, cx: &mut Context<Self>) {
        if self.commands_ui.busy {
            return;
        }
        let Some(worktree) = self.selected_worktree().map(|worktree| worktree.id) else {
            self.commands_ui.error = Some("Select a worktree to dispatch a command.".into());
            cx.notify();
            return;
        };
        self.commands_ui.busy = true;
        self.commands_ui.error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .call(Request::DispatchCommand {
                    worktree,
                    name,
                    mode: Some(DispatchMode::NewPrepare),
                    alternate: false,
                    context: String::new(),
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.commands_ui.busy = false;
                match result {
                    Ok(Response::TaskDispatch(dispatch)) => {
                        view.commands_ui.dispatch = Some(dispatch);
                        view.commands_ui.error = None;
                    }
                    Ok(_) => {
                        view.commands_ui.error =
                            Some("unexpected daemon answer to a dispatch".into())
                    }
                    Err(error) => view.commands_ui.error = Some(error_text(&error)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- prompt template editing ----------------------------------------------

    fn open_command_template_editor(
        &mut self,
        workspace: WorkspaceId,
        scope: Scope,
        action: GitTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.commands_enabled() {
            return;
        }
        let Some(routing) = self.commands_routing() else {
            self.commands_ui.error =
                Some("Routes need daemon protocol 0.4 (or are still loading).".into());
            cx.notify();
            return;
        };
        let configured = routing
            .templates
            .iter()
            .find(|template| template.scope == scope && template.action == action)
            .map(|template| template.body.clone());
        let rev = routing.rev;
        let body_text = configured
            .clone()
            .unwrap_or_else(|| default_template(action).to_owned());
        let body = cx.new(|cx| EditorView::new("Prompt.md", body_text.as_bytes(), None, cx));
        self.dialog = None;
        self.commands_ui.error = None;
        self.commands_ui.dialog = Some(CommandsDialog::EditTemplate(TemplateDraft {
            workspace,
            scope,
            action,
            rev,
            configured: configured.is_some(),
            body: body.clone(),
            error: None,
            busy: false,
        }));
        body.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn save_command_template(&mut self, cx: &mut Context<Self>) {
        if self.commands_ui.busy {
            return;
        }
        let Some(CommandsDialog::EditTemplate(draft)) = self.commands_ui.dialog.as_ref() else {
            return;
        };
        let body = draft.body.read(cx).buffer().text();
        if body.trim().is_empty() {
            if let Some(CommandsDialog::EditTemplate(draft)) = self.commands_ui.dialog.as_mut() {
                draft.error = Some("the prompt template is empty".into());
            }
            cx.notify();
            return;
        }
        if body.len() > MAX_TEMPLATE_BYTES {
            if let Some(CommandsDialog::EditTemplate(draft)) = self.commands_ui.dialog.as_mut() {
                draft.error =
                    Some(format!("the prompt template exceeds {MAX_TEMPLATE_BYTES} bytes").into());
            }
            cx.notify();
            return;
        }
        let (workspace, scope, action, rev) =
            (draft.workspace, draft.scope, draft.action, draft.rev);
        if let Some(CommandsDialog::EditTemplate(draft)) = self.commands_ui.dialog.as_mut() {
            draft.busy = true;
            draft.error = None;
        }
        self.commands_ui.busy = true;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .mutate_routing(
                    rev,
                    RoutingMutation::SetTemplate(PromptTemplate {
                        scope,
                        action,
                        body,
                    }),
                )
                .await;
            let fresh = if matches!(&result, Err(error) if is_conflict(error)) {
                sync.routing().await.ok()
            } else {
                None
            };
            let _ = this.update(cx, |view, cx| {
                view.commands_ui.busy = false;
                match result {
                    Ok(state) => {
                        view.commands_ui.routing = Some(state);
                        view.commands_ui.error = None;
                        view.commands_ui.dialog = Some(CommandsDialog::Manage { workspace, scope });
                    }
                    Err(error) => {
                        if let Some(state) = fresh {
                            view.commands_ui.routing = Some(state);
                        }
                        let fresh_rev = view.commands_routing().map(|routing| routing.rev);
                        if let Some(CommandsDialog::EditTemplate(draft)) =
                            view.commands_ui.dialog.as_mut()
                        {
                            draft.busy = false;
                            if is_conflict(&error)
                                && let Some(rev) = fresh_rev
                            {
                                draft.rev = rev;
                            }
                            draft.error = Some(error_text(&error));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn remove_command_template(&mut self, scope: Scope, action: GitTask, cx: &mut Context<Self>) {
        if self.commands_ui.busy {
            return;
        }
        let Some(routing) = self.commands_routing().cloned() else {
            self.commands_ui.error =
                Some("Routes need daemon protocol 0.4 (or are still loading).".into());
            cx.notify();
            return;
        };
        self.commands_ui.busy = true;
        self.commands_ui.error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .mutate_routing(
                    routing.rev,
                    RoutingMutation::RemoveTemplate { scope, action },
                )
                .await;
            let fresh = if matches!(&result, Err(error) if is_conflict(error)) {
                sync.routing().await.ok()
            } else {
                None
            };
            let _ = this.update(cx, |view, cx| {
                view.commands_ui.busy = false;
                if let Some(state) = fresh {
                    view.commands_ui.routing = Some(state);
                }
                match result {
                    Ok(state) => {
                        view.commands_ui.routing = Some(state);
                        view.commands_ui.error = None;
                    }
                    Err(error) => view.commands_ui.error = Some(error_text(&error)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- rendering -------------------------------------------------------------

    pub(super) fn render_commands_dialog(
        &self,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.commands_ui.dialog.as_ref()?;
        let (id, width, body): (&'static str, f32, AnyElement) = match dialog {
            CommandsDialog::Manage { workspace, scope } => (
                "commands-card",
                760_f32,
                self.commands_manage(*workspace, *scope, t, cx),
            ),
            CommandsDialog::EditCommand(draft) => {
                ("command-editor", 680_f32, self.command_editor(draft, t, cx))
            }
            CommandsDialog::EditTemplate(draft) => (
                "command-template-editor",
                680_f32,
                self.command_template_editor(draft, t, cx),
            ),
        };
        Some(
            div()
                .id("commands-overlay")
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.45))
                .child(
                    div()
                        .id(id)
                        .w(px(width))
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

    fn commands_manage(
        &self,
        workspace: WorkspaceId,
        scope: Scope,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut body = div().flex().flex_col().gap(px(10.)).child(
            div()
                .text_size(px(16.))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Commands and prompt templates"),
        );
        body = body.child(note(
            "Commands resolve project → workspace → global; the most specific definition wins. Templates apply to routed Git actions and dispatch through the same routes. Every edit is compare-and-set.",
            t,
        ));
        let mut tabs = div().flex().items_center().gap(px(6.)).child(
            div()
                .text_size(t.text_size_sm)
                .text_color(t.text_secondary)
                .child("Show"),
        );
        for (index, (tab, label)) in [(Tab::Commands, "Commands"), (Tab::Templates, "Templates")]
            .into_iter()
            .enumerate()
        {
            let active = self.commands_ui.tab == tab;
            let hover = t.raised_hover;
            tabs = tabs.child(
                div()
                    .id(("commands-tab", index))
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(t.radius_sm)
                    .text_size(t.text_size_sm)
                    .bg(if active { t.row_selected } else { t.surface_bg })
                    .text_color(if active {
                        t.text_primary
                    } else {
                        t.text_secondary
                    })
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(label)
                    .on_click(cx.listener(move |view, _, _, cx| view.set_commands_tab(tab, cx))),
            );
        }
        body = body.child(tabs);
        let mut chips = div().flex().items_center().gap(px(4.)).flex_wrap();
        for (index, (choice, label)) in self
            .commands_scope_choices(workspace)
            .into_iter()
            .enumerate()
        {
            let active = choice == scope;
            let hover = t.raised_hover;
            chips = chips.child(
                div()
                    .id(("commands-scope", index))
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(t.radius_sm)
                    .text_size(t.text_size_sm)
                    .bg(if active { t.row_selected } else { t.surface_bg })
                    .text_color(if active {
                        t.text_primary
                    } else {
                        t.text_secondary
                    })
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.set_commands_scope(workspace, choice, cx)
                    })),
            );
        }
        body = body.child(chips);
        if let Some(error) = &self.commands_ui.error {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error.clone()),
            );
        }
        body = match self.commands_ui.tab {
            Tab::Commands => body.child(self.commands_list(workspace, scope, t, cx)),
            Tab::Templates => body.child(self.templates_list(workspace, scope, t, cx)),
        };
        if let Some(dispatch) = &self.commands_ui.dispatch {
            body = body.child(self.dispatch_preview(dispatch, t));
        }
        let busy = self.commands_ui.busy;
        let close_id = "commands-close";
        body.child(
            div()
                .flex()
                .justify_end()
                .child(button(close_id, "Close", !busy, t).when(!busy, |d| {
                    d.on_click(cx.listener(|view, _, window, cx| {
                        view.commands_ui.close();
                        view.close_dialog(window, cx);
                    }))
                })),
        )
        .into_any_element()
    }

    fn commands_list(
        &self,
        workspace: WorkspaceId,
        scope: Scope,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows = div().flex().flex_col().gap(px(4.));
        rows = rows.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(heading("Commands", t))
                .child(
                    button("command-new", "New command", !self.commands_ui.busy, t).when(
                        !self.commands_ui.busy,
                        |d| {
                            d.on_click(cx.listener(move |view, _, window, cx| {
                                view.open_command_editor(workspace, scope, None, window, cx)
                            }))
                        },
                    ),
                ),
        );
        let commands = self.commands_for_scope(scope);
        if commands.is_empty() {
            rows = rows.child(note("No command is defined at this scope.", t));
        }
        for (index, command) in commands.into_iter().enumerate() {
            let name = command.name.clone();
            let delete_name = name.clone();
            let run_name = name.clone();
            let description = command
                .description
                .clone()
                .unwrap_or_else(|| command.body.lines().next().unwrap_or_default().to_owned());
            let busy = self.commands_ui.busy;
            rows = rows.child(
                div()
                    .id(("command-row", index))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(t.radius_sm)
                    .bg(t.surface_bg)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(t.text_size_sm)
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(command.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(t.text_size_sm)
                                    .text_color(t.text_secondary)
                                    .child(description),
                            ),
                    )
                    .child(
                        button(format!("command-edit-{index}"), "Edit", !busy, t).when(
                            !busy,
                            |d| {
                                d.on_click(cx.listener({
                                    let command = Some(command.clone());
                                    move |view, _, window, cx| {
                                        view.open_command_editor(
                                            workspace,
                                            scope,
                                            command.clone(),
                                            window,
                                            cx,
                                        )
                                    }
                                }))
                            },
                        ),
                    )
                    .child(
                        button(format!("command-run-{index}"), "Prepare", !busy, t).when(
                            !busy,
                            |d| {
                                d.on_click(cx.listener(move |view, _, _, cx| {
                                    view.run_command(run_name.clone(), cx)
                                }))
                            },
                        ),
                    )
                    .child(
                        button(format!("command-delete-{index}"), "Delete", !busy, t).when(
                            !busy,
                            |d| {
                                d.on_click(cx.listener(move |view, _, _, cx| {
                                    view.delete_command(scope, delete_name.clone(), cx)
                                }))
                            },
                        ),
                    ),
            );
        }
        rows.into_any_element()
    }

    fn templates_list(
        &self,
        workspace: WorkspaceId,
        scope: Scope,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(heading("Prompt templates for routed actions", t));
        let routing = self.commands_routing();
        for (index, action) in GitTask::ALL.into_iter().enumerate() {
            let stored = routing.and_then(|routing| {
                routing
                    .templates
                    .iter()
                    .find(|template| template.scope == scope && template.action == action)
            });
            let detail = match stored {
                Some(template) => format!("{} bytes at this scope", template.body.len()),
                None => "built-in default".to_owned(),
            };
            let busy = self.commands_ui.busy;
            rows = rows.child(
                div()
                    .id(("template-row", index))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(t.radius_sm)
                    .bg(t.surface_bg)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(t.text_size_sm)
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(action.label()),
                            )
                            .child(
                                div()
                                    .text_size(t.text_size_sm)
                                    .text_color(t.text_secondary)
                                    .child(detail),
                            ),
                    )
                    .child(
                        button(format!("template-edit-{index}"), "Edit", !busy, t).when(
                            !busy,
                            |d| {
                                d.on_click(cx.listener(move |view, _, window, cx| {
                                    view.open_command_template_editor(
                                        workspace, scope, action, window, cx,
                                    )
                                }))
                            },
                        ),
                    )
                    .child(
                        button(
                            format!("template-remove-{index}"),
                            "Remove",
                            !busy && stored.is_some(),
                            t,
                        )
                        .when(!busy && stored.is_some(), |d| {
                            d.on_click(cx.listener(move |view, _, _, cx| {
                                view.remove_command_template(scope, action, cx)
                            }))
                        }),
                    ),
            );
        }
        rows.into_any_element()
    }

    fn dispatch_preview(&self, dispatch: &TaskDispatch, t: &Theme) -> AnyElement {
        let state = match &dispatch.state {
            TaskDispatchState::Prepared => "Prepared — not sent".to_owned(),
            TaskDispatchState::Running => "Running".to_owned(),
            TaskDispatchState::Completed => "Completed".to_owned(),
            TaskDispatchState::Failed { message } => format!("Failed: {message}"),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(8.))
            .rounded(t.radius_sm)
            .bg(t.surface_bg)
            .child(
                div()
                    .text_size(t.text_size_sm)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{} — {}", dispatch.task.label(), state)),
            )
            .child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.text_secondary)
                    .child(dispatch.prompt.clone()),
            )
            .into_any_element()
    }

    fn command_editor(
        &self,
        draft: &CommandDraft,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if draft.existing {
                        "Edit command"
                    } else {
                        "New command"
                    }),
            )
            .child(field("Name", draft.name.clone(), t))
            .child(field("Description", draft.description.clone(), t))
            .child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.text_secondary)
                    .child("Prompt body — {{branch}}, {{base}}, {{diff}} and {{task}} are filled at dispatch"),
            )
            .child(div().h(px(320.)).border_1().border_color(t.border).child(draft.body.clone()));
        if let Some(error) = &draft.error {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error.clone()),
            );
        }
        let busy = draft.busy || self.commands_ui.busy;
        let (workspace, scope) = (draft.workspace, draft.scope);
        body.child(
            div()
                .flex()
                .justify_end()
                .gap(px(6.))
                .child(
                    button("command-cancel", "Cancel", !busy, t).when(!busy, |d| {
                        d.on_click(cx.listener(move |view, _, _, cx| {
                            view.back_to_commands(workspace, scope, cx)
                        }))
                    }),
                )
                .child(button("command-save", "Save", !busy, t).when(!busy, |d| {
                    d.on_click(cx.listener(|view, _, _, cx| view.save_command(cx)))
                })),
        )
        .into_any_element()
    }

    fn command_template_editor(
        &self,
        draft: &TemplateDraft,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Prompt template — {}", draft.action.label())),
            )
            .child(note(
                if draft.configured {
                    "This scope stores a template; the built-in default applies if you remove it."
                } else {
                    "The editor starts from the built-in default; saving stores it at this scope."
                },
                t,
            ))
            .child(
                div()
                    .h(px(360.))
                    .border_1()
                    .border_color(t.border)
                    .child(draft.body.clone()),
            );
        if let Some(error) = &draft.error {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error.clone()),
            );
        }
        let busy = draft.busy || self.commands_ui.busy;
        let (workspace, scope) = (draft.workspace, draft.scope);
        body.child(
            div()
                .flex()
                .justify_end()
                .gap(px(6.))
                .child(
                    button("template-cancel", "Cancel", !busy, t).when(!busy, |d| {
                        d.on_click(cx.listener(move |view, _, _, cx| {
                            view.back_to_commands(workspace, scope, cx)
                        }))
                    }),
                )
                .child(button("template-save", "Save", !busy, t).when(!busy, |d| {
                    d.on_click(cx.listener(|view, _, _, cx| view.save_command_template(cx)))
                })),
        )
        .into_any_element()
    }
}
