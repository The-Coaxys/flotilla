//! Generic Agent Client Protocol adapter (ACP v1, JSON-RPC over stdio).
//!
//! Discovery performs the real `initialize` handshake and reports the exact
//! failure. Launch repeats it, then creates or loads a session. Only explicit
//! `approve` calls answer `session/request_permission`; there is no automatic
//! approval and `allow_always` is never offered. Agent-supplied text and tool
//! content is untrusted data. Unknown notifications are diagnosed, not
//! mistaken for turn completion.

use crate::{
    host::{self, Wire},
    process::{Interaction, Machine, Pending, PendingKind, nonempty, valid_text},
    *,
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::{collections::HashMap, time::Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::LinesCodec;

pub(crate) const VERSION: u64 = 1;

struct Permission {
    allow_once: Option<String>,
    reject_once: Option<String>,
}

pub(crate) struct Acp {
    session: String,
    ready: bool,
    next_id: u64,
    prompt_wire: Option<String>,
    permissions: HashMap<String, Permission>,
    tools: HashMap<String, ToolCall>,
    text_id: String,
    thinking_id: String,
}

impl Acp {
    fn new() -> Self {
        Self {
            session: String::new(),
            ready: false,
            next_id: 1,
            prompt_wire: None,
            permissions: HashMap::new(),
            tools: HashMap::new(),
            text_id: "assistant".into(),
            thinking_id: "thinking".into(),
        }
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id - 1
    }

    fn track(machine: &mut Machine, id: u64, kind: PendingKind) {
        machine.pending.insert(
            id.to_string(),
            Pending {
                kind,
                reply: None,
                started: Instant::now(),
            },
        );
    }

    fn fail(&mut self, machine: &mut Machine, message: String) -> Result<()> {
        self.prompt_wire = None;
        self.permissions.clear();
        self.tools.clear();
        machine.emit(ChatEvent::Error {
            code: "provider_turn_failed".into(),
            message: bounded(message).text,
            fatal: false,
        })?;
        machine.end_turn(None, 1, TurnOutcome::Failed)
    }

    fn update(&mut self, machine: &mut Machine, update: &Value) -> Result<()> {
        let kind = match update["sessionUpdate"].as_str() {
            Some(kind) => kind,
            None => {
                return Err(AdapterError::Protocol(
                    "ACP session update lacks sessionUpdate".into(),
                ));
            }
        };
        match kind {
            "agent_message_chunk" => {
                let content = &update["content"];
                match content["type"].as_str() {
                    Some("text") => machine.emit(ChatEvent::AssistantText {
                        id: self
                            .message_id(update)
                            .unwrap_or_else(|| self.text_id.clone()),
                        update: TextUpdate::Delta,
                        text: string(content, "text"),
                    })?,
                    _ => machine.emit(ChatEvent::Diagnostic {
                        stream: "acp".into(),
                        output: bounded(format!("non-text agent content: {}", content["type"])),
                    })?,
                }
            }
            "agent_thought_chunk" => {
                let content = &update["content"];
                if content["type"] == "text" {
                    machine.emit(ChatEvent::Thinking {
                        id: self
                            .message_id(update)
                            .unwrap_or_else(|| self.thinking_id.clone()),
                        delta: string(content, "text"),
                    })?;
                }
            }
            "user_message_chunk" => {}
            "tool_call" | "tool_call_update" => {
                let mut tool = self.tool(update)?;
                let diff = update["content"].as_array().and_then(|content| {
                    content.iter().find_map(|block| {
                        let path = (block["type"] == "diff")
                            .then(|| string(block, "path"))
                            .filter(|path| !path.is_empty())?;
                        Some((
                            path.clone(),
                            bounded(host::whole_file_diff(
                                &path,
                                block["oldText"].as_str().unwrap_or_default(),
                                block["newText"].as_str().unwrap_or_default(),
                            )),
                        ))
                    })
                });
                if let Some((path, patch)) = diff {
                    tool.diff = Some(patch.clone());
                    machine.emit(ChatEvent::FileEdit {
                        path: path.into(),
                        unified_diff: patch,
                    })?;
                }
                self.tools.insert(tool.id.clone(), tool.clone());
                machine.emit(ChatEvent::ToolCall(tool))?;
            }
            "plan"
            | "available_commands_update"
            | "current_mode_update"
            | "config_option_update"
            | "session_info_update"
            | "usage_update" => {}
            other => machine.emit(ChatEvent::Diagnostic {
                stream: "acp".into(),
                output: bounded(format!("unhandled ACP session update {other}")),
            })?,
        }
        Ok(())
    }

    fn message_id(&self, update: &Value) -> Option<String> {
        update["messageId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    }

    fn tool(&self, update: &Value) -> Result<ToolCall> {
        let id = nonempty(update, "toolCallId")?;
        let existing = self.tools.get(&id);
        let title = update["title"]
            .as_str()
            .filter(|title| !title.is_empty())
            .map(str::to_owned)
            .or_else(|| existing.map(|tool| tool.title.clone()))
            .unwrap_or_else(|| id.clone());
        let kind = match update["kind"].as_str() {
            Some("read") => ToolKind::Read,
            Some("edit" | "delete" | "move") => ToolKind::Edit,
            Some("search") => ToolKind::Search,
            Some("execute") => ToolKind::Exec,
            Some("fetch") => ToolKind::Web,
            Some(_) => ToolKind::Other,
            None => existing
                .map(|tool| tool.kind.clone())
                .unwrap_or(ToolKind::Other),
        };
        let status = match update["status"].as_str() {
            Some("pending" | "in_progress") => ToolStatus::Running,
            Some("completed") => ToolStatus::Completed,
            Some("failed") => ToolStatus::Failed,
            Some(_) | None => existing
                .map(|tool| tool.status.clone())
                .unwrap_or(ToolStatus::Running),
        };
        let input_summary = update["rawInput"]
            .is_null()
            .then(|| existing.map(|tool| tool.input_summary.clone()))
            .flatten()
            .unwrap_or_else(|| bounded(update["rawInput"].to_string()));
        let output = host::text_content(&update["rawOutput"]);
        let output = (!output.is_empty())
            .then(|| bounded(output))
            .or_else(|| existing.and_then(|tool| tool.output.clone()));
        Ok(ToolCall {
            id,
            kind,
            title,
            input_summary,
            status,
            output,
            diff: existing.and_then(|tool| tool.diff.clone()),
        })
    }
}

impl Wire for Acp {
    fn start(&mut self, machine: &mut Machine) -> Result<Vec<Value>> {
        let id = self.next();
        Self::track(machine, id, PendingKind::Initialize);
        Ok(vec![json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": VERSION,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false
                },
                "clientInfo": {
                    "name": "flotilla",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        })])
    }

    fn incoming(&mut self, machine: &mut Machine, value: Value) -> Result<Vec<Value>> {
        if let Some(method) = value["method"].as_str() {
            if method == "session/update" {
                let params = &value["params"];
                if !self.session.is_empty()
                    && params["sessionId"].as_str() != Some(self.session.as_str())
                {
                    return Err(AdapterError::Protocol(
                        "ACP update for a different session".into(),
                    ));
                }
                if !self.ready {
                    return Ok(Vec::new());
                }
                self.update(machine, &params["update"])?;
                return Ok(Vec::new());
            }
            if value.get("id").is_some() {
                return self.request(machine, method, &value);
            }
            machine.emit(ChatEvent::Diagnostic {
                stream: "acp".into(),
                output: bounded(format!("unhandled ACP notification {method}")),
            })?;
            return Ok(Vec::new());
        }
        let Some(wire_id) = value.get("id") else {
            return Err(AdapterError::Protocol(
                "ACP message lacks method or id".into(),
            ));
        };
        let key = wire_key(wire_id);
        if self.prompt_wire.as_deref() == Some(key.as_str()) {
            self.prompt_wire = None;
            self.permissions.clear();
            self.tools.clear();
            if let Some(error) = value.get("error") {
                self.fail(machine, error.to_string())?;
                return Ok(Vec::new());
            }
            let result = &value["result"];
            let stop = string(result, "stopReason");
            let usage = &result["usage"];
            machine.usage = Usage {
                input_tokens: usage["inputTokens"].as_u64(),
                output_tokens: usage["outputTokens"].as_u64(),
                cache_read_tokens: usage["cachedReadTokens"].as_u64(),
                cache_write_tokens: usage["cachedWriteTokens"].as_u64(),
                cost_usd: result["cost"]["amount"]
                    .as_f64()
                    .map(|value| format!("{value:.6}")),
            };
            let outcome = match stop.as_str() {
                "cancelled" => TurnOutcome::Cancelled,
                "refusal" => TurnOutcome::Failed,
                _ => TurnOutcome::Completed,
            };
            if outcome == TurnOutcome::Failed {
                machine.emit(ChatEvent::Error {
                    code: "provider_turn_failed".into(),
                    message: format!("ACP turn ended with stopReason {stop}"),
                    fatal: false,
                })?;
            }
            let duration = machine
                .turn_started
                .map(|started| started.elapsed().as_millis().min(u64::MAX as u128) as u64);
            return machine.end_turn(duration, 1, outcome).map(|_| Vec::new());
        }
        let Some(pending) = machine.pending.remove(&key) else {
            return Err(AdapterError::Protocol(format!(
                "unmatched ACP response id {key}"
            )));
        };
        if let Some(error) = value.get("error") {
            let message = bounded(error.to_string()).text;
            if matches!(&pending.kind, PendingKind::Initialize) {
                return Err(AdapterError::Unsupported(message));
            }
            return Err(AdapterError::Rejected(message));
        }
        let result = &value["result"];
        match pending.kind {
            PendingKind::Initialize => {
                let version = result["protocolVersion"].as_u64().unwrap_or_default();
                if version != VERSION {
                    return Err(AdapterError::Unsupported(format!(
                        "ACP agent speaks protocol {version}, adapter speaks {VERSION}"
                    )));
                }
                if result.get("agentInfo").is_some()
                    && machine.info.model.is_none()
                    && let Some(model) = result["models"]["currentModelId"].as_str()
                {
                    machine.info.model = Some(model.into());
                }
                let load = result["agentCapabilities"]["loadSession"]
                    .as_bool()
                    .unwrap_or(false);
                let resume = result["agentCapabilities"]["sessionCapabilities"]["resume"]
                    .as_object()
                    .is_some();
                if let Some(target) = machine.config.resume.clone() {
                    let id = self.next();
                    Self::track(machine, id, PendingKind::OpenSession);
                    if load {
                        return Ok(vec![json!({
                            "jsonrpc": "2.0", "id": id, "method": "session/load",
                            "params": {
                                "sessionId": target.native_id,
                                "cwd": machine.config.cwd.to_string_lossy(),
                                "mcpServers": []
                            }
                        })]);
                    }
                    if resume {
                        return Ok(vec![json!({
                            "jsonrpc": "2.0", "id": id, "method": "session/resume",
                            "params": {
                                "sessionId": target.native_id,
                                "cwd": machine.config.cwd.to_string_lossy(),
                                "mcpServers": []
                            }
                        })]);
                    }
                    return Err(AdapterError::InvalidResume(
                        "ACP agent advertises neither session/load nor session/resume".into(),
                    ));
                }
                let id = self.next();
                Self::track(machine, id, PendingKind::OpenSession);
                Ok(vec![json!({
                    "jsonrpc": "2.0", "id": id, "method": "session/new",
                    "params": {
                        "cwd": machine.config.cwd.to_string_lossy(),
                        "mcpServers": []
                    }
                })])
            }
            PendingKind::OpenSession => {
                let session = result["sessionId"]
                    .as_str()
                    .filter(|session| !session.is_empty())
                    .map(str::to_owned)
                    .or_else(|| machine.config.resume.as_ref().map(|r| r.native_id.clone()))
                    .ok_or_else(|| AdapterError::Protocol("ACP session lacks sessionId".into()))?;
                if let Some(target) = &machine.config.resume
                    && session != target.native_id
                {
                    return Err(AdapterError::InvalidResume(
                        "ACP agent resumed a different session ID".into(),
                    ));
                }
                if let Some(model) = result["models"]["currentModelId"].as_str() {
                    machine.info.model = Some(model.into());
                }
                let current = result["models"]["currentModelId"].as_str();
                self.session = session.clone();
                if let Some(model) = machine.config.model.clone()
                    && current != Some(model.as_str())
                {
                    let id = self.next();
                    Self::track(machine, id, PendingKind::Control);
                    return Ok(vec![json!({
                        "jsonrpc": "2.0", "id": id, "method": "session/set_model",
                        "params": {"sessionId": session, "modelId": model}
                    })]);
                }
                self.ready = true;
                machine.session_ready(session).map(|_| Vec::new())
            }
            PendingKind::Control => {
                if self.session.is_empty() {
                    return Err(AdapterError::Protocol(
                        "ACP model response before a session existed".into(),
                    ));
                }
                self.ready = true;
                machine
                    .session_ready(self.session.clone())
                    .map(|_| Vec::new())
            }
            _ => Err(AdapterError::Protocol(
                "unexpected ACP response kind".into(),
            )),
        }
    }

    fn operation(
        &mut self,
        machine: &mut Machine,
        operation: Operation,
        reply: oneshot::Sender<Result<()>>,
    ) -> Result<Vec<Value>> {
        let validation = match &operation {
            Operation::Send(text) => valid_text(text).and_then(|_| {
                if machine.active_turn.is_some() {
                    Err(AdapterError::Busy)
                } else if !self.ready {
                    Err(AdapterError::Closed("ACP session is not ready".into()))
                } else {
                    Ok(())
                }
            }),
            Operation::Steer(_) => Err(AdapterError::Unsupported(
                "ACP has no mid-turn steer; use cancel and send".into(),
            )),
            Operation::Cancel => {
                if machine.active_turn.is_none() {
                    Err(AdapterError::NoActiveTurn)
                } else {
                    Ok(())
                }
            }
            Operation::Approve(id, _) => {
                if self.permissions.contains_key(id) {
                    Ok(())
                } else {
                    Err(AdapterError::UnknownRequest(id.clone()))
                }
            }
            Operation::Question(id, _) => Err(AdapterError::UnknownRequest(id.clone())),
            Operation::Shutdown => unreachable!("shutdown is handled by runtime"),
        };
        if let Err(error) = validation {
            let _ = reply.send(Err(error));
            return Ok(Vec::new());
        }
        match operation {
            Operation::Send(text) => {
                let turn = uuid::Uuid::new_v4().to_string();
                machine.active_turn = Some(turn.clone());
                machine.turn_started = Some(Instant::now());
                self.text_id = turn.clone();
                self.thinking_id = format!("{turn}:thinking");
                machine.user_event(turn.clone(), text.clone())?;
                machine.set_state(SessionState::Running)?;
                let id = self.next();
                self.prompt_wire = Some(id.to_string());
                machine.delivered(reply);
                Ok(vec![json!({
                    "jsonrpc": "2.0", "id": id, "method": "session/prompt",
                    "params": {
                        "sessionId": self.session.clone(),
                        "prompt": [{"type": "text", "text": text}]
                    }
                })])
            }
            Operation::Cancel => {
                machine.delivered(reply);
                Ok(vec![json!({
                    "jsonrpc": "2.0", "method": "session/cancel",
                    "params": {"sessionId": self.session.clone()}
                })])
            }
            Operation::Approve(id, decision) => {
                let permission = self
                    .permissions
                    .remove(&id)
                    .ok_or_else(|| AdapterError::UnknownRequest(id.clone()))?;
                let request = machine
                    .requests
                    .remove(&id)
                    .ok_or_else(|| AdapterError::UnknownRequest(id.clone()))?;
                let outcome = match decision {
                    ApprovalDecision::AllowOnce => {
                        let option = permission.allow_once.clone().ok_or_else(|| {
                            AdapterError::UnknownRequest(
                                "ACP agent offered no one-shot allow option".into(),
                            )
                        })?;
                        json!({"outcome": "selected", "optionId": option})
                    }
                    ApprovalDecision::Deny => match permission.reject_once {
                        Some(option) => json!({"outcome": "selected", "optionId": option}),
                        None => json!({"outcome": "cancelled"}),
                    },
                    ApprovalDecision::Cancel => json!({"outcome": "cancelled"}),
                };
                machine.emit(ChatEvent::RequestResolved { id })?;
                machine.set_state(if machine.requests.is_empty() {
                    SessionState::Running
                } else {
                    SessionState::AwaitingInput
                })?;
                machine.delivered(reply);
                Ok(vec![json!({
                    "jsonrpc": "2.0", "id": request.wire_id,
                    "result": {"outcome": outcome}
                })])
            }
            Operation::Steer(_) | Operation::Question(_, _) | Operation::Shutdown => {
                unreachable!()
            }
        }
    }

    fn exited(&mut self, _machine: &mut Machine, expected: bool) -> Result<()> {
        if !expected {
            self.prompt_wire = None;
        }
        Ok(())
    }
}

impl Acp {
    fn request(
        &mut self,
        machine: &mut Machine,
        method: &str,
        value: &Value,
    ) -> Result<Vec<Value>> {
        let wire_id = value
            .get("id")
            .filter(|id| id.is_string() || id.is_number())
            .cloned()
            .ok_or_else(|| AdapterError::Protocol("ACP client request lacks id".into()))?;
        if method != "session/request_permission" {
            machine.emit(ChatEvent::Diagnostic {
                stream: "acp".into(),
                output: bounded(format!("rejected unsupported ACP client request {method}")),
            })?;
            return Ok(vec![json!({
                "jsonrpc": "2.0", "id": wire_id,
                "error": {"code": -32601, "message": "Flotilla does not implement this ACP client method"}
            })]);
        }
        let params = &value["params"];
        if !self.session.is_empty()
            && params["sessionId"]
                .as_str()
                .is_some_and(|session| session != self.session.as_str())
        {
            return Ok(vec![json!({
                "jsonrpc": "2.0", "id": wire_id,
                "error": {"code": -32602, "message": "unknown ACP session"}
            })]);
        }
        let tool = &params["toolCall"];
        let readish = matches!(tool["kind"].as_str(), Some("read" | "search" | "fetch"));
        if machine.config.permission_mode == PermissionMode::ReadOnly && !readish {
            machine.emit(ChatEvent::Error {
                code: "permission_denied".into(),
                message: "ACP tool denied in read-only mode".into(),
                fatal: false,
            })?;
            return Ok(vec![json!({
                "jsonrpc": "2.0", "id": wire_id,
                "result": {"outcome": {"outcome": "cancelled"}}
            })]);
        }
        let title = tool["title"].as_str().filter(|title| !title.is_empty());
        let tool_name = title.map(str::to_owned).unwrap_or_else(|| {
            tool["kind"]
                .as_str()
                .filter(|kind| !kind.is_empty())
                .unwrap_or("tool")
                .to_owned()
        });
        let options = params["options"].as_array().cloned().unwrap_or_default();
        let allow_once = options
            .iter()
            .find(|option| option["kind"] == "allow_once")
            .and_then(|option| option["optionId"].as_str())
            .map(str::to_owned);
        let reject_once = options
            .iter()
            .find(|option| option["kind"] == "reject_once")
            .and_then(|option| option["optionId"].as_str())
            .map(str::to_owned);
        let mut offered = Vec::new();
        if allow_once.is_some() {
            offered.push(ApprovalDecision::AllowOnce);
        }
        offered.push(ApprovalDecision::Deny);
        offered.push(ApprovalDecision::Cancel);
        let id = machine.request_id(
            wire_id,
            Interaction::CodexApproval {
                method: "session/request_permission".into(),
                params: params.clone(),
            },
        )?;
        self.permissions.insert(
            id.clone(),
            Permission {
                allow_once,
                reject_once,
            },
        );
        machine.emit(ChatEvent::ApprovalRequest {
            id,
            tool: tool_name,
            details: bounded(params.to_string()),
            options: offered,
        })?;
        machine.set_state(SessionState::AwaitingInput)?;
        Ok(Vec::new())
    }
}

fn wire_key(value: &Value) -> String {
    match value {
        Value::String(id) => id.clone(),
        other => other.to_string(),
    }
}

pub(crate) async fn launch(provider: ProviderSpec, config: SessionConfig) -> Result<Adapter> {
    let config = host::prepare(&provider, config).await?;
    if config.instructions.is_some() {
        return Err(AdapterError::Unsupported(
            "ACP has no system-prompt parameter".into(),
        ));
    }
    if config.output_schema_json.is_some() {
        return Err(AdapterError::Unsupported(
            "ACP has no structured-output parameter".into(),
        ));
    }
    if config.provider_config_json.is_some() {
        return Err(AdapterError::Unsupported(
            "ACP has no settings-file parameter; use provider args or env".into(),
        ));
    }
    if let Some(resume) = &config.resume {
        host::token(&resume.native_id)?;
    }
    let native_id = config
        .resume
        .as_ref()
        .map(|resume| resume.native_id.clone())
        .unwrap_or_default();
    let info = SessionInfo {
        id: config.session_id.unwrap_or_else(uuid::Uuid::new_v4),
        incarnation: uuid::Uuid::new_v4(),
        resume: ResumeTarget {
            provider_id: provider.id.clone(),
            adapter: provider.adapter,
            native_id,
            cwd: config.cwd.clone(),
        },
        model: config.model.clone(),
        resumed: config.resume.is_some(),
    };
    let mut command = host::command(&provider);
    command.current_dir(&config.cwd);
    let child = command.spawn()?;
    let (mut machine, events) = host::machine(provider, config, info);
    let state = machine.state.subscribe();
    let (commands, receiver) = mpsc::channel(32);
    let (ready, ready_rx) = oneshot::channel();
    machine.ready = Some(ready);
    tokio::spawn(host::serve(child, machine, receiver, Box::new(Acp::new())));
    let info = tokio::time::timeout(process::OP_TIMEOUT, ready_rx)
        .await
        .map_err(|_| AdapterError::Timeout)?
        .map_err(|_| AdapterError::Closed("ACP initialization task exited".into()))??;
    Ok(Adapter {
        info,
        events,
        commands,
        state,
    })
}

/// Real initialize handshake for discovery. No session is created and no
/// model request is sent.
pub(crate) async fn discover(
    provider: &ProviderSpec,
    result: &mut ProviderDiscovery,
) -> Result<()> {
    let version = host::capture(provider, &["--version"], host::PROBE_TIMEOUT)
        .await
        .ok()
        .map(|value| value.lines().next().unwrap_or_default().trim().to_owned())
        .filter(|value| !value.is_empty());
    let mut child = host::command(provider)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let guard = host::Guard::new(&child);
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| AdapterError::Protocol("missing stdin".into()))?;
    let mut lines = tokio_util::codec::FramedRead::new(
        child
            .stdout
            .take()
            .ok_or_else(|| AdapterError::Protocol("missing stdout".into()))?,
        LinesCodec::new_with_max_length(MAX_WIRE_LINE),
    );
    let mut errors = tokio_util::codec::FramedRead::new(
        child
            .stderr
            .take()
            .ok_or_else(|| AdapterError::Protocol("missing stderr".into()))?,
        LinesCodec::new_with_max_length(MAX_WIRE_LINE),
    );
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": VERSION,
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false
            },
            "clientInfo": {"name": "flotilla", "version": env!("CARGO_PKG_VERSION")}
        }
    });
    let attempt = tokio::time::timeout(host::PROBE_TIMEOUT, async {
        host::write(&mut input, &init).await?;
        loop {
            tokio::select! {
                line = lines.next() => {
                    let value = process::parse_line(line)?;
                    if value.get("id") == Some(&Value::from(1)) {
                        if let Some(error) = value.get("error") {
                            return Err(AdapterError::Unsupported(bounded(error.to_string()).text));
                        }
                        let init = &value["result"];
                        let protocol = init["protocolVersion"].as_u64().unwrap_or_default();
                        if protocol != VERSION {
                            return Err(AdapterError::Unsupported(format!(
                                "ACP agent speaks protocol {protocol}, adapter speaks {VERSION}"
                            )));
                        }
                        if !init["agentCapabilities"].is_object() {
                            return Err(AdapterError::Unsupported(
                                "ACP initialize result lacks agentCapabilities".into(),
                            ));
                        }
                        let load = init["agentCapabilities"]["loadSession"].as_bool().unwrap_or(false);
                        let resume = init["agentCapabilities"]["sessionCapabilities"]["resume"].is_object();
                        let version = init["agentInfo"]["version"]
                            .as_str()
                            .filter(|version| !version.is_empty())
                            .map(str::to_owned);
                        if result.version.is_none() {
                            result.version = version;
                        }
                        result.capabilities = Capabilities {
                            chat: true,
                            resume: load || resume,
                            steer: false,
                            approvals: true,
                            questions: false,
                            partial_messages: true,
                        };
                        return Ok(());
                    }
                    if value.get("method").is_some() {
                        return Err(AdapterError::Unsupported(
                            "ACP agent requested interaction before initialize completed".into(),
                        ));
                    }
                }
                line = errors.next() => match line {
                    Some(Ok(line)) => {
                        if line.len() > 4096 {
                            return Err(AdapterError::Unsupported("ACP stderr line is oversized".into()));
                        }
                    }
                    Some(Err(error)) => return Err(AdapterError::Unsupported(format!("ACP stderr: {error}"))),
                    None => return Err(AdapterError::Unsupported("ACP agent closed stderr during initialize".into())),
                }
            }
        }
    }).await;
    drop(input);
    guard.kill();
    let _ = child.start_kill();
    let _ = child.wait().await;
    if let Some(version) = version.filter(|_| result.version.is_none()) {
        result.version = Some(version);
    }
    attempt.map_err(|_| AdapterError::Timeout)??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host;
    use tokio::sync::mpsc;

    fn provider() -> ProviderSpec {
        ProviderSpec {
            id: "opencode".into(),
            name: "OpenCode".into(),
            command: "opencode".into(),
            args: vec!["acp".into()],
            env: Default::default(),
            adapter: AdapterKind::Acp,
            resume: ResumeStrategy::AcpSession,
            hook_support: false,
        }
    }

    fn machine() -> (Machine, mpsc::Receiver<EventRecord>) {
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
                provider_id: "opencode".into(),
                adapter: AdapterKind::Acp,
                native_id: String::new(),
                cwd: config.cwd.clone(),
            },
            model: None,
            resumed: false,
        };
        host::machine(provider(), config, info)
    }

    fn feed(wire: &mut Acp, machine: &mut Machine, text: &str) -> Vec<Value> {
        let mut out = Vec::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            out.extend(
                wire.incoming(machine, serde_json::from_str(line).unwrap())
                    .unwrap(),
            );
        }
        out
    }

    fn ready() -> (Acp, Machine, mpsc::Receiver<EventRecord>) {
        let (mut machine, events) = machine();
        let mut wire = Acp::new();
        wire.start(&mut machine).unwrap();
        let out = feed(
            &mut wire,
            &mut machine,
            include_str!("../tests/fixtures/acp-opencode-initialize.recorded.jsonl"),
        );
        assert_eq!(out[0]["method"], "session/new");
        wire.incoming(
            &mut machine,
            serde_json::from_str(include_str!(
                "../tests/fixtures/acp-opencode-session-new.recorded.jsonl"
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(wire.session, "ses_e59446be4ffenueEyrhwRleVra");
        (wire, machine, events)
    }

    fn drain(events: &mut mpsc::Receiver<EventRecord>) -> Vec<EventRecord> {
        let mut records = Vec::new();
        while let Ok(record) = events.try_recv() {
            records.push(record);
        }
        records
    }

    #[test]
    fn recorded_initialize_and_session_new_are_accepted() {
        let (wire, machine, mut events) = ready();
        assert!(wire.ready);
        let records = drain(&mut events);
        assert!(records.iter().any(|record| matches!(
            &record.event,
            ChatEvent::SessionReady(info) if info.resume.native_id == "ses_e59446be4ffenueEyrhwRleVra"
        )));
        assert_eq!(machine.state.borrow().clone(), SessionState::Idle);
    }

    #[test]
    fn turn_updates_permission_requests_and_denial_normalize() {
        let (mut wire, mut machine, mut events) = ready();
        let (reply, _) = tokio::sync::oneshot::channel();
        let out = wire
            .operation(&mut machine, Operation::Send("pwd".into()), reply)
            .unwrap();
        assert_eq!(out[0]["method"], "session/prompt");
        let prompt_wire = wire.prompt_wire.clone().unwrap();
        feed(
            &mut wire,
            &mut machine,
            include_str!("../tests/fixtures/acp-turn.synthetic.jsonl"),
        );
        let records = drain(&mut events);
        assert!(records.iter().any(|record| matches!(
            &record.event,
            ChatEvent::AssistantText { update: TextUpdate::Delta, text, .. } if text == "Hel"
        )));
        assert!(records.iter().any(|record| matches!(
            &record.event,
            ChatEvent::Thinking { delta, .. } if delta == "plan"
        )));
        assert!(records.iter().any(|record| matches!(
            &record.event,
            ChatEvent::ToolCall(call) if call.id == "t1" && call.status == ToolStatus::Completed && call.output.as_ref().is_some_and(|output| output.text == "/tmp")
        )));
        let approval = records
            .iter()
            .find_map(|record| match &record.event {
                ChatEvent::ApprovalRequest { id, options, .. } => {
                    Some((id.clone(), options.clone()))
                }
                _ => None,
            })
            .expect("permission request becomes an approval card");
        assert_eq!(
            approval.1,
            vec![
                ApprovalDecision::AllowOnce,
                ApprovalDecision::Deny,
                ApprovalDecision::Cancel
            ]
        );
        // Nothing is answered implicitly: the request is still pending.
        assert!(machine.requests.contains_key(&approval.0));
        let (reply, _) = tokio::sync::oneshot::channel();
        let out = wire
            .operation(
                &mut machine,
                Operation::Approve(approval.0.clone(), ApprovalDecision::Deny),
                reply,
            )
            .unwrap();
        assert_eq!(out[0]["result"]["outcome"]["outcome"], "selected");
        assert_eq!(out[0]["result"]["outcome"]["optionId"], "deny");
        let (reply, mut receive) = tokio::sync::oneshot::channel();
        assert!(
            wire.operation(
                &mut machine,
                Operation::Approve(approval.0, ApprovalDecision::AllowOnce),
                reply
            )
            .unwrap()
            .is_empty()
        );
        assert!(matches!(
            receive.try_recv().unwrap(),
            Err(AdapterError::UnknownRequest(_))
        ));
        let out = wire
            .incoming(
                &mut machine,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": prompt_wire.parse::<u64>().unwrap(),
                    "result": {"stopReason": "end_turn"}
                }),
            )
            .unwrap();
        assert!(out.is_empty());
        assert!(drain(&mut events).iter().any(|record| matches!(
            record.event,
            ChatEvent::TurnEnd {
                outcome: TurnOutcome::Completed,
                ..
            }
        )));
    }

    #[test]
    fn persistent_allow_never_offers_a_one_shot_approval() {
        let (mut wire, mut machine, mut events) = ready();
        let session = wire.session.clone();
        wire.incoming(
            &mut machine,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 7, "method": "session/request_permission",
                "params": {
                    "sessionId": session,
                    "toolCall": {"toolCallId": "t9", "title": "Always", "kind": "execute"},
                    "options": [{"optionId": "always", "kind": "allow_always", "name": "Always"}]
                }
            }),
        )
        .unwrap();
        let approval = drain(&mut events)
            .into_iter()
            .find_map(|record| match record.event {
                ChatEvent::ApprovalRequest { options, .. } => Some(options),
                _ => None,
            })
            .expect("approval card");
        assert!(!approval.contains(&ApprovalDecision::AllowOnce));
        assert!(approval.contains(&ApprovalDecision::Deny));
    }

    #[test]
    fn malformed_updates_and_wrong_protocol_fail_closed() {
        let (mut wire, mut state_machine, _) = ready();
        assert!(matches!(
            wire.update(&mut state_machine, &serde_json::json!({})),
            Err(AdapterError::Protocol(_))
        ));
        let (mut accepted, _) = machine();
        let mut wire = Acp::new();
        wire.start(&mut accepted).unwrap();
        assert!(matches!(
            wire.incoming(
                &mut accepted,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": {"protocolVersion": 2, "agentCapabilities": {}}
                })
            ),
            Err(AdapterError::Unsupported(_))
        ));
    }

    #[test]
    fn resume_without_an_advertised_capability_is_rejected() {
        let (mut machine, _) = machine();
        let mut wire = Acp::new();
        machine.config.resume = Some(ResumeTarget {
            provider_id: "opencode".into(),
            adapter: AdapterKind::Acp,
            native_id: "ses_123".into(),
            cwd: machine.config.cwd.clone(),
        });
        wire.start(&mut machine).unwrap();
        assert!(matches!(
            wire.incoming(
                &mut machine,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": {"protocolVersion": 1, "agentCapabilities": {}}
                })
            ),
            Err(AdapterError::InvalidResume(_))
        ));
    }

    #[test]
    fn read_only_denies_acp_execution_without_an_approval_card() {
        let (mut wire, mut machine, mut events) = ready();
        let session = wire.session.clone();
        machine.config.permission_mode = PermissionMode::ReadOnly;
        let out = wire
            .incoming(
                &mut machine,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 8, "method": "session/request_permission",
                    "params": {
                        "sessionId": session,
                        "toolCall": {"toolCallId": "t8", "title": "Run", "kind": "execute"},
                        "options": [{"optionId": "allow", "kind": "allow_once", "name": "Allow"}]
                    }
                }),
            )
            .unwrap();
        assert_eq!(out[0]["result"]["outcome"]["outcome"], "cancelled");
        assert!(
            !drain(&mut events)
                .iter()
                .any(|record| matches!(record.event, ChatEvent::ApprovalRequest { .. }))
        );
    }

    #[test]
    fn unknown_notifications_are_diagnosed_not_fatal() {
        let (mut wire, mut machine, mut events) = ready();
        let session = wire.session.clone();
        wire.incoming(
            &mut machine,
            serde_json::json!({
                "jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": session, "update": {"sessionUpdate": "future_thing"}}
            }),
        )
        .unwrap();
        assert!(
            drain(&mut events)
                .iter()
                .any(|record| matches!(&record.event, ChatEvent::Diagnostic { .. }))
        );
    }
}
