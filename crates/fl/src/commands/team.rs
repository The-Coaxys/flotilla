//! `fl team …` — parallel role runs with one lead fan-in.
use fl_model::team::{MAX_TEAM_SUMMARY_BYTES, TeamRun, TeamRunSpec, TeamState};
use fl_proto::{Request, Response};
use uuid::Uuid;

use super::{env_session, resolve_worktree, unexpected};
use crate::cli::{Cli, TeamCommand};
use crate::client::DaemonClient;
use crate::error::CliError;
use crate::output::Reporter;

pub async fn run(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &TeamCommand,
) -> Result<(), CliError> {
    match command {
        TeamCommand::Run { file } => launch(client, reporter, cli, file).await,
        TeamCommand::Status { run } => status(client, reporter, cli, *run).await,
        TeamCommand::List => list(client, reporter, cli).await,
        TeamCommand::Cancel { run } => cancel(client, reporter, cli, *run).await,
        TeamCommand::Report {
            run,
            role,
            summary,
            summary_file,
            result_file,
        } => {
            report(
                client,
                reporter,
                cli,
                *run,
                role.as_deref(),
                summary.as_deref(),
                summary_file.as_deref(),
                result_file.as_deref(),
            )
            .await
        }
    }
}

async fn launch(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    file: &str,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let bytes = if file == "-" {
        let mut text = String::new();
        use std::io::Read;
        std::io::stdin()
            .take(1024 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(|error| CliError::invalid(format!("reading stdin: {error}")))?;
        text
    } else {
        std::fs::read_to_string(file)
            .map_err(|error| CliError::invalid(format!("reading `{file}`: {error}")))?
    };
    let spec: TeamRunSpec = serde_json::from_str(&bytes).map_err(|error| {
        CliError::invalid(format!(
            "`{file}` is not a valid team run JSON file: {error}; expected \
             {{\"lead\": \"<label>\", \"roles\": [{{\"label\", \"provider\", \"prompt\"}}]}}"
        ))
    })?;
    spec.validate().map_err(CliError::invalid)?;
    match client
        .rpc(Request::TeamRun {
            worktree: worktree.id,
            spec,
        })
        .await?
    {
        Response::TeamRun(run) => {
            if reporter.json {
                return reporter.value(&serde_json::json!({ "run": run }));
            }
            print_run(reporter, &run, false);
            Ok(())
        }
        other => Err(unexpected("team run", &other)),
    }
}

async fn run_or_latest(
    client: &mut DaemonClient,
    cli: &Cli,
    run: Option<Uuid>,
) -> Result<TeamRun, CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    match client
        .rpc(Request::TeamStatus {
            worktree: worktree.id,
            run,
        })
        .await?
    {
        Response::TeamRun(run) => Ok(run),
        other => Err(unexpected("team run", &other)),
    }
}

async fn status(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    run: Option<Uuid>,
) -> Result<(), CliError> {
    let run = run_or_latest(client, cli, run).await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({ "run": run }));
    }
    print_run(reporter, &run, true);
    Ok(())
}

async fn list(client: &mut DaemonClient, reporter: &Reporter, cli: &Cli) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let runs = match client
        .rpc(Request::TeamList {
            worktree: worktree.id,
        })
        .await?
    {
        Response::TeamRuns(runs) => runs,
        other => return Err(unexpected("team runs", &other)),
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "runs": runs,
        }));
    }
    if runs.is_empty() {
        reporter.text("no team runs");
        return Ok(());
    }
    for run in runs {
        reporter.text(format!(
            "{} {} lead={} {}/{} finished",
            run.id,
            run.state.as_str(),
            run.lead,
            run.roles
                .iter()
                .filter(|role| role.state == TeamState::Finished)
                .count(),
            run.roles.len()
        ));
    }
    Ok(())
}

async fn cancel(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    run: Option<Uuid>,
) -> Result<(), CliError> {
    let run = run_or_latest(client, cli, run).await?;
    match client.rpc(Request::TeamCancel { run: run.id }).await? {
        Response::TeamRun(run) => {
            if reporter.json {
                return reporter.value(&serde_json::json!({ "run": run }));
            }
            reporter.text(format!("cancelled team run {}", run.id));
            Ok(())
        }
        other => Err(unexpected("team run", &other)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn report(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    run: Option<Uuid>,
    role: Option<&str>,
    summary: Option<&str>,
    summary_file: Option<&str>,
    result_file: Option<&str>,
) -> Result<(), CliError> {
    let run = run_or_latest(client, cli, run).await?;
    let summary = match (summary, summary_file) {
        (Some(text), _) => Some(text.to_owned()),
        (None, Some("-")) => {
            let mut text = String::new();
            use std::io::Read;
            std::io::stdin()
                .take((MAX_TEAM_SUMMARY_BYTES + 1) as u64)
                .read_to_string(&mut text)
                .map_err(|error| CliError::invalid(format!("reading stdin: {error}")))?;
            Some(text)
        }
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .map_err(|error| CliError::invalid(format!("reading `{path}`: {error}")))?,
        ),
        (None, None) => None,
    };
    let Some(summary) = summary else {
        // Read mode: every role's report.
        if reporter.json {
            return reporter.value(&serde_json::json!({ "run": run }));
        }
        print_run(reporter, &run, true);
        return Ok(());
    };
    if summary.trim().is_empty() {
        return Err(CliError::invalid("the report summary is empty"));
    }
    if summary.len() > MAX_TEAM_SUMMARY_BYTES {
        return Err(CliError::invalid(format!(
            "the report summary is at most {MAX_TEAM_SUMMARY_BYTES} bytes (got {})",
            summary.len()
        )));
    }
    let role = match role {
        Some(role) => role.to_owned(),
        None => {
            let session = match env_session().ok().flatten() {
                Some(session) => Some(session),
                None => super::caller(client, cli).await?.session.map(|s| s.id),
            };
            let Some(session) = session else {
                return Err(CliError::invalid(
                    "cannot tell which role is reporting; pass --role <label> or set FLOTILLA_SESSION_ID",
                ));
            };
            run.roles
                .iter()
                .find(|candidate| candidate.session == Some(session))
                .map(|role| role.label.clone())
                .ok_or_else(|| {
                    CliError::invalid("this session is not a role of the run; pass --role <label>")
                })?
        }
    };
    match client
        .rpc(Request::TeamReportSubmit {
            run: run.id,
            role,
            summary,
            result_file: result_file.map(str::to_owned),
        })
        .await?
    {
        Response::TeamRun(run) => {
            if reporter.json {
                return reporter.value(&serde_json::json!({ "run": run }));
            }
            reporter.text(format!("report recorded for team run {}", run.id));
            Ok(())
        }
        other => Err(unexpected("team run", &other)),
    }
}

fn print_run(reporter: &Reporter, run: &TeamRun, reports: bool) {
    reporter.text(format!(
        "team run {} — {} (lead {}, {} roles)",
        run.id,
        run.state.as_str(),
        run.lead,
        run.roles.len()
    ));
    for role in &run.roles {
        let session = role
            .session
            .map(|session| session.to_string())
            .unwrap_or_else(|| "-".into());
        let tab = role
            .tab
            .map(|tab| tab.to_string())
            .unwrap_or_else(|| "-".into());
        reporter.text(format!(
            "  {:<16} {:<10} {:<10} session {} tab {}",
            role.label,
            role.provider,
            role.state.as_str(),
            session,
            tab
        ));
        if reports && let Some(report) = &role.report {
            for line in report.summary.lines() {
                reporter.text(format!("      {line}"));
            }
            if let Some(path) = &report.result_file {
                reporter.text(format!("      result file: {path}"));
            }
        }
    }
    if run.fan_in_sent {
        reporter.text(format!("fan-in delivered to {}", run.lead));
    }
}
