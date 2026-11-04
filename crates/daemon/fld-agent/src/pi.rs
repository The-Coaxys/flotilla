//! Pi / Oh My Pi `--mode rpc` adapter.
//!
//! Both CLIs speak the same JSONL command/response/session-event protocol.
//! OMP additionally announces `{"type":"ready"}` and tolerates protocol
//! version 2; this adapter speaks version 1. Extension UI dialogs are the only
//! permission surface and are never auto-answered. Built-in tool execution is
//! controlled by the CLI, so discovery reports `approvals: false`; read-only
//! mode is rejected explicitly rather than silently ignored.

use crate::{
    host::{self, Wire},
    process::{Interaction, Pending, PendingKind, checked_answers, nonempty, valid_text},
    *,
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    time::Instant,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::LinesCodec;

enum Ui {
    Confirm,
    Question(Vec<Question>),
}

struct Pi {
    resume: Option<String>,
    ready: bool,
    next: u64,
    turns: u64,
    blocks: HashMap<String, String>,
    tools: HashMap<String, ToolCall>,
    ui: HashMap<String, Ui>,
}

impl Pi {
    fn new(resume: Option<String>) -> Self {
        Self {
            resume,
            ready: false,
            next: 1,
            turns: 0,
            blocks: HashMap::new(),
            tools: HashMap::new(),
            ui: HashMap::new(),
        }
    }

    fn id(&mut self) -> String {
        let id = format!("flotilla-{}", self.next);
        self.next += 1;
        id
    }

    fn pending(machine: &mut Machine, id: &str, kind: PendingKind, reply: Option<oneshot::Sender<Result<()>>>) {
        machine.pending.insert(
            id.into(),
            Pending {
                kind,
                reply,
                started: Instant::now(),
            },
        );
    }

    fn block(&mut self, machine: &Machine, index: &Value) -> String {
        let key = format!(
            "{}:{}",
            machine.active_turn.as_deref().unwrap_or("idle"),
            index.as_u64().unwrap_or_default()
        );
        let next = self.blocks.len();
        self.blocks
            .entry(key)
            .or_insert_with(|| format!("pi:{next}"))
            .clone()
    }

    fn usage(&self, value: &Value) -> Usage {
        let usage = &value["usage"];
        Usage {
            input_tokens: usage["input"].as_u64(),
            output_tokens: usage["output"].as_u64(),
            cache_read_tokens: usage["cacheRead"].as_u64(),
            cache_write_tokens: usage["cacheWrite"].as_u64(),
            cost_usd: usage["cost"]["total"]
                .as_f64()
                .map(|value| format!("{value:.6}")),
        }
    }

    fn event(&mut self, machine: &mut Machine, value: &Value) -> Result<()> {
        match value["type"].as_str().unwrap_or_default() {
            "agent_start" => {
                if machine.active_turn.is_none() {
                    machine.active_turn = Some(uuid::Uuid::new_v4().to_string());
                    machine.turn_started = Some(Instant::now());
                }
                machine.set_state(SessionState::Running)?;
            }
            "message_start" => {
                let message = &value["message"];
                if message["role"] == "assistant"
                    && machine.info.model.is_none()
                    && let Some(model) = message["model"].as_str()
                {
                    machine.info.model = Some(model.into());
                }
            }
            "message_update" => self.delta(machine, &value["assistantMessageEvent"])?,
            "message_end" => {
                if value["message"]["role"] == "assistant" {
                    machine.usage = self.usage(&value["message"]);
                }
            }
            "tool_execution_start" => {
                let id = nonempty(value, "toolCallId")?;
                let name = nonempty(value, "toolName")?;
                let tool = ToolCall {
                    id: id.clone(),
                    kind: tool_kind(&name),
                    title: name,
                    input_summary: bounded(value["args"].to_string()),
                    status: ToolStatus::Running,
                    output: None,
                    diff: None,
                };
                self.tools.insert(id, tool.clone());
                machine.emit(ChatEvent::ToolCall(tool))?;
            }
            "tool_execution_update" => {
                let id = nonempty(value, "toolCallId")?;
                let output = bounded(host::text_content(&value["partialResult"]));
                if let Some(tool) = self.tools.get_mut(&id) {
                    tool.output = Some(output.clone());
                }
                machine.emit(ChatEvent::ToolOutput { id, delta: output })?;
            }
            "tool_execution_end" => {
                let id = nonempty(value, "toolCallId")?;
                let name = string(value, "toolName");
                let output = host::text_content(&value["result"]);
                let mut tool = self.tools.remove(&id).unwrap_or_else(|| ToolCall {
                    id: id.clone(),
                    kind: tool_kind(&name),
                    title: name,
                    input_summary: bounded(""),
                    status: ToolStatus::Running,
                    output: None,
                    diff: None,
                });
                tool.status = if value["isError"].as_bool().unwrap_or(false) {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Completed
                };
                if !output.is_empty() {
                    tool.output = Some(bounded(output));
                }
                machine.emit(ChatEvent::ToolCall(tool))?;
            }
            "turn_end" => {
                self.turns += 1;
                machine.usage = self.usage(&value["message"]);
                if let Some(model) = value["message"]["model"].as_str() {
                    machine.info.model = Some(model.into());
                }
            }
            "agent_end" => {
                if value["willRetry"].as_bool().unwrap_or(false) {
                    machine.emit(ChatEvent::Diagnostic {
                        stream: "pi".into(),
                        output: bounded("agent run will retry"),
                    })?;
                }
            }
            "agent_settled" => {
                self.ui.clear();
                self.tools.clear();
                if machine.active_turn.is_some() {
                    let duration = machine
                        .turn_started
                        .map(|started| started.elapsed().as_millis().min(u64::MAX as u128) as u64);
                    machine.end_turn(
                        duration,
                        self.turns.max(1),
                        if value["aborted"].as_bool().unwrap_or(false) {
                            TurnOutcome::Cancelled
                        } else {
                            TurnOutcome::Completed
                        },
                    )?;
                }
                self.turns = 0;
            }
            "extension_error" => machine.emit(ChatEvent::Error {
                code: "extension_failed".into(),
                message: bounded(value.to_string()).text,
                fatal: false,
            })?,
            "auto_retry_start" | "auto_retry_end" | "compaction_start" | "compaction_end"
            | "summarization_retry_scheduled" | "summarization_retry_attempt_start"
            | "summarization_retry_finished" | "queue_update" | "entry_appended"
            | "session_info_changed" | "thinking_level_changed" | "bash_execution_update" => {}
            other => machine.emit(ChatEvent::Diagnostic {
                stream: "pi".into(),
                output: bounded(format!("unhandled Pi event {other}")),
            })?,
        }
        Ok(())
    }

    fn delta(&mut self, machine: &mut Machine, event: &Value) -> Result<()> {
        let index = &event["contentIndex"];
        match event["type"].as_str().unwrap_or_default() {
            "text_start" | "thinking_start" | "toolcall_start" => {
                if event["type"] == "toolcall_start" {
                    let id = event["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| AdapterError::Protocol("Pi tool call lacks id".into()))?
                        .to_owned();
                    let name = string(event, "toolName");
                    machine.emit(ChatEvent::ToolCall(ToolCall {
                        id,
                        kind: tool_kind(&name),
                        title: name,
                        input_summary: bounded(""),
                        status: ToolStatus::Running,
                        output: None,
                        diff: None,
                    }))?;
                }
            }
            "text_delta" => {
                let id = self.block(machine, index);
                machine.emit(ChatEvent::AssistantText {
                    id,
                    update: TextUpdate::Delta,
                    text: string(event, "delta"),
                })?;
            }
            "text_end" => {
                let id = self.block(machine, index);
                machine.emit(ChatEvent::AssistantText {
                    id,
                    update: TextUpdate::Final,
                    text: string(event, "content"),
                })?;
            }
            "thinking_delta" => {
                let id = self.block(machine, index);
                machine.emit(ChatEvent::Thinking {
                    id,
                    delta: string(event, "delta"),
                })?;
            }
            "thinking_end" => {
                let id = self.block(machine, index);
                machine.emit(ChatEvent::Thinking {
                    id,
                    delta: string(event, "content"),
                })?;
            }
            "toolcall_end" => {
                let call = &event["toolCall"];
                let id = call["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| AdapterError::Protocol("Pi tool call result lacks id".into()))?
                    .to_owned();
                let name = string(call, "name");
                machine.emit(ChatEvent::ToolCall(ToolCall {
                    id,
                    kind: tool_kind(&name),
                    title: name,
                    input_summary: bounded(call["arguments"].to_string()),
                    status: ToolStatus::Running,
                    output: None,
                    diff: None,
                }))?;
            }
            "toolcall_delta" => {}
            "error" => machine.emit(ChatEvent::Error {
                code: "provider_stream_error".into(),
                message: bounded(event.to_string()).text,
                fatal: false,
            })?,
            "done" => {}
            other => machine.emit(ChatEvent::Diagnostic {
                stream: "pi".into(),
                output: bounded(format!("unhandled Pi assistant event {other}")),
            })?,
        }
        Ok(())
    }

    fn ui_request(&mut self, machine: &mut Machine, value: &Value) -> Result<Vec<Value>> {
        let wire = value
            .get("id")
            .filter(|id| id.is_string() || id.is_number())
            .cloned()
            .ok_or_else(|| AdapterError::Protocol("Pi extension UI request lacks id".into()))?;
        let method = string(value, "method");
        match method.as_str() {
            "confirm" => {
                let title = string(value, "title");
                let message = string(value, "message");
                let id = machine.request_id(wire, Interaction::CodexElicitation)?;
                self.ui.insert(id.clone(), Ui::Confirm);
                machine.emit(ChatEvent::ApprovalRequest {
                    id,
                    tool: "confirm".into(),
                    details: bounded(format!("{title}\n{message}")),
                    options: vec![
                        ApprovalDecision::AllowOnce,
                        ApprovalDecision::Deny,
                        ApprovalDecision::Cancel,
                    ],
                })?;
                machine.set_state(SessionState::AwaitingInput)?;
            }
            "select" => {
                let options: Vec<QuestionOption> = value["options"]
                    .as_array()
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|label| QuestionOption {
                                label: label.into(),
                                description: String::new(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if options.is_empty() || options.len() > 32 {
                    return Err(AdapterError::Protocol(
                        "Pi select request has no bounded options".into(),
                    ));
                }
                let questions = vec![Question {
                    id: "value".into(),
                    text: string(value, "title"),
                    options,
                    multi: false,
                    free_text: false,
                    secret: false,
                }];
                let id = machine.request_id(wire, Interaction::CodexElicitation)?;
                self.ui.insert(id.clone(), Ui::Question(questions.clone()));
                machine.emit(ChatEvent::QuestionRequest {
                    id,
                    questions,
                    schema_json: None,
                })?;
                machine.set_state(SessionState::AwaitingInput)?;
            }
            "input" | "editor" => {
                let questions = vec![Question {
                    id: "value".into(),
                    text: string(value, "title"),
                    options: Vec::new(),
                    multi: false,
                    free_text: true,
                    secret: false,
                }];
                let id = machine.request_id(wire, Interaction::CodexElicitation)?;
                self.ui.insert(id.clone(), Ui::Question(questions.clone()));
                machine.emit(ChatEvent::QuestionRequest {
                    id,
                    questions,
                    schema_json: None,
                })?;
                machine.set_state(SessionState::AwaitingInput)?;
            }
            "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text" => {
                machine.emit(ChatEvent::Diagnostic {
                    stream: "pi-ui".into(),
                    output: bounded(value.to_string()),
                })?;
            }
            other => {
                return Err(AdapterError::Unsupported(format!(
                    "unsupported Pi extension UI method {other}"
                )));
            }
        }
        Ok(Vec::new())
    }
}

impl Wire for Pi {
    fn start(&mut self, machine: &mut Machine) -> Result<Vec<Value>> {
        let id = "init".to_owned();
        Self::pending(machine, &id, PendingKind::Initialize, None);
        Ok(vec![json!({"id": id, "type": "get_state"})])
    }

    fn incoming(&mut self, machine: &mut Machine, value: Value) -> Result<Vec<Value>> {
        match value["type"].as_str().unwrap_or_default() {
            "ready" => {
                let version = value["protocolVersion"].as_u64().unwrap_or_default();
                let supported = value["supportedProtocolVersions"]
                    .as_array()
                    .map(|versions| versions.iter().any(|version| version == 1))
                    .unwrap_or(version == 1);
                if !supported {
                    return Err(AdapterError::Unsupported(format!(
                        "RPC peer does not support protocol 1 (announced {version})"
                    )));
                }
                Ok(Vec::new())
            }
            "response" => self.response(machine, &value),
            "extension_ui_request" => self.ui_request(machine, &value),
            _ => {
                self.event(machine, &value)?;
                Ok(Vec::new())
            }
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
                    Err(AdapterError::Closed("Pi session is not ready".into()))
                } else {
                    Ok(())
                }
            }),
            Operation::Steer(text) => {
                valid_text(text).and_then(|_| {
                    if machine.active_turn.is_none() {
                        Err(AdapterError::NoActiveTurn)
                    } else {
                        Ok(())
                    }
                })
            }
            Operation::Cancel => {
                if machine.active_turn.is_none() {
                    Err(AdapterError::NoActiveTurn)
                } else {
                    Ok(())
                }
            }
            Operation::Approve(id, _) => match self.ui.get(id) {
                Some(Ui::Confirm) => Ok(()),
                _ => Err(AdapterError::UnknownRequest(id.clone())),
            },
            Operation::Question(id, response) => match self.ui.get(id) {
                Some(Ui::Question(questions)) => checked_answers(questions, response),
                _ => Err(AdapterError::UnknownRequest(id.clone())),
            },
            Operation::Shutdown => unreachable!("shutdown is handled by runtime"),
        };
        if let Err(error) = validation {
            let _ = reply.send(Err(error));
            return Ok(Vec::new());
        }
        match operation {
            Operation::Send(text) => {
                let turn = uuid::Uuid::new_v4().to_string();
                machine.active_turn = Some(turn);
                machine.turn_started = Some(Instant::now());
                self.turns = 0;
                self.blocks.clear();
                self.tools.clear();
                machine.user_event(
                    machine.active_turn.clone().unwrap_or_default(),
                    text.clone(),
                )?;
                machine.set_state(SessionState::Running)?;
                let id = self.id();
                let turn = machine.active_turn.clone().unwrap_or_default();
                Self::pending(
                    machine,
                    &id,
                    PendingKind::Send {
                        id: turn,
                        text: text.clone(),
                    },
                    Some(reply),
                );
                Ok(vec![json!({"id": id, "type": "prompt", "message": text})])
            }
            Operation::Steer(text) => {
                let id = self.id();
                let turn = machine.active_turn.clone().unwrap_or_default();
                Self::pending(
                    machine,
                    &id,
                    PendingKind::Steer {
                        id: turn,
                        text: text.clone(),
                    },
                    Some(reply),
                );
                Ok(vec![json!({"id": id, "type": "steer", "message": text})])
            }
            Operation::Cancel => {
                let id = self.id();
                Self::pending(machine, &id, PendingKind::Control, Some(reply));
                Ok(vec![json!({"id": id, "type": "abort"})])
            }
            Operation::Approve(id, decision) => {
                let request = machine
                    .requests
                    .remove(&id)
                    .ok_or_else(|| AdapterError::UnknownRequest(id.clone()))?;
                self.ui.remove(&id);
                let response = match decision {
                    ApprovalDecision::AllowOnce => {
                        json!({"type": "extension_ui_response", "id": request.wire_id, "confirmed": true})
                    }
                    ApprovalDecision::Deny => {
                        json!({"type": "extension_ui_response", "id": request.wire_id, "confirmed": false})
                    }
                    ApprovalDecision::Cancel => {
                        json!({"type": "extension_ui_response", "id": request.wire_id, "cancelled": true})
                    }
                };
                machine.emit(ChatEvent::RequestResolved { id })?;
                machine.set_state(if machine.requests.is_empty() {
                    SessionState::Running
                } else {
                    SessionState::AwaitingInput
                })?;
                machine.delivered(reply);
                Ok(vec![response])
            }
            Operation::Question(id, response) => {
                let request = machine
                    .requests
                    .remove(&id)
                    .ok_or_else(|| AdapterError::UnknownRequest(id.clone()))?;
                let ui = self
                    .ui
                    .remove(&id)
                    .ok_or_else(|| AdapterError::UnknownRequest(id.clone()))?;
                let value = match ui {
                    Ui::Question(questions) => {
                        let question = questions
                            .first()
                            .ok_or_else(|| AdapterError::UnknownRequest(id.clone()))?;
                        response
                            .answers
                            .get(&question.id)
                            .and_then(|answers| answers.first())
                            .cloned()
                            .ok_or_else(|| {
                                AdapterError::InvalidConfig("missing Pi answer".into())
                            })?
                    }
                    Ui::Confirm => {
                        return Err(AdapterError::UnknownRequest(id));
                    }
                };
                machine.emit(ChatEvent::RequestResolved { id })?;
                machine.set_state(if machine.requests.is_empty() {
                    SessionState::Running
                } else {
                    SessionState::AwaitingInput
                })?;
                machine.delivered(reply);
                Ok(vec![json!({
                    "type": "extension_ui_response", "id": request.wire_id, "value": value
                })])
            }
            Operation::Shutdown => unreachable!(),
        }
    }
}

impl Pi {
    fn response(&mut self, machine: &mut Machine, value: &Value) -> Result<Vec<Value>> {
        let id = match value["id"].as_str() {
            Some(id) => id.to_owned(),
            None => value["id"].to_string(),
        };
        let Some(pending) = machine.pending.remove(&id) else {
            return Err(AdapterError::Protocol(format!(
                "unmatched Pi response id {id}"
            )));
        };
        let success = value["success"].as_bool().unwrap_or(false);
        if !success {
            let message = bounded(value["error"].to_string()).text;
            if matches!(&pending.kind, PendingKind::Initialize) {
                return Err(AdapterError::Unsupported(message));
            }
            if let Some(reply) = pending.reply {
                let _ = reply.send(Err(AdapterError::Rejected(message.clone())));
            }
            if matches!(&pending.kind, PendingKind::Send { .. }) {
                self.ui.clear();
                machine.emit(ChatEvent::Error {
                    code: "provider_prompt_rejected".into(),
                    message,
                    fatal: false,
                })?;
                return machine.end_turn(None, 1, TurnOutcome::Failed).map(|_| Vec::new());
            }
            return Ok(Vec::new());
        }
        match pending.kind {
            PendingKind::Initialize => {
                let data = &value["data"];
                let session = data["sessionId"]
                    .as_str()
                    .filter(|session| !session.is_empty())
                    .ok_or_else(|| AdapterError::Protocol("Pi get_state lacks sessionId".into()))?
                    .to_owned();
                new_session_id(&session)?;
                if let Some(target) = &self.resume {
                    if target != &session {
                        return Err(AdapterError::InvalidResume(
                            "Pi resumed a different session ID".into(),
                        ));
                    }
                }
                if machine.info.model.is_none()
                    && let Some(model) = data["model"]["id"].as_str()
                {
                    machine.info.model = Some(model.into());
                }
                self.ready = true;
                if data["isStreaming"].as_bool().unwrap_or(false) {
                    machine.active_turn = Some(uuid::Uuid::new_v4().to_string());
                    machine.turn_started = Some(Instant::now());
                }
                machine.session_ready(session).map(|_| Vec::new())
            }
            PendingKind::Send { .. } => {
                if let Some(reply) = pending.reply {
                    let _ = reply.send(Ok(()));
                }
                if value["data"]["disposition"] == "handled" {
                    self.ui.clear();
                    machine.end_turn(None, 1, TurnOutcome::Completed)?;
                }
                Ok(Vec::new())
            }
            PendingKind::Steer { .. } | PendingKind::Control => {
                if let Some(reply) = pending.reply {
                    let _ = reply.send(Ok(()));
                }
                Ok(Vec::new())
            }
            _ => Err(AdapterError::Protocol(
                "unexpected Pi response kind".into(),
            )),
        }
    }
}

fn new_session_id(session: &str) -> Result<()> {
    host::uuid(session, "Pi")
}

fn is_omp(provider: &ProviderSpec) -> bool {
    provider.id == "omp"
        || provider
            .command
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "omp")
}

pub(crate) async fn launch(provider: ProviderSpec, config: SessionConfig) -> Result<Adapter> {
    let config = host::prepare(&provider, config).await?;
    if config.permission_mode == PermissionMode::ReadOnly {
        return Err(AdapterError::InvalidConfig(
            "Pi/OMP rpc cannot enforce read-only tools; use terminal or a provider policy".into(),
        ));
    }
    if config.output_schema_json.is_some() {
        return Err(AdapterError::Unsupported(
            "Pi/OMP rpc has no structured-output flag".into(),
        ));
    }
    if config.provider_config_json.is_some() {
        return Err(AdapterError::Unsupported(
            "Pi/OMP rpc settings go through provider args or env".into(),
        ));
    }
    let resume = config.resume.as_ref().map(|target| target.native_id.clone());
    if let Some(resume) = &resume {
        host::uuid(resume, "Pi")?;
    }
    let omp = is_omp(&provider);
    let info = SessionInfo {
        id: config.session_id.unwrap_or_else(uuid::Uuid::new_v4),
        incarnation: uuid::Uuid::new_v4(),
        resume: ResumeTarget {
            provider_id: provider.id.clone(),
            adapter: provider.adapter,
            native_id: resume.clone().unwrap_or_default(),
            cwd: config.cwd.clone(),
        },
        model: config.model.clone(),
        resumed: resume.is_some(),
    };
    let mut command = host::command(&provider);
    command.current_dir(&config.cwd);
    command.args(["--mode", "rpc"]);
    if let Some(resume) = &resume {
        if omp {
            command.arg(format!("--resume={resume}"));
        } else {
            command.arg("--session").arg(resume);
        }
    }
    if let Some(model) = &config.model {
        command.arg("--model").arg(model);
    }
    if let Some(instructions) = &config.instructions {
        command.arg("--append-system-prompt").arg(instructions);
    }
    let child = command.spawn()?;
    let (mut machine, events) = host::machine(provider, config, info);
    let state = machine.state.subscribe();
    let (commands, receiver) = mpsc::channel(32);
    let (ready, ready_rx) = oneshot::channel();
    machine.ready = Some(ready);
    tokio::spawn(host::serve(
        child,
        machine,
        receiver,
        Box::new(Pi::new(resume)),
    ));
    let info = tokio::time::timeout(process::OP_TIMEOUT, ready_rx)
        .await
        .map_err(|_| AdapterError::Timeout)?
        .map_err(|_| AdapterError::Closed("Pi initialization task exited".into()))??;
    Ok(Adapter {
        info,
        events,
        commands,
        state,
    })
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
    for required in ["--mode", "rpc"] {
        if !help.contains(required) {
            return Err(AdapterError::Unsupported(format!(
                "installed CLI help lacks {required}"
            )));
        }
    }
    let mut command = host::command(provider);
    command.args(["--mode", "rpc", "--no-session"]);
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
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
    let attempt = tokio::time::timeout(host::PROBE_TIMEOUT, async {
        host::write(&mut input, &json!({"id": "init", "type": "get_state"})).await?;
        loop {
            let value = process::parse_line(lines.next().await)?;
            if value["type"] == "ready" {
                continue;
            }
            if value["type"] == "response" && value["id"] == "init" {
                if !value["success"].as_bool().unwrap_or(false) {
                    return Err(AdapterError::Unsupported(
                        bounded(value["error"].to_string()).text,
                    ));
                }
                let session = value["data"]["sessionId"]
                    .as_str()
                    .filter(|session| !session.is_empty())
                    .ok_or_else(|| AdapterError::Unsupported("get_state lacks sessionId".into()))?;
                new_session_id(session)?;
                return Ok(());
            }
            // OMP emits fire-and-forget extension UI records (setWidget) before
            // the get_state response; they do not gate discovery.
            if value["type"] == "extension_ui_request" {
                continue;
            }
        }
    })
    .await;
    drop(input);
    guard.kill();
    let _ = child.start_kill();
    let _ = child.wait().await;
    attempt.map_err(|_| AdapterError::Timeout)??;
    result.version = Some(version);
    result.capabilities = Capabilities {
        chat: true,
        resume: true,
        steer: true,
        approvals: false,
        questions: true,
        partial_messages: true,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host;
    use tokio::sync::mpsc;

    fn provider() -> ProviderSpec {
        ProviderSpec {
            id: "pi".into(),
            name: "Pi".into(),
            command: "pi".into(),
            args: Vec::new(),
            env: Default::default(),
            adapter: AdapterKind::PiRpc,
            resume: ResumeStrategy::PiSession,
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
                provider_id: "pi".into(),
                adapter: AdapterKind::PiRpc,
                native_id: String::new(),
                cwd: config.cwd.clone(),
            },
            model: None,
            resumed: false,
        };
        host::machine(provider(), config, info)
    }

    fn feed(wire: &mut Pi, machine: &mut Machine, text: &str) {
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            wire.incoming(machine, serde_json::from_str(line).unwrap())
                .unwrap();
        }
    }

    fn ready(fixture: &str) -> (Pi, Machine, mpsc::Receiver<EventRecord>) {
        let (mut machine, events) = machine();
        let mut wire = Pi::new(None);
        wire.start(&mut machine).unwrap();
        feed(&mut wire, &mut machine, fixture);
        assert!(wire.ready);
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
    fn recorded_pi_and_omp_handshakes_establish_a_session() {
        for (fixture, session) in [
            (
                include_str!("../tests/fixtures/pi-handshake.recorded.jsonl"),
                "01a1a6ba-786e-7532-92a7-07306dc69e02",
            ),
            (
                include_str!("../tests/fixtures/omp-handshake.recorded.jsonl"),
                "01a1a6ba-900a-713f-9d67-2f8484310993",
            ),
        ] {
            let (wire, machine, mut events) = ready(fixture);
            assert_eq!(machine.info.resume.native_id, session);
            assert_eq!(wire.resume, None);
            assert!(drain(&mut events).iter().any(|record| matches!(
                record.event,
                ChatEvent::SessionReady(_)
            )));
            assert_eq!(machine.state.borrow().clone(), SessionState::Idle);
        }
    }

    #[test]
    fn synthetic_turn_normalizes_text_thinking_tools_and_usage() {
        let (mut wire, mut machine, mut events) =
            ready(include_str!("../tests/fixtures/pi-handshake.recorded.jsonl"));
        let (reply, _) = tokio::sync::oneshot::channel();
        wire.operation(&mut machine, Operation::Send("pwd".into()), reply)
            .unwrap();
        feed(
            &mut wire,
            &mut machine,
            include_str!("../tests/fixtures/pi-turn.synthetic.jsonl"),
        );
        let records = drain(&mut events);
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
            ChatEvent::ToolCall(call) if call.id == "call_1" && call.status == ToolStatus::Completed && call.output.as_ref().is_some_and(|output| output.text == "/tmp")
        )));
        assert!(records.iter().any(|record| matches!(
            &record.event,
            ChatEvent::TurnEnd {
                usage,
                outcome: TurnOutcome::Completed,
                turns: 1,
                ..
            } if usage.input_tokens == Some(12) && usage.output_tokens == Some(3) && usage.cost_usd.as_deref() == Some("0.001000")
        )));
    }

    #[test]
    fn extension_confirmation_requires_an_explicit_denial() {
        let (mut wire, mut machine, mut events) =
            ready(include_str!("../tests/fixtures/pi-handshake.recorded.jsonl"));
        wire.incoming(
            &mut machine,
            serde_json::json!({
                "type": "extension_ui_request", "id": "ui-1", "method": "confirm",
                "title": "Allow dangerous command?", "message": "rm -rf"
            }),
        )
        .unwrap();
        let id = drain(&mut events)
            .into_iter()
            .find_map(|record| match record.event {
                ChatEvent::ApprovalRequest { id, options, .. } => {
                    assert_eq!(
                        options,
                        vec![
                            ApprovalDecision::AllowOnce,
                            ApprovalDecision::Deny,
                            ApprovalDecision::Cancel
                        ]
                    );
                    Some(id)
                }
                _ => None,
            })
            .expect("approval card");
        let (reply, _) = tokio::sync::oneshot::channel();
        let out = wire
            .operation(
                &mut machine,
                Operation::Approve(id.clone(), ApprovalDecision::Deny),
                reply,
            )
            .unwrap();
        assert_eq!(out[0]["confirmed"], false);
        assert_eq!(out[0]["type"], "extension_ui_response");
        let (reply, mut receive) = tokio::sync::oneshot::channel();
        assert!(
            wire.operation(
                &mut machine,
                Operation::Approve(id, ApprovalDecision::AllowOnce),
                reply
            )
            .unwrap()
            .is_empty()
        );
        assert!(matches!(
            receive.try_recv().unwrap(),
            Err(AdapterError::UnknownRequest(_))
        ));
    }

    #[test]
    fn resume_identity_mismatch_is_explicit() {
        let (mut machine, _) = machine();
        let mut wire = Pi::new(Some("11111111-1111-4111-8111-111111111111".into()));
        wire.start(&mut machine).unwrap();
        assert!(matches!(
            wire.incoming(
                &mut machine,
                serde_json::from_str(
                    include_str!("../tests/fixtures/pi-handshake.recorded.jsonl")
                )
                .unwrap()
            ),
            Err(AdapterError::InvalidResume(_))
        ));
    }

    #[test]
    fn rejected_prompt_ends_the_turn_as_failed() {
        let (mut wire, mut machine, mut events) =
            ready(include_str!("../tests/fixtures/pi-handshake.recorded.jsonl"));
        let (reply, _) = tokio::sync::oneshot::channel();
        wire.operation(&mut machine, Operation::Send("hi".into()), reply)
            .unwrap();
        let id = machine
            .pending
            .keys()
            .find(|id| id.as_str() != "init")
            .cloned()
            .unwrap();
        wire.incoming(
            &mut machine,
            serde_json::json!({"id": id, "type": "response", "success": false, "error": "model missing"}),
        )
        .unwrap();
        assert!(drain(&mut events).iter().any(|record| matches!(
            record.event,
            ChatEvent::TurnEnd {
                outcome: TurnOutcome::Failed,
                ..
            }
        )));
    }
}
