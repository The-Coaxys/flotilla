use crate::*;
use futures::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command as ProcessCommand},
    sync::{mpsc, oneshot, watch},
};
use tokio_util::codec::{FramedRead, LinesCodec};
use uuid::Uuid;

pub(crate) const OP_TIMEOUT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Matches the wire contract for provider-native identities.
const MAX_NATIVE_ID: usize = 1024;

pub(crate) fn command(provider: &ProviderSpec) -> ProcessCommand {
    let mut command = ProcessCommand::new(&provider.command);
    command
        .args(&provider.args)
        .envs(&provider.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    command
}

struct ProcessGroup(Option<u32>);
impl ProcessGroup {
    fn kill(&self) {
        #[cfg(unix)]
        if let Some(id) = self.0 {
            // The child was spawned into a new process group owned exclusively by us.
            unsafe {
                libc::kill(-(id as i32), libc::SIGKILL);
            }
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

pub(crate) async fn capture(provider: &ProviderSpec, args: &[&str]) -> Result<String> {
    let mut child = command(provider).args(args).stdin(Stdio::null()).spawn()?;
    let group = ProcessGroup(child.id());
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AdapterError::Protocol("missing stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AdapterError::Protocol("missing stderr".into()))?;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut out_stream = stdout.take((MAX_WIRE_LINE + 1) as u64);
        let mut err_stream = stderr.take((MAX_WIRE_LINE + 1) as u64);
        let (a, b) = tokio::join!(
            out_stream.read_to_end(&mut out),
            err_stream.read_to_end(&mut err)
        );
        a?;
        b?;
        if out.len() > MAX_WIRE_LINE || err.len() > MAX_WIRE_LINE {
            return Err(AdapterError::Protocol(
                "CLI probe output exceeds 1 MiB".into(),
            ));
        }
        let status = child.wait().await?;
        if !status.success() {
            return Err(AdapterError::Unsupported(format!(
                "CLI probe exited {status}: {}",
                bounded(String::from_utf8_lossy(&err)).text
            )));
        }
        String::from_utf8(out)
            .map_err(|_| AdapterError::Protocol("CLI probe returned invalid UTF-8".into()))
    })
    .await;
    group.kill();
    if !matches!(&result, Ok(Ok(_))) {
        let _ = child.start_kill();
    }
    let _ = child.wait().await;
    result.map_err(|_| AdapterError::Timeout)?
}

pub(crate) async fn validate_codex_schema(provider: &ProviderSpec) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir
        .path()
        .to_str()
        .ok_or_else(|| AdapterError::InvalidConfig("schema temporary path is not UTF-8".into()))?;
    capture(
        provider,
        &["app-server", "generate-json-schema", "--out", path],
    )
    .await?;
    for (file, methods) in [
        (
            "ClientRequest.json",
            &[
                "initialize",
                "thread/start",
                "thread/read",
                "thread/resume",
                "turn/start",
                "turn/steer",
                "turn/interrupt",
            ][..],
        ),
        (
            "ServerRequest.json",
            &[
                "item/commandExecution/requestApproval",
                "item/fileChange/requestApproval",
                "item/tool/requestUserInput",
            ][..],
        ),
        (
            "ServerNotification.json",
            &[
                "item/agentMessage/delta",
                "turn/completed",
                "item/completed",
            ][..],
        ),
    ] {
        let file = tokio::fs::File::open(dir.path().join(file)).await?;
        let mut bytes = Vec::new();
        file.take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(AdapterError::Unsupported(
                "Codex schema is oversized".into(),
            ));
        }
        let schema: Value =
            serde_json::from_slice(&bytes).map_err(|e| AdapterError::Unsupported(e.to_string()))?;
        let variants = schema["oneOf"]
            .as_array()
            .ok_or_else(|| AdapterError::Unsupported("Codex schema lacks method union".into()))?;
        for method in methods {
            let variant = variants
                .iter()
                .find(|v| v["properties"]["method"]["enum"][0] == *method)
                .ok_or_else(|| AdapterError::Unsupported(format!("Codex schema lacks {method}")))?;
            let params_ref = variant["properties"]["params"]["$ref"]
                .as_str()
                .unwrap_or("");
            let shape = schema
                .pointer(params_ref.strip_prefix('#').unwrap_or(params_ref))
                .unwrap_or(&Value::Null);
            let required: &[&str] = match *method {
                "initialize" => &["clientInfo", "capabilities"],
                "thread/start" => &[
                    "cwd",
                    "model",
                    "approvalPolicy",
                    "approvalsReviewer",
                    "sandbox",
                    "developerInstructions",
                    "config",
                ],
                "thread/resume" => &["threadId", "cwd", "approvalPolicy", "approvalsReviewer"],
                "thread/read" => &["threadId", "includeTurns"],
                "turn/start" => &[
                    "threadId",
                    "input",
                    "outputSchema",
                    "approvalPolicy",
                    "approvalsReviewer",
                ],
                "turn/steer" => &["threadId", "expectedTurnId", "input"],
                "turn/interrupt" => &["threadId", "turnId"],
                _ => &[],
            };
            for field in required {
                if shape["properties"].get(field).is_none() {
                    return Err(AdapterError::Unsupported(format!(
                        "Codex {method} schema lacks {field}"
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn probe(provider: &ProviderSpec) -> Result<()> {
    let mut cmd = command(provider);
    match provider.adapter {
        AdapterKind::ClaudeStream => {
            cmd.args(crate::claude::WIRE_ARGS).args([
                "--permission-mode",
                "manual",
                "--permission-prompt-tool",
                "stdio",
            ]);
        }
        AdapterKind::CodexAppServer => {
            cmd.args(["app-server", "--listen", "stdio://"]);
        }
    }
    let mut child = cmd.spawn()?;
    let group = ProcessGroup(child.id());
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| AdapterError::Protocol("missing stdin".into()))?;
    let mut lines = FramedRead::new(
        child
            .stdout
            .take()
            .ok_or_else(|| AdapterError::Protocol("missing stdout".into()))?,
        LinesCodec::new_with_max_length(MAX_WIRE_LINE),
    );
    let mut errors = FramedRead::new(
        child
            .stderr
            .take()
            .ok_or_else(|| AdapterError::Protocol("missing stderr".into()))?,
        LinesCodec::new_with_max_length(MAX_WIRE_LINE),
    );
    let init = initialize(provider.adapter, "1");
    let result = tokio::time::timeout(OP_TIMEOUT, async {
        write_message(&mut input, &init).await?;
        loop { tokio::select! {
            line = lines.next() => {
                let value = parse_line(line)?;
                match provider.adapter {
                    AdapterKind::ClaudeStream if value["type"] == "control_response" && value["response"]["request_id"] == "1" => {
                        if value["response"]["subtype"] != "success" { return Err(AdapterError::Unsupported(bounded(value.to_string()).text)); }
                        return Ok(());
                    }
                    AdapterKind::CodexAppServer if value["id"] == 1 => {
                        if value.get("error").is_some() || !value["result"].is_object() { return Err(AdapterError::Unsupported(bounded(value.to_string()).text)); }
                        write_message(&mut input, &json!({"method":"initialized"})).await?;
                        return Ok(());
                    }
                    _ if value.get("request").is_some() || (value.get("id").is_some() && value.get("method").is_some()) => return Err(AdapterError::Unsupported("provider requested interaction before initialize completed".into())),
                    _ => {}
                }
            }
            line = errors.next() => {
                match line {
                    Some(Ok(_)) => {},
                    Some(Err(error)) => return Err(AdapterError::Unsupported(format!("provider stderr: {error}"))),
                    None => return Err(AdapterError::Unsupported("provider closed during initialize".into())),
                }
            }
        }}
    }).await;
    drop(input);
    group.kill();
    let _ = child.start_kill();
    let _ = child.wait().await;
    result.map_err(|_| AdapterError::Timeout)?
}

pub(crate) fn initialize(kind: AdapterKind, id: &str) -> Value {
    match kind {
        AdapterKind::ClaudeStream => {
            json!({"type":"control_request","request_id":id,"request":{"subtype":"initialize","hooks":null,"forwardSubagentText":true}})
        }
        AdapterKind::CodexAppServer => {
            json!({"id":id.parse::<u64>().unwrap_or(1),"method":"initialize","params":{"clientInfo":{"name":"flotilla","title":"Flotilla","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}})
        }
    }
}

pub(crate) fn parse_line(
    line: Option<std::result::Result<String, tokio_util::codec::LinesCodecError>>,
) -> Result<Value> {
    let line = line
        .ok_or_else(|| AdapterError::Closed("provider stdout closed".into()))?
        .map_err(|e| AdapterError::Protocol(e.to_string()))?;
    let value: Value = serde_json::from_str(&line)
        .map_err(|e| AdapterError::Protocol(format!("malformed provider JSON: {e}")))?;
    if !value.is_object() {
        return Err(AdapterError::Protocol(
            "provider message is not an object".into(),
        ));
    }
    Ok(value)
}
async fn write_message(input: &mut ChildStdin, message: &Value) -> Result<()> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|e| AdapterError::Protocol(e.to_string()))?;
    if bytes.len() > MAX_WIRE_LINE {
        return Err(AdapterError::InvalidConfig(
            "outgoing message exceeds 1 MiB".into(),
        ));
    }
    bytes.push(b'\n');
    tokio::time::timeout(WRITE_TIMEOUT, async {
        input.write_all(&bytes).await?;
        input.flush().await
    })
    .await
    .map_err(|_| AdapterError::Timeout)??;
    Ok(())
}

pub(crate) enum PendingKind {
    Initialize,
    ReadResume,
    OpenSession,
    Send { id: String, text: String },
    Steer { id: String, text: String },
    Control,
}
pub(crate) struct Pending {
    pub kind: PendingKind,
    pub reply: Option<oneshot::Sender<Result<()>>>,
    pub started: Instant,
}
pub(crate) enum Interaction {
    Claude {
        input: Value,
        questions: Vec<Question>,
    },
    CodexApproval {
        method: String,
        params: Value,
    },
    CodexQuestion {
        questions: Vec<Question>,
    },
    CodexElicitation,
}
pub(crate) struct Request {
    pub wire_id: Value,
    pub interaction: Interaction,
}

pub(crate) struct Machine {
    pub provider: ProviderSpec,
    pub config: SessionConfig,
    pub info: SessionInfo,
    pub config_json: Value,
    pub active_turn: Option<String>,
    pub turn_started: Option<Instant>,
    pub starting_turn: bool,
    pub pending: HashMap<String, Pending>,
    pub requests: HashMap<String, Request>,
    pub tools: HashMap<String, ToolCall>,
    pub partial_ids: std::collections::HashSet<String>,
    pub claude_messages: HashMap<String, String>,
    pub claude_blocks: HashMap<String, String>,
    pub subagent_parents: HashMap<String, String>,
    pub event_parent: Option<String>,
    pub final_seen: bool,
    pub structured_seen: bool,
    pub deliveries: Vec<oneshot::Sender<Result<()>>>,
    pub usage: Usage,
    pub next_id: u64,
    pub state: watch::Sender<SessionState>,
    pub events: mpsc::Sender<EventRecord>,
    pub ready: Option<oneshot::Sender<Result<SessionInfo>>>,
    pub sequence: u64,
}
impl Machine {
    pub fn id(&mut self) -> String {
        self.next_id += 1;
        self.next_id.to_string()
    }
    pub fn delivered(&mut self, reply: oneshot::Sender<Result<()>>) {
        self.deliveries.push(reply);
    }
    pub fn scoped(&mut self, event: ChatEvent, parent: &str) -> Result<()> {
        if parent.is_empty() {
            self.emit(event)
        } else {
            self.emit(ChatEvent::Subagent {
                parent_tool_id: parent.into(),
                events: vec![event],
            })
        }
    }
    pub fn emit(&mut self, event: ChatEvent) -> Result<()> {
        let event = match &self.event_parent {
            Some(parent) => ChatEvent::Subagent {
                parent_tool_id: parent.clone(),
                events: vec![event],
            },
            None => event,
        };
        let record = EventRecord {
            schema_version: CHAT_SCHEMA_VERSION,
            session_id: self.info.id,
            incarnation: self.info.incarnation,
            sequence: self.sequence,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            native_session_id: if self.info.resume.native_id.is_empty() {
                None
            } else {
                Some(self.info.resume.native_id.clone())
            },
            turn_id: self.active_turn.clone(),
            event,
        };
        self.events
            .try_send(record)
            .map_err(|_| AdapterError::Backpressure)?;
        self.sequence += 1;
        Ok(())
    }
    pub fn set_state(&mut self, state: SessionState) -> Result<()> {
        self.state.send_replace(state.clone());
        self.emit(ChatEvent::StateChanged(state))
    }
    pub fn session_ready(&mut self, native_id: String) -> Result<()> {
        if native_id.is_empty() {
            return Err(AdapterError::Protocol(
                "provider omitted session identity".into(),
            ));
        }
        self.info.resume.native_id = native_id;
        self.emit(ChatEvent::SessionReady(self.info.clone()))?;
        self.set_state(SessionState::Idle)?;
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(Ok(self.info.clone()));
        }
        Ok(())
    }
    pub fn request_id(&mut self, wire_id: Value, interaction: Interaction) -> Result<String> {
        if self.requests.len() >= MAX_PENDING {
            return Err(AdapterError::Protocol(
                "too many pending permission requests".into(),
            ));
        }
        if self.requests.values().any(|r| r.wire_id == wire_id) {
            return Err(AdapterError::Protocol(
                "duplicate provider permission request ID".into(),
            ));
        }
        let counter = self.id();
        let id = format!("{}:{counter}", self.info.incarnation);
        self.requests.insert(
            id.clone(),
            Request {
                wire_id,
                interaction,
            },
        );
        Ok(id)
    }
    pub fn rpc(
        &mut self,
        kind: PendingKind,
        method: &str,
        params: Value,
        reply: Option<oneshot::Sender<Result<()>>>,
    ) -> Result<Value> {
        if self.pending.len() >= MAX_PENDING {
            return Err(AdapterError::Protocol("too many outstanding RPCs".into()));
        }
        let id = self.id();
        self.pending.insert(
            id.clone(),
            Pending {
                kind,
                reply,
                started: Instant::now(),
            },
        );
        Ok(match self.provider.adapter {
            AdapterKind::ClaudeStream => {
                json!({"type":"control_request","request_id":id,"request":params})
            }
            AdapterKind::CodexAppServer => {
                json!({"id":id.parse::<u64>().unwrap_or(0),"method":method,"params":params})
            }
        })
    }
    pub fn clear_requests(&mut self) -> Result<()> {
        let ids: Vec<_> = self.requests.drain().map(|(id, _)| id).collect();
        for id in ids {
            self.emit(ChatEvent::RequestResolved { id })?;
        }
        Ok(())
    }
    pub fn end_turn(
        &mut self,
        duration_ms: Option<u64>,
        turns: u64,
        outcome: TurnOutcome,
    ) -> Result<()> {
        self.clear_requests()?;
        let usage = std::mem::take(&mut self.usage);
        self.emit(ChatEvent::TurnEnd {
            duration_ms,
            turns,
            usage,
            model: self.info.model.clone(),
            outcome: outcome.clone(),
        })?;
        self.active_turn = None;
        self.starting_turn = false;
        self.turn_started = None;
        self.tools.clear();
        self.partial_ids.clear();
        self.claude_messages.clear();
        self.claude_blocks.clear();
        self.final_seen = false;
        self.structured_seen = false;
        match outcome {
            // A failed turn stays visible until the user sends or steers
            // again; never reinterpret it as a clean idle session.
            TurnOutcome::Failed => self.set_state(SessionState::Failed {
                message: "the provider reported a failed turn".into(),
            }),
            TurnOutcome::Completed | TurnOutcome::Cancelled => self.set_state(SessionState::Idle),
        }
    }
    pub fn user_event(&mut self, id: String, text: String) -> Result<()> {
        self.emit(ChatEvent::UserMessage { id, text })
    }
    fn incoming(&mut self, value: Value) -> Result<Vec<Value>> {
        match self.provider.adapter {
            AdapterKind::ClaudeStream => self.claude_incoming(value),
            AdapterKind::CodexAppServer => self.codex_incoming(value),
        }
    }
    fn operation(
        &mut self,
        operation: Operation,
        reply: oneshot::Sender<Result<()>>,
    ) -> Result<Vec<Value>> {
        match self.provider.adapter {
            AdapterKind::ClaudeStream => self.claude_operation(operation, reply),
            AdapterKind::CodexAppServer => self.codex_operation(operation, reply),
        }
    }
}

pub(crate) async fn launch(provider: ProviderSpec, mut config: SessionConfig) -> Result<Adapter> {
    config.cwd = tokio::fs::canonicalize(&config.cwd)
        .await
        .map_err(|e| AdapterError::InvalidConfig(format!("cwd: {e}")))?;
    if !tokio::fs::metadata(&config.cwd).await?.is_dir() {
        return Err(AdapterError::InvalidConfig("cwd is not a directory".into()));
    }
    if config.cwd.to_str().is_none() {
        return Err(AdapterError::InvalidConfig(
            "provider cwd is not UTF-8".into(),
        ));
    }
    if let Some(resume) = &config.resume {
        // A vanished path is a transient environment failure, not proof that
        // the stored conversation is semantically wrong. Keep it restorable.
        let cwd = tokio::fs::canonicalize(&resume.cwd)
            .await
            .map_err(|e| AdapterError::Protocol(format!("cannot verify resume cwd: {e}")))?;
        if resume.provider_id != provider.id
            || resume.adapter != provider.adapter
            || cwd != config.cwd
            || resume.native_id.is_empty()
            || resume.native_id.len() > MAX_NATIVE_ID
        {
            return Err(AdapterError::InvalidResume(
                "provider, cwd, or identity mismatch".into(),
            ));
        }
        if provider.adapter == AdapterKind::ClaudeStream {
            crate::claude::validate_resume(&provider, resume, &config.cwd).await?;
        }
    }
    let config_json: Value = config
        .provider_config_json
        .as_ref()
        .map(|s| serde_json::from_str(s))
        .transpose()
        .map_err(|e| AdapterError::InvalidConfig(format!("provider config: {e}")))?
        .unwrap_or_else(|| json!({}));
    if !config_json.is_object() {
        return Err(AdapterError::InvalidConfig(
            "provider config must be an object".into(),
        ));
    }
    if let Some(schema) = &config.output_schema_json {
        let value: Value = serde_json::from_str(schema)
            .map_err(|e| AdapterError::InvalidConfig(format!("output schema: {e}")))?;
        if !value.is_object() {
            return Err(AdapterError::InvalidConfig(
                "output schema must be an object".into(),
            ));
        }
    }
    let native_id = config
        .resume
        .as_ref()
        .map(|r| r.native_id.clone())
        .unwrap_or_else(|| {
            if provider.adapter == AdapterKind::ClaudeStream {
                Uuid::new_v4().to_string()
            } else {
                String::new()
            }
        });
    let info = SessionInfo {
        id: config.session_id.unwrap_or_else(Uuid::new_v4),
        incarnation: Uuid::new_v4(),
        resume: ResumeTarget {
            provider_id: provider.id.clone(),
            adapter: provider.adapter,
            native_id,
            cwd: config.cwd.clone(),
        },
        model: config.model.clone(),
        resumed: config.resume.is_some(),
    };
    let mut cmd = command(&provider);
    cmd.current_dir(&config.cwd);
    match provider.adapter {
        AdapterKind::ClaudeStream => {
            crate::claude::configure(&mut cmd, &config, &info, &config_json)?
        }
        AdapterKind::CodexAppServer => {
            cmd.args(["app-server", "--listen", "stdio://"]);
        }
    }
    let child = cmd.spawn()?;
    let (events_tx, events) = mpsc::channel(EVENT_CAPACITY);
    let (commands, receiver) = mpsc::channel(32);
    let (state_tx, state) = watch::channel(SessionState::Starting);
    let (ready_tx, ready) = oneshot::channel();
    let machine = Machine {
        provider,
        config,
        info,
        config_json,
        active_turn: None,
        starting_turn: false,
        turn_started: None,
        pending: HashMap::new(),
        requests: HashMap::new(),
        tools: HashMap::new(),
        partial_ids: Default::default(),
        usage: Usage::default(),
        next_id: 1,
        claude_messages: HashMap::new(),
        claude_blocks: HashMap::new(),
        final_seen: false,
        structured_seen: false,
        deliveries: Vec::new(),
        subagent_parents: HashMap::new(),
        event_parent: None,
        state: state_tx,
        events: events_tx,
        ready: Some(ready_tx),
        sequence: 0,
    };
    tokio::spawn(run(child, machine, receiver));
    let info = ready
        .await
        .map_err(|_| AdapterError::Closed("initialization task exited".into()))??;
    Ok(Adapter {
        info,
        events,
        commands,
        state,
    })
}

async fn run(mut child: Child, mut machine: Machine, mut commands: mpsc::Receiver<Command>) {
    let group = ProcessGroup(child.id());
    let Some(mut input) = child.stdin.take() else {
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        return;
    };
    let Some(stderr) = child.stderr.take() else {
        return;
    };
    let mut output = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_WIRE_LINE));
    let mut errors = FramedRead::new(stderr, LinesCodec::new_with_max_length(MAX_WIRE_LINE));
    let mut stderr_open = true;
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    let mut shutdown_reply = None;
    let result: Result<()> = async {
        let init = initialize(machine.provider.adapter, "1");
        machine.pending.insert("1".into(), Pending { kind: PendingKind::Initialize, reply: None, started: Instant::now() });
        write_message(&mut input, &init).await?;
        loop {
            let messages = tokio::select! {
                command = commands.recv() => match command {
                    Some(Command { operation: Operation::Shutdown, reply }) => { shutdown_reply = Some(reply); break; }
                    Some(Command { operation, reply }) => machine.operation(operation, reply)?,
                    None => break,
                },
                line = output.next() => { let value = parse_line(line)?; machine.incoming(value)? },
                line = errors.next(), if stderr_open => {
                    match line {
                        Some(Ok(line)) => machine.emit(ChatEvent::Diagnostic { stream: "stderr".into(), output: bounded(line) })?,
                        Some(Err(error)) => return Err(AdapterError::Protocol(format!("stderr: {error}"))),
                        None => stderr_open = false,
                    }
                    Vec::new()
                },
                _ = interval.tick() => {
                    if machine.pending.values().any(|p| p.started.elapsed() > OP_TIMEOUT) { return Err(AdapterError::Timeout); }
                    if let Some(status) = child.try_wait()? { return Err(AdapterError::Closed(format!("provider exited {status}"))); }
                    Vec::new()
                }
            };
            for message in messages { write_message(&mut input, &message).await?; }
            for reply in machine.deliveries.drain(..) { let _ = reply.send(Ok(())); }
        }
        Ok(())
    }.await;
    let expected = result.is_ok();
    drop(input);
    if !expected {
        group.kill();
    }
    let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            group.kill();
            let _ = child.start_kill();
            child.wait().await.ok()
        }
    };
    group.kill();
    let reason = result
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_else(|| "session shut down".into());
    if let Err(error) = &result {
        let _ = machine.emit(ChatEvent::Error {
            code: "adapter_failed".into(),
            message: error.to_string(),
            fatal: true,
        });
    }
    let _ = machine.clear_requests();
    let _ = machine.emit(ChatEvent::ProcessExited {
        code: status.and_then(|s| s.code()),
        expected,
    });
    let state = if expected {
        SessionState::Closed
    } else {
        SessionState::Failed {
            message: reason.clone(),
        }
    };
    machine.state.send_replace(state.clone());
    let _ = machine.emit(ChatEvent::StateChanged(state));
    if let Some(ready) = machine.ready.take() {
        let _ = ready.send(Err(AdapterError::Closed(reason.clone())));
    }
    for (_, pending) in machine.pending.drain() {
        if let Some(reply) = pending.reply {
            let _ = reply.send(Err(AdapterError::Closed(reason.clone())));
        }
    }
    for reply in machine.deliveries.drain(..) {
        let _ = reply.send(Err(AdapterError::Closed(reason.clone())));
    }
    if let Some(reply) = shutdown_reply {
        let _ = reply.send(result);
    }
}

pub(crate) fn nonempty(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AdapterError::Protocol(format!("missing {field}")))
}
pub(crate) fn valid_text(text: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > MAX_TEXT {
        return Err(AdapterError::InvalidConfig(
            "message must contain 1..65536 UTF-8 bytes".into(),
        ));
    }
    Ok(())
}
pub(crate) fn checked_answers(questions: &[Question], response: &QuestionResponse) -> Result<()> {
    if response.structured_json.is_some() {
        return Err(AdapterError::InvalidConfig(
            "structured JSON is only supported for MCP forms".into(),
        ));
    }
    if response.answers.len() != questions.len() {
        return Err(AdapterError::InvalidConfig(
            "answer every question exactly once".into(),
        ));
    }
    for question in questions {
        let answers = response.answers.get(&question.id).ok_or_else(|| {
            AdapterError::InvalidConfig(format!("missing answer {}", question.id))
        })?;
        if answers.is_empty() || (!question.multi && answers.len() != 1) {
            return Err(AdapterError::InvalidConfig("invalid answer count".into()));
        }
        for answer in answers {
            valid_text(answer)?;
            if !question.free_text && !question.options.iter().any(|o| o.label == *answer) {
                return Err(AdapterError::InvalidConfig(
                    "answer is not an offered option".into(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_machine(kind: AdapterKind) -> (Machine, mpsc::Receiver<EventRecord>) {
    let registry = ProviderRegistry::default();
    let provider = registry
        .get(if kind == AdapterKind::ClaudeStream {
            "claude"
        } else {
            "codex"
        })
        .unwrap()
        .clone();
    let config = SessionConfig {
        cwd: std::path::Path::new("/tmp").into(),
        model: None,
        permission_mode: PermissionMode::Ask,
        instructions: None,
        session_id: None,
        provider_config_json: None,
        output_schema_json: None,
        resume: None,
    };
    let info = SessionInfo {
        id: Uuid::nil(),
        incarnation: Uuid::nil(),
        resume: ResumeTarget {
            provider_id: provider.id.clone(),
            adapter: kind,
            native_id: "native".into(),
            cwd: config.cwd.clone(),
        },
        model: None,
        resumed: false,
    };
    let (events, receive) = mpsc::channel(EVENT_CAPACITY);
    let (state, _) = watch::channel(SessionState::Idle);
    (
        Machine {
            provider,
            config,
            info,
            config_json: json!({}),
            active_turn: None,
            turn_started: None,
            starting_turn: false,
            pending: HashMap::new(),
            requests: HashMap::new(),
            tools: HashMap::new(),
            partial_ids: Default::default(),
            claude_messages: HashMap::new(),
            claude_blocks: HashMap::new(),
            subagent_parents: HashMap::new(),
            event_parent: None,
            final_seen: false,
            structured_seen: false,
            deliveries: Vec::new(),
            usage: Usage::default(),
            next_id: 1,
            state,
            events,
            ready: None,
            sequence: 0,
        },
        receive,
    )
}
