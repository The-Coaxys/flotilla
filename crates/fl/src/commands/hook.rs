//! `fl hook <event>` — ingest a provider hook as a durable chat fact.
//!
//! Providers run this as a hook subprocess. It speaks to the daemon over the
//! same user-only Unix socket as every other `fl` command: the socket's 0600
//! mode is the authentication, and no new network surface is opened. The
//! provider's JSON is normalized here; the daemon only accepts typed fields.
use std::io::{IsTerminal, Read};

use fl_model::chat::MAX_CHAT_TEXT;
use fl_model::coordination::HookEvent;
use fl_proto::{Request, Response};

use super::{caller, session_from, unexpected};
use crate::cli::Cli;
use crate::client::DaemonClient;
use crate::error::CliError;
use crate::output::Reporter;

/// A hook payload larger than this is not a hook; refuse it instead of
/// spending unbounded memory on provider-controlled input.
const MAX_HOOK_PAYLOAD: usize = 1024 * 1024;

pub async fn run(
    client: &mut DaemonClient,
    reporter: &Reporter,
    cli: &Cli,
    event: &str,
    explicit_session: Option<uuid::Uuid>,
    payload: Option<&str>,
) -> Result<(), CliError> {
    if event.trim().is_empty() {
        return Err(CliError::invalid("a hook event needs a name"));
    }
    let text = read_payload(payload)?;
    let value: serde_json::Value = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text)
            .map_err(|error| CliError::invalid(format!("hook payload is not JSON: {error}")))?
    };
    if !value.is_null() && !value.is_object() {
        return Err(CliError::invalid("a hook payload must be a JSON object"));
    }
    let hook = HookEvent {
        name: event.to_owned(),
        tool: field(&value, &["tool_name", "tool"]),
        tool_id: field(&value, &["tool_use_id", "tool_call_id", "tool_id"]),
        detail: detail(&value),
        success: success(&value),
    };
    let caller = caller(client, cli).await?;
    let session = session_from(explicit_session, &caller)?;
    match client
        .rpc(Request::HookEvent {
            session,
            event: hook,
        })
        .await?
    {
        Response::HookRecorded { session, sequence } => {
            if reporter.json {
                return reporter.value(&serde_json::json!({
                    "session": session,
                    "sequence": sequence,
                }));
            }
            reporter.text(format!("hook recorded for session {session} at {sequence}"));
            Ok(())
        }
        other => Err(unexpected("hook record", &other)),
    }
}

fn read_payload(payload: Option<&str>) -> Result<String, CliError> {
    match payload {
        Some("-") => read_stdin(),
        Some(value) => Ok(value.to_owned()),
        None => {
            if std::io::stdin().is_terminal() {
                Ok(String::new())
            } else {
                read_stdin()
            }
        }
    }
}

fn read_stdin() -> Result<String, CliError> {
    let mut text = String::new();
    std::io::stdin()
        .take((MAX_HOOK_PAYLOAD + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|error| CliError::invalid(format!("reading hook payload: {error}")))?;
    if text.len() > MAX_HOOK_PAYLOAD {
        return Err(CliError::invalid(
            "hook payload exceeds 1 MiB; hooks carry small structured events",
        ));
    }
    Ok(text)
}

fn field(value: &serde_json::Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .filter(|text| !text.trim().is_empty())
}

/// The most useful bounded description the provider supplied.
fn detail(value: &serde_json::Value) -> String {
    let raw = value
        .get("tool_input")
        .filter(|input| !input.is_null())
        .map(|input| input.to_string())
        .or_else(|| {
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            value
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| value.to_string());
    truncate(&raw, MAX_CHAT_TEXT)
}

fn success(value: &serde_json::Value) -> Option<bool> {
    value
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            value
                .get("tool_response")
                .and_then(|response| response.get("is_error"))
                .and_then(serde_json::Value::as_bool)
                .map(|error| !error)
        })
        .or_else(|| value.get("error").map(|_| false))
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
