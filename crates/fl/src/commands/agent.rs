//! `fl agent …`: orchestration messages over the existing chat RPCs.
use std::time::{Duration, Instant};

use fl_model::chat::{ChatSession, SessionState};
use fl_proto::{Request, Response};
use uuid::Uuid;

use super::chat::{event_page, reporter_page};
use super::{caller, done, get_session, read_message, session_from, tree, unexpected};
use crate::cli::{AgentCommand, Cli};
use crate::client::DaemonClient;
use crate::error::CliError;
use crate::output::Reporter;

pub async fn run(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &AgentCommand,
) -> Result<(), CliError> {
    match command {
        AgentCommand::Send {
            message,
            session,
            target,
            queue,
            interrupt,
            wait_until_idle,
            timeout,
        } => {
            send(
                client,
                reporter,
                cli,
                message,
                *session,
                target.as_deref(),
                *queue,
                *interrupt,
                *wait_until_idle,
                *timeout,
            )
            .await
        }
        AgentCommand::Read {
            session,
            target,
            from,
            limit,
        } => {
            read(
                client,
                reporter,
                cli,
                *session,
                target.as_deref(),
                *from,
                *limit,
            )
            .await
        }
        AgentCommand::Interrupt { session, target } => {
            interrupt(client, reporter, cli, *session, target.as_deref()).await
        }
        AgentCommand::Stop {
            session,
            target,
            kill,
        } => stop(client, reporter, cli, *session, target.as_deref(), *kill).await,
    }
}

async fn resolve_session(
    client: &mut DaemonClient,
    cli: &Cli,
    explicit: Option<Uuid>,
    target: Option<&str>,
) -> Result<ChatSession, CliError> {
    if explicit.is_some() && target.is_some() {
        return Err(CliError::invalid("pass --session or --target, not both"));
    }
    let caller = caller(client, cli).await?;
    let id = match target {
        Some(target) => target_session(client, cli, &caller, target).await?,
        None => session_from(explicit, &caller)?,
    };
    get_session(client, id).await
}

async fn target_session(
    client: &mut DaemonClient,
    cli: &Cli,
    caller: &fl_model::workspace::CallerContext,
    target: &str,
) -> Result<Uuid, CliError> {
    if let Some(id) = target.strip_prefix("id:") {
        return Uuid::parse_str(id)
            .map_err(|_| CliError::invalid(format!("`{target}` is not a session id")));
    }
    if let Some(label) = target.strip_prefix("label:") {
        let worktree = match (&cli.worktree, &caller.worktree) {
            (Some(needle), _) => {
                let tree = tree(client).await?;
                super::find_worktree(&tree, needle)?
            }
            (None, Some(worktree)) => worktree.clone(),
            (None, None) => {
                return Err(CliError::invalid(
                    "label targets need a worktree; pass --worktree or run inside one",
                ));
            }
        };
        let sessions = match client
            .rpc(Request::ListChatSessions {
                worktree: worktree.id,
            })
            .await?
        {
            Response::ChatSessions(sessions) => sessions,
            other => return Err(unexpected("chat sessions", &other)),
        };
        let matches: Vec<_> = sessions
            .into_iter()
            .filter(|session| session.label == label)
            .collect();
        return match matches.as_slice() {
            [session] => Ok(session.id),
            [] => Err(CliError::not_found(format!(
                "no chat session in worktree {} has label `{label}`",
                worktree.label
            ))),
            many => Err(CliError::invalid(format!(
                "{} sessions have label `{label}`; use --session: {}",
                many.len(),
                many.iter()
                    .map(|session| session.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        };
    }
    if target.starts_with("group:") {
        return Err(CliError::unsupported(
            "agent groups are not implemented in this daemon yet; address sessions with --session or label:<label>",
        ));
    }
    Uuid::parse_str(target).map_err(|_| {
        CliError::invalid(format!(
            "`{target}` is not an id:<uuid> or label:<name> target"
        ))
    })
}

#[allow(clippy::too_many_arguments)]
async fn send(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    message: &str,
    explicit: Option<Uuid>,
    target: Option<&str>,
    queue: bool,
    interrupt: bool,
    wait_until_idle: bool,
    timeout: u64,
) -> Result<(), CliError> {
    let session = resolve_session(client, cli, explicit, target).await?;
    let text = read_message(message)?;
    if text.trim().is_empty() {
        return Err(CliError::invalid("message is empty"));
    }
    let mode = if interrupt {
        "interrupt"
    } else if queue {
        "queue"
    } else {
        "send"
    };
    let mut waited = false;
    if interrupt {
        done(
            client
                .rpc(Request::SteerChat {
                    session: session.id,
                    text,
                })
                .await?,
            "agent steer",
        )?;
    } else {
        if matches!(
            &session.status,
            SessionState::Starting | SessionState::Running
        ) {
            if !wait_until_idle {
                return Err(CliError::unsupported(format!(
                    "session {} is {:?}; this daemon has no durable send queue yet — pass --interrupt or --wait-until-idle",
                    session.id, session.status
                )));
            }
            wait_for_idle(client, session.id, timeout).await?;
            waited = true;
        }
        done(
            client
                .rpc(Request::SendChat {
                    session: session.id,
                    text,
                })
                .await?,
            "agent send",
        )?;
    }
    let final_session = if wait_until_idle {
        let session = wait_for_idle(client, session.id, timeout).await?;
        waited = true;
        Some(session)
    } else {
        None
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "session": session.id,
            "sent": true,
            "mode": mode,
            "waited": waited,
            "state": final_session.map(|session| session.status),
        }));
    }
    reporter.text(format!(
        "sent to session {} ({mode}{})",
        session.id,
        if waited { ", waited for idle" } else { "" }
    ));
    Ok(())
}

async fn wait_for_idle(
    client: &mut DaemonClient,
    session: Uuid,
    timeout: u64,
) -> Result<ChatSession, CliError> {
    let timeout = Duration::from_secs(timeout.clamp(1, 86_400));
    let deadline = Instant::now() + timeout;
    loop {
        let current = get_session(client, session).await?;
        match &current.status {
            SessionState::Starting | SessionState::Running => {}
            _ => return Ok(current),
        }
        if Instant::now() >= deadline {
            return Err(CliError::new(
                "timeout",
                format!(
                    "session {session} is still {:?} after {}s",
                    current.status,
                    timeout.as_secs()
                ),
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn read(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    explicit: Option<Uuid>,
    target: Option<&str>,
    from: u64,
    limit: u16,
) -> Result<(), CliError> {
    let session = resolve_session(client, cli, explicit, target).await?;
    let page = event_page(client, session.id, from, limit).await?;
    reporter_page(reporter, &page)
}

async fn interrupt(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    explicit: Option<Uuid>,
    target: Option<&str>,
) -> Result<(), CliError> {
    let session = resolve_session(client, cli, explicit, target).await?;
    if matches!(
        &session.status,
        SessionState::Idle | SessionState::Closed | SessionState::Failed { .. }
    ) {
        return Err(CliError::invalid(format!(
            "session {} is {:?}; there is no running turn to interrupt",
            session.id, session.status
        )));
    }
    done(
        client
            .rpc(Request::CancelChat {
                session: session.id,
            })
            .await?,
        "agent interrupt",
    )?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "session": session.id,
            "interrupted": true,
        }));
    }
    reporter.text(format!("interrupted session {}", session.id));
    Ok(())
}

async fn stop(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    explicit: Option<Uuid>,
    target: Option<&str>,
    kill: bool,
) -> Result<(), CliError> {
    let session = resolve_session(client, cli, explicit, target).await?;
    // A session with no active turn normalizes instead of failing; record
    // whether the interrupt actually landed rather than claiming it did.
    let interrupted = kill
        && matches!(
            client
                .rpc(Request::CancelChat {
                    session: session.id,
                })
                .await,
            Ok(Response::Done)
        );
    done(
        client
            .rpc(Request::CloseChat {
                session: session.id,
            })
            .await?,
        "agent stop",
    )?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "session": session.id,
            "stopped": true,
            "killed": kill,
            "interrupted": interrupted,
        }));
    }
    reporter.text(format!(
        "stopped session {}{}",
        session.id,
        if interrupted {
            " (current turn interrupted)"
        } else if kill {
            " (no active turn to interrupt)"
        } else {
            ""
        }
    ));
    Ok(())
}
