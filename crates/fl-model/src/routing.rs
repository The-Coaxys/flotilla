//! Fast/Thorough routes, per-scope prompt templates and dispatch modes (spec §4, §6).
//!
//! A route says which provider, profile, model and thinking setting an action
//! uses, and how a click or alternate click dispatches it. Resolution is
//! project → workspace → global, and an action-specific override beats the
//! override for its group at the same scope.
use crate::{ProjectId, WorkspaceId};
use serde::{Deserialize, Serialize};

pub const MAX_ROUTES: usize = 256;
pub const MAX_TEMPLATES: usize = 256;
pub const MAX_TEMPLATE_BYTES: usize = 32 * 1024;
pub const MAX_FIELD_BYTES: usize = 1024;
pub const MAX_CONTEXT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    Global,
    Workspace(WorkspaceId),
    Project(ProjectId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RouteGroup {
    Fast,
    Thorough,
}

/// The Git and prompt actions that can be routed to an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GitTask {
    /// Name a worktree's temporary branch from its first prompt (Fast).
    NameWorktree,
    Commit,
    CommitPush,
    CreatePr,
    EnhancePrompt,
    Resolve,
    FixCi,
    FixChanges,
    FixComments,
    Investigate,
    /// A stored custom command dispatched by name (spec §6, §7).
    Command,
}

impl GitTask {
    pub fn group(self) -> RouteGroup {
        match self {
            Self::NameWorktree
            | Self::Commit
            | Self::CommitPush
            | Self::CreatePr
            | Self::EnhancePrompt => RouteGroup::Fast,
            Self::Resolve
            | Self::FixCi
            | Self::FixChanges
            | Self::FixComments
            | Self::Investigate
            | Self::Command => RouteGroup::Thorough,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NameWorktree => "Name worktree",
            Self::Commit => "Commit",
            Self::CommitPush => "Commit & push",
            Self::CreatePr => "Create PR",
            Self::EnhancePrompt => "Enhance prompt",
            Self::Resolve => "Resolve",
            Self::FixCi => "Fix CI",
            Self::FixChanges => "Fix changes",
            Self::FixComments => "Fix comments",
            Self::Investigate => "Investigate",
            Self::Command => "Custom command",
        }
    }

    /// Stable identifier used by `.flotilla/config.json` and the CLI.
    pub fn id(self) -> &'static str {
        match self {
            Self::NameWorktree => "name_worktree",
            Self::Commit => "commit",
            Self::CommitPush => "commit_push",
            Self::CreatePr => "create_pr",
            Self::EnhancePrompt => "enhance_prompt",
            Self::Resolve => "resolve",
            Self::FixCi => "fix_ci",
            Self::FixChanges => "fix_changes",
            Self::FixComments => "fix_comments",
            Self::Investigate => "investigate",
            Self::Command => "command",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|task| task.id() == id)
    }

    /// Every routable Git action, in menu order. A stored custom command
    /// (`Command`) is configured through its own scope store, so it is not a
    /// Git action here; it resolves through its group route.
    pub const ALL: [Self; 10] = [
        Self::NameWorktree,
        Self::Commit,
        Self::CommitPush,
        Self::CreatePr,
        Self::EnhancePrompt,
        Self::Resolve,
        Self::FixCi,
        Self::FixChanges,
        Self::FixComments,
        Self::Investigate,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RouteKey {
    Group(RouteGroup),
    Action(GitTask),
}

/// What a click does with the routed prompt (spec §6, dispatch modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DispatchMode {
    /// Show the rendered prompt without starting anything.
    ReusePrepare,
    /// Send into the worktree's current idle chat, or a new one if none.
    ReuseSubmit,
    /// Open a new chat tab with the prompt prepared.
    NewPrepare,
    /// Submit in a new chat tab.
    NewSubmit,
    /// Run with no chat surface, reporting in the inbox.
    Background,
}

impl DispatchMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReusePrepare => "Reuse chat, prepare",
            Self::ReuseSubmit => "Reuse chat, send",
            Self::NewPrepare => "New tab, prepare",
            Self::NewSubmit => "New tab, send",
            Self::Background => "Background",
        }
    }

    pub fn prepares_only(self) -> bool {
        matches!(self, Self::ReusePrepare | Self::NewPrepare)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteSpec {
    pub provider: String,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub click: DispatchMode,
    pub alternate_click: DispatchMode,
}

impl RouteSpec {
    /// The built-in default when nothing is configured: the given provider,
    /// sending in a new chat tab on click, background on alternate click.
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            profile: None,
            model: None,
            thinking: None,
            click: DispatchMode::NewSubmit,
            alternate_click: DispatchMode::Background,
        }
    }

    pub fn mode(&self, alternate: bool) -> DispatchMode {
        if alternate {
            self.alternate_click
        } else {
            self.click
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.provider.trim().is_empty() {
            return Err("route needs a provider");
        }
        for value in [
            Some(&self.provider),
            self.profile.as_ref(),
            self.model.as_ref(),
            self.thinking.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if value.len() > MAX_FIELD_BYTES || value.chars().any(char::is_control) {
                return Err("route field is too long or contains control characters");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteOverride {
    pub scope: Scope,
    pub key: RouteKey,
    pub route: RouteSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub scope: Scope,
    pub action: GitTask,
    pub body: String,
}

impl PromptTemplate {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.body.trim().is_empty() {
            return Err("prompt template is empty");
        }
        if self.body.len() > MAX_TEMPLATE_BYTES {
            return Err("prompt template is too long");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingState {
    pub rev: u64,
    pub routes: Vec<RouteOverride>,
    pub templates: Vec<PromptTemplate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutingMutation {
    SetRoute(RouteOverride),
    RemoveRoute { scope: Scope, key: RouteKey },
    SetTemplate(PromptTemplate),
    RemoveTemplate { scope: Scope, action: GitTask },
}

impl RoutingMutation {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::SetRoute(route) => route.route.validate(),
            Self::SetTemplate(template) => template.validate(),
            Self::RemoveRoute { .. } | Self::RemoveTemplate { .. } => Ok(()),
        }
    }
}

/// One dispatch of a routed action, from prepared prompt to outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDispatch {
    pub id: crate::TaskRunId,
    pub worktree: crate::WorktreeId,
    pub task: GitTask,
    pub mode: DispatchMode,
    /// The chat session that ran or will run it, once one exists.
    pub session: Option<uuid::Uuid>,
    pub prompt: String,
    pub state: TaskDispatchState,
    pub created_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskDispatchState {
    /// The rendered prompt, shown without running anything.
    Prepared,
    Running,
    Completed,
    Failed {
        message: String,
    },
}

/// Values a template may interpolate; nothing is executed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateContext {
    pub branch: Option<String>,
    pub base: Option<String>,
    pub diff: Option<String>,
    pub checks: Option<String>,
    pub reviews: Option<String>,
    pub task: Option<String>,
}

/// Fills `{{name}}` placeholders, leaving unknown names untouched, and bounds
/// the result. A value is inserted verbatim: templates never run shell code.
pub fn render_template(body: &str, context: &TemplateContext) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("{{") {
        let (head, tail) = rest.split_at(start);
        out.push_str(head);
        let Some(end) = tail.find("}}") else {
            out.push_str(tail);
            return bounded(&out);
        };
        let name = tail[2..end].trim();
        let value = match name {
            "branch" => context.branch.as_deref(),
            "base" => context.base.as_deref(),
            "diff" => context.diff.as_deref(),
            "checks" => context.checks.as_deref(),
            "reviews" => context.reviews.as_deref(),
            "task" => context.task.as_deref(),
            _ => None,
        };
        match value {
            Some(value) => out.push_str(value),
            None => out.push_str(&tail[..end + 2]),
        }
        rest = &tail[end + 2..];
    }
    out.push_str(rest);
    bounded(&out)
}

fn bounded(text: &str) -> String {
    if text.len() <= MAX_CONTEXT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_CONTEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The default prompt for an action, used when no scope configures one.
pub fn default_template(action: GitTask) -> &'static str {
    match action {
        GitTask::NameWorktree => "Name this worktree in one line.\n\nFirst prompt:\n{{task}}",
        GitTask::Commit => {
            "Write a commit message for the staged changes on `{{branch}}`.\n\n{{diff}}"
        }
        GitTask::CommitPush => {
            "Write a commit message for the staged changes on `{{branch}}`, then push.\n\n{{diff}}"
        }
        GitTask::CreatePr => {
            "Write a pull request title and body for `{{branch}}` into `{{base}}`.\n\n{{diff}}"
        }
        GitTask::EnhancePrompt => "Improve this prompt for clarity and precision:\n\n{{task}}",
        GitTask::Resolve => {
            "Resolve the conflicts with `{{base}}` in this worktree and stage the result.\n\n{{diff}}"
        }
        GitTask::FixCi => "These checks fail on `{{branch}}`; fix the cause.\n\n{{checks}}",
        GitTask::FixChanges => {
            "Reviewers requested changes on `{{branch}}`; make them.\n\n{{reviews}}"
        }
        GitTask::FixComments => {
            "Address the pending review comments in this worktree.\n\n{{reviews}}"
        }
        GitTask::Investigate => {
            "Investigate these failing checks and report what is wrong.\n\n{{checks}}"
        }
        // Custom commands carry their own body; this text is only a fallback.
        GitTask::Command => "{{task}}",
    }
}

impl RoutingState {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.routes.len() > MAX_ROUTES || self.templates.len() > MAX_TEMPLATES {
            return Err("too many routing entries");
        }
        for (index, route) in self.routes.iter().enumerate() {
            route.route.validate()?;
            if self.routes[..index]
                .iter()
                .any(|other| other.scope == route.scope && other.key == route.key)
            {
                return Err("duplicate route");
            }
        }
        for (index, template) in self.templates.iter().enumerate() {
            template.validate()?;
            if self.templates[..index]
                .iter()
                .any(|other| other.scope == template.scope && other.action == template.action)
            {
                return Err("duplicate prompt template");
            }
        }
        Ok(())
    }

    pub fn apply(&mut self, mutation: RoutingMutation) -> Result<(), &'static str> {
        mutation.validate()?;
        let mut next = self.clone();
        match mutation {
            RoutingMutation::SetRoute(route) => {
                match next
                    .routes
                    .iter_mut()
                    .find(|known| known.scope == route.scope && known.key == route.key)
                {
                    Some(known) => *known = route,
                    None => next.routes.push(route),
                }
            }
            RoutingMutation::RemoveRoute { scope, key } => {
                next.routes
                    .retain(|known| !(known.scope == scope && known.key == key));
            }
            RoutingMutation::SetTemplate(template) => {
                match next
                    .templates
                    .iter_mut()
                    .find(|known| known.scope == template.scope && known.action == template.action)
                {
                    Some(known) => *known = template,
                    None => next.templates.push(template),
                }
            }
            RoutingMutation::RemoveTemplate { scope, action } => {
                next.templates
                    .retain(|known| !(known.scope == scope && known.action == action));
            }
        }
        next.validate()?;
        next.rev = self.rev + 1;
        *self = next;
        Ok(())
    }

    /// Scopes that apply to `project`, most specific first.
    fn scopes(&self, project: ProjectId, workspace: WorkspaceId) -> [Option<Scope>; 3] {
        [
            Some(Scope::Project(project)),
            Some(Scope::Workspace(workspace)),
            Some(Scope::Global),
        ]
    }

    /// The route for `action`: an explicit action override at any scope beats
    /// a group default, and among equal keys the most specific scope wins.
    pub fn route(
        &self,
        action: GitTask,
        project: ProjectId,
        workspace: WorkspaceId,
    ) -> Option<RouteSpec> {
        let scopes = self.scopes(project, workspace);
        for key in [RouteKey::Action(action), RouteKey::Group(action.group())] {
            for scope in scopes.into_iter().flatten() {
                if let Some(route) = self
                    .routes
                    .iter()
                    .find(|route| route.scope == scope && route.key == key)
                {
                    return Some(route.route.clone());
                }
            }
        }
        None
    }

    /// The prompt template for `action`: a stored project, workspace or global
    /// template wins, then the repository's own `.flotilla/config.json`, then
    /// the built-in default.
    pub fn template_with_repo(
        &self,
        action: GitTask,
        project: ProjectId,
        workspace: WorkspaceId,
        repo_prompt: Option<&str>,
    ) -> String {
        self.scopes(project, workspace)
            .into_iter()
            .flatten()
            .find_map(|scope| {
                self.templates
                    .iter()
                    .find(|template| template.scope == scope && template.action == action)
                    .map(|template| template.body.clone())
            })
            .or_else(|| repo_prompt.map(str::to_owned))
            .unwrap_or_else(|| default_template(action).to_owned())
    }

    /// The scope whose stored template wins for `action`, if any.
    pub fn template_scope(
        &self,
        action: GitTask,
        project: ProjectId,
        workspace: WorkspaceId,
    ) -> Option<Scope> {
        self.scopes(project, workspace)
            .into_iter()
            .flatten()
            .find(|scope| {
                self.templates
                    .iter()
                    .any(|template| template.scope == *scope && template.action == action)
            })
    }

    /// The prompt template for `action`, most specific first.
    pub fn template(&self, action: GitTask, project: ProjectId, workspace: WorkspaceId) -> String {
        self.scopes(project, workspace)
            .into_iter()
            .flatten()
            .find_map(|scope| {
                self.templates
                    .iter()
                    .find(|template| template.scope == scope && template.action == action)
                    .map(|template| template.body.clone())
            })
            .unwrap_or_else(|| default_template(action).to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn override_(scope: Scope, key: RouteKey, provider: &str) -> RouteOverride {
        RouteOverride {
            scope,
            key,
            route: RouteSpec::new(provider),
        }
    }

    #[test]
    fn project_beats_workspace_beats_global_and_action_beats_group() {
        let project = ProjectId::new();
        let workspace = WorkspaceId::new();
        let mut state = RoutingState::default();
        for mutation in [
            RoutingMutation::SetRoute(override_(
                Scope::Global,
                RouteKey::Group(RouteGroup::Fast),
                "global-fast",
            )),
            RoutingMutation::SetRoute(override_(
                Scope::Workspace(workspace),
                RouteKey::Group(RouteGroup::Fast),
                "workspace-fast",
            )),
            RoutingMutation::SetRoute(override_(
                Scope::Project(project),
                RouteKey::Group(RouteGroup::Fast),
                "project-fast",
            )),
            RoutingMutation::SetRoute(override_(
                Scope::Workspace(workspace),
                RouteKey::Action(GitTask::Commit),
                "commit-specific",
            )),
        ] {
            state.apply(mutation).unwrap();
        }
        let resolve = |action| {
            state
                .route(action, project, workspace)
                .map(|route| route.provider)
        };
        assert_eq!(resolve(GitTask::Commit).as_deref(), Some("commit-specific"));
        assert_eq!(resolve(GitTask::CreatePr).as_deref(), Some("project-fast"));
        // A group override never applies to the other group.
        assert_eq!(resolve(GitTask::FixCi), None);
        assert_eq!(state.rev, 4);
    }

    #[test]
    fn templates_interpolate_known_names_only() {
        let mut context = TemplateContext {
            branch: Some("feature".into()),
            diff: Some("+1 -0".into()),
            ..TemplateContext::default()
        };
        assert_eq!(
            render_template("{{branch}} vs {{unknown}}", &context),
            "feature vs {{unknown}}"
        );
        context.branch = Some("x".repeat(MAX_CONTEXT_BYTES + 10));
        assert_eq!(
            render_template("{{branch}}!", &context).len(),
            MAX_CONTEXT_BYTES
        );
        assert!(default_template(GitTask::FixCi).contains("{{checks}}"));
        assert_eq!(GitTask::from_id("fix_ci"), Some(GitTask::FixCi));
        assert_eq!(GitTask::from_id("nope"), None);
    }

    #[test]
    fn repository_templates_fill_in_below_configured_scopes() {
        let project = ProjectId::new();
        let workspace = WorkspaceId::new();
        let mut state = RoutingState::default();
        let repo = "Repo template for {{branch}}";
        assert_eq!(
            state.template_with_repo(GitTask::FixCi, project, workspace, Some(repo)),
            repo
        );
        state
            .apply(RoutingMutation::SetTemplate(PromptTemplate {
                scope: Scope::Workspace(workspace),
                action: GitTask::FixCi,
                body: "Workspace template".into(),
            }))
            .unwrap();
        assert_eq!(
            state.template_with_repo(GitTask::FixCi, project, workspace, Some(repo)),
            "Workspace template"
        );
        // A repository template never leaks into another action.
        assert_eq!(
            state.template_with_repo(GitTask::Investigate, project, workspace, None),
            default_template(GitTask::Investigate)
        );
    }

    #[test]
    fn dispatch_modes_and_bounds_round_trip() {
        let project = ProjectId::new();
        let workspace = WorkspaceId::new();
        let mut state = RoutingState::default();
        let mut route = RouteSpec::new("claude");
        route.click = DispatchMode::NewPrepare;
        route.alternate_click = DispatchMode::ReuseSubmit;
        state
            .apply(RoutingMutation::SetRoute(RouteOverride {
                scope: Scope::Global,
                key: RouteKey::Action(GitTask::FixCi),
                route,
            }))
            .unwrap();
        let resolved = state.route(GitTask::FixCi, project, workspace).unwrap();
        assert_eq!(resolved.mode(false), DispatchMode::NewPrepare);
        assert_eq!(resolved.mode(true), DispatchMode::ReuseSubmit);
        assert!(DispatchMode::NewPrepare.prepares_only());
        assert!(!DispatchMode::Background.prepares_only());

        // Duplicates and empties are refused without changing the state.
        let before = state.clone();
        assert!(
            state
                .apply(RoutingMutation::SetRoute(override_(
                    Scope::Global,
                    RouteKey::Action(GitTask::FixCi),
                    "  "
                )))
                .is_err()
        );
        assert_eq!(state, before);
        let mut empty = RoutingState::default();
        assert!(
            empty
                .apply(RoutingMutation::SetTemplate(PromptTemplate {
                    scope: Scope::Global,
                    action: GitTask::Commit,
                    body: "  ".into(),
                }))
                .is_err()
        );
        assert_eq!(empty.rev, 0);
    }
}
