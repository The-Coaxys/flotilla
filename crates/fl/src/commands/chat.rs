//! `fl chat …` and `fl history …`.
use fl_model::chat::{ChatEvent, ChatEventPage, ChatSession};
use fl_proto::{Request, Response};

use super::{caller, done, read_message, resolve_worktree, session_from, unexpected};
use crate::cli::{ChatCommand, Cli, HistoryCommand};
use crate::client::DaemonClient;
use crate::error::CliError;
use crate::output::Reporter;

pub async fn run(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    command: &ChatCommand,
) -> Result<(), CliError> {
    match command {
        ChatCommand::Providers => providers(client, reporter).await,
        ChatCommand::List => list(client, reporter, cli).await,
        ChatCommand::New {
            provider,
            model,
            thinking,
            permission_mode,
        } => {
            new(
                client,
                reporter,
                cli,
                provider,
                model,
                thinking,
                *permission_mode,
            )
            .await
        }
        ChatCommand::Send { session, message } => {
            send(client, reporter, cli, *session, message).await
        }
        ChatCommand::Events {
            session,
            from,
            limit,
        } => events(client, reporter, cli, *session, *from, *limit).await,
        ChatCommand::Cancel { session } => cancel(client, reporter, cli, *session).await,
        ChatCommand::Close { session } => close(client, reporter, cli, *session).await,
    }
}

async fn providers(client: &mut DaemonClient, reporter: &Reporter) -> Result<(), CliError> {
    let providers = match client.rpc(Request::ListChatProviders).await? {
        Response::ChatProviders(providers) => providers,
        other => return Err(unexpected("chat providers", &other)),
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({ "providers": providers }));
    }
    if providers.is_empty() {
        reporter.text("no chat providers");
        return Ok(());
    }
    for provider in providers {
        match provider.unavailable_reason {
            Some(reason) => reporter.text(format!(
                "{} ({}) unavailable: {reason}",
                provider.provider.name, provider.provider.id
            )),
            None => reporter.text(format!(
                "{} ({}) {}",
                provider.provider.name,
                provider.provider.id,
                provider.version.as_deref().unwrap_or("ready")
            )),
        }
    }
    Ok(())
}

async fn list(client: &mut DaemonClient, reporter: &Reporter, cli: &Cli) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let sessions = list_sessions(client, worktree.id).await?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "sessions": sessions,
        }));
    }
    if sessions.is_empty() {
        reporter.text("no chat sessions");
        return Ok(());
    }
    for session in sessions {
        reporter.text(format!(
            "{} {} {:?} {}",
            session.id, session.provider, session.status, session.label
        ));
    }
    Ok(())
}

async fn list_sessions(
    client: &mut DaemonClient,
    worktree: fl_model::WorktreeId,
) -> Result<Vec<ChatSession>, CliError> {
    match client.rpc(Request::ListChatSessions { worktree }).await? {
        Response::ChatSessions(sessions) => Ok(sessions),
        other => Err(unexpected("chat sessions", &other)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn new(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    provider: &str,
    model: &Option<String>,
    thinking: &Option<String>,
    permission_mode: crate::cli::PermissionModeArg,
) -> Result<(), CliError> {
    let (worktree, _project) = resolve_worktree(client, cli).await?;
    let session = match client
        .rpc(Request::StartChat {
            worktree: worktree.id,
            provider: provider.to_owned(),
            model: model.clone(),
            thinking: thinking.clone(),
            permission_mode: permission_mode.into(),
            resume: None,
        })
        .await?
    {
        Response::ChatSessions(sessions) if sessions.len() == 1 => {
            sessions.into_iter().next().expect("length checked")
        }
        Response::ChatSession(session) => session,
        other => return Err(unexpected("chat session", &other)),
    };
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "worktree": worktree,
            "session": session,
        }));
    }
    reporter.text(format!(
        "started {} session {} ({:?})",
        session.provider, session.id, session.status
    ));
    Ok(())
}

async fn send(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    explicit: Option<uuid::Uuid>,
    message: &str,
) -> Result<(), CliError> {
    let caller = caller(client, cli).await?;
    let session = session_from(explicit, &caller)?;
    let text = read_message(message)?;
    if text.trim().is_empty() {
        return Err(CliError::invalid("message is empty"));
    }
    done(
        client.rpc(Request::SendChat { session, text }).await?,
        "chat send",
    )?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "session": session,
            "sent": true,
        }));
    }
    reporter.text(format!("sent to session {session}"));
    Ok(())
}

async fn events(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    explicit: Option<uuid::Uuid>,
    from: u64,
    limit: u16,
) -> Result<(), CliError> {
    let caller = caller(client, cli).await?;
    let session = session_from(explicit, &caller)?;
    let page = event_page(client, session, from, limit).await?;
    reporter_page(reporter, &page)
}

pub(crate) async fn event_page(
    client: &mut DaemonClient,
    session: uuid::Uuid,
    from: u64,
    limit: u16,
) -> Result<ChatEventPage, CliError> {
    match client
        .rpc(Request::GetChatEvents {
            session,
            from,
            limit,
        })
        .await?
    {
        Response::ChatEvents(page) => Ok(page),
        other => Err(unexpected("chat events", &other)),
    }
}

async fn cancel(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    explicit: Option<uuid::Uuid>,
) -> Result<(), CliError> {
    let caller = caller(client, cli).await?;
    let session = session_from(explicit, &caller)?;
    done(
        client.rpc(Request::CancelChat { session }).await?,
        "chat cancel",
    )?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "session": session,
            "cancelled": true,
        }));
    }
    reporter.text(format!("cancelled session {session}"));
    Ok(())
}

async fn close(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    explicit: Option<uuid::Uuid>,
) -> Result<(), CliError> {
    let caller = caller(client, cli).await?;
    let session = session_from(explicit, &caller)?;
    done(
        client.rpc(Request::CloseChat { session }).await?,
        "chat close",
    )?;
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "session": session,
            "closed": true,
        }));
    }
    reporter.text(format!("closed session {session}"));
    Ok(())
}

pub async fn run_history(
    client: &mut DaemonClient,
    reporter: &Reporter,
    command: &HistoryCommand,
) -> Result<(), CliError> {
    match command {
        HistoryCommand::List { query, limit } => {
            if let Some(query) = query {
                let hits = match client
                    .rpc(Request::SearchChats {
                        query: query.clone(),
                        limit: *limit,
                    })
                    .await?
                {
                    Response::ChatHits(hits) => hits,
                    other => return Err(unexpected("chat hits", &other)),
                };
                if reporter.json {
                    return reporter.value(&serde_json::json!({
                        "query": query,
                        "hits": hits,
                    }));
                }
                if hits.is_empty() {
                    reporter.text("no matches");
                    return Ok(());
                }
                for hit in hits {
                    reporter.text(format!(
                        "{} @{} {}: {}",
                        hit.session, hit.sequence, hit.worktree, hit.snippet
                    ));
                }
                Ok(())
            } else {
                let sessions = match client
                    .rpc(Request::ListChatHistory { limit: *limit })
                    .await?
                {
                    Response::ChatSessions(sessions) => sessions,
                    other => return Err(unexpected("chat history", &other)),
                };
                if reporter.json {
                    return reporter.value(&serde_json::json!({ "sessions": sessions }));
                }
                if sessions.is_empty() {
                    reporter.text("no chat history");
                    return Ok(());
                }
                for session in sessions {
                    reporter.text(format!(
                        "{} {} {:?} {}",
                        session.id, session.provider, session.status, session.label
                    ));
                }
                Ok(())
            }
        }
        HistoryCommand::Get {
            session,
            from,
            limit,
        } => {
            let page = event_page(client, *session, *from, *limit).await?;
            reporter_page(reporter, &page)
        }
    }
}

pub(crate) fn reporter_page(reporter: &Reporter, page: &ChatEventPage) -> Result<(), CliError> {
    if reporter.json {
        return reporter.value(page);
    }
    reporter.text(format!(
        "session {} incarnation {}: {} event(s){}",
        page.session,
        page.incarnation,
        page.events.len(),
        if page.more { " (more)" } else { "" }
    ));
    for record in &page.events {
        reporter.text(format!(
            "{:>6} {:>13} {}",
            record.sequence,
            record.timestamp_ms,
            event_summary(&record.event)
        ));
    }
    Ok(())
}

fn event_summary(event: &ChatEvent) -> String {
    match event {
        ChatEvent::SessionReady(info) => format!("session ready {}", info.resume.native_id),
        ChatEvent::StateChanged(state) => format!("state {state:?}"),
        ChatEvent::UserMessage { text, .. } => format!("user: {}", excerpt(text)),
        ChatEvent::AssistantText { text, .. } => format!("assistant: {}", excerpt(text)),
        ChatEvent::StructuredOutput { json } => format!("structured: {}", excerpt(json)),
        ChatEvent::Thinking { delta, .. } => format!("thinking: {}", excerpt(delta)),
        ChatEvent::ToolCall(call) => format!("tool {}: {:?}", call.title, call.status),
        ChatEvent::ToolOutput { delta, .. } => format!("tool output: {}", excerpt(&delta.text)),
        ChatEvent::FileEdit { path, .. } => format!("edit {}", path.display()),
        ChatEvent::ApprovalRequest { id, tool, .. } => format!("approval {id} for {tool}"),
        ChatEvent::QuestionRequest { id, questions, .. } => {
            format!("question {id} ({} parts)", questions.len())
        }
        ChatEvent::RequestResolved { id } => format!("request {id} resolved"),
        ChatEvent::Subagent {
            parent_tool_id,
            events,
        } => {
            format!("subagent {parent_tool_id} ({} events)", events.len())
        }
        ChatEvent::TurnEnd { outcome, turns, .. } => {
            format!("turn end {outcome:?} after {turns} turn(s)")
        }
        ChatEvent::Error { code, message, .. } => format!("error {code}: {}", excerpt(message)),
        ChatEvent::Diagnostic { stream, output } => {
            format!("{stream}: {}", excerpt(&output.text))
        }
        ChatEvent::ProcessExited { code, expected } => {
            format!("process exited {code:?} expected={expected}")
        }
    }
}

fn excerpt(text: &str) -> String {
    let mut excerpt: String = text.chars().take(120).collect();
    if excerpt.len() < text.len() {
        excerpt.push('…');
    }
    excerpt.replace('\n', " ")
}
