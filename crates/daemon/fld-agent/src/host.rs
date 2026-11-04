//! Shared plumbing for the line-oriented M5 adapters.
//!
//! These adapters own their child process exactly like the Claude/Codex core:
//! a bounded stdout reader, a stderr diagnostic channel, a command channel and
//! a bounded event channel whose overflow is fatal. Protocol handling stays in
//! `acp.rs`, `pi.rs` and `cursor.rs`; this module only moves bytes, enforces
//! limits and drives the common lifecycle.

use crate::*;
use futures::StreamExt;
use serde_json::Value;
use std::{collections::HashMap, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command as ProcessCommand},
    sync::{mpsc, oneshot, watch},
};
use tokio_util::codec::{FramedRead, LinesCodec};

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Cursor's `create-chat` performs auth and session allocation without a model
/// request; measured runs take ~30 s on the installed CLI.
pub(crate) const CURSOR_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn command(provider: &ProviderSpec) -> ProcessCommand {
    process::command(provider)
}

pub(crate) struct Guard(Option<u32>);
impl Guard {
    pub(crate) fn new(child: &Child) -> Self {
        Self(child.id())
    }
    pub(crate) fn kill(&self) {
        #[cfg(unix)]
        if let Some(id) = self.0 {
            // The child was spawned into a process group owned exclusively by us.
            unsafe {
                libc::kill(-(id as i32), libc::SIGKILL);
            }
        }
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Canonicalize the working directory and reject resume targets that could
/// redirect the child to another provider, adapter, directory or identity.
pub(crate) async fn prepare(
    provider: &ProviderSpec,
    mut config: SessionConfig,
) -> Result<SessionConfig> {
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
        let cwd = tokio::fs::canonicalize(&resume.cwd)
            .await
            .map_err(|e| AdapterError::InvalidResume(e.to_string()))?;
        if resume.provider_id != provider.id
            || resume.adapter != provider.adapter
            || cwd != config.cwd
            || resume.native_id.is_empty()
            || resume.native_id.len() > 512
            || resume.native_id.chars().any(char::is_control)
        {
            return Err(AdapterError::InvalidResume(
                "provider, cwd, or identity mismatch".into(),
            ));
        }
    }
    Ok(config)
}

/// Resume identifiers are provider-generated, bounded, control-free tokens.
/// Pi and OMP use UUID session IDs; ACP session IDs are opaque but may not
/// contain path separators or control characters.
pub(crate) fn token(native_id: &str) -> Result<&str> {
    if native_id.is_empty()
        || native_id.len() > 512
        || native_id.chars().any(char::is_control)
        || native_id.contains('/')
        || native_id.contains('\\')
        || native_id == "."
        || native_id == ".."
    {
        return Err(AdapterError::InvalidResume(
            "resume identity is not a safe token".into(),
        ));
    }
    Ok(native_id)
}

pub(crate) fn uuid(native_id: &str, adapter: &str) -> Result<()> {
    token(native_id)?;
    uuid::Uuid::parse_str(native_id)
        .map(|_| ())
        .map_err(|_| AdapterError::InvalidResume(format!("{adapter} session ID must be a UUID")))
}

/// Build the shared machine used by every adapter. `ready` is intentionally
/// absent: launch paths report readiness synchronously after their handshake.
pub(crate) fn machine(
    provider: ProviderSpec,
    config: SessionConfig,
    info: SessionInfo,
) -> (Machine, mpsc::Receiver<EventRecord>) {
    let (events, receive) = mpsc::channel(EVENT_CAPACITY);
    let (state, _) = watch::channel(SessionState::Starting);
    (
        Machine {
            provider,
            config,
            info,
            config_json: serde_json::json!({}),
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

pub(crate) async fn write(input: &mut ChildStdin, message: &Value) -> Result<()> {
    let mut bytes =
        serde_json::to_vec(message).map_err(|e| AdapterError::Protocol(e.to_string()))?;
    if bytes.len() > MAX_WIRE_LINE {
        return Err(AdapterError::InvalidConfig(
            "outgoing message exceeds 1 MiB".into(),
        ));
    }
    bytes.push(b'\n');
    let write = async {
        input.write_all(&bytes).await?;
        input.flush().await
    };
    tokio::time::timeout(Duration::from_secs(10), write)
        .await
        .map_err(|_| AdapterError::Timeout)??;
    Ok(())
}

/// Run one protocol over an owned child. `wire` receives every parsed object
/// and returns the frames to write back.
pub(crate) trait Wire: Send {
    fn start(&mut self, machine: &mut Machine) -> Result<Vec<Value>>;
    fn incoming(&mut self, machine: &mut Machine, value: Value) -> Result<Vec<Value>>;
    fn operation(
        &mut self,
        machine: &mut Machine,
        operation: Operation,
        reply: oneshot::Sender<Result<()>>,
    ) -> Result<Vec<Value>>;
    /// Called with the observed exit status before the adapter finalizes state.
    fn exited(&mut self, _machine: &mut Machine, _expected: bool) -> Result<()> {
        Ok(())
    }
}

pub(crate) async fn serve(
    mut child: Child,
    mut machine: Machine,
    mut commands: mpsc::Receiver<Command>,
    mut wire: Box<dyn Wire>,
) {
    let group = Guard::new(&child);
    let Some(mut input) = child.stdin.take() else {
        finalize(machine, None, false, Some(AdapterError::Closed("missing stdin".into()))).await;
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        finalize(machine, None, false, Some(AdapterError::Closed("missing stdout".into()))).await;
        return;
    };
    let Some(stderr) = child.stderr.take() else {
        finalize(machine, None, false, Some(AdapterError::Closed("missing stderr".into()))).await;
        return;
    };
    let mut output = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_WIRE_LINE));
    let mut errors = FramedRead::new(stderr, LinesCodec::new_with_max_length(MAX_WIRE_LINE));
    let mut stderr_open = true;
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    let mut shutdown_reply = None;
    let result: Result<()> = async {
        for message in wire.start(&mut machine)? {
            write(&mut input, &message).await?;
        }
        loop {
            let messages = tokio::select! {
                command = commands.recv() => match command {
                    Some(Command { operation: Operation::Shutdown, reply }) => {
                        shutdown_reply = Some(reply);
                        break;
                    }
                    Some(Command { operation, reply }) => wire.operation(&mut machine, operation, reply)?,
                    None => break,
                },
                line = output.next() => {
                    let value = process::parse_line(line)?;
                    wire.incoming(&mut machine, value)?
                },
                line = errors.next(), if stderr_open => {
                    match line {
                        Some(Ok(line)) => machine.emit(ChatEvent::Diagnostic { stream: "stderr".into(), output: bounded(line) })?,
                        Some(Err(error)) => return Err(AdapterError::Protocol(format!("stderr: {error}"))),
                        None => stderr_open = false,
                    }
                    Vec::new()
                },
                _ = interval.tick() => {
                    if machine.pending.values().any(|p| p.started.elapsed() > process::OP_TIMEOUT) {
                        return Err(AdapterError::Timeout);
                    }
                    if let Some(status) = child.try_wait()? {
                        return Err(AdapterError::Closed(format!("provider exited {status}")));
                    }
                    Vec::new()
                }
            };
            for message in messages {
                write(&mut input, &message).await?;
            }
            for reply in machine.deliveries.drain(..) {
                let _ = reply.send(Ok(()));
            }
        }
        Ok(())
    }
    .await;
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
    let record = wire.exited(&mut machine, expected);
    if let Err(error) = record {
        let _ = machine.emit(ChatEvent::Error {
            code: "adapter_failed".into(),
            message: error.to_string(),
            fatal: true,
        });
    }
    finalize(
        machine,
        status.and_then(|status| status.code()),
        expected,
        result.err(),
    )
    .await;
    if let Some(reply) = shutdown_reply {
        let _ = reply.send(Ok(()));
    }
}

async fn finalize(
    mut machine: Machine,
    code: Option<i32>,
    expected: bool,
    error: Option<AdapterError>,
) {
    let reason = error
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "session shut down".into());
    if let Some(error) = &error {
        if machine.active_turn.is_some() {
            let _ = machine.end_turn(None, 1, TurnOutcome::Failed);
        }
        let _ = machine.emit(ChatEvent::Error {
            code: "adapter_failed".into(),
            message: error.to_string(),
            fatal: true,
        });
    }
    if let Some(ready) = machine.ready.take() {
        let _ = ready.send(Err(AdapterError::Closed(reason.clone())));
    }
    let _ = machine.clear_requests();
    let _ = machine.emit(ChatEvent::ProcessExited { code, expected });
    let state = if expected {
        SessionState::Closed
    } else {
        SessionState::Failed {
            message: reason.clone(),
        }
    };
    machine.state.send_replace(state.clone());
    let _ = machine.emit(ChatEvent::StateChanged(state));
    for (_, pending) in machine.pending.drain() {
        if let Some(reply) = pending.reply {
            let _ = reply.send(Err(AdapterError::Closed(reason.clone())));
        }
    }
    for reply in machine.deliveries.drain(..) {
        let _ = reply.send(Err(AdapterError::Closed(reason.clone())));
    }
}

/// Bounded one-shot child capture used for `--version`, `--help` and
/// `create-chat`. This does not reuse `process::capture` because the M5
/// handshakes need a per-adapter timeout.
pub(crate) async fn capture(
    provider: &ProviderSpec,
    args: &[&str],
    timeout: Duration,
) -> Result<String> {
    let mut child = command(provider)
        .args(args)
        .stdin(Stdio::null())
        .spawn()?;
    let group = Guard::new(&child);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AdapterError::Protocol("missing stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AdapterError::Protocol("missing stderr".into()))?;
    let result = tokio::time::timeout(timeout, async {
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

/// Extract a bounded text value from an ACP/Pi content block or tool result.
pub(crate) fn text_content(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks.iter().map(text_content).collect(),
        Value::Object(object) => {
            if let Some(text) = object.get("text").and_then(Value::as_str) {
                return text.to_owned();
            }
            object
                .get("content")
                .map(text_content)
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// Minimal whole-file unified diff for ACP diff content. The daemon stores it
/// as text; it never applies patches from provider output.
pub(crate) fn whole_file_diff(path: &str, old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut diff = format!(
        "--- a/{path}\n+++ b/{path}\n@@ -1,{} +1,{} @@\n",
        old_lines.len(),
        new_lines.len()
    );
    for line in old_lines {
        diff.push('-');
        diff.push_str(line);
        diff.push('\n');
    }
    for line in new_lines {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}
