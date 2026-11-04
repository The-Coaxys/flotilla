//! `fl commands …` and `fl prompt-template …`: durable, scoped prompts.
//!
//! Both surfaces are compare-and-set over the daemon's store. A refusal (a
//! duplicate name, a missing definition, an invalid body or a stale revision)
//! exits non-zero with the daemon's reason; a mutation prints the resulting
//! state so the durable effect is visible.
use std::io::Read;

use fl_model::command::{CommandMutation, CommandState, CustomCommand, MAX_COMMAND_BODY_BYTES};
use fl_model::routing::{
    GitTask, MAX_TEMPLATE_BYTES, PromptTemplate, RoutingMutation, RoutingState, Scope,
};
use fl_proto::{Request, Response};

use super::{caller, unexpected};
use crate::cli::{Cli, CommandScopeArg, CommandsCommand, PromptTemplateCommand};
use crate::client::DaemonClient;
use crate::error::CliError;
use crate::output::Reporter;

pub async fn run(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &CommandsCommand,
) -> Result<(), CliError> {
    match command {
        CommandsCommand::List { scope } => list(client, reporter, cli, *scope).await,
        CommandsCommand::Create {
            name,
            body,
            body_file,
            description,
            scope,
        } => {
            let stored = resolve_scope(client, cli, *scope).await?;
            let command = CustomCommand {
                scope: stored,
                name: name.clone(),
                description: description.clone(),
                body: body_text(body, body_file, MAX_COMMAND_BODY_BYTES, "command body")?,
            };
            check_definition(&command)?;
            create(client, reporter, command).await
        }
        CommandsCommand::Get { name } => get(client, reporter, cli, name).await,
        CommandsCommand::Update {
            name,
            body,
            body_file,
            description,
            scope,
        } => {
            let stored = resolve_scope(client, cli, *scope).await?;
            let state = command_state(client).await?;
            let existing = state
                .commands
                .iter()
                .find(|command| command.scope == stored && command.name.eq_ignore_ascii_case(name))
                .cloned()
                .ok_or_else(|| not_defined(&state, stored, name))?;
            let body = match (body, body_file) {
                (None, None) => existing.body,
                _ => body_text(body, body_file, MAX_COMMAND_BODY_BYTES, "command body")?,
            };
            let description = description.clone().or(existing.description);
            let command = CustomCommand {
                scope: stored,
                name: existing.name,
                description,
                body,
            };
            check_definition(&command)?;
            update(client, reporter, command).await
        }
        CommandsCommand::Delete { name, scope } => {
            let stored = resolve_scope(client, cli, *scope).await?;
            delete(client, reporter, stored, name).await
        }
    }
}

pub async fn run_template(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &PromptTemplateCommand,
) -> Result<(), CliError> {
    match command {
        PromptTemplateCommand::List { scope } => {
            list_templates(client, reporter, cli, *scope).await
        }
        PromptTemplateCommand::Get { action } => get_template(client, reporter, cli, action).await,
        PromptTemplateCommand::Set {
            action,
            body,
            body_file,
            scope,
        } => {
            let stored = resolve_scope(client, cli, *scope).await?;
            let template = PromptTemplate {
                scope: stored,
                action: parse_action(action)?,
                body: body_text(body, body_file, MAX_TEMPLATE_BYTES, "prompt template")?,
            };
            set_template(client, reporter, template).await
        }
        PromptTemplateCommand::Remove { action, scope } => {
            let stored = resolve_scope(client, cli, *scope).await?;
            remove_template(client, reporter, stored, parse_action(action)?).await
        }
    }
}

// --- Scopes ---------------------------------------------------------------

async fn resolve_scope(
    client: &mut DaemonClient,
    cli: &Cli,
    arg: CommandScopeArg,
) -> Result<Scope, CliError> {
    match arg {
        CommandScopeArg::Global => Ok(Scope::Global),
        CommandScopeArg::Project => {
            let caller = caller(client, cli).await?;
            let project = caller.project.ok_or_else(|| {
                CliError::invalid(
                    "no project in the caller's context; pass --scope workspace|global",
                )
            })?;
            Ok(Scope::Project(project.id))
        }
        CommandScopeArg::Workspace => {
            let caller = caller(client, cli).await?;
            if let Some(project) = caller.project {
                return Ok(Scope::Workspace(project.workspace));
            }
            match client.rpc(Request::GetSelectedWorkspace).await? {
                Response::SelectedWorkspace(Some(workspace)) => Ok(Scope::Workspace(workspace.id)),
                Response::SelectedWorkspace(None) => Err(CliError::invalid(
                    "no workspace is selected; pass --scope global or run inside a project",
                )),
                other => Err(unexpected("selected workspace", &other)),
            }
        }
    }
}

/// The caller's project and workspace, when it is inside a worktree.
async fn caller_scopes(
    client: &mut DaemonClient,
    cli: &Cli,
) -> Result<Option<(fl_model::ProjectId, fl_model::WorkspaceId)>, CliError> {
    let caller = caller(client, cli).await?;
    Ok(caller
        .project
        .map(|project| (project.id, project.workspace)))
}

/// The same bounds the daemon and the wire apply, so a bad name or body is
/// an argument refusal before a request is written.
fn check_definition(command: &CustomCommand) -> Result<(), CliError> {
    command.validate().map_err(CliError::invalid)
}

fn scope_label(scope: Scope) -> String {
    match scope {
        Scope::Global => "global".into(),
        Scope::Workspace(id) => format!("workspace {id}"),
        Scope::Project(id) => format!("project {id}"),
    }
}

// --- Bodies ---------------------------------------------------------------

fn body_text(
    body: &Option<String>,
    body_file: &Option<String>,
    max: usize,
    what: &str,
) -> Result<String, CliError> {
    let text = match (body, body_file) {
        (Some(text), _) if text == "-" => read_stdin(max, what)?,
        (Some(text), _) => text.clone(),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|error| CliError::invalid(format!("reading {path}: {error}")))?,
        (None, None) => {
            return Err(CliError::invalid(format!(
                "pass --body or --body-file for the {what}"
            )));
        }
    };
    if text.trim().is_empty() {
        return Err(CliError::invalid(format!("the {what} is empty")));
    }
    if text.len() > max {
        return Err(CliError::invalid(format!("the {what} exceeds {max} bytes")));
    }
    Ok(text)
}

fn read_stdin(max: usize, what: &str) -> Result<String, CliError> {
    let mut text = String::new();
    std::io::stdin()
        .take((max + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|error| CliError::invalid(format!("reading stdin: {error}")))?;
    if text.len() > max {
        return Err(CliError::invalid(format!(
            "stdin exceeds the {max}-byte {what} limit"
        )));
    }
    Ok(text)
}

// --- Commands -------------------------------------------------------------

async fn command_state(client: &mut DaemonClient) -> Result<CommandState, CliError> {
    match client.rpc(Request::ListCommands).await? {
        Response::CommandState(state) => Ok(state),
        other => Err(unexpected("command state", &other)),
    }
}

/// Retry once on a stale revision so `fl` stays usable while another writer
/// (the client) commits a command edit between our read and write.
async fn mutate_command(
    client: &mut DaemonClient,
    mutation: CommandMutation,
) -> Result<CommandState, CliError> {
    let mut expected = command_state(client).await?.rev;
    for attempt in 0..2 {
        let request = Request::MutateCommand {
            expected_rev: expected,
            mutation: mutation.clone(),
        };
        match client.rpc(request).await {
            Ok(Response::CommandState(state)) => return Ok(state),
            Ok(other) => return Err(unexpected("command state", &other)),
            Err(error) if error.code == "revision_conflict" && attempt == 0 => {
                expected = command_state(client).await?.rev;
            }
            Err(error) => return Err(error),
        }
    }
    Err(CliError::internal("command revision kept changing"))
}

fn not_defined(state: &CommandState, scope: Scope, name: &str) -> CliError {
    let known: Vec<&str> = state
        .commands
        .iter()
        .filter(|command| command.scope == scope)
        .map(|command| command.name.as_str())
        .collect();
    CliError::not_found(format!(
        "command `{name}` is not defined in {}; known: {}",
        scope_label(scope),
        if known.is_empty() {
            "(none)".to_owned()
        } else {
            known.join(", ")
        }
    ))
}

async fn list(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    scope: Option<CommandScopeArg>,
) -> Result<(), CliError> {
    let state = command_state(client).await?;
    let (commands, label) = match scope {
        Some(arg) => {
            let scope = resolve_scope(client, cli, arg).await?;
            let mut commands: Vec<CustomCommand> = state
                .commands
                .iter()
                .filter(|command| command.scope == scope)
                .cloned()
                .collect();
            commands.sort_by(|left, right| left.name.cmp(&right.name));
            (commands, scope_label(scope))
        }
        None => match caller_scopes(client, cli).await? {
            Some((project, workspace)) => {
                (state.visible(project, workspace), "the caller".to_owned())
            }
            None => {
                let mut commands = state.commands.clone();
                commands.sort_by(|left, right| {
                    scope_label(left.scope)
                        .cmp(&scope_label(right.scope))
                        .then_with(|| left.name.cmp(&right.name))
                });
                (commands, "every scope".to_owned())
            }
        },
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "rev": state.rev,
            "scope": label,
            "commands": commands,
        }));
    }
    reporter.text(format!(
        "{} command(s) for {label}, rev {}",
        commands.len(),
        state.rev
    ));
    for command in &commands {
        reporter.text(format!(
            "  {:<9} {}{}",
            scope_label(command.scope),
            command.name,
            command
                .description
                .as_deref()
                .map(|description| format!("  {description}"))
                .unwrap_or_default()
        ));
    }
    Ok(())
}

async fn get(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    name: &str,
) -> Result<(), CliError> {
    let state = command_state(client).await?;
    let command = match caller_scopes(client, cli).await? {
        Some((project, workspace)) => state.resolve(name, project, workspace).cloned(),
        None => state
            .commands
            .iter()
            .find(|command| command.name.eq_ignore_ascii_case(name))
            .cloned(),
    };
    let Some(command) = command else {
        return Err(CliError::not_found(format!(
            "no command `{name}` is defined for the caller"
        )));
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "rev": state.rev,
            "command": command,
        }));
    }
    reporter.text(format!(
        "{}  {}",
        command.name,
        command.description.as_deref().unwrap_or("(no description)")
    ));
    reporter.text(format!("scope {}", scope_label(command.scope)));
    reporter.text(command.body.trim_end());
    Ok(())
}

async fn create(
    client: &mut DaemonClient,
    reporter: &Reporter,
    command: CustomCommand,
) -> Result<(), CliError> {
    let state = mutate_command(client, CommandMutation::Create(command.clone())).await?;
    emit_command(reporter, &state, "created", &command.name)
}

async fn update(
    client: &mut DaemonClient,
    reporter: &Reporter,
    command: CustomCommand,
) -> Result<(), CliError> {
    let state = mutate_command(client, CommandMutation::Update(command.clone())).await?;
    emit_command(reporter, &state, "updated", &command.name)
}

async fn delete(
    client: &mut DaemonClient,
    reporter: &Reporter,
    scope: Scope,
    name: &str,
) -> Result<(), CliError> {
    let state = mutate_command(
        client,
        CommandMutation::Delete {
            scope,
            name: name.to_owned(),
        },
    )
    .await?;
    emit_command(reporter, &state, "deleted", name)
}

fn emit_command(
    reporter: &Reporter,
    state: &CommandState,
    action: &str,
    name: &str,
) -> Result<(), CliError> {
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "action": action,
            "rev": state.rev,
            "commands": state.commands,
        }));
    }
    reporter.text(format!(
        "{action} command `{name}`; {} command(s) stored at rev {}",
        state.commands.len(),
        state.rev
    ));
    Ok(())
}

// --- Prompt templates -----------------------------------------------------

fn parse_action(action: &str) -> Result<GitTask, CliError> {
    GitTask::from_id(action).ok_or_else(|| {
        CliError::invalid(format!(
            "`{action}` is not a routed action; use one of: {}",
            GitTask::ALL
                .iter()
                .map(|task| task.id())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

async fn routing_state(client: &mut DaemonClient) -> Result<RoutingState, CliError> {
    match client.rpc(Request::GetRouting).await? {
        Response::RoutingState(state) => Ok(state),
        other => Err(unexpected("routing state", &other)),
    }
}

async fn mutate_routing(
    client: &mut DaemonClient,
    mutation: RoutingMutation,
) -> Result<RoutingState, CliError> {
    let mut expected = routing_state(client).await?.rev;
    for attempt in 0..2 {
        let request = Request::MutateRouting {
            expected_rev: expected,
            mutation: mutation.clone(),
        };
        match client.rpc(request).await {
            Ok(Response::RoutingState(state)) => return Ok(state),
            Ok(other) => return Err(unexpected("routing state", &other)),
            Err(error) if error.code == "revision_conflict" && attempt == 0 => {
                expected = routing_state(client).await?.rev;
            }
            Err(error) => return Err(error),
        }
    }
    Err(CliError::internal("routing revision kept changing"))
}

async fn list_templates(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    scope: Option<CommandScopeArg>,
) -> Result<(), CliError> {
    let state = routing_state(client).await?;
    let scope = match scope {
        Some(arg) => Some(resolve_scope(client, cli, arg).await?),
        None => None,
    };
    let mut templates: Vec<PromptTemplate> = state
        .templates
        .iter()
        .filter(|template| scope.is_none_or(|scope| template.scope == scope))
        .cloned()
        .collect();
    templates.sort_by(|left, right| {
        scope_label(left.scope)
            .cmp(&scope_label(right.scope))
            .then_with(|| left.action.id().cmp(right.action.id()))
    });
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "rev": state.rev,
            "templates": templates,
        }));
    }
    reporter.text(format!(
        "{} prompt template(s), routing rev {}",
        templates.len(),
        state.rev
    ));
    for template in &templates {
        reporter.text(format!(
            "  {:<9} {:<14} {} bytes",
            scope_label(template.scope),
            template.action.id(),
            template.body.len()
        ));
    }
    Ok(())
}

async fn get_template(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    action: &str,
) -> Result<(), CliError> {
    let action = parse_action(action)?;
    let state = routing_state(client).await?;
    let (project, workspace) = match caller(client, cli).await?.project {
        Some(project) => (project.id, project.workspace),
        None => {
            return Err(CliError::invalid(
                "no project in the caller's context; prompt templates resolve per project",
            ));
        }
    };
    let body = state.template(action, project, workspace);
    let scope = state
        .template_scope(action, project, workspace)
        .map(scope_label);
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "action": action.id(),
            "rev": state.rev,
            "scope": scope,
            "stored": scope.is_some(),
            "template": body,
        }));
    }
    match scope {
        Some(scope) => reporter.text(format!("{} from {scope}", action.id())),
        None => reporter.text(format!("{} from the built-in default", action.id())),
    }
    reporter.text(body.trim_end());
    Ok(())
}

async fn set_template(
    client: &mut DaemonClient,
    reporter: &Reporter,
    template: PromptTemplate,
) -> Result<(), CliError> {
    let state = mutate_routing(client, RoutingMutation::SetTemplate(template.clone())).await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "action": template.action.id(),
            "scope": scope_label(template.scope),
            "rev": state.rev,
            "templates": state.templates,
        }));
    }
    reporter.text(format!(
        "set the prompt template for {} in {}; routing rev {}",
        template.action.id(),
        scope_label(template.scope),
        state.rev
    ));
    Ok(())
}

async fn remove_template(
    client: &mut DaemonClient,
    reporter: &Reporter,
    scope: Scope,
    action: GitTask,
) -> Result<(), CliError> {
    let state = mutate_routing(client, RoutingMutation::RemoveTemplate { scope, action }).await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "action": action.id(),
            "scope": scope_label(scope),
            "rev": state.rev,
            "templates": state.templates,
        }));
    }
    reporter.text(format!(
        "removed the prompt template for {} from {}; routing rev {}",
        action.id(),
        scope_label(scope),
        state.rev
    ));
    Ok(())
}
