//! Command dispatch and the helpers shared by the command families.
mod agent;
mod chat;
mod coordination;
mod custom;
mod hook;
mod instructions;
mod layout;
mod scripts;
mod team;
mod workspace;
mod worktree;

use std::io::Read;

use fl_model::chat::{ChatSession, MAX_CHAT_TEXT};
use fl_model::layout::{Layout, LayoutOp};
use fl_model::workspace::{CallerContext, LayoutScope, Project, WorkspaceTree, Worktree};
use fl_model::{OperationId, WorkspaceId, WorktreeId};
use fl_proto::{Request, Response};
use uuid::Uuid;

use crate::cli::{Cli, Command, OutputFormat};
use crate::client::{DaemonClient, socket_path};
use crate::error::CliError;
use crate::output::Reporter;

pub async fn run(cli: &Cli) -> Result<(), CliError> {
    let reporter = Reporter::new(cli.json || cli.output == OutputFormat::Json);
    // Embedded guides never need the daemon: a broken connection must not
    // hide the trusted contract from an agent.
    if let Command::Instructions { topic } = &cli.command {
        return instructions::run(&reporter, *topic);
    }
    let socket = socket_path()?;
    let mut client = DaemonClient::connect(&socket).await?;
    match &cli.command {
        Command::Status => status(&mut client, &reporter, cli).await,
        Command::Workspace { command } => {
            workspace::run(&mut client, &reporter, cli, command).await
        }
        Command::Section { command } => {
            workspace::run_section(&mut client, &reporter, cli, command).await
        }
        Command::Worktree { command } => worktree::run(&mut client, &reporter, cli, command).await,
        Command::Project { command } => scripts::run(&mut client, &reporter, cli, command).await,
        Command::Tab { command } => layout::run_tab(&mut client, &reporter, cli, command).await,
        Command::Layout { command } => {
            layout::run_layout(&mut client, &reporter, cli, command).await
        }
        Command::Chat { command } => chat::run(&mut client, &reporter, cli, command).await,
        Command::History { command } => chat::run_history(&mut client, &reporter, command).await,
        Command::Agent { command } => agent::run(&mut client, &reporter, cli, command).await,
        Command::Instructions { .. } => unreachable!("handled before connecting"),
        Command::CoordinationState { command } => {
            coordination::run(&mut client, &reporter, cli, command).await
        }
        Command::Team { command } => team::run(&mut client, &reporter, cli, command).await,
        Command::Commands { command } => custom::run(&mut client, &reporter, cli, command).await,
        Command::PromptTemplate { command } => {
            custom::run_template(&mut client, &reporter, cli, command).await
        }
        Command::Hook {
            event,
            session,
            payload,
        } => {
            hook::run(
                &mut client,
                &reporter,
                cli,
                event,
                *session,
                payload.as_deref(),
            )
            .await
        }
    }
}

async fn status(client: &mut DaemonClient, reporter: &Reporter, cli: &Cli) -> Result<(), CliError> {
    let daemon = match client.rpc(Request::DaemonInfo).await? {
        Response::DaemonInfo(info) => info,
        other => return Err(unexpected("daemon info", &other)),
    };
    let caller = caller(client, cli).await?;
    let selected = match client.rpc(Request::GetSelectedWorkspace).await? {
        Response::SelectedWorkspace(workspace) => workspace,
        other => return Err(unexpected("selected workspace", &other)),
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "daemon": daemon,
            "caller": caller,
            "selected_workspace": selected,
        }));
    }
    reporter.text(format!(
        "daemon {} ({}) pid {} up {}s\nsocket {}",
        daemon.version, daemon.build, daemon.pid, daemon.uptime_secs, daemon.socket_path
    ));
    match (&caller.worktree, &caller.project) {
        (Some(worktree), Some(project)) => reporter.text(format!(
            "worktree {} [{}] on {} target {}\npath {}",
            worktree.label,
            project.name,
            worktree.branch.as_deref().unwrap_or("(detached)"),
            worktree.target.as_deref().unwrap_or("(none)"),
            worktree.path
        )),
        _ => reporter.text(format!("worktree (none) — cwd {}", caller.cwd)),
    }
    if let Some(session) = &caller.session {
        reporter.text(format!(
            "session {} {} ({})",
            session.id, session.label, session.provider
        ));
    }
    match selected {
        Some(workspace) => reporter.text(format!("selected workspace {}", workspace.name)),
        None => reporter.text("selected workspace (none)"),
    }
    Ok(())
}

// --- Shared resolution and request helpers --------------------------------

pub async fn caller(client: &mut DaemonClient, _cli: &Cli) -> Result<CallerContext, CliError> {
    let cwd = std::env::current_dir().map_err(|error| {
        CliError::invalid(format!("cannot read the current directory: {error}"))
    })?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let cwd = cwd
        .to_str()
        .ok_or_else(|| CliError::invalid("the current directory is not valid UTF-8"))?
        .to_owned();
    // A malformed environment value must not break commands that do not need
    // a chat session; commands that do call `env_session` themselves.
    let session = env_session().ok().flatten();
    match client.rpc(Request::ResolveCaller { cwd, session }).await? {
        Response::Caller(caller) => Ok(caller),
        other => Err(unexpected("caller context", &other)),
    }
}

pub fn env_session() -> Result<Option<Uuid>, CliError> {
    match std::env::var("FLOTILLA_SESSION_ID") {
        Ok(value) if !value.trim().is_empty() => {
            Uuid::parse_str(value.trim()).map(Some).map_err(|_| {
                CliError::invalid(format!(
                    "FLOTILLA_SESSION_ID `{value}` is not a chat session id"
                ))
            })
        }
        _ => Ok(None),
    }
}

pub async fn tree(client: &mut DaemonClient) -> Result<WorkspaceTree, CliError> {
    match client.rpc(Request::GetWorkspaceTree).await? {
        Response::WorkspaceTree(tree) => Ok(tree),
        other => Err(unexpected("workspace tree", &other)),
    }
}

pub async fn resolve_worktree(
    client: &mut DaemonClient,
    cli: &Cli,
) -> Result<(Worktree, Project), CliError> {
    if let Some(target) = &cli.worktree {
        let tree = tree(client).await?;
        let worktree = find_worktree(&tree, target)?;
        let project = tree
            .projects
            .iter()
            .find(|project| project.id == worktree.project)
            .cloned()
            .ok_or_else(|| {
                CliError::not_found(format!(
                    "worktree {} refers to an absent project",
                    worktree.id
                ))
            })?;
        return Ok((worktree, project));
    }
    let caller = caller(client, cli).await?;
    match (caller.worktree, caller.project) {
        (Some(worktree), Some(project)) => Ok((worktree, project)),
        _ => Err(CliError::invalid(
            "not inside a worktree; pass --worktree <id|label|path>",
        )),
    }
}

pub fn find_worktree(tree: &WorkspaceTree, needle: &str) -> Result<Worktree, CliError> {
    if let Ok(id) = needle.parse::<WorktreeId>() {
        return tree
            .worktrees
            .iter()
            .find(|worktree| worktree.id == id)
            .cloned()
            .ok_or_else(|| CliError::not_found(format!("worktree {id} not found")));
    }
    let exact: Vec<_> = tree
        .worktrees
        .iter()
        .filter(|worktree| worktree.label == needle || worktree.path == needle)
        .collect();
    match exact.as_slice() {
        [worktree] => return Ok((**worktree).clone()),
        [] => {}
        many => {
            return Err(CliError::invalid(format!(
                "`{needle}` matches {} worktrees; use an id: {}",
                many.len(),
                many.iter()
                    .map(|worktree| worktree.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let insensitive: Vec<_> = tree
        .worktrees
        .iter()
        .filter(|worktree| worktree.label.eq_ignore_ascii_case(needle))
        .collect();
    match insensitive.as_slice() {
        [worktree] => Ok((**worktree).clone()),
        [] => Err(CliError::not_found(format!(
            "no worktree matches `{needle}`; run `fl workspace list`"
        ))),
        many => Err(CliError::invalid(format!(
            "`{needle}` matches {} worktrees; use an id: {}",
            many.len(),
            many.iter()
                .map(|worktree| worktree.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

pub fn find_workspace(
    tree: &WorkspaceTree,
    needle: &str,
) -> Result<fl_model::workspace::WorkspaceInfo, CliError> {
    if let Ok(id) = needle.parse::<WorkspaceId>() {
        return tree
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .cloned()
            .ok_or_else(|| CliError::not_found(format!("workspace {id} not found")));
    }
    let exact: Vec<_> = tree
        .workspaces
        .iter()
        .filter(|workspace| workspace.name == needle)
        .collect();
    match exact.as_slice() {
        [workspace] => return Ok((**workspace).clone()),
        [] => {}
        many => {
            return Err(CliError::invalid(format!(
                "`{needle}` matches {} workspaces; use an id: {}",
                many.len(),
                many.iter()
                    .map(|workspace| workspace.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let insensitive: Vec<_> = tree
        .workspaces
        .iter()
        .filter(|workspace| workspace.name.eq_ignore_ascii_case(needle))
        .collect();
    match insensitive.as_slice() {
        [workspace] => Ok((**workspace).clone()),
        [] => Err(CliError::not_found(format!(
            "no workspace matches `{needle}`; run `fl workspace list`"
        ))),
        many => Err(CliError::invalid(format!(
            "`{needle}` matches {} workspaces; use an id: {}",
            many.len(),
            many.iter()
                .map(|workspace| workspace.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

pub fn worktree_scope(worktree: &Worktree) -> LayoutScope {
    LayoutScope::Worktree(worktree.id)
}

pub async fn apply_layout(
    client: &mut DaemonClient,
    scope: LayoutScope,
    op: LayoutOp,
) -> Result<Layout, CliError> {
    let mut last = None;
    for _ in 0..2 {
        let layout = match client.rpc(Request::GetScopedLayout { scope }).await? {
            Response::Layout(layout) => layout,
            other => return Err(unexpected("layout", &other)),
        };
        match client
            .rpc(Request::ApplyScopedLayout {
                scope,
                operation: OperationId::new(),
                base_rev: layout.rev,
                op: op.clone(),
            })
            .await
        {
            Ok(Response::Layout(layout)) => return Ok(layout),
            Ok(other) => return Err(unexpected("layout", &other)),
            Err(error) if error.code == "revision_conflict" => last = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| CliError::internal("layout retry exhausted")))
}

pub async fn current_layout(
    client: &mut DaemonClient,
    scope: LayoutScope,
) -> Result<Layout, CliError> {
    match client.rpc(Request::GetScopedLayout { scope }).await? {
        Response::Layout(layout) => Ok(layout),
        other => Err(unexpected("layout", &other)),
    }
}

pub async fn get_session(client: &mut DaemonClient, id: Uuid) -> Result<ChatSession, CliError> {
    match client.rpc(Request::GetChatSession { session: id }).await? {
        Response::ChatSession(session) => Ok(session),
        other => Err(unexpected("chat session", &other)),
    }
}

pub fn session_from(explicit: Option<Uuid>, caller: &CallerContext) -> Result<Uuid, CliError> {
    if let Some(id) = explicit {
        return Ok(id);
    }
    if let Some(id) = env_session()? {
        return Ok(id);
    }
    caller
        .session
        .as_ref()
        .map(|session| session.id)
        .ok_or_else(|| {
            CliError::invalid("no chat session; pass --session <uuid> or set FLOTILLA_SESSION_ID")
        })
}

pub fn read_message(message: &str) -> Result<String, CliError> {
    if message != "-" {
        return Ok(message.to_owned());
    }
    let mut text = String::new();
    std::io::stdin()
        .take((MAX_CHAT_TEXT + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|error| CliError::invalid(format!("reading stdin: {error}")))?;
    if text.len() > MAX_CHAT_TEXT {
        return Err(CliError::invalid(format!(
            "stdin exceeds the {MAX_CHAT_TEXT}-byte message limit"
        )));
    }
    Ok(text)
}

pub fn unexpected(what: &str, response: &Response) -> CliError {
    CliError::internal(format!(
        "unexpected daemon response for {what}: {response:?}"
    ))
}

pub fn done(response: Response, what: &str) -> Result<(), CliError> {
    match response {
        Response::Done => Ok(()),
        other => Err(unexpected(what, &other)),
    }
}
