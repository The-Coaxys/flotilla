//! Fast/Thorough routes: per-scope route and prompt-template editing, routed
//! Git dispatch, and the prepared-prompt modal.
//!
//! Route edits retain their opening revision: a concurrent change fails with a
//! revision conflict, the draft is preserved, and the editor adopts the new
//! revision so saving again applies the user's work instead of overwriting it.
//! A routed action is only ever dispatched through the daemon; a prepared
//! prompt is shown without claiming it ran. Submit failures keep the modal open
//! with the daemon's message. Pending rows are local, dispatch rows reflect the
//! daemon's `TaskDispatch` state, and success is only shown for `Completed`.
use super::*;
use fl_model::chat::ProviderDiscovery;
use fl_model::routing::{
    self, DispatchMode, GitTask, PromptTemplate, RouteKey, RouteOverride, RouteSpec,
    RoutingMutation, RoutingState, Scope, TaskDispatch, TaskDispatchState, TemplateContext,
    default_template, render_template,
};
use fl_model::{ProjectId, WorkspaceId, WorktreeId};

const DISPATCH_MODES: [DispatchMode; 5] = [
    DispatchMode::ReusePrepare,
    DispatchMode::ReuseSubmit,
    DispatchMode::NewPrepare,
    DispatchMode::NewSubmit,
    DispatchMode::Background,
];

/// Dispatch rows kept for the current session. Prevents unbounded growth.
const MAX_NOTICES: usize = 16;
/// Rows shown in the strip above the bottom bar.
const MAX_DISPATCH_ROWS: usize = 4;

#[derive(Default)]
pub(super) struct RoutesUi {
    dialog: Option<RoutesDialog>,
    pub(super) busy: bool,
    error: Option<SharedString>,
    /// A routing snapshot learned by an RPC. It wins over pushed state when
    /// newer, so an edit right after a save uses the revision the daemon
    /// returned rather than a stale push.
    snapshot: Option<RoutingState>,
    providers: Option<Vec<ProviderDiscovery>>,
    providers_busy: bool,
    providers_error: Option<SharedString>,
    /// Dispatches this client saw; the daemon has not (yet) echoed them.
    notices: Vec<DispatchNotice>,
    /// Requests in flight; replaced by the daemon's record.
    pending: Vec<PendingDispatch>,
    next_pending: u64,
    next_notice: u64,
}

impl RoutesUi {
    pub(super) fn close(&mut self) {
        if self.busy {
            return;
        }
        self.dialog = None;
        self.error = None;
    }
}

enum RoutesDialog {
    Manage {
        workspace: WorkspaceId,
        scope: Scope,
    },
    EditRoute(RouteDraft),
    EditTemplate(TemplateDraft),
    Dispatch(DispatchModal),
}

struct RouteDraft {
    workspace: WorkspaceId,
    scope: Scope,
    key: RouteKey,
    rev: u64,
    provider: Option<String>,
    profile: Entity<TextInput>,
    model: Entity<TextInput>,
    thinking: Entity<TextInput>,
    click: DispatchMode,
    alternate_click: DispatchMode,
    error: Option<SharedString>,
    busy: bool,
    _events: Vec<Subscription>,
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
    /// Keeps the preview live while the template is edited.
    _observe: Subscription,
}

struct DispatchModal {
    dispatch: TaskDispatch,
    /// The extra context the prompt was prepared with, re-sent on Send.
    context: String,
    sending: bool,
    error: Option<SharedString>,
    copied: bool,
}

struct PendingDispatch {
    seq: u64,
    worktree: WorktreeId,
    task: GitTask,
    mode: DispatchMode,
}

enum DispatchNotice {
    Record {
        dispatch: TaskDispatch,
        context: String,
    },
    Failed {
        /// Unique per client, so several failures of one action coexist.
        seq: u64,
        worktree: WorktreeId,
        task: GitTask,
        mode: DispatchMode,
        message: SharedString,
    },
}

/// The next dispatch mode in the picker.
fn next_dispatch_mode(mode: DispatchMode) -> DispatchMode {
    let index = DISPATCH_MODES
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(0);
    DISPATCH_MODES[(index + 1) % DISPATCH_MODES.len()]
}

/// The mode a plain or alternate click dispatches with, resolved by the
/// daemon's own route precedence.
fn resolve_dispatch_mode(
    routing: &RoutingState,
    task: GitTask,
    project: ProjectId,
    workspace: WorkspaceId,
    alternate: bool,
) -> Option<DispatchMode> {
    routing
        .route(task, project, workspace)
        .map(|route| route.mode(alternate))
}

/// The submit form of a prepare mode; `None` when the mode already submits.
fn submit_mode(mode: DispatchMode) -> Option<DispatchMode> {
    match mode {
        DispatchMode::ReusePrepare => Some(DispatchMode::ReuseSubmit),
        DispatchMode::NewPrepare => Some(DispatchMode::NewSubmit),
        DispatchMode::ReuseSubmit | DispatchMode::NewSubmit | DispatchMode::Background => None,
    }
}

/// Render a prompt draft with the values the daemon has. Templates never run
/// shell code: interpolation is the model's verbatim substitution.
fn preview_prompt(body: &str, context: &TemplateContext) -> String {
    render_template(body, context)
}

/// The state a dispatch row reports. Completed is only ever the daemon's.
fn dispatch_state_label(state: &TaskDispatchState) -> SharedString {
    match state {
        TaskDispatchState::Prepared => "Prepared — not sent".into(),
        TaskDispatchState::Running => "Running".into(),
        TaskDispatchState::Completed => "Completed".into(),
        TaskDispatchState::Failed { message } => format!("Failed: {message}").into(),
    }
}

fn optional_field(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

/// The editable form of a route, validated before it reaches the daemon.
#[derive(Debug, Clone)]
struct RouteForm {
    provider: String,
    profile: String,
    model: String,
    thinking: String,
    click: DispatchMode,
    alternate_click: DispatchMode,
}

impl RouteForm {
    fn to_spec(&self) -> Result<RouteSpec, &'static str> {
        let spec = RouteSpec {
            provider: self.provider.trim().to_owned(),
            profile: optional_field(&self.profile),
            model: optional_field(&self.model),
            thinking: optional_field(&self.thinking),
            click: self.click,
            alternate_click: self.alternate_click,
        };
        spec.validate()?;
        Ok(spec)
    }
}

fn validate_template_body(body: &str) -> Result<(), &'static str> {
    PromptTemplate {
        scope: Scope::Global,
        action: GitTask::Commit,
        body: body.to_owned(),
    }
    .validate()
}

fn group_label(group: routing::RouteGroup) -> &'static str {
    match group {
        routing::RouteGroup::Fast => "Fast",
        routing::RouteGroup::Thorough => "Thorough",
    }
}

fn key_label(key: RouteKey) -> String {
    match key {
        RouteKey::Group(group) => format!("{} group", group_label(group)),
        RouteKey::Action(action) => action.label().to_owned(),
    }
}

fn note(text: impl Into<SharedString>, t: &Theme) -> gpui::Div {
    div()
        .text_size(t.text_size_sm)
        .text_color(t.text_secondary)
        .child(text.into())
}

fn action_button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let hover = t.raised_hover;
    div()
        .id(id.into())
        .px(px(8.))
        .py(px(4.))
        .rounded(t.radius_sm)
        .text_size(t.text_size_sm)
        .text_color(if enabled {
            t.text_secondary
        } else {
            t.text_tertiary
        })
        .when(enabled, |d| d.cursor_pointer().hover(move |s| s.bg(hover)))
        .child(label.into())
}

fn heading(text: impl Into<SharedString>, t: &Theme) -> gpui::Div {
    div()
        .mt(px(4.))
        .text_size(t.text_size_sm)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text_secondary)
        .child(text.into())
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

    fn routes_enabled(&self) -> bool {
        self.state.live && self.routing().is_some() && !self.routes_ui.busy
    }

    /// The newest routing snapshot this client knows, borrowed in place.
    pub(super) fn routing(&self) -> Option<&RoutingState> {
        match (
            self.state.routing.as_ref(),
            self.routes_ui.snapshot.as_ref(),
        ) {
            (Some(pushed), Some(learned)) if learned.rev > pushed.rev => Some(learned),
            (Some(pushed), _) => Some(pushed),
            (None, learned) => learned,
        }
    }

    fn scope_label(&self, scope: Scope) -> String {
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

    fn scope_choices(&self, workspace: WorkspaceId) -> Vec<(Scope, String)> {
        let mut choices = vec![(Scope::Global, self.scope_label(Scope::Global))];
        choices.push((
            Scope::Workspace(workspace),
            self.scope_label(Scope::Workspace(workspace)),
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

    /// Branch and base of the selected worktree, for the live preview.
    fn preview_context(&self) -> TemplateContext {
        let Some(worktree) = self.selected_worktree() else {
            return TemplateContext::default();
        };
        let base = worktree.target.clone().or_else(|| {
            self.state.tree.as_ref().and_then(|tree| {
                tree.projects
                    .iter()
                    .find(|project| project.id == worktree.project)
                    .and_then(|project| project.default_branch.clone())
            })
        });
        TemplateContext {
            branch: worktree.branch.clone(),
            base,
            ..TemplateContext::default()
        }
    }

    pub(super) fn open_routes(&mut self, workspace: WorkspaceId, cx: &mut Context<Self>) {
        if !self.state.live {
            self.show_error("Offline — routing changes unavailable.", cx);
            return;
        }
        let has_routing = self.routing().is_some();
        if !has_routing {
            self.show_error(
                "Routes need daemon protocol 0.4 (or are still loading).",
                cx,
            );
            return;
        }
        self.dialog = None;
        self.sections_ui.close();
        self.git_menu = false;
        self.routes_ui.error = None;
        self.routes_ui.dialog = Some(RoutesDialog::Manage {
            workspace,
            scope: Scope::Workspace(workspace),
        });
        self.refresh_routing_snapshot(cx);
        if self.routes_ui.providers.is_none() && !self.routes_ui.providers_busy {
            self.refresh_providers(cx);
        }
        cx.notify();
    }

    fn refresh_routing_snapshot(&mut self, cx: &mut Context<Self>) {
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.routing().await;
            let _ = this.update(cx, |view, cx| {
                if let Ok(state) = result {
                    let pushed = view
                        .state
                        .routing
                        .as_ref()
                        .map(|pushed| pushed.rev)
                        .unwrap_or(0);
                    if state.rev >= pushed {
                        view.routes_ui.snapshot = Some(state);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_providers(&mut self, cx: &mut Context<Self>) {
        if self.routes_ui.providers_busy {
            return;
        }
        self.routes_ui.providers_busy = true;
        self.routes_ui.providers_error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync.chat_providers().await;
            let _ = this.update(cx, |view, cx| {
                view.routes_ui.providers_busy = false;
                match result {
                    Ok(providers) => view.routes_ui.providers = Some(providers),
                    Err(error) => view.routes_ui.providers_error = Some(error_text(&error)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn available_provider(&self) -> Option<String> {
        self.routes_ui
            .providers
            .as_ref()?
            .iter()
            .find(|provider| provider.unavailable_reason.is_none() && provider.capabilities.chat)
            .map(|provider| provider.provider.id.clone())
    }

    fn choose_provider(&mut self, provider: String, cx: &mut Context<Self>) {
        if let Some(RoutesDialog::EditRoute(draft)) = self.routes_ui.dialog.as_mut() {
            draft.provider = Some(provider);
            draft.error = None;
        }
        cx.notify();
    }

    fn back_to_manage(&mut self, workspace: WorkspaceId, scope: Scope, cx: &mut Context<Self>) {
        if self.routes_ui.busy {
            return;
        }
        self.routes_ui.error = None;
        self.routes_ui.dialog = Some(RoutesDialog::Manage { workspace, scope });
        cx.notify();
    }

    // ---- route editing ---------------------------------------------------------

    pub(super) fn open_route_editor(
        &mut self,
        workspace: WorkspaceId,
        scope: Scope,
        key: RouteKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.routes_enabled() {
            return;
        }
        let (existing, rev) = {
            let Some(routing) = self.routing() else {
                return;
            };
            (
                routing
                    .routes
                    .iter()
                    .find(|route| route.scope == scope && route.key == key)
                    .map(|route| route.route.clone()),
                routing.rev,
            )
        };
        let spec = existing.unwrap_or_else(|| {
            RouteSpec::new(self.available_provider().unwrap_or_default())
        });
        let profile = cx.new(|cx| TextInput::new(cx, "Provider profile (optional)"));
        let model = cx.new(|cx| TextInput::new(cx, "Model (optional)"));
        let thinking = cx.new(|cx| TextInput::new(cx, "Thinking setting (optional)"));
        profile.update(cx, |input, cx| {
            input.set_text(spec.profile.clone().unwrap_or_default(), cx)
        });
        model.update(cx, |input, cx| {
            input.set_text(spec.model.clone().unwrap_or_default(), cx)
        });
        thinking.update(cx, |input, cx| {
            input.set_text(spec.thinking.clone().unwrap_or_default(), cx)
        });
        let mut events = Vec::new();
        for input in [&profile, &model, &thinking] {
            events.push(cx.subscribe_in(
                input,
                window,
                |view, _, event: &TextInputEvent, window, cx| match event {
                    TextInputEvent::Submitted(_) => view.save_route(cx),
                    TextInputEvent::Cancelled => view.close_dialog(window, cx),
                },
            ));
        }
        self.dialog = None;
        self.sections_ui.close();
        self.routes_ui.error = None;
        self.routes_ui.dialog = Some(RoutesDialog::EditRoute(RouteDraft {
            workspace,
            scope,
            key,
            rev,
            provider: soft_provider(&spec.provider),
            profile,
            model,
            thinking,
            click: spec.click,
            alternate_click: spec.alternate_click,
            error: None,
            busy: false,
            _events: events,
        }));
        profile.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn cycle_click(&mut self, cx: &mut Context<Self>) {
        if let Some(RoutesDialog::EditRoute(draft)) = self.routes_ui.dialog.as_mut() {
            draft.click = next_dispatch_mode(draft.click);
        }
        cx.notify();
    }

    fn cycle_alternate(&mut self, cx: &mut Context<Self>) {
        if let Some(RoutesDialog::EditRoute(draft)) = self.routes_ui.dialog.as_mut() {
            draft.alternate_click = next_dispatch_mode(draft.alternate_click);
        }
        cx.notify();
    }

    fn save_route(&mut self, cx: &mut Context<Self>) {
        if self.routes_ui.busy {
            return;
        }
        let Some(RoutesDialog::EditRoute(draft)) = self.routes_ui.dialog.as_mut() else {
            return;
        };
        let form = RouteForm {
            provider: draft.provider.clone().unwrap_or_default(),
            profile: draft.profile.read(cx).text().to_owned(),
            model: draft.model.read(cx).text().to_owned(),
            thinking: draft.thinking.read(cx).text().to_owned(),
            click: draft.click,
            alternate_click: draft.alternate_click,
        };
        let spec = match form.to_spec() {
            Ok(spec) => spec,
            Err(error) => {
                draft.error = Some(error.into());
                cx.notify();
                return;
            }
        };
        if let Some(providers) = self.routes_ui.providers.as_ref()
            && let Some(found) = providers
                .iter()
                .find(|provider| provider.provider.id == spec.provider)
            && let Some(reason) = found
                .unavailable_reason
                .clone()
                .or_else(|| (!found.capabilities.chat).then(|| "no chat capability".to_owned()))
        {
            draft.error = Some(format!("{} is unavailable: {reason}", spec.provider).into());
            cx.notify();
            return;
        }
        let (workspace, scope, key, rev) = (draft.workspace, draft.scope, draft.key, draft.rev);
        draft.busy = true;
        draft.error = None;
        self.routes_ui.busy = true;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .mutate_routing(
                    rev,
                    RoutingMutation::SetRoute(RouteOverride {
                        scope,
                        key,
                        route: spec,
                    }),
                )
                .await;
            let fresh = if matches!(&result, Err(error) if is_conflict(error)) {
                Some(sync.routing().await)
            } else {
                None
            };
            let _ = this.update(cx, |view, cx| {
                view.routes_ui.busy = false;
                match result {
                    Ok(state) => {
                        view.routes_ui.snapshot = Some(state);
                        view.routes_ui.error = None;
                        view.routes_ui.dialog = Some(RoutesDialog::Manage { workspace, scope });
                    }
                    Err(error) => {
                        if let Some(Ok(state)) = fresh.as_ref() {
                            view.routes_ui.snapshot = Some(state.clone());
                        }
                        if let Some(RoutesDialog::EditRoute(draft)) =
                            view.routes_ui.dialog.as_mut()
                        {
                            draft.busy = false;
                            if is_conflict(&error) {
                                if let Some(Ok(state)) = fresh.as_ref() {
                                    draft.rev = state.rev;
                                }
                                draft.error = Some(conflict_message(fresh.as_ref()).into());
                            } else {
                                draft.error = Some(error_text(&error));
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn remove_route(
        &mut self,
        workspace: WorkspaceId,
        scope: Scope,
        key: RouteKey,
        rev: u64,
        cx: &mut Context<Self>,
    ) {
        if self.routes_ui.busy {
            return;
        }
        self.routes_ui.busy = true;
        self.routes_ui.error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .mutate_routing(rev, RoutingMutation::RemoveRoute { scope, key })
                .await;
            let fresh = if matches!(&result, Err(error) if is_conflict(error)) {
                Some(sync.routing().await)
            } else {
                None
            };
            let _ = this.update(cx, |view, cx| {
                view.routes_ui.busy = false;
                match result {
                    Ok(state) => {
                        view.routes_ui.snapshot = Some(state);
                        view.routes_ui.error = None;
                        view.routes_ui.dialog = Some(RoutesDialog::Manage { workspace, scope });
                    }
                    Err(error) => {
                        if let Some(Ok(state)) = fresh.as_ref() {
                            view.routes_ui.snapshot = Some(state.clone());
                        }
                        view.routes_ui.error = Some(if is_conflict(&error) {
                            conflict_message(fresh.as_ref()).into()
                        } else {
                            error_text(&error)
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- template editing ------------------------------------------------------

    pub(super) fn open_template_editor(
        &mut self,
        workspace: WorkspaceId,
        scope: Scope,
        action: GitTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.routes_enabled() {
            return;
        }
        let (configured, rev) = {
            let Some(routing) = self.routing() else {
                return;
            };
            (
                routing
                    .templates
                    .iter()
                    .find(|template| template.scope == scope && template.action == action)
                    .map(|template| template.body.clone()),
                routing.rev,
            )
        };
        let body_text = configured
            .clone()
            .unwrap_or_else(|| default_template(action).to_owned());
        let body = cx.new(|cx| EditorView::new("Prompt.md", body_text.as_bytes(), None, cx));
        let observe = cx.observe(&body, |_, _, cx| cx.notify());
        self.dialog = None;
        self.sections_ui.close();
        self.routes_ui.error = None;
        self.routes_ui.dialog = Some(RoutesDialog::EditTemplate(TemplateDraft {
            workspace,
            scope,
            action,
            rev,
            body: body.clone(),
            configured: configured.is_some(),
            error: None,
            busy: false,
            _observe: observe,
        }));
        body.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn save_template(&mut self, cx: &mut Context<Self>) {
        if self.routes_ui.busy {
            return;
        }
        let Some(RoutesDialog::EditTemplate(draft)) = self.routes_ui.dialog.as_mut() else {
            return;
        };
        let body = draft.body.read(cx).buffer().text();
        if let Err(error) = validate_template_body(&body) {
            draft.error = Some(error.into());
            cx.notify();
            return;
        }
        let (workspace, scope, action, rev) =
            (draft.workspace, draft.scope, draft.action, draft.rev);
        draft.busy = true;
        draft.error = None;
        self.routes_ui.busy = true;
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
                Some(sync.routing().await)
            } else {
                None
            };
            let _ = this.update(cx, |view, cx| {
                view.routes_ui.busy = false;
                match result {
                    Ok(state) => {
                        view.routes_ui.snapshot = Some(state);
                        view.routes_ui.error = None;
                        view.routes_ui.dialog = Some(RoutesDialog::Manage { workspace, scope });
                    }
                    Err(error) => {
                        if let Some(Ok(state)) = fresh.as_ref() {
                            view.routes_ui.snapshot = Some(state.clone());
                        }
                        if let Some(RoutesDialog::EditTemplate(draft)) =
                            view.routes_ui.dialog.as_mut()
                        {
                            draft.busy = false;
                            if is_conflict(&error) {
                                if let Some(Ok(state)) = fresh.as_ref() {
                                    draft.rev = state.rev;
                                }
                                draft.error = Some(conflict_message(fresh.as_ref()).into());
                            } else {
                                draft.error = Some(error_text(&error));
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn remove_template(
        &mut self,
        workspace: WorkspaceId,
        scope: Scope,
        action: GitTask,
        rev: u64,
        cx: &mut Context<Self>,
    ) {
        if self.routes_ui.busy {
            return;
        }
        self.routes_ui.busy = true;
        self.routes_ui.error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .mutate_routing(rev, RoutingMutation::RemoveTemplate { scope, action })
                .await;
            let fresh = if matches!(&result, Err(error) if is_conflict(error)) {
                Some(sync.routing().await)
            } else {
                None
            };
            let _ = this.update(cx, |view, cx| {
                view.routes_ui.busy = false;
                match result {
                    Ok(state) => {
                        view.routes_ui.snapshot = Some(state);
                        view.routes_ui.error = None;
                        view.routes_ui.dialog = Some(RoutesDialog::Manage { workspace, scope });
                    }
                    Err(error) => {
                        if let Some(Ok(state)) = fresh.as_ref() {
                            view.routes_ui.snapshot = Some(state.clone());
                        }
                        if let Some(RoutesDialog::EditTemplate(draft)) =
                            view.routes_ui.dialog.as_mut()
                        {
                            draft.busy = false;
                            if is_conflict(&error) {
                                if let Some(Ok(state)) = fresh.as_ref() {
                                    draft.rev = state.rev;
                                }
                                draft.error = Some(conflict_message(fresh.as_ref()).into());
                            } else {
                                draft.error = Some(error_text(&error));
                            }
                        } else {
                            view.routes_ui.error = Some(if is_conflict(&error) {
                                conflict_message(fresh.as_ref()).into()
                            } else {
                                error_text(&error)
                            });
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- dispatch --------------------------------------------------------------

    /// The click mode a routed action would use in this worktree, for the
    /// action menu's mode hint.
    pub(super) fn route_click_hint(
        &self,
        worktree: &Worktree,
        task: GitTask,
    ) -> Option<DispatchMode> {
        let routing = self.routing()?;
        let workspace = self
            .state
            .tree
            .as_ref()?
            .projects
            .iter()
            .find(|project| project.id == worktree.project)?
            .workspace;
        routing
            .route(task, worktree.project, workspace)
            .map(|route| route.click)
    }

    /// Whether a routed dispatch for this worktree is still in flight.
    pub(super) fn dispatch_pending(&self, worktree: WorktreeId) -> bool {
        self.routes_ui
            .pending
            .iter()
            .any(|pending| pending.worktree == worktree)
    }

    pub(super) fn dispatch_routed(
        &mut self,
        worktree: WorktreeId,
        task: GitTask,
        alternate: bool,
        cx: &mut Context<Self>,
    ) {
        if self.dispatch_pending(worktree) {
            self.show_error(
                "A routed action is already dispatching for this worktree; wait for its row.",
                cx,
            );
            return;
        }
        let Some(project_id) = self.state.worktree(worktree).map(|entry| entry.project) else {
            return;
        };
        let Some(project) = self.state.tree.as_ref().and_then(|tree| {
            tree.projects
                .iter()
                .find(|project| project.id == project_id)
                .cloned()
        }) else {
            return;
        };
        let resolved = self.routing().map(|routing| {
            resolve_dispatch_mode(routing, task, project.id, project.workspace, alternate)
        });
        let mode = match resolved {
            Some(Some(mode)) => mode,
            Some(None) => {
                self.show_error(
                    format!(
                        "No route for {}. Open Routes to configure one for the {} group.",
                        task.label(),
                        group_label(task.group())
                    ),
                    cx,
                );
                return;
            }
            None => {
                self.show_error(
                    "Routes unavailable: reconnect to a daemon supporting protocol 0.4.",
                    cx,
                );
                return;
            }
        };
        self.dispatch_task(worktree, task, mode, String::new(), cx);
    }

    fn dispatch_task(
        &mut self,
        worktree: WorktreeId,
        task: GitTask,
        mode: DispatchMode,
        context: String,
        cx: &mut Context<Self>,
    ) {
        let seq = self.routes_ui.next_pending;
        self.routes_ui.next_pending += 1;
        self.routes_ui.pending.push(PendingDispatch {
            seq,
            worktree,
            task,
            mode,
        });
        self.routes_ui.error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .dispatch_git_task(worktree, task, Some(mode), false, context.clone())
                .await;
            let _ = this.update(cx, |view, cx| {
                view.routes_ui.pending.retain(|pending| pending.seq != seq);
                match result {
                    Ok(dispatch) => {
                        if dispatch.state == TaskDispatchState::Prepared {
                            view.routes_ui.dialog =
                                Some(RoutesDialog::Dispatch(DispatchModal {
                                    dispatch: dispatch.clone(),
                                    context: context.clone(),
                                    sending: false,
                                    error: None,
                                    copied: false,
                                }));
                        }
                        view.push_notice(DispatchNotice::Record { dispatch, context });
                    }
                    Err(error) => {
                        let notice_seq = view.routes_ui.next_notice;
                        view.routes_ui.next_notice = notice_seq + 1;
                        let message = error_text(&error);
                        view.push_notice(DispatchNotice::Failed {
                            seq: notice_seq,
                            worktree,
                            task,
                            mode,
                            message: message.clone(),
                        });
                        view.show_error(message, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn push_notice(&mut self, notice: DispatchNotice) {
        self.routes_ui.notices.insert(0, notice);
        self.routes_ui.notices.truncate(MAX_NOTICES);
    }

    fn open_prepared(&mut self, dispatch: TaskDispatch, context: String, cx: &mut Context<Self>) {
        self.dialog = None;
        self.sections_ui.close();
        self.routes_ui.dialog = Some(RoutesDialog::Dispatch(DispatchModal {
            dispatch,
            context,
            sending: false,
            error: None,
            copied: false,
        }));
        cx.notify();
    }

    fn copy_prepared(&mut self, cx: &mut Context<Self>) {
        let Some(RoutesDialog::Dispatch(modal)) = self.routes_ui.dialog.as_ref() else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(modal.dispatch.prompt.clone()));
        if let Some(RoutesDialog::Dispatch(modal)) = self.routes_ui.dialog.as_mut() {
            modal.copied = true;
        }
        cx.notify();
    }

    fn send_prepared(&mut self, cx: &mut Context<Self>) {
        let Some(RoutesDialog::Dispatch(modal)) = self.routes_ui.dialog.as_mut() else {
            return;
        };
        if modal.sending {
            return;
        }
        let Some(mode) = submit_mode(modal.dispatch.mode) else {
            modal.error =
                Some("This dispatch already submits or runs in the background.".into());
            cx.notify();
            return;
        };
        let (worktree, task, context, id) = (
            modal.dispatch.worktree,
            modal.dispatch.task,
            modal.context.clone(),
            modal.dispatch.id,
        );
        modal.sending = true;
        modal.error = None;
        let sync = self.sync.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = sync
                .dispatch_git_task(worktree, task, Some(mode), false, context.clone())
                .await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(dispatch) => {
                        // The prepared record was never sent; its submitted
                        // form replaces it.
                        view.routes_ui.notices.retain(|notice| {
                            !matches!(
                                notice,
                                DispatchNotice::Record { dispatch, .. } if dispatch.id == id
                            )
                        });
                        view.routes_ui.dialog = None;
                        view.push_notice(DispatchNotice::Record { dispatch, context });
                    }
                    Err(error) => {
                        if let Some(RoutesDialog::Dispatch(modal)) =
                            view.routes_ui.dialog.as_mut()
                        {
                            modal.sending = false;
                            modal.error = Some(error_text(&error));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- rendering -------------------------------------------------------------

    pub(super) fn render_routes_dialog(
        &self,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.routes_ui.dialog.as_ref()?;
        let (id, width, body): (&'static str, f32, AnyElement) = match dialog {
            RoutesDialog::Manage { workspace, scope } => (
                "routes-card",
                760_f32,
                self.routes_manage(*workspace, *scope, t, cx),
            ),
            RoutesDialog::EditRoute(draft) => {
                ("route-card", 640_f32, self.route_editor(draft, t, cx))
            }
            RoutesDialog::EditTemplate(draft) => {
                ("template-card", 680_f32, self.template_editor(draft, t, cx))
            }
            RoutesDialog::Dispatch(modal) => {
                ("dispatch-card", 640_f32, self.dispatch_modal(modal, t, cx))
            }
        };
        Some(
            div()
                .id("routes-overlay")
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

    fn routes_manage(
        &self,
        workspace: WorkspaceId,
        scope: Scope,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut body = div().flex().flex_col().gap(px(10.));
        body = body
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Routes and prompt templates"),
            )
            .child(note(
                "Resolved project → workspace → global; an action override beats its group, and among equal keys the most specific scope wins. Click dispatches with the route's click mode and Ctrl+click with the alternate.",
                t,
            ));
        let mut tabs = div().flex().items_center().gap(px(4.)).flex_wrap();
        for (index, (choice, label)) in self.scope_choices(workspace).into_iter().enumerate() {
            let active = choice == scope;
            let hover = t.raised_hover;
            tabs = tabs.child(
                div()
                    .id(("route-scope", index))
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
                        view.back_to_manage(workspace, choice, cx)
                    })),
            );
        }
        body = body.child(tabs);
        let Some(routing) = self.routing() else {
            body = body.child(note(
                "Routing is unavailable. Reconnect to a daemon supporting protocol 0.4.",
                t,
            ));
            return body.into_any_element();
        };
        body = body.child(heading("Groups", t));
        for group in [routing::RouteGroup::Fast, routing::RouteGroup::Thorough] {
            body = body.child(self.route_row(
                routing,
                workspace,
                scope,
                RouteKey::Group(group),
                t,
                cx,
            ));
        }
        body = body.child(heading("Actions", t));
        for action in GitTask::ALL {
            body = body.child(self.route_row(
                routing,
                workspace,
                scope,
                RouteKey::Action(action),
                t,
                cx,
            ));
        }
        body = body.child(heading("Prompt templates", t));
        for action in GitTask::ALL {
            body = body.child(self.template_row(
                routing, workspace, scope, action, t, cx,
            ));
        }
        if let Some(error) = self.routes_ui.error.clone() {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error),
            );
        }
        let can_close = !self.routes_ui.busy;
        body = body.child(
            div().flex().justify_end().child(
                action_button("routes-done", "Done", can_close, t).when(can_close, |d| {
                    d.on_click(cx.listener(|view, _, window, cx| view.close_dialog(window, cx)))
                }),
            ),
        );
        body.into_any_element()
    }

    fn route_row(
        &self,
        routing: &RoutingState,
        workspace: WorkspaceId,
        scope: Scope,
        key: RouteKey,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = !self.routes_ui.busy;
        let existing = routing
            .routes
            .iter()
            .find(|route| route.scope == scope && route.key == key)
            .map(|route| route.route.clone());
        let configured = existing.is_some();
        let detail = existing
            .as_ref()
            .map(|route| {
                format!(
                    "{} · {} · click {} · ctrl {}",
                    route.provider,
                    route.model.as_deref().unwrap_or("provider default"),
                    route.click.label(),
                    route.alternate_click.label()
                )
            })
            .unwrap_or_else(|| "Not set — inherits".to_owned());
        let rev = routing.rev;
        let edit_scope = scope;
        let edit_key = key;
        let remove_scope = scope;
        let row = div()
            .id(SharedString::from(format!("route-row-{scope:?}-{key:?}")))
            .flex()
            .items_center()
            .gap(px(8.))
            .py(px(3.))
            .child(
                div()
                    .w(px(150.))
                    .flex_none()
                    .text_size(t.text_size_sm)
                    .child(SharedString::from(key_label(key))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(t.text_size_sm)
                    .text_color(if configured {
                        t.text_primary
                    } else {
                        t.text_tertiary
                    })
                    .child(SharedString::from(detail)),
            )
            .child(
                action_button(
                    format!("route-edit-{scope:?}-{key:?}"),
                    if configured { "Edit" } else { "Set" },
                    enabled,
                    t,
                )
                .when(enabled, |d| {
                    d.on_click(cx.listener(move |view, _, window, cx| {
                        view.open_route_editor(workspace, edit_scope, edit_key, window, cx)
                    }))
                }),
            );
        let row = if configured {
            row.child(
                action_button(
                    format!("route-remove-{scope:?}-{key:?}"),
                    "Remove",
                    enabled,
                    t,
                )
                .when(enabled, |d| {
                    d.on_click(cx.listener(move |view, _, _, cx| {
                        view.remove_route(workspace, remove_scope, edit_key, rev, cx)
                    }))
                }),
            )
        } else {
            row
        };
        row.into_any_element()
    }

    fn template_row(
        &self,
        routing: &RoutingState,
        workspace: WorkspaceId,
        scope: Scope,
        action: GitTask,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = !self.routes_ui.busy;
        let configured = routing
            .templates
            .iter()
            .find(|template| template.scope == scope && template.action == action)
            .map(|template| template.body.clone());
        let detail = match configured.as_ref() {
            Some(body) => body.lines().next().unwrap_or_default().to_owned(),
            None => {
                let first = default_template(action).lines().next().unwrap_or_default();
                format!("Default · {first}")
            }
        };
        let rev = routing.rev;
        let row = div()
            .id(SharedString::from(format!("template-row-{scope:?}-{action:?}")))
            .flex()
            .items_center()
            .gap(px(8.))
            .py(px(3.))
            .child(
                div()
                    .w(px(150.))
                    .flex_none()
                    .text_size(t.text_size_sm)
                    .child(action.label()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(t.text_size_sm)
                    .text_color(if configured.is_some() {
                        t.text_primary
                    } else {
                        t.text_tertiary
                    })
                    .child(SharedString::from(detail)),
            )
            .child(
                action_button(
                    format!("template-edit-{scope:?}-{action:?}"),
                    "Edit",
                    enabled,
                    t,
                )
                .when(enabled, |d| {
                    d.on_click(cx.listener(move |view, _, window, cx| {
                        view.open_template_editor(workspace, scope, action, window, cx)
                    }))
                }),
            );
        let row = if configured.is_some() {
            row.child(
                action_button(
                    format!("template-reset-{scope:?}-{action:?}"),
                    "Reset",
                    enabled,
                    t,
                )
                .when(enabled, |d| {
                    d.on_click(cx.listener(move |view, _, _, cx| {
                        view.remove_template(workspace, scope, action, rev, cx)
                    }))
                }),
            )
        } else {
            row
        };
        row.into_any_element()
    }

    fn route_editor(&self, draft: &RouteDraft, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let enabled = !draft.busy && !self.routes_ui.busy;
        let (workspace, scope) = (draft.workspace, draft.scope);
        let mut body = div().flex().flex_col().gap(px(10.));
        body = body
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(format!(
                        "Route · {} · {}",
                        key_label(draft.key),
                        self.scope_label(draft.scope)
                    ))),
            )
            .child(note(
                "The provider comes from what the daemon actually discovered. Profile, model and thinking are passed to the adapter only when set.",
                t,
            ))
            .child(note(
                "Prepare modes show the prompt without running anything. A send mode asks the daemon's chat runtime to run it; if this daemon has none, the refusal appears exactly on the dispatch row.",
                t,
            ))
            .child(heading("Provider", t));
        body = body.children(self.provider_picker(draft, enabled, t, cx));
        let discovered_current = self.routes_ui.providers.as_ref().is_some_and(|providers| {
            providers
                .iter()
                .any(|provider| Some(provider.provider.id.as_str()) == draft.provider.as_deref())
        });
        if draft.provider.is_some() && !discovered_current {
            body = body.child(note(
                format!(
                    "Current provider “{}” was not discovered; pick one below.",
                    draft.provider.as_deref().unwrap_or_default()
                ),
                t,
            ));
        }
        body = body
            .child(field("Profile", draft.profile.clone(), t))
            .child(field("Model", draft.model.clone(), t))
            .child(field("Thinking", draft.thinking.clone(), t))
            .child(heading("Dispatch modes", t))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .w(px(90.))
                            .text_size(t.text_size_sm)
                            .text_color(t.text_secondary)
                            .child("Click"),
                    )
                    .child(
                        action_button("route-click-mode", draft.click.label(), enabled, t).when(
                            enabled,
                            |d| {
                                d.on_click(
                                    cx.listener(|view, _, _, cx| view.cycle_click(cx)),
                                )
                            },
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .w(px(90.))
                            .text_size(t.text_size_sm)
                            .text_color(t.text_secondary)
                            .child("Ctrl+click"),
                    )
                    .child(
                        action_button(
                            "route-alt-mode",
                            draft.alternate_click.label(),
                            enabled,
                            t,
                        )
                        .when(enabled, |d| {
                            d.on_click(cx.listener(|view, _, _, cx| view.cycle_alternate(cx)))
                        }),
                    ),
            );
        if let Some(error) = draft.error.clone() {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error),
            );
        }
        body = body.child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .child(
                    action_button("route-back", "Cancel", enabled, t).when(enabled, |d| {
                        d.on_click(cx.listener(move |view, _, _, cx| {
                            view.back_to_manage(workspace, scope, cx)
                        }))
                    }),
                )
                .child(
                    action_button(
                        "route-save",
                        if draft.busy { "Saving…" } else { "Save route" },
                        enabled,
                        t,
                    )
                    .when(enabled, |d| {
                        d.on_click(cx.listener(|view, _, _, cx| view.save_route(cx)))
                    }),
                ),
        );
        body.into_any_element()
    }

    fn provider_picker(
        &self,
        draft: &RouteDraft,
        enabled: bool,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        let hover = t.raised_hover;
        if self.routes_ui.providers_busy {
            rows.push(note("Discovering providers…", t).into_any_element());
        }
        if let Some(error) = self.routes_ui.providers_error.clone() {
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(t.text_size_sm)
                            .text_color(t.danger)
                            .child(error),
                    )
                    .child(
                        action_button("providers-retry", "Retry", enabled, t).when(enabled, |d| {
                            d.on_click(cx.listener(|view, _, _, cx| view.refresh_providers(cx)))
                        }),
                    )
                    .into_any_element(),
            );
        }
        let Some(providers) = self.routes_ui.providers.as_ref() else {
            if !self.routes_ui.providers_busy && self.routes_ui.providers_error.is_none() {
                rows.push(note("No provider discovery yet.", t).into_any_element());
            }
            return rows;
        };
        if providers.is_empty() {
            rows.push(
                note(
                    "The daemon discovered no installable chat providers.",
                    t,
                )
                .into_any_element(),
            );
        }
        for (index, provider) in providers.iter().enumerate() {
            let id = provider.provider.id.clone();
            let selected = draft.provider.as_deref() == Some(id.as_str());
            let unavailable = provider.unavailable_reason.clone().or_else(|| {
                (!provider.capabilities.chat).then(|| "no chat capability".to_owned())
            });
            let version = provider
                .version
                .as_ref()
                .map(|version| format!(" {version}"))
                .unwrap_or_default();
            let mut row = div()
                .id(("route-provider", index))
                .px(px(8.))
                .py(px(5.))
                .rounded(t.radius_sm)
                .flex()
                .flex_col()
                .gap(px(2.))
                .bg(if selected { t.row_selected } else { t.surface_bg })
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
                None if enabled => row.cursor_pointer().hover(move |s| s.bg(hover)),
                None => row,
            };
            if unavailable.is_none() && enabled {
                row = row.on_click(cx.listener(move |view, _, _, cx| {
                    view.choose_provider(id.clone(), cx)
                }));
            }
            rows.push(row.into_any_element());
        }
        rows
    }

    fn template_editor(
        &self,
        draft: &TemplateDraft,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = !draft.busy && !self.routes_ui.busy;
        let (workspace, scope, action, rev) =
            (draft.workspace, draft.scope, draft.action, draft.rev);
        let context = self.preview_context();
        let body_text = draft.body.read(cx).buffer().text();
        let preview = preview_prompt(&body_text, &context);
        let scope_label = self.scope_label(draft.scope);
        let refers = match (&context.branch, &context.base) {
            (Some(branch), Some(base)) => format!("{branch} → {base}"),
            (Some(branch), None) => format!("{branch} → base unknown"),
            (None, Some(base)) => format!("no selected branch → {base}"),
            (None, None) => "select a worktree to preview its branch and base".to_owned(),
        };
        let mut body = div().flex().flex_col().gap(px(10.));
        body = body
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(format!(
                        "Prompt template · {} · {scope_label}",
                        draft.action.label()
                    ))),
            )
            .child(note(
                "The daemon fills only {{branch}}, {{base}}, {{diff}}, {{checks}}, {{reviews}} and {{task}}; unknown placeholders stay literal.",
                t,
            ))
            .child(heading("Default", t))
            .child(
                div()
                    .id("template-default")
                    .max_h(px(110.))
                    .overflow_y_scroll()
                    .p(px(8.))
                    .rounded(t.radius_md)
                    .bg(t.surface_bg)
                    .border_1()
                    .border_color(t.border)
                    .font_family(t.font_mono.clone())
                    .text_size(t.text_size_sm)
                    .text_color(t.text_secondary)
                    .child(SharedString::from(default_template(draft.action))),
            )
            .child(heading("Template", t))
            .child(
                div()
                    .id("template-body")
                    .h(px(150.))
                    .rounded(t.radius_md)
                    .overflow_hidden()
                    .border_1()
                    .border_color(t.border)
                    .child(draft.body.clone()),
            )
            .child(note(SharedString::from(format!("Preview · {refers}")), t))
            .child(
                div()
                    .id("template-preview")
                    .max_h(px(170.))
                    .overflow_y_scroll()
                    .p(px(8.))
                    .rounded(t.radius_md)
                    .bg(t.surface_bg)
                    .border_1()
                    .border_color(t.border)
                    .font_family(t.font_mono.clone())
                    .text_size(t.text_size_sm)
                    .child(SharedString::from(preview)),
            );
        if let Some(error) = draft.error.clone() {
            body = body.child(
                div()
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error),
            );
        }
        body = body.child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .child(
                    action_button("template-back", "Cancel", enabled, t).when(enabled, |d| {
                        d.on_click(cx.listener(move |view, _, _, cx| {
                            view.back_to_manage(workspace, scope, cx)
                        }))
                    }),
                )
                .when(draft.configured, |d| {
                    d.child(
                        action_button("template-reset", "Reset to default", enabled, t).when(
                            enabled,
                            |d| {
                                d.on_click(cx.listener(move |view, _, _, cx| {
                                    view.remove_template(workspace, scope, action, rev, cx)
                                }))
                            },
                        ),
                    )
                })
                .child(
                    action_button(
                        "template-save",
                        if draft.busy { "Saving…" } else { "Save template" },
                        enabled,
                        t,
                    )
                    .when(enabled, |d| {
                        d.on_click(cx.listener(|view, _, _, cx| view.save_template(cx)))
                    }),
                ),
        );
        body.into_any_element()
    }

    fn dispatch_modal(&self, modal: &DispatchModal, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let dispatch = &modal.dispatch;
        let can_close = !modal.sending;
        let mut body = div().flex().flex_col().gap(px(10.));
        body = body
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(format!(
                        "Prepared · {}",
                        dispatch.task.label()
                    ))),
            )
            .child(note(
                SharedString::from(format!(
                    "{} · {}",
                    dispatch.mode.label(),
                    dispatch_state_label(&dispatch.state)
                )),
                t,
            ))
            .child(note(
                "Nothing has been sent. Review the prompt, copy it, or send it explicitly.",
                t,
            ))
            .child(
                div()
                    .id("dispatch-prompt")
                    .h(px(220.))
                    .overflow_y_scroll()
                    .p(px(10.))
                    .rounded(t.radius_md)
                    .bg(t.surface_bg)
                    .border_1()
                    .border_color(t.border)
                    .font_family(t.font_mono.clone())
                    .text_size(t.text_size_sm)
                    .child(SharedString::from(dispatch.prompt.clone())),
            );
        if let Some(error) = modal.error.clone() {
            body = body.child(
                div()
                    .id("dispatch-error")
                    .text_size(t.text_size_sm)
                    .text_color(t.danger)
                    .child(error),
            );
        }
        let can_send = submit_mode(dispatch.mode).is_some() && !modal.sending;
        body = body.child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .child(
                    action_button(
                        "dispatch-copy",
                        if modal.copied { "Copied" } else { "Copy prompt" },
                        true,
                        t,
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.copy_prepared(cx))),
                )
                .child(
                    action_button(
                        "dispatch-send",
                        if modal.sending { "Sending…" } else { "Send" },
                        can_send,
                        t,
                    )
                    .when(can_send, |d| {
                        d.on_click(cx.listener(|view, _, _, cx| view.send_prepared(cx)))
                    }),
                )
                .child(
                    action_button("dispatch-close", "Close", can_close, t).when(can_close, |d| {
                        d.on_click(cx.listener(|view, _, window, cx| {
                            view.close_dialog(window, cx)
                        }))
                    }),
                ),
        );
        body.into_any_element()
    }

    /// Dispatch activity for the selected worktree. Pending rows are local and
    /// labelled as such; success is never inferred.
    pub(super) fn render_dispatch_strip(
        &self,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let LayoutScope::Worktree(worktree) = self.scope else {
            return None;
        };
        enum Row {
            Pending(PendingDispatch),
            Record {
                dispatch: TaskDispatch,
                context: String,
            },
            Failed {
                seq: u64,
                task: GitTask,
                mode: DispatchMode,
                message: SharedString,
            },
        }
        let mut rows: Vec<Row> = Vec::new();
        for pending in self.routes_ui.pending.iter().filter(|p| p.worktree == worktree) {
            rows.push(Row::Pending(PendingDispatch {
                seq: pending.seq,
                worktree: pending.worktree,
                task: pending.task,
                mode: pending.mode,
            }));
        }
        for notice in &self.routes_ui.notices {
            match notice {
                DispatchNotice::Record { dispatch, context } if dispatch.worktree == worktree => {
                    // A pushed daemon record supersedes the local copy.
                    if self.state.dispatches.contains_key(&dispatch.id) {
                        continue;
                    }
                    rows.push(Row::Record {
                        dispatch: dispatch.clone(),
                        context: context.clone(),
                    });
                }
                DispatchNotice::Failed {
                    seq,
                    worktree: failed,
                    task,
                    mode,
                    message,
                } if *failed == worktree => rows.push(Row::Failed {
                    seq: *seq,
                    task: *task,
                    mode: *mode,
                    message: message.clone(),
                }),
                _ => {}
            }
        }
        let mut pushed: Vec<TaskDispatch> = self
            .state
            .dispatches
            .values()
            .filter(|dispatch| dispatch.worktree == worktree)
            .cloned()
            .collect();
        pushed.sort_by_key(|dispatch| std::cmp::Reverse(dispatch.created_ms));
        for dispatch in pushed {
            rows.push(Row::Record {
                dispatch,
                context: String::new(),
            });
        }
        rows.truncate(MAX_DISPATCH_ROWS);
        if rows.is_empty() {
            return None;
        }
        let mut strip = div()
            .flex_none()
            .px(px(10.))
            .py(px(4.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .bg(t.surface_raised)
            .border_t_1()
            .border_color(t.border)
            .text_size(t.text_size_sm);
        for row in rows {
            match row {
                Row::Pending(pending) => {
                    let phase = if pending.mode.prepares_only() {
                        "preparing…"
                    } else {
                        "dispatching…"
                    };
                    strip = strip.child(
                        div()
                            .id(SharedString::from(format!("dispatch-pending-{}", pending.seq)))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_color(t.warning)
                            .child(SharedString::from(format!(
                                "{} · {} — {phase}",
                                pending.task.label(),
                                pending.mode.label()
                            ))),
                    );
                }
                Row::Record { dispatch, context } => {
                    let prepared = dispatch.state == TaskDispatchState::Prepared;
                    let color = match &dispatch.state {
                        TaskDispatchState::Prepared => t.text_secondary,
                        TaskDispatchState::Running => t.warning,
                        TaskDispatchState::Completed => t.success,
                        TaskDispatchState::Failed { .. } => t.danger,
                    };
                    let text = format!(
                        "{} · {} — {}",
                        dispatch.task.label(),
                        dispatch.mode.label(),
                        dispatch_state_label(&dispatch.state)
                    );
                    let row = div()
                        .id(SharedString::from(format!("dispatch-row-{}", dispatch.id)))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_color(color)
                        .child(SharedString::from(text));
                    let row = if prepared {
                        // Clones keep the handler `Fn`: a click can repeat.
                        row.cursor_pointer()
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.open_prepared(dispatch.clone(), context.clone(), cx)
                            }))
                    } else {
                        row
                    };
                    strip = strip.child(row);
                }
                Row::Failed {
                    seq,
                    task,
                    mode,
                    message,
                } => {
                    strip = strip.child(
                        div()
                            .id(SharedString::from(format!("dispatch-failed-{seq}")))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_color(t.danger)
                                    .child(SharedString::from(format!(
                                        "{} · {} — failed",
                                        task.label(),
                                        mode.label()
                                    ))),
                            )
                            .child(div().text_color(t.text_secondary).child(message)),
                    );
                }
            }
        }
        Some(strip.into_any_element())
    }
}

fn soft_provider(provider: &str) -> Option<String> {
    if provider.is_empty() {
        None
    } else {
        Some(provider.to_owned())
    }
}

fn conflict_message(fresh: Option<&Result<RoutingState, CallError>>) -> &'static str {
    match fresh {
        Some(Ok(_)) => {
            "Routes changed on another client. Your draft is kept and now targets the latest revision — save again."
        }
        Some(Err(_)) => {
            "Routes changed on another client. Your draft is kept, but the latest revision could not be fetched; retry when the connection recovers."
        }
        None => "Routes changed on another client. Your draft is kept.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routing_with(route: RouteSpec, key: RouteKey, scope: Scope) -> RoutingState {
        let mut state = RoutingState::default();
        state
            .apply(RoutingMutation::SetRoute(RouteOverride {
                scope,
                key,
                route,
            }))
            .unwrap();
        state
    }

    fn base_route() -> RouteSpec {
        RouteSpec {
            provider: "claude".into(),
            profile: Some("work".into()),
            model: Some("opus".into()),
            thinking: Some("high".into()),
            click: DispatchMode::ReusePrepare,
            alternate_click: DispatchMode::Background,
        }
    }

    #[test]
    fn click_and_alternate_resolve_from_the_route() {
        let workspace = WorkspaceId::new();
        let project = ProjectId::new();
        let state = routing_with(base_route(), RouteKey::Group(routing::RouteGroup::Thorough), Scope::Global);
        assert_eq!(
            resolve_dispatch_mode(&state, GitTask::FixCi, project, workspace, false),
            Some(DispatchMode::ReusePrepare)
        );
        assert_eq!(
            resolve_dispatch_mode(&state, GitTask::FixCi, project, workspace, true),
            Some(DispatchMode::Background)
        );
        // Without any route there is nothing to dispatch.
        assert_eq!(
            resolve_dispatch_mode(
                &RoutingState::default(),
                GitTask::FixCi,
                project,
                workspace,
                false
            ),
            None
        );
    }

    #[test]
    fn an_action_override_changes_the_click_mode() {
        let workspace = WorkspaceId::new();
        let project = ProjectId::new();
        let mut state = routing_with(
            base_route(),
            RouteKey::Group(routing::RouteGroup::Thorough),
            Scope::Global,
        );
        state
            .apply(RoutingMutation::SetRoute(RouteOverride {
                scope: Scope::Workspace(workspace),
                key: RouteKey::Action(GitTask::Investigate),
                route: RouteSpec {
                    click: DispatchMode::NewSubmit,
                    ..base_route()
                },
            }))
            .unwrap();
        assert_eq!(
            resolve_dispatch_mode(&state, GitTask::Investigate, project, workspace, false),
            Some(DispatchMode::NewSubmit)
        );
        assert_eq!(
            resolve_dispatch_mode(&state, GitTask::FixComments, project, workspace, false),
            Some(DispatchMode::ReusePrepare)
        );
    }

    #[test]
    fn prompt_preview_uses_the_scope_template_and_current_branch() {
        let workspace = WorkspaceId::new();
        let project = ProjectId::new();
        let context = TemplateContext {
            branch: Some("feature/login".into()),
            base: Some("main".into()),
            ..TemplateContext::default()
        };
        let defaults = RoutingState::default().template(GitTask::CreatePr, project, workspace);
        let preview = preview_prompt(&defaults, &context);
        assert!(preview.contains("feature/login"));
        assert!(preview.contains("main"));

        let mut state = RoutingState::default();
        state
            .apply(RoutingMutation::SetTemplate(PromptTemplate {
                scope: Scope::Workspace(workspace),
                action: GitTask::CreatePr,
                body: "Open {{branch}} into {{base}}.\n{{checks}}".into(),
            }))
            .unwrap();
        let template = state.template(GitTask::CreatePr, project, workspace);
        assert_eq!(
            preview_prompt(&template, &context),
            "Open feature/login into main.\n{{checks}}"
        );
        // A project scope still beats the workspace template.
        state
            .apply(RoutingMutation::SetTemplate(PromptTemplate {
                scope: Scope::Project(project),
                action: GitTask::CreatePr,
                body: "project override".into(),
            }))
            .unwrap();
        assert_eq!(
            preview_prompt(
                &state.template(GitTask::CreatePr, project, workspace),
                &context
            ),
            "project override"
        );
    }

    #[test]
    fn prepared_prompts_send_in_their_submit_form() {
        assert_eq!(
            submit_mode(DispatchMode::ReusePrepare),
            Some(DispatchMode::ReuseSubmit)
        );
        assert_eq!(
            submit_mode(DispatchMode::NewPrepare),
            Some(DispatchMode::NewSubmit)
        );
        assert_eq!(submit_mode(DispatchMode::Background), None);
        assert_eq!(submit_mode(DispatchMode::NewSubmit), None);
    }

    #[test]
    fn dispatch_states_never_claim_success_early() {
        assert_eq!(
            dispatch_state_label(&TaskDispatchState::Prepared),
            SharedString::from("Prepared — not sent")
        );
        assert_eq!(
            dispatch_state_label(&TaskDispatchState::Running),
            SharedString::from("Running")
        );
        assert_eq!(
            dispatch_state_label(&TaskDispatchState::Completed),
            SharedString::from("Completed")
        );
        assert_eq!(
            dispatch_state_label(&TaskDispatchState::Failed {
                message: "no chat runtime".into()
            }),
            SharedString::from("Failed: no chat runtime")
        );
    }

    #[test]
    fn route_forms_require_a_provider_and_bound_their_fields() {
        let mut form = RouteForm {
            provider: " claude ".into(),
            profile: " work ".into(),
            model: String::new(),
            thinking: "high".into(),
            click: DispatchMode::NewPrepare,
            alternate_click: DispatchMode::Background,
        };
        let spec = form.to_spec().unwrap();
        assert_eq!(spec.provider, "claude");
        assert_eq!(spec.profile.as_deref(), Some("work"));
        assert_eq!(spec.model, None);
        assert_eq!(spec.thinking.as_deref(), Some("high"));
        assert_eq!(spec.click, DispatchMode::NewPrepare);

        form.provider = "   ".into();
        assert_eq!(form.to_spec(), Err("route needs a provider"));
        form.provider = "claude".into();
        form.model = "x".repeat(routing::MAX_FIELD_BYTES + 1);
        assert_eq!(
            form.to_spec(),
            Err("route field is too long or contains control characters")
        );
        form.model = "bad\u{7}model".into();
        assert!(form.to_spec().is_err());
    }

    #[test]
    fn template_forms_reject_empty_and_oversized_bodies() {
        assert_eq!(validate_template_body(""), Err("prompt template is empty"));
        assert_eq!(
            validate_template_body("   \n"),
            Err("prompt template is empty")
        );
        assert_eq!(validate_template_body("{{branch}}"), Ok(()));
        let oversized = "x".repeat(routing::MAX_TEMPLATE_BYTES + 1);
        assert_eq!(
            validate_template_body(&oversized),
            Err("prompt template is too long")
        );
    }

    #[test]
    fn dispatch_mode_picker_cycles_through_every_mode() {
        let mut mode = DispatchMode::ReusePrepare;
        for _ in 0..DISPATCH_MODES.len() {
            mode = next_dispatch_mode(mode);
        }
        assert_eq!(mode, DispatchMode::ReusePrepare);
        assert_eq!(
            next_dispatch_mode(DispatchMode::Background),
            DispatchMode::ReusePrepare
        );
    }

}
