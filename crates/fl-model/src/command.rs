//! Custom commands and their prompt bodies at every scope (spec §6, §7).
//!
//! A custom command is a named prompt. Resolution is project → workspace →
//! global, exactly like routes and prompt templates, and a dispatched command
//! renders `{{branch}}`, `{{base}}`, `{{diff}}` and `{{task}}` through the
//! routing template context. Nothing in a command executes shell code.
use crate::routing::{Scope, TemplateContext, render_template};
use crate::{ProjectId, WorkspaceId};
use serde::{Deserialize, Serialize};

/// Most commands a Flotilla home stores, across every scope.
pub const MAX_COMMANDS: usize = 256;
pub const MAX_COMMAND_NAME_BYTES: usize = 128;
pub const MAX_COMMAND_BODY_BYTES: usize = 32 * 1024;
pub const MAX_COMMAND_DESCRIPTION_BYTES: usize = 1024;

/// A name usable from `fl commands`, a route dispatch and the palette.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_COMMAND_NAME_BYTES {
        return Err(format!(
            "a command name is 1-{MAX_COMMAND_NAME_BYTES} bytes"
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("name is non-empty");
    if !first.is_ascii_alphanumeric() {
        return Err(format!(
            "command name `{name}` must start with an ASCII letter or digit"
        ));
    }
    if let Some(bad) = chars.find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "command name `{name}` contains `{bad}`; use letters, digits, `.`, `_` or `-`"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomCommand {
    pub scope: Scope,
    pub name: String,
    /// One-line summary shown by `fl commands list`.
    pub description: Option<String>,
    /// The prompt body, with `{{name}}` placeholders filled at dispatch.
    pub body: String,
}

impl CustomCommand {
    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.name)?;
        if self.body.trim().is_empty() {
            return Err(format!("command `{}` has an empty body", self.name));
        }
        if self.body.len() > MAX_COMMAND_BODY_BYTES || self.body.contains('\0') {
            return Err(format!(
                "command `{}` body exceeds {MAX_COMMAND_BODY_BYTES} bytes or contains NUL",
                self.name
            ));
        }
        if let Some(description) = &self.description
            && (description.trim().is_empty()
                || description.len() > MAX_COMMAND_DESCRIPTION_BYTES
                || description.contains('\0'))
        {
            return Err(format!(
                "command `{}` description is empty, contains NUL or exceeds {MAX_COMMAND_DESCRIPTION_BYTES} bytes",
                self.name
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandState {
    pub rev: u64,
    pub commands: Vec<CustomCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandMutation {
    /// Refused when the name already exists at that scope.
    Create(CustomCommand),
    /// Refused when the name is not defined at that scope.
    Update(CustomCommand),
    /// Refused when the name is not defined at that scope.
    Delete { scope: Scope, name: String },
}

impl CommandMutation {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Create(command) | Self::Update(command) => command.validate(),
            Self::Delete { name, .. } => validate_name(name),
        }
    }

    pub fn scope(&self) -> Scope {
        match self {
            Self::Create(command) | Self::Update(command) => command.scope,
            Self::Delete { scope, .. } => *scope,
        }
    }
}

fn scope_label(scope: Scope) -> String {
    match scope {
        Scope::Global => "the global scope".into(),
        Scope::Workspace(id) => format!("workspace {id}"),
        Scope::Project(id) => format!("project {id}"),
    }
}

impl CommandState {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.commands.len() > MAX_COMMANDS {
            return Err("too many commands");
        }
        for (index, command) in self.commands.iter().enumerate() {
            command.validate().map_err(|_| "invalid command")?;
            if self.commands[..index]
                .iter()
                .any(|other| other.scope == command.scope && other.name == command.name)
            {
                return Err("duplicate command");
            }
        }
        Ok(())
    }

    /// Apply one edit to a copy, then validate and bump the revision.
    pub fn apply(&mut self, mutation: CommandMutation) -> Result<(), String> {
        mutation.validate()?;
        let mut next = self.clone();
        match mutation {
            CommandMutation::Create(command) => {
                if next.commands.iter().any(|known| {
                    known.scope == command.scope && known.name.eq_ignore_ascii_case(&command.name)
                }) {
                    return Err(format!(
                        "command `{}` already exists in {}",
                        command.name,
                        scope_label(command.scope)
                    ));
                }
                next.commands.push(command);
            }
            CommandMutation::Update(command) => {
                let Some(known) = next.commands.iter_mut().find(|known| {
                    known.scope == command.scope && known.name.eq_ignore_ascii_case(&command.name)
                }) else {
                    return Err(format!(
                        "command `{}` is not defined in {}",
                        command.name,
                        scope_label(command.scope)
                    ));
                };
                *known = command;
            }
            CommandMutation::Delete { scope, ref name } => {
                let before = next.commands.len();
                next.commands.retain(|known| {
                    !(known.scope == scope && known.name.eq_ignore_ascii_case(name))
                });
                if next.commands.len() == before {
                    return Err(format!(
                        "command `{name}` is not defined in {}",
                        scope_label(scope)
                    ));
                }
            }
        }
        next.validate().map_err(|error| error.to_owned())?;
        next.rev = self.rev + 1;
        *self = next;
        Ok(())
    }

    /// The command `name` resolves to for a worktree in `project`/`workspace`:
    /// project scope first, then workspace, then global.
    pub fn resolve(
        &self,
        name: &str,
        project: ProjectId,
        workspace: WorkspaceId,
    ) -> Option<&CustomCommand> {
        [
            Scope::Project(project),
            Scope::Workspace(workspace),
            Scope::Global,
        ]
        .into_iter()
        .find_map(|scope| {
            self.commands
                .iter()
                .find(|command| command.scope == scope && command.name.eq_ignore_ascii_case(name))
        })
    }

    /// Every command visible to a worktree in `project`/`workspace`, most
    /// specific definition per name, then by name.
    pub fn visible(&self, project: ProjectId, workspace: WorkspaceId) -> Vec<CustomCommand> {
        let scopes = [
            Scope::Project(project),
            Scope::Workspace(workspace),
            Scope::Global,
        ];
        let mut visible: Vec<CustomCommand> = Vec::new();
        for scope in scopes {
            for command in self
                .commands
                .iter()
                .filter(|command| command.scope == scope)
            {
                if !visible
                    .iter()
                    .any(|known| known.name.eq_ignore_ascii_case(&command.name))
                {
                    visible.push(command.clone());
                }
            }
        }
        visible.sort_by(|left, right| left.name.cmp(&right.name));
        visible
    }
}

/// Render a command body for a worktree.
pub fn render_command(command: &CustomCommand, context: &TemplateContext) -> String {
    render_template(&command.body, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceId;

    fn command(scope: Scope, name: &str, body: &str) -> CustomCommand {
        CustomCommand {
            scope,
            name: name.into(),
            description: None,
            body: body.into(),
        }
    }

    #[test]
    fn names_are_bounded_and_typed() {
        assert!(validate_name("deploy").is_ok());
        assert!(validate_name("deploy.prod-2_x").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("-deploy").is_err());
        assert!(validate_name("deploy prod").is_err());
        assert!(validate_name(&"a".repeat(MAX_COMMAND_NAME_BYTES + 1)).is_err());
    }

    #[test]
    fn create_update_and_delete_are_checked() {
        let mut state = CommandState::default();
        state
            .apply(CommandMutation::Create(command(
                Scope::Global,
                "deploy",
                "Ship {{branch}}",
            )))
            .unwrap();
        assert_eq!(state.rev, 1);
        let duplicate = state
            .apply(CommandMutation::Create(command(
                Scope::Global,
                "DEPLOY",
                "Ship",
            )))
            .unwrap_err();
        assert!(duplicate.contains("already exists"), "{duplicate}");
        let missing = state
            .apply(CommandMutation::Update(command(
                Scope::Global,
                "absent",
                "Ship",
            )))
            .unwrap_err();
        assert!(missing.contains("not defined"), "{missing}");
        state
            .apply(CommandMutation::Update(command(
                Scope::Global,
                "deploy",
                "Ship {{branch}} to {{base}}",
            )))
            .unwrap();
        assert_eq!(state.rev, 2);
        assert_eq!(state.commands[0].body, "Ship {{branch}} to {{base}}");
        state
            .apply(CommandMutation::Delete {
                scope: Scope::Global,
                name: "deploy".into(),
            })
            .unwrap();
        assert_eq!(state.rev, 3);
        assert!(state.commands.is_empty());
        let removed = state
            .apply(CommandMutation::Delete {
                scope: Scope::Global,
                name: "deploy".into(),
            })
            .unwrap_err();
        assert!(removed.contains("not defined"), "{removed}");
    }

    #[test]
    fn scope_resolution_prefers_the_most_specific_definition() {
        let project = ProjectId::new();
        let workspace = WorkspaceId::new();
        let mut state = CommandState::default();
        for (scope, body) in [
            (Scope::Global, "global"),
            (Scope::Workspace(workspace), "workspace"),
            (Scope::Project(project), "project"),
        ] {
            state
                .apply(CommandMutation::Create(command(scope, "run", body)))
                .unwrap();
        }
        let resolved = state.resolve("run", project, workspace).unwrap();
        assert_eq!(resolved.body, "project");
        let other = ProjectId::new();
        let resolved = state.resolve("run", other, workspace).unwrap();
        assert_eq!(resolved.body, "workspace");
        let resolved = state.resolve("run", other, WorkspaceId::new()).unwrap();
        assert_eq!(resolved.body, "global");
        assert!(state.resolve("absent", project, workspace).is_none());
        // One definition per name: the workspace one hides the global one.
        let visible = state.visible(other, workspace);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].body, "workspace");
    }

    #[test]
    fn rendering_fills_known_placeholders_only() {
        let command = command(
            Scope::Global,
            "run",
            "Ship {{branch}} on {{base}} {{unknown}}",
        );
        let context = TemplateContext {
            branch: Some("feature".into()),
            base: Some("main".into()),
            ..TemplateContext::default()
        };
        assert_eq!(
            render_command(&command, &context),
            "Ship feature on main {{unknown}}"
        );
    }
}
