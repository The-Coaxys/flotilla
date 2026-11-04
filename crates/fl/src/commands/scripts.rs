//! `fl project scripts …`: config checks and setup/run/teardown control.
use fl_model::workspace::{ProjectScriptCheck, ScriptState};
use fl_proto::{Request, Response};

use super::{resolve_worktree, unexpected};
use crate::cli::{Cli, ProjectCommand, ScriptCommand};
use crate::client::DaemonClient;
use crate::error::CliError;
use crate::output::Reporter;

pub async fn run(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &ProjectCommand,
) -> Result<(), CliError> {
    let ProjectCommand::Scripts { command } = command;
    match command {
        ScriptCommand::Check => check(client, reporter, cli).await,
        ScriptCommand::Run { .. } => run_script(client, reporter, cli, command).await,
        ScriptCommand::Status { .. } => status(client, reporter, cli, command).await,
        ScriptCommand::Stop { .. } => stop(client, reporter, cli, command).await,
    }
}

async fn check(client: &mut DaemonClient, reporter: &Reporter, cli: &Cli) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let check: ProjectScriptCheck = match client
        .rpc(Request::CheckProjectScripts {
            worktree: worktree.id,
        })
        .await?
    {
        Response::ScriptCheck(check) => check,
        other => return Err(unexpected("script check", &other)),
    };
    if let Some(error) = &check.error {
        return Err(CliError::invalid(format!("{}: {error}", check.config_path)));
    }
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "check": check,
        }));
    }
    reporter.text(format!("config {}", check.config_path));
    reporter.text(format!(
        "valid; setup {}, teardown {}, {} run target(s)",
        if check.setup { "yes" } else { "no" },
        if check.teardown { "yes" } else { "no" },
        check.runs.len()
    ));
    for run in &check.runs {
        reporter.text(format!("  run {run}"));
    }
    Ok(())
}

async fn run_script(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &ScriptCommand,
) -> Result<(), CliError> {
    let kind = command
        .kind()
        .ok_or_else(|| CliError::internal("run without a script kind"))?;
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let state: ScriptState = match client
        .rpc(Request::RunScript {
            worktree: worktree.id,
            kind,
        })
        .await?
    {
        Response::ScriptState(state) => state,
        other => return Err(unexpected("script state", &other)),
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "script": state,
        }));
    }
    reporter.text(format!(
        "started {:?} ({:?}) terminal {}",
        state.kind,
        state.state,
        state
            .terminal
            .map(|terminal| terminal.to_string())
            .unwrap_or_else(|| "(none)".into())
    ));
    Ok(())
}

async fn status(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &ScriptCommand,
) -> Result<(), CliError> {
    let filter = command.kind();
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let mut scripts: Vec<ScriptState> = match client
        .rpc(Request::ListScripts {
            worktree: worktree.id,
        })
        .await?
    {
        Response::Scripts(scripts) => scripts,
        other => return Err(unexpected("scripts", &other)),
    };
    if let Some(kind) = &filter {
        scripts.retain(|script| &script.kind == kind);
    }
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "scripts": scripts,
        }));
    }
    if scripts.is_empty() {
        reporter.text("no scripts");
        return Ok(());
    }
    for script in &scripts {
        reporter.text(format!(
            "{:?} {:?} terminal {}",
            script.kind,
            script.state,
            script
                .terminal
                .map(|terminal| terminal.to_string())
                .unwrap_or_else(|| "(none)".into())
        ));
    }
    Ok(())
}

async fn stop(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &ScriptCommand,
) -> Result<(), CliError> {
    let kind = command
        .kind()
        .ok_or_else(|| CliError::internal("stop without a script kind"))?;
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let state: ScriptState = match client
        .rpc(Request::StopScript {
            worktree: worktree.id,
            kind,
        })
        .await?
    {
        Response::ScriptState(state) => state,
        other => return Err(unexpected("script state", &other)),
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "script": state,
        }));
    }
    reporter.text(format!("stopped {:?} ({:?})", state.kind, state.state));
    Ok(())
}
