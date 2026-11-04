//! Concrete subprocess adapters: Claude Code stream-json, Codex app-server,
//! generic ACP, Cursor stream-json and Pi/OMP rpc.
//!
//! # Contract
//! `ProviderRegistry::discover` probes bounded CLI help/version output and a real
//! local protocol initialize handshake (no prompts or model requests). Failure is
//! terminal-only with an explanation. `Adapter::launch` repeats negotiation,
//! validates the requested resume target, and returns only after initialization.
//! Only a semantically invalid resume target (different native identity, cwd or
//! provider) is `InvalidResume` and may archive the old transcript. Transient
//! I/O, network and authentication failures are ordinary errors, so the caller
//! keeps the transcript restorable and retries later; neither case silently
//! continues with a new identity.
//!
//! Drain `Adapter::events` continuously and append records to storage. The channel
//! is bounded: a slow/dropped consumer terminates the child rather than silently
//! dropping chat records. `state()` remains authoritative even when the event
//! channel overflows. Backpressure is a fatal, observable failure, not success.
//! `send` requires idle; the daemon may queue messages outside this core. `steer`
//! requires a running turn. Claude acknowledges stdin delivery; Codex acknowledges
//! the correlated RPC response. Neither acknowledgment means the turn completed.
//! Approval IDs are opaque, incarnation-local, one-shot IDs. Only an explicit
//! `approve`/`respond_question` call can grant a pending permission. There is no
//! auto-approval or persistent-grant API. Pending requests are bounded and cleared
//! on turn end/cancel/exit. Unknown server requests receive protocol errors.
//!
//! `shutdown` closes stdin, waits briefly, then kills/reaps the owned process
//! group. Dropping the adapter also requests shutdown; dropping a launch future
//! closes its command channel. Child kill-on-drop is the final safety net. These
//! children are owned by this adapter task; prochost supervision/reattachment and
//! append-only storage are deliberately the caller's responsibility.
//!
//! # Manual invocation (uses real credentials and may incur cost)
//! `cargo run -p fld-agent --example chat -- claude /absolute/trusted/worktree`
//! (or `codex`). Enter a prompt, `/cancel`, `/approve <id>`, `/deny <id>`, or
//! `/quit`. Copy the `SessionReady.resume` record to relaunch via the Rust API.
//! This example never approves by default. Adapter protocol fixtures contain
//! recorded local initialize responses plus explicitly labeled synthetic turn
//! transcripts; no live model response is claimed by those fixtures.
mod acp;
mod claude;
mod codex;
mod cursor;
mod host;
mod pi;
mod process;
mod registry;

pub use fl_model::chat::*;
pub use registry::ProviderRegistry;
use tokio::sync::{mpsc, oneshot, watch};

pub const MAX_WIRE_LINE: usize = 1024 * 1024;
pub const MAX_TEXT: usize = 64 * 1024;
pub const EVENT_CAPACITY: usize = 256;
pub(crate) const MAX_PENDING: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported protocol: {0}")]
    Unsupported(String),
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid resume target: {0}")]
    InvalidResume(String),
    #[error("provider protocol: {0}")]
    Protocol(String),
    #[error("provider rejected request: {0}")]
    Rejected(String),
    #[error("session is busy; use steer or queue in the chat service")]
    Busy,
    #[error("there is no active turn")]
    NoActiveTurn,
    #[error("unknown, resolved, or wrong-kind request: {0}")]
    UnknownRequest(String),
    #[error("adapter closed: {0}")]
    Closed(String),
    #[error("provider operation timed out")]
    Timeout,
    #[error("event consumer overflowed or disconnected")]
    Backpressure,
}
pub type Result<T, E = AdapterError> = std::result::Result<T, E>;

pub struct Adapter {
    pub info: SessionInfo,
    pub events: mpsc::Receiver<EventRecord>,
    commands: mpsc::Sender<Command>,
    state: watch::Receiver<SessionState>,
}

enum Operation {
    Send(String),
    Steer(String),
    Cancel,
    Approve(String, ApprovalDecision),
    Question(String, QuestionResponse),
    Shutdown,
}
struct Command {
    operation: Operation,
    reply: oneshot::Sender<Result<()>>,
}

impl Adapter {
    pub async fn launch(provider: ProviderSpec, config: SessionConfig) -> Result<Self> {
        registry::validate_provider(&provider)?;
        match provider.adapter {
            AdapterKind::ClaudeStream | AdapterKind::CodexAppServer => {
                process::launch(provider, config).await
            }
            AdapterKind::Acp => acp::launch(provider, config).await,
            AdapterKind::CursorStreamJson => cursor::launch(provider, config).await,
            AdapterKind::PiRpc => pi::launch(provider, config).await,
        }
    }
    pub fn state(&self) -> SessionState {
        self.state.borrow().clone()
    }
    /// Move the event receiver out so a supervisor task can drain it while
    /// this adapter stays shared for commands. The replacement receiver is
    /// already closed and must never be drained.
    pub fn take_events(&mut self) -> mpsc::Receiver<EventRecord> {
        let (_sender, receiver) = mpsc::channel(1);
        std::mem::replace(&mut self.events, receiver)
    }
    pub fn watch_state(&self) -> watch::Receiver<SessionState> {
        self.state.clone()
    }
    pub async fn send(&self, text: impl Into<String>) -> Result<()> {
        self.request(Operation::Send(text.into())).await
    }
    pub async fn steer(&self, text: impl Into<String>) -> Result<()> {
        self.request(Operation::Steer(text.into())).await
    }
    pub async fn cancel(&self) -> Result<()> {
        self.request(Operation::Cancel).await
    }
    pub async fn approve(&self, id: impl Into<String>, decision: ApprovalDecision) -> Result<()> {
        self.request(Operation::Approve(id.into(), decision)).await
    }
    pub async fn respond_question(
        &self,
        id: impl Into<String>,
        response: QuestionResponse,
    ) -> Result<()> {
        self.request(Operation::Question(id.into(), response)).await
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.request(Operation::Shutdown).await
    }
    async fn request(&self, operation: Operation) -> Result<()> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command { operation, reply })
            .await
            .map_err(|_| AdapterError::Closed(format!("{:?}", self.state())))?;
        receive
            .await
            .map_err(|_| AdapterError::Closed(format!("{:?}", self.state())))?
    }
}
impl Drop for Adapter {
    fn drop(&mut self) {
        let (reply, _) = oneshot::channel();
        let _ = self.commands.try_send(Command {
            operation: Operation::Shutdown,
            reply,
        });
    }
}

pub(crate) fn bounded(value: impl AsRef<str>) -> BoundedText {
    let value = value.as_ref();
    let mut end = value.len().min(MAX_TEXT);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    BoundedText {
        text: value[..end].to_owned(),
        truncated: end != value.len(),
    }
}
pub(crate) fn string(value: &serde_json::Value, field: &str) -> String {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Adapters that have a real developer-instruction channel. ACP and Cursor
/// reject `SessionConfig::instructions` instead of silently dropping it.
pub fn supports_instructions(kind: AdapterKind) -> bool {
    matches!(
        kind,
        AdapterKind::ClaudeStream | AdapterKind::CodexAppServer | AdapterKind::PiRpc
    )
}
pub fn tool_kind(name: &str) -> ToolKind {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("mcp") {
        ToolKind::Mcp
    } else if lower.contains("edit")
        || lower.contains("write")
        || lower.contains("patch")
        || lower == "filechange"
    {
        ToolKind::Edit
    } else if lower.contains("bash") || lower.contains("command") || lower.contains("exec") {
        ToolKind::Exec
    } else if lower.contains("web") {
        ToolKind::Web
    } else if lower.contains("grep") || lower.contains("glob") || lower.contains("search") {
        ToolKind::Search
    } else if lower.contains("read") {
        ToolKind::Read
    } else {
        ToolKind::Other
    }
}
