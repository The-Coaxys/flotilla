//! `fl workspace …` and `fl section list`.
use fl_model::workspace::WorkspaceInfo;
use fl_proto::{Request, Response};

use super::{caller, find_workspace, tree, unexpected};
use crate::cli::{Cli, SectionCommand, WorkspaceCommand};
use crate::client::DaemonClient;
use crate::error::CliError;
use crate::output::Reporter;

pub async fn run(
    client: &mut DaemonClient,
    reporter: &Reporter,
    _cli: &Cli,
    command: &WorkspaceCommand,
) -> Result<(), CliError> {
    match command {
        WorkspaceCommand::List => list(client, reporter).await,
        WorkspaceCommand::Get { workspace } => get(client, reporter, workspace.as_deref()).await,
        WorkspaceCommand::Select { workspace } => select(client, reporter, workspace).await,
    }
}

async fn selected(client: &mut DaemonClient) -> Result<Option<WorkspaceInfo>, CliError> {
    match client.rpc(Request::GetSelectedWorkspace).await? {
        Response::SelectedWorkspace(workspace) => Ok(workspace),
        other => Err(unexpected("selected workspace", &other)),
    }
}

async fn list(client: &mut DaemonClient, reporter: &Reporter) -> Result<(), CliError> {
    let tree = tree(client).await?;
    let selected = selected(client).await?;
    let selected_id = selected.as_ref().map(|workspace| workspace.id);
    if reporter.json {
        let workspaces: Vec<_> = tree
            .workspaces
            .iter()
            .map(|workspace| {
                let projects = tree
                    .projects
                    .iter()
                    .filter(|project| project.workspace == workspace.id)
                    .count();
                serde_json::json!({
                    "id": workspace.id,
                    "name": workspace.name,
                    "order": workspace.order,
                    "projects": projects,
                })
            })
            .collect();
        return reporter.value(&serde_json::json!({
            "selected": selected_id,
            "workspaces": workspaces,
        }));
    }
    if tree.workspaces.is_empty() {
        reporter.text("no workspaces");
        return Ok(());
    }
    for workspace in &tree.workspaces {
        let projects = tree
            .projects
            .iter()
            .filter(|project| project.workspace == workspace.id)
            .count();
        reporter.text(format!(
            "{} {} ({}) — {projects} project(s)",
            if Some(workspace.id) == selected_id {
                "*"
            } else {
                " "
            },
            workspace.name,
            workspace.id
        ));
    }
    Ok(())
}

async fn get(
    client: &mut DaemonClient,
    reporter: &Reporter,
    needle: Option<&str>,
) -> Result<(), CliError> {
    let tree = tree(client).await?;
    let workspace = match needle {
        Some(needle) => find_workspace(&tree, needle)?,
        None => selected(client).await?.ok_or_else(|| {
            CliError::not_found("no workspace is selected; run `fl workspace select <id|name>`")
        })?,
    };
    let projects: Vec<_> = tree
        .projects
        .iter()
        .filter(|project| project.workspace == workspace.id)
        .cloned()
        .collect();
    let worktrees: Vec<_> = tree
        .worktrees
        .iter()
        .filter(|worktree| {
            projects
                .iter()
                .any(|project| project.id == worktree.project)
        })
        .cloned()
        .collect();
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "workspace": workspace,
            "projects": projects,
            "worktrees": worktrees,
        }));
    }
    reporter.text(format!("{} ({})", workspace.name, workspace.id));
    for project in &projects {
        reporter.text(format!(
            "  project {} ({:?}) {}",
            project.name, project.kind, project.path
        ));
        for worktree in worktrees
            .iter()
            .filter(|worktree| worktree.project == project.id)
        {
            reporter.text(format!(
                "    worktree {} [{}] {}",
                worktree.label,
                worktree.branch.as_deref().unwrap_or("(detached)"),
                worktree.id
            ));
        }
    }
    Ok(())
}

async fn select(
    client: &mut DaemonClient,
    reporter: &Reporter,
    needle: &str,
) -> Result<(), CliError> {
    let tree = tree(client).await?;
    let workspace = find_workspace(&tree, needle)?;
    let selected = match client
        .rpc(Request::SelectWorkspace {
            workspace: workspace.id,
        })
        .await?
    {
        Response::Workspace(workspace) => workspace,
        other => return Err(unexpected("selected workspace", &other)),
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({ "selected": selected }));
    }
    reporter.text(format!(
        "selected workspace {} ({})",
        selected.name, selected.id
    ));
    Ok(())
}

pub async fn run_section(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &SectionCommand,
) -> Result<(), CliError> {
    match command {
        SectionCommand::List => {
            let _ = caller(client, cli).await?;
            let state = match client.rpc(Request::ListSections).await? {
                Response::SectionsState(state) => state,
                other => return Err(unexpected("sections", &other)),
            };
            if reporter.json {
                return reporter.value(&state);
            }
            reporter.text(format!("sections rev {}", state.rev));
            for section in &state.sections {
                reporter.text(format!(
                    "  {} ({}) order {} priority {} {:?}",
                    section.name, section.id, section.order, section.priority, section.kind
                ));
            }
            for membership in &state.memberships {
                reporter.text(format!(
                    "  membership worktree {} section {:?} pinned {}",
                    membership.worktree, membership.manual, membership.pinned
                ));
            }
            Ok(())
        }
    }
}
