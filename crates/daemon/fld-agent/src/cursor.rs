//! Cursor headless `stream-json` adapter.
//!
//! Cursor's print mode is one process per turn. `create-chat` allocates the
//! durable chat ID used for `--resume`; each prompt spawns a child and streams
//! JSONL until `result`. There is no stdin permission channel: interaction
//! queries are surfaced as diagnostics and never auto-approved. Read-only mode
//! uses Cursor's own `--mode plan`; `--force`/`--yolo` are rejected arguments.

use crate::{host, process, *};
use futures::StreamExt;
use serde_json::Value;
use std::{
    collections::HashMap,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    process::Child,
    sync::mpsc,
};
use tokio_util::codec::{FramedRead, LinesCodec};

const TURN_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) async fn launch(provider: ProviderSpec, config: SessionConfig) -> Result<Adapter> {
    let config = host::prepare(&provider, config).await?;
    if config.output_schema_json.is_some() {
        return Err(AdapterError::Unsupported(
            "Cursor stream-json has no structured-output flag".into(),
        ));
    }
    if config.instructions.is_some() {
        return Err(AdapterError::Unsupported(
            "Cursor stream-json has no system-prompt flag".into(),
        ));
    }
    if config.provider_config_json.is_some() {
        return Err(AdapterError::Unsupported(
            "Cursor stream-json has no settings-file flag".into(),
        ));
    }
    let session = match &config.resume {
        Some(target) => {
            host::uuid(&target.native_id, "Cursor")?;
            target.native_id.clone()
        }
        None => create_chat(&provider).await?,
    };
    let info = SessionInfo {
        id: config.session_id.unwrap_or_else(uuid::Uuid::new_v4),
        incarnation: uuid::Uuid::new_v4(),
        resume: ResumeTarget {
            provider_id: provider.id.clone(),
            adapter: provider.adapter,
            native_id: session.clone(),
            cwd: config.cwd.clone(),
        },
        model: config.model.clone(),
        resumed: config.resume.is_some(),
    };
    let (mut machine, events) = host::machine(provider, config, info);
    let state = machine.state.subscribe();
    let (commands, receiver) = mpsc::channel(32);
    machine.session_ready(session.clone())?;
    let info = machine.info.clone();
    tokio::spawn(run(machine, receiver, session));
    Ok(Adapter {
        info,
        events,
        commands,
        state,
    })
}

async fn create_chat(provider: &ProviderSpec) -> Result<String> {
    let output = host::capture(
        provider,
        &["create-chat"],
        host::CURSOR_HANDSHAKE_TIMEOUT,
    )
    .await
    .map_err(|error| match error {
        AdapterError::Timeout => {
            AdapterError::Timeout
        }
        other => AdapterError::Unsupported(format!("cursor create-chat handshake: {other}")),
    })?;
    let session = output
        .lines()
        .map(str::trim)
        .find(|line| host::uuid(line, "Cursor").is_ok())
        .ok_or_else(|| AdapterError::Protocol("create-chat returned no chat ID".into()))?;
    Ok(session.to_owned())
}

enum TurnEnd {
    Done,
    Cancelled,
    Failed,
    Shutdown,
}

async fn run(mut machine: Machine, mut commands: mpsc::Receiver<Command>, session: String) {
    loop {
        let command = match commands.recv().await {
            Some(command) => command,
            None => {
                machine.state.send_replace(SessionState::Closed);
                return;
            }
        };
        match command.operation {
            Operation::Send(text) => {
                if !matches!(machine.state.borrow().clone(), SessionState::Idle) {
                    let _ = command.reply.send(Err(AdapterError::Busy));
                    continue;
                }
                if let Err(error) = process::valid_text(&text) {
                    let _ = command.reply.send(Err(error));
                    continue;
                }
                match start_turn(&machine, &session, &text) {
                    Ok(child) => {
                        let _ = command.reply.send(Ok(()));
                        let turn_id = uuid::Uuid::new_v4().to_string();
                        machine.active_turn = Some(turn_id);
                        machine.turn_started = Some(Instant::now());
                        let _ = machine.user_event(
                            machine.active_turn.clone().unwrap_or_default(),
                            text,
                        );
                        let _ = machine.set_state(SessionState::Running);
                        match drive(&mut machine, &mut commands, child, &session).await {
                            TurnEnd::Done | TurnEnd::Cancelled => {
                                if machine.state.borrow().clone() == SessionState::Failed {
                                    break;
                                }
                            }
                            TurnEnd::Failed => break,
                            TurnEnd::Shutdown => {
                                machine.state.send_replace(SessionState::Closed);
                                let _ =
                                    machine.emit(ChatEvent::StateChanged(SessionState::Closed));
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = command.reply.send(Err(error));
                    }
                }
            }
            Operation::Cancel => {
                let _ = command
                    .reply
                    .send(Err(AdapterError::NoActiveTurn));
            }
            Operation::Steer(_) => {
                let _ = command.reply.send(Err(AdapterError::Unsupported(
                    "Cursor stream-json has no mid-turn steer".into(),
                )));
            }
            Operation::Approve(_, _) | Operation::Question(_, _) => {
                let _ = command.reply.send(Err(AdapterError::Unsupported(
                    "Cursor headless has no answerable permission channel".into(),
                )));
            }
            Operation::Shutdown => {
                machine.state.send_replace(SessionState::Closed);
                let _ = machine.emit(ChatEvent::StateChanged(SessionState::Closed));
                let _ = command.reply.send(Ok(()));
                return;
            }
        }
    }
    if !matches!(machine.state.borrow().clone(), SessionState::Failed { .. }) {
        machine.state.send_replace(SessionState::Failed {
            message: "cursor turn failed".into(),
        });
    }
}

fn start_turn(machine: &Machine, session: &str, text: &str) -> Result<Child> {
    let mut command = host::command(&machine.provider);
    command
        .current_dir(&machine.config.cwd)
        .stdin(Stdio::null())
        .args([
            "-p",
            "--output-format",
            "stream-json",
            "--stream-partial-output",
            "--resume",
            session,
        ]);
    if machine.config.permission_mode == PermissionMode::ReadOnly {
        command.args(["--mode", "plan"]);
    }
    if let Some(model) = &machine.config.model {
        command.arg("--model").arg(model);
    }
    command.arg("--").arg(text);
    command.spawn().map_err(AdapterError::Io)
}

async fn drive(
    machine: &mut Machine,
    commands: &mut mpsc::Receiver<Command>,
    mut child: Child,
    session: &str,
) -> TurnEnd {
    let guard = host::Guard::new(&child);
    let Some(stdout) = child.stdout.take() else {
        return fail(machine, "missing stdout".into());
    };
    let Some(stderr) = child.stderr.take() else {
        return fail(machine, "missing stderr".into());
    };
    let mut output = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_WIRE_LINE));
    let mut errors = FramedRead::new(stderr, LinesCodec::new_with_max_length(MAX_WIRE_LINE));
    let mut pending_text = String::new();
    let mut emitted = false;
    let mut tools: HashMap<String, ToolCall> = HashMap::new();
    let mut result_seen = false;
    let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
    while !result_seen {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command { operation: Operation::Cancel, reply }) => {
                    guard.kill();
                    let _ = child.wait().await;
                    let _ = machine.end_turn(None, 1, TurnOutcome::Cancelled);
                    let _ = machine.emit(ChatEvent::ProcessExited { code: None, expected: true });
                    let _ = reply.send(Ok(()));
                    return TurnEnd::Cancelled;
                }
                Some(Command { operation: Operation::Shutdown, reply }) => {
                    guard.kill();
                    let _ = child.wait().await;
                    let _ = machine.end_turn(None, 1, TurnOutcome::Cancelled);
                    let _ = machine.emit(ChatEvent::ProcessExited { code: None, expected: true });
                    let _ = reply.send(Ok(()));
                    return TurnEnd::Shutdown;
                }
                Some(Command { operation, reply }) => {
                    let error = match operation {
                        Operation::Send(_) => AdapterError::Busy,
                        Operation::Steer(_) => AdapterError::Unsupported(
                            "Cursor stream-json has no mid-turn steer".into(),
                        ),
                        Operation::Approve(_, _) | Operation::Question(_, _) => AdapterError::Unsupported(
                            "Cursor headless has no answerable permission channel".into(),
                        ),
                        Operation::Cancel | Operation::Shutdown => unreachable!(),
                    };
                    let _ = reply.send(Err(error));
                }
                None => {
                    guard.kill();
                    let _ = child.wait().await;
                    return TurnEnd::Shutdown;
                }
            },
            line = output.next() => {
                match line {
                    Some(Ok(line)) => {
                        let value: Value = match serde_json::from_str(&line) {
                            Ok(value) => value,
                            Err(error) => {
                                guard.kill();
                                let _ = child.wait().await;
                                return fail(machine, format!("malformed provider JSON: {error}"));
                            }
                        };
                        match cursor_event(machine, &value, session, &mut pending_text, &mut emitted, &mut tools) {
                            Ok(Some(outcome)) => {
                                let _ = machine.end_turn(
                                    value["duration_ms"].as_u64(),
                                    1,
                                    outcome,
                                );
                                result_seen = true;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                guard.kill();
                                let _ = child.wait().await;
                                return fail(machine, error.to_string());
                            }
                        }
                    }
                    Some(Err(error)) => {
                        guard.kill();
                        let _ = child.wait().await;
                        return fail(machine, format!("provider stdout: {error}"));
                    }
                    None => {
                        let status = child.wait().await.ok();
                        let code = status.as_ref().and_then(std::process::ExitStatus::code);
                        let _ = machine.emit(ChatEvent::ProcessExited { code, expected: false });
                        return fail(machine, format!("Cursor exited without a result ({status:?})"));
                    }
                }
            }
            line = errors.next() => {
                match line {
                    Some(Ok(line)) => {
                        let _ = machine.emit(ChatEvent::Diagnostic { stream: "stderr".into(), output: bounded(line) });
                    }
                    Some(Err(error)) => {
                        guard.kill();
                        let _ = child.wait().await;
                        return fail(machine, format!("provider stderr: {error}"));
                    }
                    None => {}
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                guard.kill();
                let _ = child.wait().await;
                return fail(machine, "Cursor turn timed out".into());
            }
        }
    }
    // `result` is the last protocol record. Reap the child without treating a
    // normal exit as a dead session.
    let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            guard.kill();
            child.wait().await.ok()
        }
    };
    let _ = machine.emit(ChatEvent::ProcessExited {
        code: status.as_ref().and_then(std::process::ExitStatus::code),
        expected: true,
    });
    let _ = machine.set_state(SessionState::Idle);
    TurnEnd::Done
}

fn fail(machine: &mut Machine, message: String) -> TurnEnd {
    if machine.active_turn.is_some() {
        let _ = machine.end_turn(None, 1, TurnOutcome::Failed);
    }
    let _ = machine.emit(ChatEvent::Error {
        code: "adapter_failed".into(),
        message: bounded(message).text,
        fatal: true,
    });
    let _ = machine.set_state(SessionState::Failed {
        message: "cursor turn failed".into(),
    });
    TurnEnd::Failed
}

fn cursor_event(
    machine: &mut Machine,
    value: &Value,
    session: &str,
    pending_text: &mut String,
    emitted: &mut bool,
    tools: &mut HashMap<String, ToolCall>,
) -> Result<Option<TurnOutcome>> {
    match value["type"].as_str().unwrap_or_default() {
        "system" if value["subtype"] == "init" => {
            let native = value["session_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| AdapterError::Protocol("Cursor init lacks session_id".into()))?;
            if native != session {
                return Err(AdapterError::InvalidResume(
                    "Cursor resumed a different chat ID".into(),
                ));
            }
            if let Some(model) = value["model"].as_str() {
                machine.info.model = Some(model.into());
            }
        }
        "system" => {
            machine.emit(ChatEvent::Diagnostic {
                stream: "cursor".into(),
                output: bounded(value.to_string()),
            })?;
        }
        "user" => {}
        "assistant" => {
            let text = host::text_content(&value["message"]["content"]);
            if text.is_empty() {
                return Ok(None);
            }
            let id = format!(
                "{}:assistant",
                machine.active_turn.as_deref().unwrap_or("cursor")
            );
            if !pending_text.is_empty() && text == *pending_text {
                machine.emit(ChatEvent::AssistantText {
                    id,
                    update: TextUpdate::Final,
                    text: text.clone(),
                })?;
                pending_text.clear();
            } else {
                pending_text.push_str(&text);
                machine.emit(ChatEvent::AssistantText {
                    id,
                    update: TextUpdate::Delta,
                    text,
                })?;
            }
            *emitted = true;
        }
        "thinking" => {
            let text = string(value, "text");
            if !text.is_empty() {
                machine.emit(ChatEvent::Thinking {
                    id: format!(
                        "{}:thinking",
                        machine.active_turn.as_deref().unwrap_or("cursor")
                    ),
                    delta: text,
                })?;
            }
        }
        "tool_call" => {
            let id = value["call_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| AdapterError::Protocol("Cursor tool call lacks call_id".into()))?
                .to_owned();
            let raw = &value["tool_call"];
            let case = raw["tool"]["case"]
                .as_str()
                .or_else(|| raw["tool"].as_object().and_then(|tool| {
                    tool.keys().find(|key| key.as_str() != "case")
                }).map(String::as_str))
                .or_else(|| raw["case"].as_str())
                .unwrap_or("tool");
            let payload = raw["tool"]["value"]
                .as_object()
                .map(|_| &raw["tool"]["value"])
                .or_else(|| raw["tool"].get(case))
                .unwrap_or(raw);
            let lower = case.to_ascii_lowercase();
            let kind = if lower.contains("shell") || lower.contains("terminal") || lower.contains("command") {
                ToolKind::Exec
            } else if lower.contains("edit") || lower.contains("diff") || lower.contains("write") {
                ToolKind::Edit
            } else if lower.contains("read") {
                ToolKind::Read
            } else if lower.contains("grep") || lower.contains("glob") || lower.contains("search") {
                ToolKind::Search
            } else if lower.contains("web") || lower.contains("fetch") {
                ToolKind::Web
            } else if lower.contains("mcp") {
                ToolKind::Mcp
            } else {
                ToolKind::Other
            };
            let title = raw["title"]
                .as_str()
                .or_else(|| payload["description"].as_str())
                .or_else(|| payload["command"].as_str())
                .unwrap_or(case)
                .to_owned();
            let input = payload["args"]
                .as_object()
                .map(|_| &payload["args"])
                .unwrap_or(payload);
            let completed = value["subtype"] == "completed";
            let error = payload["isError"].as_bool().unwrap_or(false)
                || !payload["error"].is_null();
            let output = host::text_content(&payload["result"]);
            let output = if output.is_empty() {
                host::text_content(&payload["output"])
            } else {
                output
            };
            let mut tool = tools.remove(&id).unwrap_or_else(|| ToolCall {
                id: id.clone(),
                kind,
                title: title.clone(),
                input_summary: bounded(input.to_string()),
                status: ToolStatus::Running,
                output: None,
                diff: None,
            });
            tool.status = if completed {
                if error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Completed
                }
            } else {
                ToolStatus::Running
            };
            if !output.is_empty() {
                tool.output = Some(bounded(output));
            }
            if let Some(path) = payload["path"].as_str().or_else(|| payload["filePath"].as_str()) {
                let patch = payload["diff"]
                    .as_str()
                    .or_else(|| payload["patch"].as_str())
                    .unwrap_or_default();
                if !patch.is_empty() {
                    tool.diff = Some(bounded(patch));
                    machine.emit(ChatEvent::FileEdit {
                        path: path.into(),
                        unified_diff: bounded(patch),
                    })?;
                }
            }
            tools.insert(id, tool.clone());
            machine.emit(ChatEvent::ToolCall(tool))?;
        }
        "turnEnded" => {
            let usage = &value["usage"];
            machine.usage = Usage {
                input_tokens: usage["inputTokens"]
                    .as_u64()
                    .or_else(|| usage["input_tokens"].as_u64()),
                output_tokens: usage["outputTokens"]
                    .as_u64()
                    .or_else(|| usage["output_tokens"].as_u64()),
                cache_read_tokens: usage["cacheReadTokens"]
                    .as_u64()
                    .or_else(|| usage["cache_read_tokens"].as_u64()),
                cache_write_tokens: usage["cacheWriteTokens"]
                    .as_u64()
                    .or_else(|| usage["cache_write_tokens"].as_u64()),
                cost_usd: None,
            };
        }
        "result" => {
            let id = format!(
                "{}:assistant",
                machine.active_turn.as_deref().unwrap_or("cursor")
            );
            if !pending_text.is_empty() {
                machine.emit(ChatEvent::AssistantText {
                    id,
                    update: TextUpdate::Final,
                    text: pending_text.clone(),
                })?;
                pending_text.clear();
            } else if !*emitted && value["result"].as_str().is_some_and(|text| !text.is_empty()) {
                machine.emit(ChatEvent::AssistantText {
                    id,
                    update: TextUpdate::Final,
                    text: string(value, "result"),
                })?;
            }
            let error = value["is_error"].as_bool().unwrap_or(false)
                || value["subtype"].as_str().is_some_and(|subtype| subtype != "success");
            if error {
                machine.emit(ChatEvent::Error {
                    code: "provider_turn_failed".into(),
                    message: bounded(value.to_string()).text,
                    fatal: false,
                })?;
            }
            return Ok(Some(if error {
                TurnOutcome::Failed
            } else {
                TurnOutcome::Completed
            }));
        }
        "interaction_query" => {
            machine.emit(ChatEvent::Diagnostic {
                stream: "cursor-query".into(),
                output: bounded(value.to_string()),
            })?;
            machine.emit(ChatEvent::Error {
                code: "unanswerable_permission".into(),
                message: "Cursor headless resolves interaction queries itself; Flotilla never auto-approves them"
                    .into(),
                fatal: false,
            })?;
        }
        "retry" => machine.emit(ChatEvent::Diagnostic {
            stream: "cursor".into(),
            output: bounded(value.to_string()),
        })?,
        other => machine.emit(ChatEvent::Diagnostic {
            stream: "cursor".into(),
            output: bounded(format!("unhandled Cursor event {other}")),
        })?,
    }
    Ok(None)
}

pub(crate) async fn discover(
    provider: &ProviderSpec,
    result: &mut ProviderDiscovery,
) -> Result<()> {
    let version = host::capture(provider, &["--version"], host::PROBE_TIMEOUT)
        .await?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let help = host::capture(provider, &["--help"], host::PROBE_TIMEOUT).await?;
    for required in [
        "--output-format",
        "stream-json",
        "--stream-partial-output",
        "--resume",
        "create-chat",
        "--mode",
    ] {
        if !help.contains(required) {
            return Err(AdapterError::Unsupported(format!(
                "installed CLI help lacks {required}"
            )));
        }
    }
    create_chat(provider).await?;
    result.version = Some(version);
    result.capabilities = Capabilities {
        chat: true,
        resume: true,
        steer: false,
        approvals: false,
        questions: false,
        partial_messages: true,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{host, process};

    const SESSION: &str = "11111111-1111-4111-8111-111111111111";

    fn machine() -> (Machine, mpsc::Receiver<EventRecord>) {
        let provider = ProviderSpec {
            id: "cursor".into(),
            name: "Cursor".into(),
            command: "cursor-agent".into(),
            args: Vec::new(),
            env: Default::default(),
            adapter: AdapterKind::CursorStreamJson,
            resume: ResumeStrategy::CursorChat,
            hook_support: false,
        };
        let config = SessionConfig {
            cwd: "/tmp".into(),
            model: None,
            permission_mode: PermissionMode::Ask,
            instructions: None,
            session_id: None,
            provider_config_json: None,
            output_schema_json: None,
            resume: None,
        };
        let info = SessionInfo {
            id: uuid::Uuid::nil(),
            incarnation: uuid::Uuid::nil(),
            resume: ResumeTarget {
                provider_id: "cursor".into(),
                adapter: AdapterKind::CursorStreamJson,
                native_id: SESSION.into(),
                cwd: config.cwd.clone(),
            },
            model: None,
            resumed: false,
        };
        host::machine(provider, config, info)
    }

    #[test]
    fn recorded_turn_shape_normalizes_deltas_tools_and_result() {
        let (mut machine, mut events) = machine();
        machine.active_turn = Some("turn".into());
        let mut pending = String::new();
        let mut emitted = false;
        let mut tools = HashMap::new();
        let mut outcome = None;
        for line in include_str!("../tests/fixtures/cursor-turn.synthetic.jsonl")
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            let value: Value = serde_json::from_str(line).unwrap();
            if let Some(result) = cursor_event(
                &mut machine,
                &value,
                SESSION,
                &mut pending,
                &mut emitted,
                &mut tools,
            )
            .unwrap()
            {
                outcome = Some(result);
            }
        }
        assert_eq!(outcome, Some(TurnOutcome::Completed));
        assert!(pending.is_empty());
        let mut records = Vec::new();
        while let Ok(record) = events.try_recv() {
            records.push(record);
        }
        assert!(records.iter().any(|record| matches!(
            &record.event,
            ChatEvent::AssistantText { update: TextUpdate::Delta, text, .. } if text == "Hel"
        )));
        assert!(records.iter().any(|record| matches!(
            &record.event,
            ChatEvent::AssistantText { update: TextUpdate::Final, text, .. } if text == "Hello"
        )));
        assert!(records.iter().any(|record| matches!(
            &record.event,
            ChatEvent::Thinking { delta, .. } if delta == "plan"
        )));
        assert!(records.iter().any(|record| matches!(
            &record.event,
            ChatEvent::ToolCall(call) if call.id == "c1" && call.status == ToolStatus::Completed && call.output.as_ref().is_some_and(|output| output.text == "/tmp")
        )));
        assert_eq!(machine.usage.input_tokens, Some(12));
        assert_eq!(machine.info.model.as_deref(), Some("sonnet-4"));
    }

    #[test]
    fn session_identity_mismatch_is_an_invalid_resume() {
        let (mut machine, _events) = machine();
        let mut pending = String::new();
        let mut emitted = false;
        let mut tools = HashMap::new();
        assert!(matches!(
            cursor_event(
                &mut machine,
                &serde_json::json!({
                    "type": "system", "subtype": "init",
                    "session_id": "22222222-2222-4222-8222-222222222222"
                }),
                SESSION,
                &mut pending,
                &mut emitted,
                &mut tools
            ),
            Err(AdapterError::InvalidResume(_))
        ));
    }

    #[test]
    fn interaction_queries_are_never_turned_into_approvals() {
        let (mut machine, mut events) = machine();
        machine.active_turn = Some("turn".into());
        let mut pending = String::new();
        let mut emitted = false;
        let mut tools = HashMap::new();
        cursor_event(
            &mut machine,
            &serde_json::json!({
                "type": "interaction_query", "subtype": "request",
                "query_type": "shellCommand", "session_id": SESSION
            }),
            SESSION,
            &mut pending,
            &mut emitted,
            &mut tools,
        )
        .unwrap();
        let mut error = None;
        let mut approval = false;
        while let Ok(record) = events.try_recv() {
            match record.event {
                ChatEvent::Error { code, fatal, .. } => {
                    error = Some((code, fatal));
                }
                ChatEvent::ApprovalRequest { .. } => approval = true,
                _ => {}
            }
        }
        assert_eq!(
            error,
            Some(("unanswerable_permission".to_string(), false))
        );
        assert!(!approval);
    }

    #[test]
    fn resume_identifiers_must_be_bounded_uuids() {
        assert!(host::uuid(SESSION, "Cursor").is_ok());
        for invalid in ["", "../chat", "not-a-uuid", "1111\n"] {
            assert!(host::uuid(invalid, "Cursor").is_err());
        }
    }

    #[test]
    fn malformed_json_and_oversized_text_fail_closed() {
        assert!(process::parse_line(Some(Ok("{broken".into()))).is_err());
        assert!(bounded("€".repeat(MAX_TEXT * 2)).truncated);
    }
}
