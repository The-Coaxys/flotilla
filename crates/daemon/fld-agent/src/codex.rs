use crate::{
    process::{Interaction, Machine, PendingKind, Request, checked_answers, nonempty, valid_text},
    *,
};
use serde_json::{Value, json};
use std::time::Instant;
use tokio::sync::oneshot;

fn rpc_error(id: Value, message: &str) -> Value {
    json!({"id":id,"error":{"code":-32601,"message":message}})
}
fn rpc_id(value: &Value) -> Result<String> {
    match value {
        Value::String(s) if !s.is_empty() => Ok(s.clone()),
        Value::Number(n) if n.is_u64() || n.is_i64() => Ok(n.to_string()),
        _ => Err(AdapterError::Protocol("invalid JSON-RPC request ID".into())),
    }
}
fn questions(params: &Value) -> Result<Vec<Question>> {
    let questions = params["questions"]
        .as_array()
        .filter(|q| !q.is_empty() && q.len() <= 32)
        .ok_or_else(|| AdapterError::Protocol("invalid Codex questions".into()))?;
    questions
        .iter()
        .map(|q| {
            Ok(Question {
                id: nonempty(q, "id")?,
                text: nonempty(q, "question")?,
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
                multi: false,
                free_text: q["isOther"].as_bool().unwrap_or(false) || q["options"].is_null(),
                secret: q["isSecret"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

impl Machine {
    fn codex_open(&mut self) -> Result<Value> {
        let mut params = json!({"cwd":self.config.cwd,"model":self.config.model,"developerInstructions":self.config.instructions,"config":self.config_json,
            "approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":if self.config.permission_mode == PermissionMode::ReadOnly { "read-only" } else { "workspace-write" }});
        let method = if self.config.resume.is_some() {
            params["threadId"] = json!(self.info.resume.native_id);
            "thread/resume"
        } else {
            "thread/start"
        };
        self.rpc(PendingKind::OpenSession, method, params, None)
    }

    pub(crate) fn codex_operation(
        &mut self,
        operation: Operation,
        reply: oneshot::Sender<Result<()>>,
    ) -> Result<Vec<Value>> {
        let validation = match &operation {
            Operation::Send(text) => valid_text(text).and_then(|_| if self.active_turn.is_some() || self.starting_turn { Err(AdapterError::Busy) } else { Ok(()) }),
            Operation::Steer(text) => valid_text(text).and_then(|_| if self.active_turn.is_none() { Err(AdapterError::NoActiveTurn) } else { Ok(()) }),
            Operation::Cancel => if self.active_turn.is_none() { Err(AdapterError::NoActiveTurn) } else { Ok(()) },
            Operation::Approve(id, decision) => match self.requests.get(id) {
                Some(Request { interaction: Interaction::CodexApproval { .. }, .. }) => Ok(()),
                Some(Request { interaction: Interaction::CodexElicitation | Interaction::CodexQuestion { .. }, .. }) if *decision != ApprovalDecision::AllowOnce => Ok(()),
                _ => Err(AdapterError::UnknownRequest(id.clone())),
            },
            Operation::Question(id, response) => match self.requests.get(id) {
                Some(Request { interaction: Interaction::CodexQuestion { questions }, .. }) => checked_answers(questions, response),
                Some(Request { interaction: Interaction::CodexElicitation, .. }) => match &response.structured_json {
                    Some(json) if response.answers.is_empty() && serde_json::from_str::<Value>(json).is_ok() && json.len() <= MAX_TEXT => Ok(()),
                    _ => Err(AdapterError::InvalidConfig("MCP elicitation requires bounded, explicit structured_json and no text answers".into())),
                },
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
                let id = uuid::Uuid::new_v4().to_string();
                let mut params = json!({"threadId":self.info.resume.native_id,"input":[{"type":"text","text":text}],"clientUserMessageId":id,"approvalPolicy":"untrusted","approvalsReviewer":"user"});
                if let Some(schema) = &self.config.output_schema_json {
                    params["outputSchema"] = serde_json::from_str(schema)
                        .map_err(|e| AdapterError::InvalidConfig(e.to_string()))?;
                }
                self.starting_turn = true;
                self.turn_started = Some(Instant::now());
                self.set_state(SessionState::Running)?;
                Ok(vec![self.rpc(
                    PendingKind::Send { id, text },
                    "turn/start",
                    params,
                    Some(reply),
                )?])
            }
            Operation::Steer(text) => {
                let id = uuid::Uuid::new_v4().to_string();
                let params = json!({"threadId":self.info.resume.native_id,"expectedTurnId":self.active_turn,"input":[{"type":"text","text":text}],"clientUserMessageId":id});
                Ok(vec![self.rpc(
                    PendingKind::Steer { id, text },
                    "turn/steer",
                    params,
                    Some(reply),
                )?])
            }
            Operation::Cancel => {
                self.clear_requests()?;
                let params =
                    json!({"threadId":self.info.resume.native_id,"turnId":self.active_turn});
                Ok(vec![self.rpc(
                    PendingKind::Control,
                    "turn/interrupt",
                    params,
                    Some(reply),
                )?])
            }
            Operation::Approve(id, decision) => {
                let request = self
                    .requests
                    .remove(&id)
                    .ok_or_else(|| AdapterError::UnknownRequest(id.clone()))?;
                let response = match request.interaction {
                    Interaction::CodexApproval { method, params }
                        if method == "item/permissions/requestApproval" =>
                    {
                        json!({"permissions":if decision == ApprovalDecision::AllowOnce { params["permissions"].clone() } else { json!({}) },"scope":"turn"})
                    }
                    Interaction::CodexApproval { .. } => {
                        json!({"decision":match decision { ApprovalDecision::AllowOnce => "accept", ApprovalDecision::Deny => "decline", ApprovalDecision::Cancel => "cancel" }})
                    }
                    Interaction::CodexQuestion { .. } => json!({"answers":{}}),
                    Interaction::CodexElicitation => {
                        json!({"action":if decision == ApprovalDecision::Cancel { "cancel" } else { "decline" }})
                    }
                    _ => unreachable!(),
                };
                self.emit(ChatEvent::RequestResolved { id })?;
                self.set_state(if self.requests.is_empty() {
                    SessionState::Running
                } else {
                    SessionState::AwaitingInput
                })?;
                self.delivered(reply);
                let mut messages = vec![json!({"id":request.wire_id,"result":response})];
                // Permission grants and questions have no cancel decision in their
                // response schema. Interrupt separately when explicitly requested.
                if decision == ApprovalDecision::Cancel && self.active_turn.is_some() {
                    messages.push(self.rpc(
                        PendingKind::Control,
                        "turn/interrupt",
                        json!({"threadId":self.info.resume.native_id,"turnId":self.active_turn}),
                        None,
                    )?);
                }
                Ok(messages)
            }
            Operation::Question(id, response) => {
                let request = self
                    .requests
                    .remove(&id)
                    .ok_or_else(|| AdapterError::UnknownRequest(id.clone()))?;
                let response = match request.interaction {
                    Interaction::CodexQuestion { .. } => {
                        let answers: serde_json::Map<_, _> = response
                            .answers
                            .into_iter()
                            .map(|(id, answers)| (id, json!({"answers":answers})))
                            .collect();
                        json!({"answers":answers})
                    }
                    Interaction::CodexElicitation => {
                        json!({"action":"accept","content":serde_json::from_str::<Value>(response.structured_json.as_deref().unwrap_or("null")).map_err(|e| AdapterError::InvalidConfig(e.to_string()))?})
                    }
                    _ => unreachable!(),
                };
                self.emit(ChatEvent::RequestResolved { id })?;
                self.set_state(if self.requests.is_empty() {
                    SessionState::Running
                } else {
                    SessionState::AwaitingInput
                })?;
                self.delivered(reply);
                Ok(vec![json!({"id":request.wire_id,"result":response})])
            }
            Operation::Shutdown => unreachable!(),
        }
    }

    pub(crate) fn codex_incoming(&mut self, value: Value) -> Result<Vec<Value>> {
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            if let Some(id) = value.get("id") {
                rpc_id(id)?;
                let params = value.get("params").unwrap_or(&Value::Null);
                self.event_parent = params["threadId"]
                    .as_str()
                    .and_then(|thread| self.subagent_parents.get(thread))
                    .cloned();
                let result = self.codex_server_request(method, id.clone(), params);
                self.event_parent = None;
                return result;
            }
            return self.codex_notification(method, value.get("params").unwrap_or(&Value::Null));
        }
        let id = rpc_id(value.get("id").ok_or_else(|| {
            AdapterError::Protocol("Codex message lacks method or response ID".into())
        })?)?;
        let pending = self
            .pending
            .remove(&id)
            .ok_or_else(|| AdapterError::Protocol("unmatched Codex RPC response".into()))?;
        if let Some(error) = value.get("error") {
            let message = bounded(error.to_string()).text;
            match pending.kind {
                PendingKind::Initialize => return Err(AdapterError::Unsupported(message)),
                PendingKind::ReadResume | PendingKind::OpenSession => {
                    return Err(if self.info.resumed {
                        AdapterError::InvalidResume(message)
                    } else {
                        AdapterError::Rejected(message)
                    });
                }
                PendingKind::Send { .. } => {
                    self.starting_turn = false;
                    self.active_turn = None;
                    self.set_state(SessionState::Idle)?;
                }
                _ => {}
            }
            self.emit(ChatEvent::Error {
                code: "rpc_rejected".into(),
                message: message.clone(),
                fatal: false,
            })?;
            if let Some(reply) = pending.reply {
                let _ = reply.send(Err(AdapterError::Rejected(message)));
            }
            return Ok(Vec::new());
        }
        let result = value.get("result").ok_or_else(|| {
            AdapterError::Protocol("Codex RPC response lacks result or error".into())
        })?;
        let mut messages = Vec::new();
        match pending.kind {
            PendingKind::Initialize => {
                if !result.is_object() {
                    return Err(AdapterError::Unsupported(
                        "invalid initialize response".into(),
                    ));
                }
                messages.push(json!({"method":"initialized"}));
                if self.config.resume.is_some() {
                    messages.push(self.rpc(
                        PendingKind::ReadResume,
                        "thread/read",
                        json!({"threadId":self.info.resume.native_id,"includeTurns":false}),
                        None,
                    )?);
                } else {
                    messages.push(self.codex_open()?);
                }
            }
            PendingKind::ReadResume => {
                let thread = &result["thread"];
                if thread["id"] != self.info.resume.native_id
                    || thread["cwd"].as_str() != self.config.cwd.to_str()
                {
                    return Err(AdapterError::InvalidResume(
                        "Codex stored thread identity/cwd do not match resume target".into(),
                    ));
                }
                messages.push(self.codex_open()?);
            }
            PendingKind::OpenSession => {
                let native = nonempty(&result["thread"], "id")?;
                if self.info.resumed && native != self.info.resume.native_id {
                    return Err(AdapterError::InvalidResume(
                        "Codex resumed a different thread".into(),
                    ));
                }
                if let Some(cwd) = result["thread"]["cwd"].as_str()
                    && Some(cwd) != self.config.cwd.to_str()
                {
                    return Err(AdapterError::InvalidResume(
                        "Codex thread cwd changed".into(),
                    ));
                }
                if let Some(model) = result["model"].as_str() {
                    self.info.model = Some(model.into());
                }
                self.session_ready(native)?;
            }
            PendingKind::Send { id, text } => {
                let turn = nonempty(&result["turn"], "id")?;
                if self
                    .active_turn
                    .as_ref()
                    .is_some_and(|active| active != &turn)
                {
                    return Err(AdapterError::Protocol(
                        "turn/start response conflicts with active turn".into(),
                    ));
                }
                self.active_turn = Some(turn);
                self.starting_turn = false;
                self.user_event(id, text)?;
            }
            PendingKind::Steer { id, text } => self.user_event(id, text)?,
            PendingKind::Control => {}
        }
        if let Some(reply) = pending.reply {
            let _ = reply.send(Ok(()));
        }
        Ok(messages)
    }

    fn codex_server_request(
        &mut self,
        method: &str,
        wire_id: Value,
        params: &Value,
    ) -> Result<Vec<Value>> {
        // Only the selected thread and explicitly correlated children belong to
        // this session. Never present approvals for unrelated multiplexed work.
        if let Some(thread) = params["threadId"].as_str()
            && thread != self.info.resume.native_id
            && !self.subagent_parents.contains_key(thread)
        {
            return Ok(vec![rpc_error(wire_id, "request is for an unowned thread")]);
        }
        match method {
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval" => {
                if self.config.permission_mode == PermissionMode::ReadOnly {
                    self.emit(ChatEvent::Error {
                        code: "permission_denied".into(),
                        message: format!("{method} denied in read-only mode"),
                        fatal: false,
                    })?;
                    let result = if method == "item/permissions/requestApproval" {
                        json!({"permissions":{},"scope":"turn"})
                    } else {
                        json!({"decision":"decline"})
                    };
                    return Ok(vec![json!({"id":wire_id,"result":result})]);
                }
                let id = self.request_id(
                    wire_id,
                    Interaction::CodexApproval {
                        method: method.into(),
                        params: params.clone(),
                    },
                )?;
                self.emit(ChatEvent::ApprovalRequest {
                    id,
                    tool: method.into(),
                    details: bounded(params.to_string()),
                    options: vec![
                        ApprovalDecision::AllowOnce,
                        ApprovalDecision::Deny,
                        ApprovalDecision::Cancel,
                    ],
                })?;
                self.set_state(SessionState::AwaitingInput)?;
            }
            "item/tool/requestUserInput" => {
                let questions = questions(params)?;
                let id = self.request_id(
                    wire_id,
                    Interaction::CodexQuestion {
                        questions: questions.clone(),
                    },
                )?;
                self.emit(ChatEvent::QuestionRequest {
                    id,
                    questions,
                    schema_json: None,
                })?;
                self.set_state(SessionState::AwaitingInput)?;
            }
            "mcpServer/elicitation/request" => {
                let id = self.request_id(wire_id, Interaction::CodexElicitation)?;
                self.emit(ChatEvent::QuestionRequest {
                    id,
                    questions: Vec::new(),
                    schema_json: Some(params.to_string()),
                })?;
                self.set_state(SessionState::AwaitingInput)?;
            }
            _ => {
                self.emit(ChatEvent::Error {
                    code: "unsupported_request".into(),
                    message: format!("unsupported Codex request {method}"),
                    fatal: false,
                })?;
                return Ok(vec![rpc_error(
                    wire_id,
                    "Flotilla does not implement this request",
                )]);
            }
        }
        Ok(Vec::new())
    }

    fn codex_notification(&mut self, method: &str, params: &Value) -> Result<Vec<Value>> {
        if let Some(thread) = params["threadId"].as_str()
            && !self.info.resume.native_id.is_empty()
            && thread != self.info.resume.native_id
        {
            return self.codex_child_notification(thread, method, params);
        }
        match method {
            "turn/started" => {
                let id = nonempty(&params["turn"], "id")?;
                if self
                    .active_turn
                    .as_ref()
                    .is_some_and(|active| active != &id)
                {
                    return Err(AdapterError::Protocol(
                        "Codex started overlapping turns".into(),
                    ));
                }
                self.active_turn = Some(id);
                self.starting_turn = false;
                self.turn_started.get_or_insert_with(Instant::now);
                self.set_state(SessionState::Running)?;
            }
            "turn/completed" => {
                let turn = &params["turn"];
                let id = nonempty(turn, "id")?;
                if self
                    .active_turn
                    .as_ref()
                    .is_some_and(|active| active != &id)
                {
                    return Err(AdapterError::Protocol(
                        "Codex completed an unexpected turn".into(),
                    ));
                }
                let outcome = match turn["status"].as_str() {
                    Some("completed") => TurnOutcome::Completed,
                    Some("interrupted") => TurnOutcome::Cancelled,
                    Some("failed") => {
                        self.emit(ChatEvent::Error {
                            code: "provider_turn_failed".into(),
                            message: bounded(turn["error"].to_string()).text,
                            fatal: false,
                        })?;
                        TurnOutcome::Failed
                    }
                    _ => {
                        return Err(AdapterError::Protocol(
                            "unexpected completed turn status".into(),
                        ));
                    }
                };
                let outcome = if outcome == TurnOutcome::Completed
                    && self.config.output_schema_json.is_some()
                    && !self.structured_seen
                {
                    self.emit(ChatEvent::Error {
                        code: "missing_structured_output".into(),
                        message: "Codex completed without a structured final answer".into(),
                        fatal: false,
                    })?;
                    TurnOutcome::Failed
                } else {
                    outcome
                };
                let duration = turn["durationMs"].as_u64().or_else(|| {
                    self.turn_started
                        .map(|started| started.elapsed().as_millis().min(u64::MAX as u128) as u64)
                });
                self.end_turn(duration, 1, outcome)?;
            }
            "item/agentMessage/delta" | "item/plan/delta" => {
                self.emit(ChatEvent::AssistantText {
                    id: nonempty(params, "itemId")?,
                    update: TextUpdate::Delta,
                    text: string(params, "delta"),
                })?;
            }
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                self.emit(ChatEvent::Thinking {
                    id: nonempty(params, "itemId")?,
                    delta: string(params, "delta"),
                })?
            }
            "item/started" | "item/completed" => {
                self.codex_item(&params["item"], method == "item/completed")?
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                let id = nonempty(params, "itemId")?;
                let mut tool = self.tools.remove(&id).ok_or_else(|| {
                    AdapterError::Protocol("Codex output for unknown tool".into())
                })?;
                let output = tool.output.get_or_insert_with(|| bounded(""));
                let delta = string(params, "delta");
                if output.text.len() < MAX_TEXT {
                    let available = MAX_TEXT - output.text.len();
                    let mut end = delta.len().min(available);
                    while !delta.is_char_boundary(end) {
                        end -= 1;
                    }
                    output.text.push_str(&delta[..end]);
                    output.truncated |= end < delta.len();
                } else {
                    output.truncated |= !delta.is_empty();
                }
                self.emit(ChatEvent::ToolOutput {
                    id: id.clone(),
                    delta: bounded(&delta),
                })?;
                self.tools.insert(id, tool);
            }
            "thread/tokenUsage/updated" => {
                let last = &params["tokenUsage"]["last"];
                self.usage = Usage {
                    input_tokens: last["inputTokens"].as_u64(),
                    output_tokens: last["outputTokens"].as_u64(),
                    cache_read_tokens: last["cachedInputTokens"].as_u64(),
                    cache_write_tokens: last["cacheWriteInputTokens"].as_u64(),
                    cost_usd: None,
                };
            }
            "turn/diff/updated" => {
                let id = format!("{}:diff", self.active_turn.as_deref().unwrap_or("turn"));
                self.emit(ChatEvent::ToolCall(ToolCall {
                    id,
                    kind: ToolKind::Edit,
                    title: "Turn diff".into(),
                    input_summary: bounded(""),
                    status: ToolStatus::Running,
                    output: None,
                    diff: Some(bounded(string(params, "diff"))),
                }))?;
            }
            "error" => self.emit(ChatEvent::Error {
                code: "provider_error".into(),
                message: bounded(params.to_string()).text,
                fatal: false,
            })?,
            "serverRequest/resolved" => {
                if let Some(wire_id) = params.get("requestId")
                    && let Some(id) = self
                        .requests
                        .iter()
                        .find(|(_, r)| r.wire_id == *wire_id)
                        .map(|(id, _)| id.clone())
                {
                    self.requests.remove(&id);
                    self.emit(ChatEvent::RequestResolved { id })?;
                }
            }
            // These metadata/status notifications are not completion authority.
            "thread/started"
            | "thread/status/changed"
            | "thread/name/updated"
            | "thread/archived"
            | "thread/unarchived"
            | "thread/closed"
            | "thread/model/updated"
            | "thread/compacted"
            | "turn/plan/updated"
            | "item/reasoning/summaryPartAdded"
            | "account/updated"
            | "account/rateLimits/updated"
            | "model/rerouted"
            | "configWarning"
            | "deprecationNotice"
            | "mcpServer/startupStatus/updated"
            | "mcpServer/tools/listChanged" => {}
            other if other.starts_with("codex/event/") => {} // Legacy duplicated v1 notifications.
            other => self.emit(ChatEvent::Error {
                code: "unsupported_event".into(),
                message: format!("unrecognized Codex event {other}"),
                fatal: false,
            })?,
        }
        Ok(Vec::new())
    }

    fn codex_child_notification(
        &mut self,
        thread: &str,
        method: &str,
        params: &Value,
    ) -> Result<Vec<Value>> {
        let Some(parent) = self.subagent_parents.get(thread).cloned() else {
            return Ok(Vec::new());
        };
        self.event_parent = Some(parent);
        let result: Result<()> = (|| {
            match method {
                "item/agentMessage/delta" | "item/plan/delta" => {
                    self.emit(ChatEvent::AssistantText {
                        id: nonempty(params, "itemId")?,
                        update: TextUpdate::Delta,
                        text: string(params, "delta"),
                    })?
                }
                "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                    self.emit(ChatEvent::Thinking {
                        id: nonempty(params, "itemId")?,
                        delta: string(params, "delta"),
                    })?
                }
                "item/started" | "item/completed" => {
                    self.codex_item(&params["item"], method == "item/completed")?
                }
                "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                    self.emit(ChatEvent::ToolOutput {
                        id: nonempty(params, "itemId")?,
                        delta: bounded(string(params, "delta")),
                    })?
                }
                "turn/started" => self.emit(ChatEvent::StateChanged(SessionState::Running))?,
                "turn/completed" => {
                    let turn = &params["turn"];
                    let outcome = match turn["status"].as_str() {
                        Some("completed") => TurnOutcome::Completed,
                        Some("interrupted") => TurnOutcome::Cancelled,
                        Some("failed") => TurnOutcome::Failed,
                        _ => {
                            return Err(AdapterError::Protocol(
                                "invalid child turn completion status".into(),
                            ));
                        }
                    };
                    self.emit(ChatEvent::TurnEnd {
                        duration_ms: turn["durationMs"].as_u64(),
                        turns: 1,
                        usage: Usage::default(),
                        model: None,
                        outcome,
                    })?;
                }
                "error" => self.emit(ChatEvent::Error {
                    code: "provider_child_error".into(),
                    message: bounded(params.to_string()).text,
                    fatal: false,
                })?,
                _ => {}
            }
            Ok(())
        })();
        self.event_parent = None;
        result?;
        Ok(Vec::new())
    }

    fn codex_item(&mut self, item: &Value, completed: bool) -> Result<()> {
        let id = nonempty(item, "id")?;
        let kind = nonempty(item, "type")?;
        match kind.as_str() {
            "agentMessage" | "plan" => {
                if completed {
                    let text = string(item, "text");
                    if self.event_parent.is_none()
                        && kind == "agentMessage"
                        && self.config.output_schema_json.is_some()
                        && item["phase"].as_str() != Some("commentary")
                    {
                        match serde_json::from_str::<Value>(&text) {
                            Ok(value) => {
                                self.emit(ChatEvent::StructuredOutput {
                                    json: value.to_string(),
                                })?;
                                self.structured_seen = true;
                            }
                            Err(_) => self.emit(ChatEvent::Error {
                                code: "invalid_structured_output".into(),
                                message: "Codex final message was not JSON".into(),
                                fatal: false,
                            })?,
                        }
                    }
                    self.emit(ChatEvent::AssistantText {
                        id,
                        update: TextUpdate::Final,
                        text,
                    })?;
                }
                return Ok(());
            }
            "userMessage" | "reasoning" | "hookPrompt" | "contextCompaction" => return Ok(()),
            _ => {}
        }
        let mut tool = self.tools.remove(&id).unwrap_or_else(|| ToolCall {
            id: id.clone(),
            kind: tool_kind(&kind),
            title: kind.clone(),
            input_summary: bounded(""),
            status: ToolStatus::Running,
            output: None,
            diff: None,
        });
        tool.status = match item["status"].as_str() {
            Some("failed" | "errored") => ToolStatus::Failed,
            Some("declined") => ToolStatus::Denied,
            _ if completed => ToolStatus::Completed,
            _ => ToolStatus::Running,
        };
        match kind.as_str() {
            "commandExecution" => {
                tool.title = string(item, "command");
                tool.input_summary = bounded(tool.title.as_str());
                if let Some(output) = item["aggregatedOutput"].as_str() {
                    tool.output = Some(bounded(output));
                }
            }
            "fileChange" => {
                if let Some(changes) = item["changes"].as_array() {
                    for change in changes {
                        if completed && tool.status == ToolStatus::Completed {
                            self.emit(ChatEvent::FileEdit {
                                path: nonempty(change, "path")?.into(),
                                unified_diff: bounded(string(change, "diff")),
                            })?;
                        }
                    }
                    let diff = changes
                        .iter()
                        .filter_map(|c| c["diff"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    tool.diff = Some(bounded(diff));
                }
            }
            "mcpToolCall" | "dynamicToolCall" => {
                tool.kind = ToolKind::Mcp;
                tool.title = string(item, "tool");
                tool.input_summary = bounded(item["arguments"].to_string());
                if completed {
                    tool.output = Some(bounded(
                        item.get("result")
                            .or_else(|| item.get("contentItems"))
                            .unwrap_or(&Value::Null)
                            .to_string(),
                    ));
                }
                if item.get("error").is_some_and(|e| !e.is_null()) || item["success"] == false {
                    tool.status = ToolStatus::Failed;
                }
            }
            "webSearch" => {
                tool.kind = ToolKind::Web;
                tool.title = string(item, "query");
                tool.input_summary = bounded(item["action"].to_string());
            }
            "collabAgentToolCall" | "subAgentActivity" => {
                let children: Vec<&str> = item["receiverThreadIds"]
                    .as_array()
                    .map(|ids| ids.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_else(|| item["agentThreadId"].as_str().into_iter().collect());
                for child in children {
                    if self.subagent_parents.len() >= MAX_PENDING * 4
                        && !self.subagent_parents.contains_key(child)
                    {
                        return Err(AdapterError::Protocol(
                            "too many correlated child agents".into(),
                        ));
                    }
                    self.subagent_parents.insert(child.into(), id.clone());
                }
                let mut events = vec![ChatEvent::ToolCall(tool.clone())];
                if let Some(states) = item["agentsStates"].as_object() {
                    for (child, state) in states {
                        if let Some(message) = state["message"].as_str() {
                            events.push(ChatEvent::AssistantText {
                                id: child.clone(),
                                update: TextUpdate::Final,
                                text: message.into(),
                            });
                        }
                        let state = match state["status"].as_str() {
                            Some("completed" | "shutdown") => SessionState::Closed,
                            Some("errored" | "notFound") => SessionState::Failed {
                                message: string(state, "status"),
                            },
                            Some("pendingInit") => SessionState::Starting,
                            Some("running") => SessionState::Running,
                            _ => SessionState::Idle,
                        };
                        events.push(ChatEvent::Subagent {
                            parent_tool_id: child.clone(),
                            events: vec![ChatEvent::StateChanged(state)],
                        });
                    }
                }
                self.emit(ChatEvent::Subagent {
                    parent_tool_id: id.clone(),
                    events,
                })?;
            }
            _ => tool.input_summary = bounded(item.to_string()),
        }
        self.emit(ChatEvent::ToolCall(tool.clone()))?;
        if !completed {
            if self.tools.len() >= MAX_PENDING * 4 {
                return Err(AdapterError::Protocol("too many active Codex tools".into()));
            }
            self.tools.insert(id, tool);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{Pending, test_machine};
    #[test]
    fn initialization_precedes_thread_start_and_resume_requires_read() {
        let (mut machine, _events) = test_machine(AdapterKind::CodexAppServer);
        machine.config.resume = Some(machine.info.resume.clone());
        machine.info.resumed = true;
        machine.pending.insert(
            "1".into(),
            Pending {
                kind: PendingKind::Initialize,
                reply: None,
                started: Instant::now(),
            },
        );
        let output = machine
            .codex_incoming(json!({"id":1,"result":{"userAgent":"fixture"}}))
            .unwrap();
        assert_eq!(output[0]["method"], "initialized");
        assert_eq!(output[1]["method"], "thread/read");
        let response =
            json!({"id":output[1]["id"],"result":{"thread":{"id":"native","cwd":"/different"}}});
        assert!(matches!(
            machine.codex_incoming(response),
            Err(AdapterError::InvalidResume(_))
        ));
    }
    #[test]
    fn approvals_preserve_integer_or_string_ids_and_only_allow_once() {
        let (mut machine, _events) = test_machine(AdapterKind::CodexAppServer);
        for wire_id in [json!(42), json!("42")] {
            let output = machine.codex_incoming(json!({"id":wire_id,"method":"item/commandExecution/requestApproval","params":{"threadId":"native","itemId":"tool","command":"pwd"}})).unwrap();
            assert!(output.is_empty());
            let id = machine.requests.keys().next().unwrap().clone();
            let (reply, _) = oneshot::channel();
            let response = machine
                .codex_operation(Operation::Approve(id, ApprovalDecision::AllowOnce), reply)
                .unwrap();
            assert_eq!(response[0]["id"], wire_id);
            assert_eq!(response[0]["result"]["decision"], "accept");
        }
    }
    #[test]
    fn recorded_shape_turn_emits_diff_usage_and_completed_event() {
        let (mut machine, mut events) = test_machine(AdapterKind::CodexAppServer);
        for line in include_str!("../tests/fixtures/codex-turn.synthetic.jsonl").lines() {
            machine
                .codex_incoming(serde_json::from_str(line).unwrap())
                .unwrap();
        }
        let mut records = Vec::new();
        while let Ok(event) = events.try_recv() {
            records.push(event);
        }
        assert!(records.iter().any(|r| matches!(&r.event,ChatEvent::FileEdit { path,.. } if path.to_str() == Some("/tmp/example.rs"))));
        assert!(records.iter().any(|r| matches!(&r.event,ChatEvent::TurnEnd { usage,outcome:TurnOutcome::Completed,.. } if usage.input_tokens == Some(22))));
    }
    #[test]
    fn unknown_requests_are_explicit_protocol_errors_not_approvals() {
        let (mut machine, _events) = test_machine(AdapterKind::CodexAppServer);
        let output = machine
            .codex_incoming(json!({"id":7,"method":"future/approval","params":{}}))
            .unwrap();
        assert_eq!(output[0]["error"]["code"], -32601);
        assert!(machine.requests.is_empty());
    }
    #[test]
    fn recorded_initialize_and_schema_contract_match_client_wire_shape() {
        let (mut machine, _events) = test_machine(AdapterKind::CodexAppServer);
        machine.pending.insert(
            "1".into(),
            Pending {
                kind: PendingKind::Initialize,
                reply: None,
                started: Instant::now(),
            },
        );
        let recorded = serde_json::from_str(include_str!(
            "../tests/fixtures/codex-initialize.recorded.jsonl"
        ))
        .unwrap();
        let output = machine.codex_incoming(recorded).unwrap();
        assert_eq!(output[0]["method"], "initialized");
        assert_eq!(output[1]["method"], "thread/start");
        assert_eq!(output[1]["params"]["approvalPolicy"], "untrusted");
        assert_eq!(output[1]["params"]["approvalsReviewer"], "user");
        let contract: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/codex-contract.recorded.json"
        ))
        .unwrap();
        let steer = &contract["ClientRequest.json"]["turn/steer"]["required"];
        assert!(steer.as_array().unwrap().contains(&json!("expectedTurnId")));
    }
    #[test]
    fn steer_cancel_and_questions_use_current_turn_and_explicit_answers() {
        let (mut machine, _events) = test_machine(AdapterKind::CodexAppServer);
        machine.active_turn = Some("active-turn".into());
        let (reply, _) = oneshot::channel();
        let steer = machine
            .codex_operation(Operation::Steer("new direction".into()), reply)
            .unwrap();
        assert_eq!(steer[0]["method"], "turn/steer");
        assert_eq!(steer[0]["params"]["expectedTurnId"], "active-turn");
        machine.codex_incoming(json!({"id":"question","method":"item/tool/requestUserInput","params":{"threadId":"native","questions":[{"id":"q","question":"Which?","header":"Choice","options":[{"label":"One","description":"first"}],"isOther":false}]}})).unwrap();
        let id = machine.requests.keys().next().unwrap().clone();
        let (reply, mut receive) = oneshot::channel();
        let invalid = QuestionResponse {
            answers: [("q".into(), vec!["Not offered".into()])].into(),
            structured_json: None,
        };
        assert!(
            machine
                .codex_operation(Operation::Question(id.clone(), invalid), reply)
                .unwrap()
                .is_empty()
        );
        assert!(receive.try_recv().unwrap().is_err());
        assert!(machine.requests.contains_key(&id));
        let (reply, _) = oneshot::channel();
        let answer = QuestionResponse {
            answers: [("q".into(), vec!["One".into()])].into(),
            structured_json: None,
        };
        let response = machine
            .codex_operation(Operation::Question(id, answer), reply)
            .unwrap();
        assert_eq!(
            response[0]["result"]["answers"]["q"]["answers"],
            json!(["One"])
        );
        let (reply, _) = oneshot::channel();
        let cancel = machine.codex_operation(Operation::Cancel, reply).unwrap();
        assert_eq!(cancel[0]["method"], "turn/interrupt");
        assert_eq!(cancel[0]["params"]["turnId"], "active-turn");
    }
}
