use crate::{
    process::{Interaction, Machine, PendingKind, Request, checked_answers, nonempty, valid_text},
    *,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
    sync::oneshot,
};

pub(crate) const WIRE_ARGS: &[&str] = &[
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--include-partial-messages",
    "--verbose",
];

pub(crate) fn configure(
    command: &mut Command,
    config: &SessionConfig,
    info: &SessionInfo,
    settings: &Value,
) -> Result<()> {
    if settings.get("permissions").is_some() || settings.get("permissionMode").is_some() {
        return Err(AdapterError::InvalidConfig(
            "permission settings are owned by the adapter; use permission_mode".into(),
        ));
    }
    command.args(WIRE_ARGS).args([
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
        if config.permission_mode == PermissionMode::ReadOnly {
            "plan"
        } else {
            "manual"
        },
    ]);
    if info.resumed {
        command.arg("--resume").arg(&info.resume.native_id);
    } else {
        command.arg("--session-id").arg(&info.resume.native_id);
    }
    if let Some(model) = &config.model {
        command.arg("--model").arg(model);
    }
    if let Some(instructions) = &config.instructions {
        command.arg("--append-system-prompt").arg(instructions);
    }
    if let Some(schema) = &config.output_schema_json {
        command.arg("--json-schema").arg(schema);
    }
    if config.provider_config_json.is_some() {
        command.arg("--settings").arg(settings.to_string());
    }
    Ok(())
}

/// Claude's durable project directory uses the absolute cwd with non-alphanumeric
/// characters replaced by '-'. Never pass a name, prefix, or "latest" to --resume.
/// Validate the exact transcript and its cwd before letting the CLI load it.
pub(crate) async fn validate_resume(
    provider: &ProviderSpec,
    target: &ResumeTarget,
    cwd: &Path,
) -> Result<()> {
    uuid::Uuid::parse_str(&target.native_id)
        .map_err(|_| AdapterError::InvalidResume("Claude session ID must be a UUID".into()))?;
    let root = provider
        .env
        .get("CLAUDE_CONFIG_DIR")
        .cloned()
        .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok())
        .map(PathBuf::from)
        .or_else(|| {
            provider
                .env
                .get("HOME")
                .cloned()
                .or_else(|| std::env::var("HOME").ok())
                .map(|home| PathBuf::from(home).join(".claude"))
        })
        .ok_or_else(|| {
            AdapterError::InvalidResume("cannot locate Claude transcript directory".into())
        })?;
    let encoded: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = root
        .join("projects")
        .join(encoded)
        .join(format!("{}.jsonl", target.native_id));
    // An inaccessible cwd is an environment failure, and so is an I/O error.
    // A transcript file that no longer exists is a semantic mismatch: that
    // session is gone, so the caller may archive the chat and start fresh.
    tokio::fs::metadata(cwd)
        .await
        .map_err(|e| AdapterError::Protocol(format!("resume cwd: {e}")))?;
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(AdapterError::InvalidResume(
                "the Claude transcript for this session no longer exists".into(),
            ));
        }
        Err(e) => return Err(AdapterError::Protocol(format!("Claude transcript: {e}"))),
    };
    let mut lines = BufReader::new(file.take((MAX_WIRE_LINE * 4) as u64)).lines();
    while let Some(line) = lines.next_line().await? {
        if line.len() > MAX_WIRE_LINE {
            return Err(AdapterError::InvalidResume(
                "Claude transcript line exceeds limit".into(),
            ));
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value["sessionId"] == target.native_id
            && value.get("cwd").and_then(Value::as_str) == cwd.to_str()
        {
            return Ok(());
        }
    }
    Err(AdapterError::InvalidResume(
        "Claude transcript does not contain the requested session and cwd in its bounded header"
            .into(),
    ))
}

fn control_success(id: &Value, response: Value) -> Value {
    json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":response}})
}
fn control_error(id: &Value, error: &str) -> Value {
    json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":error}})
}
fn questions(input: &Value) -> Result<Vec<Question>> {
    let array = input["questions"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 32)
        .ok_or_else(|| AdapterError::Protocol("invalid Claude questions".into()))?;
    array
        .iter()
        .map(|q| {
            let text = nonempty(q, "question")?;
            Ok(Question {
                id: text.clone(),
                text,
                options: q["options"]
                    .as_array()
                    .map(|options| {
                        options
                            .iter()
                            .map(|o| QuestionOption {
                                label: string(o, "label"),
                                description: string(o, "description"),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                multi: q["multiSelect"].as_bool().unwrap_or(false),
                free_text: true,
                secret: false,
            })
        })
        .collect()
}

impl Machine {
    pub(crate) fn claude_operation(
        &mut self,
        operation: Operation,
        reply: oneshot::Sender<Result<()>>,
    ) -> Result<Vec<Value>> {
        // Validate before consuming the response channel or the one-shot request.
        let validation = match &operation {
            Operation::Send(text) => valid_text(text).and_then(|_| {
                if self.active_turn.is_some() {
                    Err(AdapterError::Busy)
                } else {
                    Ok(())
                }
            }),
            Operation::Steer(text) => valid_text(text).and_then(|_| {
                if self.active_turn.is_none() {
                    Err(AdapterError::NoActiveTurn)
                } else {
                    Ok(())
                }
            }),
            Operation::Cancel => {
                if self.active_turn.is_none() {
                    Err(AdapterError::NoActiveTurn)
                } else {
                    Ok(())
                }
            }
            Operation::Approve(id, decision) => match self.requests.get(id) {
                Some(Request {
                    interaction: Interaction::Claude { questions, .. },
                    ..
                }) if questions.is_empty() || *decision != ApprovalDecision::AllowOnce => Ok(()),
                _ => Err(AdapterError::UnknownRequest(id.clone())),
            },
            Operation::Question(id, response) => match self.requests.get(id) {
                Some(Request {
                    interaction: Interaction::Claude { questions, .. },
                    ..
                }) if !questions.is_empty() => checked_answers(questions, response),
                _ => Err(AdapterError::UnknownRequest(id.clone())),
            },
            Operation::Shutdown => unreachable!("shutdown is handled by runtime"),
        };
        if let Err(error) = validation {
            let _ = reply.send(Err(error));
            return Ok(Vec::new());
        }
        match operation {
            Operation::Send(text) | Operation::Steer(text) => {
                let id = uuid::Uuid::new_v4().to_string();
                if self.active_turn.is_none() {
                    self.active_turn = Some(id.clone());
                    self.turn_started = Some(Instant::now());
                }
                self.user_event(id.clone(), text.clone())?;
                self.set_state(SessionState::Running)?;
                self.delivered(reply);
                Ok(vec![
                    json!({"type":"user","uuid":id,"session_id":self.info.resume.native_id,"parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"text","text":text}]}}),
                ])
            }
            Operation::Cancel => {
                self.clear_requests()?;
                Ok(vec![self.rpc(
                    PendingKind::Control,
                    "interrupt",
                    json!({"subtype":"interrupt"}),
                    Some(reply),
                )?])
            }
            Operation::Approve(id, decision) => {
                let request = self
                    .requests
                    .remove(&id)
                    .ok_or_else(|| AdapterError::UnknownRequest(id.clone()))?;
                let Interaction::Claude { input, .. } = request.interaction else {
                    unreachable!()
                };
                let response = match decision {
                    ApprovalDecision::AllowOnce => json!({"behavior":"allow","updatedInput":input}),
                    ApprovalDecision::Deny => {
                        json!({"behavior":"deny","message":"Denied by the user"})
                    }
                    ApprovalDecision::Cancel => {
                        json!({"behavior":"deny","message":"Cancelled by the user","interrupt":true})
                    }
                };
                self.emit(ChatEvent::RequestResolved { id })?;
                self.set_state(if self.requests.is_empty() {
                    SessionState::Running
                } else {
                    SessionState::AwaitingInput
                })?;
                self.delivered(reply);
                Ok(vec![control_success(&request.wire_id, response)])
            }
            Operation::Question(id, response) => {
                let request = self
                    .requests
                    .remove(&id)
                    .ok_or_else(|| AdapterError::UnknownRequest(id.clone()))?;
                let Interaction::Claude { mut input, .. } = request.interaction else {
                    unreachable!()
                };
                let answers: serde_json::Map<String, Value> = response
                    .answers
                    .into_iter()
                    .map(|(id, answers)| (id, Value::String(answers.join(", "))))
                    .collect();
                input["answers"] = Value::Object(answers);
                self.emit(ChatEvent::RequestResolved { id })?;
                self.set_state(if self.requests.is_empty() {
                    SessionState::Running
                } else {
                    SessionState::AwaitingInput
                })?;
                self.delivered(reply);
                Ok(vec![control_success(
                    &request.wire_id,
                    json!({"behavior":"allow","updatedInput":input}),
                )])
            }
            Operation::Shutdown => unreachable!(),
        }
    }

    pub(crate) fn claude_incoming(&mut self, value: Value) -> Result<Vec<Value>> {
        let parent = string(&value, "parent_tool_use_id");
        match value["type"]
            .as_str()
            .ok_or_else(|| AdapterError::Protocol("Claude message lacks type".into()))?
        {
            "control_response" => {
                let response = &value["response"];
                let id = nonempty(response, "request_id")?;
                let Some(pending) = self.pending.remove(&id) else {
                    return Err(AdapterError::Protocol(
                        "unmatched Claude control response".into(),
                    ));
                };
                if response["subtype"] != "success" {
                    let message = bounded(response.to_string()).text;
                    if let Some(reply) = pending.reply {
                        let _ = reply.send(Err(AdapterError::Rejected(message.clone())));
                    }
                    return Err(AdapterError::Unsupported(message));
                }
                match pending.kind {
                    PendingKind::Initialize => {
                        self.session_ready(self.info.resume.native_id.clone())?
                    }
                    PendingKind::Control => {}
                    _ => {
                        return Err(AdapterError::Protocol(
                            "unexpected Claude control response kind".into(),
                        ));
                    }
                }
                if let Some(reply) = pending.reply {
                    let _ = reply.send(Ok(()));
                }
            }
            "control_request" => {
                let wire_id = value
                    .get("request_id")
                    .filter(|v| v.is_string())
                    .ok_or_else(|| AdapterError::Protocol("missing Claude request ID".into()))?
                    .clone();
                let request = &value["request"];
                if request["subtype"] != "can_use_tool" {
                    self.emit(ChatEvent::Error {
                        code: "unsupported_request".into(),
                        message: format!(
                            "unsupported Claude control request {}",
                            string(request, "subtype")
                        ),
                        fatal: false,
                    })?;
                    return Ok(vec![control_error(
                        &wire_id,
                        "Flotilla does not implement this control request",
                    )]);
                }
                let tool = nonempty(request, "tool_name")?;
                let input = request
                    .get("input")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| {
                        AdapterError::Protocol("Claude permission request lacks input".into())
                    })?
                    .clone();
                let questions = if tool == "AskUserQuestion" {
                    questions(&input)?
                } else {
                    Vec::new()
                };
                if self.config.permission_mode == PermissionMode::ReadOnly
                    && tool_kind(&tool) != ToolKind::Read
                    && !matches!(tool_kind(&tool), ToolKind::Search | ToolKind::Web)
                    && questions.is_empty()
                {
                    self.emit(ChatEvent::Error {
                        code: "permission_denied".into(),
                        message: format!("{tool} denied in read-only mode"),
                        fatal: false,
                    })?;
                    return Ok(vec![control_success(
                        &wire_id,
                        json!({"behavior":"deny","message":"Flotilla read-only mode does not grant this tool"}),
                    )]);
                }
                let id = self.request_id(
                    wire_id,
                    Interaction::Claude {
                        input,
                        questions: questions.clone(),
                    },
                )?;
                if questions.is_empty() {
                    self.emit(ChatEvent::ApprovalRequest {
                        id,
                        tool,
                        details: bounded(request.to_string()),
                        options: vec![
                            ApprovalDecision::AllowOnce,
                            ApprovalDecision::Deny,
                            ApprovalDecision::Cancel,
                        ],
                    })?;
                } else {
                    self.emit(ChatEvent::QuestionRequest {
                        id,
                        questions,
                        schema_json: None,
                    })?;
                }
                self.set_state(SessionState::AwaitingInput)?;
            }
            "control_cancel_request" => {
                let wire = &value["request_id"];
                if let Some(id) = self
                    .requests
                    .iter()
                    .find(|(_, r)| r.wire_id == *wire)
                    .map(|(id, _)| id.clone())
                {
                    self.requests.remove(&id);
                    self.emit(ChatEvent::RequestResolved { id })?;
                    self.set_state(if self.requests.is_empty() {
                        SessionState::Running
                    } else {
                        SessionState::AwaitingInput
                    })?;
                }
            }
            "system" => {
                if value["subtype"] == "init" {
                    if value["session_id"] != self.info.resume.native_id {
                        return Err(AdapterError::InvalidResume(
                            "Claude returned a different native session ID".into(),
                        ));
                    }
                    if let Some(model) = value["model"].as_str() {
                        self.info.model = Some(model.into());
                    }
                }
            }
            "stream_event" => self.claude_partial(&value["event"], &parent)?,
            "assistant" => self.claude_assistant(&value, &parent)?,
            "user" => {
                if let Some(content) = value["message"]["content"].as_array() {
                    for block in content {
                        if block["type"] == "tool_result" {
                            let id = nonempty(block, "tool_use_id")?;
                            let mut tool = self.tools.remove(&id).unwrap_or_else(|| ToolCall {
                                id: id.clone(),
                                kind: ToolKind::Other,
                                title: "Tool result".into(),
                                input_summary: bounded(""),
                                status: ToolStatus::Running,
                                output: None,
                                diff: None,
                            });
                            tool.status = if block["is_error"].as_bool().unwrap_or(false) {
                                ToolStatus::Failed
                            } else {
                                ToolStatus::Completed
                            };
                            let output = block
                                .get("content")
                                .map(|v| {
                                    v.as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| v.to_string())
                                })
                                .unwrap_or_default();
                            tool.output = Some(bounded(output));
                            self.scoped(ChatEvent::ToolCall(tool), &parent)?;
                        }
                    }
                }
                let output = &value["tool_use_result"];
                if output["staged"] != true
                    && let Some(path) = output["filePath"].as_str()
                {
                    let patch = claude_patch(output, path);
                    if let Some(patch) = patch {
                        self.scoped(
                            ChatEvent::FileEdit {
                                path: path.into(),
                                unified_diff: bounded(patch),
                            },
                            &parent,
                        )?;
                    }
                }
            }
            "result" => {
                let error = value["is_error"].as_bool().unwrap_or(false)
                    || value["subtype"].as_str().is_some_and(|s| s != "success");
                let reason = string(&value, "terminal_reason");
                let cancelled = reason.starts_with("aborted")
                    || value["subtype"] == "error_during_execution" && reason.contains("interrupt");
                if let Some(output) = value.get("structured_output").filter(|v| !v.is_null()) {
                    self.emit(ChatEvent::StructuredOutput {
                        json: output.to_string(),
                    })?;
                    self.structured_seen = true;
                }
                let text = string(&value, "result");
                if !self.final_seen && !text.is_empty() {
                    self.emit(ChatEvent::AssistantText {
                        id: "result".into(),
                        update: TextUpdate::Final,
                        text,
                    })?;
                }
                let missing_schema = self.config.output_schema_json.is_some()
                    && !self.structured_seen
                    && !error
                    && !cancelled;
                if error || missing_schema {
                    self.emit(ChatEvent::Error {
                        code: if missing_schema {
                            "missing_structured_output"
                        } else {
                            "provider_turn_failed"
                        }
                        .into(),
                        message: bounded(value.to_string()).text,
                        fatal: false,
                    })?;
                }
                let usage = &value["usage"];
                self.usage = Usage {
                    input_tokens: usage["input_tokens"].as_u64(),
                    output_tokens: usage["output_tokens"].as_u64(),
                    cache_read_tokens: usage["cache_read_input_tokens"].as_u64(),
                    cache_write_tokens: usage["cache_creation_input_tokens"].as_u64(),
                    cost_usd: value
                        .get("total_cost_usd")
                        .filter(|v| v.is_number())
                        .map(ToString::to_string),
                };
                self.end_turn(
                    value["duration_ms"].as_u64(),
                    value["num_turns"].as_u64().unwrap_or(1),
                    if cancelled {
                        TurnOutcome::Cancelled
                    } else if error || missing_schema {
                        TurnOutcome::Failed
                    } else {
                        TurnOutcome::Completed
                    },
                )?;
            }
            // Version additions are surfaced, not mistaken for assistant completion.
            "keep_alive"
            | "rate_limit_event"
            | "tool_progress"
            | "tool_use_summary"
            | "auth_status"
            | "prompt_suggestion"
            | "session_state_changed" => {}
            other => self.emit(ChatEvent::Error {
                code: "unsupported_event".into(),
                message: format!("unrecognized Claude event {other}"),
                fatal: false,
            })?,
        }
        Ok(Vec::new())
    }

    fn claude_partial(&mut self, event: &Value, parent: &str) -> Result<()> {
        match event["type"]
            .as_str()
            .ok_or_else(|| AdapterError::Protocol("Claude stream event lacks type".into()))?
        {
            "message_start" => {
                if self.claude_messages.len() >= MAX_PENDING {
                    return Err(AdapterError::Protocol(
                        "too many concurrent Claude streams".into(),
                    ));
                }
                self.claude_messages
                    .insert(parent.into(), nonempty(&event["message"], "id")?);
                if parent.is_empty()
                    && let Some(model) = event["message"]["model"].as_str()
                {
                    self.info.model = Some(model.into());
                }
            }
            "content_block_start" => {
                let key = format!("{parent}:{}", event["index"]);
                if self.claude_blocks.len() >= MAX_PENDING * 4 {
                    return Err(AdapterError::Protocol(
                        "too many concurrent Claude blocks".into(),
                    ));
                }
                let block = &event["content_block"];
                let id = block["id"].as_str().map(str::to_owned).unwrap_or_else(|| {
                    self.claude_messages
                        .get(parent)
                        .cloned()
                        .unwrap_or_else(|| key.clone())
                });
                self.claude_blocks.insert(key, id.clone());
                if block["type"] == "tool_use" {
                    self.claude_tool(block, parent)?;
                } else if block["type"] == "text" {
                    let text = string(block, "text");
                    if !text.is_empty() {
                        self.scoped(
                            ChatEvent::AssistantText {
                                id,
                                update: TextUpdate::Delta,
                                text,
                            },
                            parent,
                        )?;
                    }
                }
            }
            "content_block_delta" => {
                let key = format!("{parent}:{}", event["index"]);
                let id = self.claude_blocks.get(&key).cloned().ok_or_else(|| {
                    AdapterError::Protocol("Claude delta before block start".into())
                })?;
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => self.scoped(
                        ChatEvent::AssistantText {
                            id,
                            update: TextUpdate::Delta,
                            text: nonempty(delta, "text")?,
                        },
                        parent,
                    )?,
                    Some("thinking_delta") => {
                        if self.partial_ids.len() >= MAX_PENDING * 4 {
                            return Err(AdapterError::Protocol("too many thinking blocks".into()));
                        }
                        self.partial_ids.insert(id.clone());
                        self.scoped(
                            ChatEvent::Thinking {
                                id,
                                delta: string(delta, "thinking"),
                            },
                            parent,
                        )?;
                    }
                    Some("input_json_delta" | "signature_delta" | "citations_delta") => {}
                    _ => {
                        return Err(AdapterError::Unsupported(
                            "unknown Claude content delta kind".into(),
                        ));
                    }
                }
            }
            "content_block_stop" => {
                self.claude_blocks
                    .remove(&format!("{parent}:{}", event["index"]));
            }
            "message_stop" => {
                self.claude_messages.remove(parent);
            }
            "message_delta" => {}
            "error" => self.scoped(
                ChatEvent::Error {
                    code: "provider_stream_error".into(),
                    message: bounded(event.to_string()).text,
                    fatal: false,
                },
                parent,
            )?,
            other => {
                return Err(AdapterError::Unsupported(format!(
                    "unknown Claude stream event {other}"
                )));
            }
        }
        Ok(())
    }
    fn claude_tool(&mut self, block: &Value, parent: &str) -> Result<()> {
        let id = nonempty(block, "id")?;
        let name = nonempty(block, "name")?;
        if self.tools.len() >= MAX_PENDING * 4 && !self.tools.contains_key(&id) {
            return Err(AdapterError::Protocol("too many active tools".into()));
        }
        let tool = ToolCall {
            id: id.clone(),
            kind: tool_kind(&name),
            title: name,
            input_summary: bounded(block["input"].to_string()),
            status: ToolStatus::Running,
            output: None,
            diff: None,
        };
        self.tools.insert(id, tool.clone());
        self.scoped(ChatEvent::ToolCall(tool), parent)
    }
    fn claude_assistant(&mut self, value: &Value, parent: &str) -> Result<()> {
        let message = &value["message"];
        let id = nonempty(message, "id")?;
        if parent.is_empty()
            && let Some(model) = message["model"].as_str()
        {
            self.info.model = Some(model.into());
        }
        if let Some(content) = message["content"].as_array() {
            let mut final_text = String::new();
            for block in content {
                match block["type"].as_str() {
                    Some("text") => final_text.push_str(block["text"].as_str().unwrap_or_default()),
                    Some("thinking") if !self.partial_ids.contains(&id) => self.scoped(
                        ChatEvent::Thinking {
                            id: id.clone(),
                            delta: string(block, "thinking"),
                        },
                        parent,
                    )?,
                    Some("thinking" | "redacted_thinking") => {}
                    Some("tool_use") => self.claude_tool(block, parent)?,
                    _ => {}
                }
            }
            if !final_text.is_empty() {
                self.scoped(
                    ChatEvent::AssistantText {
                        id,
                        update: TextUpdate::Final,
                        text: final_text,
                    },
                    parent,
                )?;
                if parent.is_empty() {
                    self.final_seen = true;
                }
            }
        }
        Ok(())
    }
}

fn claude_patch(output: &Value, path: &str) -> Option<String> {
    use std::fmt::Write;
    if let Some(patch) = output["gitDiff"]["patch"].as_str() {
        return Some(patch.to_owned());
    }
    let hunks = output["structuredPatch"]
        .as_array()
        .filter(|h| !h.is_empty())?;
    let mut patch = format!("--- a/{path}\n+++ b/{path}\n");
    for hunk in hunks {
        let _ = writeln!(
            patch,
            "@@ -{},{} +{},{} @@",
            hunk["oldStart"], hunk["oldLines"], hunk["newStart"], hunk["newLines"]
        );
        if let Some(lines) = hunk["lines"].as_array() {
            for line in lines {
                if let Some(line) = line.as_str() {
                    patch.push_str(line);
                    patch.push('\n');
                }
            }
        }
    }
    Some(patch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::test_machine;
    #[test]
    fn permission_requests_wait_for_explicit_decision_and_cannot_replay() {
        let (mut machine, mut events) = test_machine(AdapterKind::ClaudeStream);
        let message = json!({"type":"control_request","request_id":"permission-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"pwd"},"tool_use_id":"tool-1"}});
        assert!(machine.claude_incoming(message).unwrap().is_empty());
        let id = machine.requests.keys().next().unwrap().clone();
        assert!(matches!(
            events.try_recv().unwrap().event,
            ChatEvent::ApprovalRequest { .. }
        ));
        let (reply, _) = oneshot::channel();
        let output = machine
            .claude_operation(
                Operation::Approve(id.clone(), ApprovalDecision::Deny),
                reply,
            )
            .unwrap();
        assert_eq!(output[0]["response"]["response"]["behavior"], "deny");
        let (reply, mut receive) = oneshot::channel();
        assert!(
            machine
                .claude_operation(Operation::Approve(id, ApprovalDecision::AllowOnce), reply)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            receive.try_recv().unwrap(),
            Err(AdapterError::UnknownRequest(_))
        ));
    }
    #[test]
    fn partial_subagent_text_and_turn_usage_normalize() {
        let (mut machine, mut events) = test_machine(AdapterKind::ClaudeStream);
        for line in include_str!("../tests/fixtures/claude-turn.synthetic.jsonl").lines() {
            machine
                .claude_incoming(serde_json::from_str(line).unwrap())
                .unwrap();
        }
        let mut records = Vec::new();
        while let Ok(event) = events.try_recv() {
            records.push(event);
        }
        assert!(records.iter().any(|r| matches!(&r.event, ChatEvent::Subagent { events, .. } if matches!(&events[0], ChatEvent::AssistantText { update: TextUpdate::Delta, text, .. } if text == "child"))));
        assert!(records.iter().any(|r| matches!(&r.event, ChatEvent::TurnEnd { usage, outcome: TurnOutcome::Completed, .. } if usage.input_tokens == Some(12) && usage.cache_read_tokens == Some(4))));
        assert!(
            records
                .windows(2)
                .all(|r| r[1].sequence == r[0].sequence + 1)
        );
    }
    #[tokio::test]
    async fn missing_transcript_means_the_session_is_gone_not_an_io_fault() {
        let dir = tempfile::tempdir().unwrap();
        let mut provider = crate::ProviderRegistry::default()
            .get("claude")
            .unwrap()
            .clone();
        provider.env.insert(
            "CLAUDE_CONFIG_DIR".into(),
            dir.path().to_string_lossy().into_owned(),
        );
        let target = ResumeTarget {
            provider_id: "claude".into(),
            adapter: AdapterKind::ClaudeStream,
            native_id: uuid::Uuid::new_v4().to_string(),
            cwd: dir.path().into(),
        };
        assert!(matches!(
            validate_resume(&provider, &target, dir.path()).await,
            Err(AdapterError::InvalidResume(_))
        ));
    }
    #[test]
    fn failed_turn_keeps_failed_state_until_the_next_send() {
        let (mut machine, mut events) = test_machine(AdapterKind::ClaudeStream);
        machine.active_turn = Some("turn".into());
        machine.end_turn(None, 1, TurnOutcome::Failed).unwrap();
        let mut records = Vec::new();
        while let Ok(record) = events.try_recv() {
            records.push(record);
        }
        assert!(matches!(
            records.last().map(|record| &record.event),
            Some(ChatEvent::StateChanged(SessionState::Failed { .. }))
        ));
        assert!(
            !records
                .iter()
                .any(|record| matches!(&record.event, ChatEvent::StateChanged(SessionState::Idle)))
        );
        assert!(matches!(
            machine.state.borrow().clone(),
            SessionState::Failed { .. }
        ));
        let (reply, _) = oneshot::channel();
        machine
            .claude_operation(Operation::Send("again".into()), reply)
            .unwrap();
        while events.try_recv().is_ok() {}
        assert!(matches!(
            machine.state.borrow().clone(),
            SessionState::Running
        ));
    }
    #[test]
    fn malformed_and_oversized_messages_fail_closed() {
        assert!(crate::process::parse_line(Some(Ok("{broken".into()))).is_err());
        assert!(crate::process::parse_line(Some(Ok("[]".into()))).is_err());
        assert!(valid_text(&"x".repeat(MAX_TEXT + 1)).is_err());
        assert_eq!(bounded("€".repeat(MAX_TEXT)).text.len() % 3, 0);
    }
}
