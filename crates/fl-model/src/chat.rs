//! Versioned, transport-independent chat records. Persist `EventRecord`s append-only;
//! sequence numbers are scoped to one adapter incarnation, not a global store cursor.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use uuid::Uuid;

pub const CHAT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterKind {
    ClaudeStream,
    CodexAppServer,
    /// Generic Agent Client Protocol (ACP) JSON-RPC over stdio.
    Acp,
    /// Cursor headless `--output-format stream-json` one process per turn.
    CursorStreamJson,
    /// Pi / Oh My Pi `--mode rpc` JSONL protocol.
    PiRpc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeStrategy {
    ClaudeSession,
    CodexThread,
    /// ACP `session/load` or the unstable `session/resume` capability.
    AcpSession,
    /// Cursor chat ID created by `create-chat` and reread via `--resume`.
    CursorChat,
    /// Pi / OMP session ID from `get_state`.
    PiSession,
}

/// Trusted, user-owned launch configuration; never populated from provider output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSpec {
    pub id: String,
    pub name: String,
    pub command: PathBuf,
    /// Wrapper arguments, before adapter-owned arguments. Protocol/permission
    /// switches are reserved and rejected by the adapter.
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub adapter: AdapterKind,
    pub resume: ResumeStrategy,
    pub hook_support: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub chat: bool,
    pub resume: bool,
    pub steer: bool,
    pub approvals: bool,
    pub questions: bool,
    pub partial_messages: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDiscovery {
    pub provider: ProviderSpec,
    pub version: Option<String>,
    pub capabilities: Capabilities,
    /// Terminal-only providers carry the concrete discovery/protocol failure.
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeTarget {
    pub provider_id: String,
    pub adapter: AdapterKind,
    pub native_id: String,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    Ask,
    ReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub permission_mode: PermissionMode,
    pub instructions: Option<String>,
    /// Supply the stored Flotilla identity on cold restore; None creates one.
    pub session_id: Option<Uuid>,
    /// Claude --settings or Codex thread config, encoded as a JSON object.
    /// JSON is a string here so domain records also round-trip with postcard.
    pub provider_config_json: Option<String>,
    /// Provider-native JSON Schema enforced for each generated answer.
    pub output_schema_json: Option<String>,
    pub resume: Option<ResumeTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: Uuid,
    pub incarnation: Uuid,
    pub resume: ResumeTarget,
    pub model: Option<String>,
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Starting,
    Idle,
    Running,
    AwaitingInput,
    Closed,
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    pub schema_version: u16,
    pub session_id: Uuid,
    pub incarnation: Uuid,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub native_session_id: Option<String>,
    pub turn_id: Option<String>,
    pub event: ChatEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextUpdate {
    Delta,
    Final,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolKind {
    Read,
    Edit,
    Exec,
    Search,
    Web,
    Mcp,
    Other,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundedText {
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub kind: ToolKind,
    pub title: String,
    pub input_summary: BoundedText,
    pub status: ToolStatus,
    pub output: Option<BoundedText>,
    pub diff: Option<BoundedText>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    AllowOnce,
    Deny,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub text: String,
    pub options: Vec<QuestionOption>,
    pub multi: bool,
    pub free_text: bool,
    pub secret: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionResponse {
    pub answers: BTreeMap<String, Vec<String>>,
    /// For JSON-schema MCP forms. This is supplied by the human, not inferred.
    pub structured_json: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    /// Decimal USD string: avoid floating-point drift in durable records.
    pub cost_usd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatEvent {
    SessionReady(SessionInfo),
    StateChanged(SessionState),
    UserMessage {
        id: String,
        text: String,
    },
    AssistantText {
        id: String,
        update: TextUpdate,
        text: String,
    },
    StructuredOutput {
        json: String,
    },
    Thinking {
        id: String,
        delta: String,
    },
    ToolCall(ToolCall),
    ToolOutput {
        id: String,
        delta: BoundedText,
    },
    FileEdit {
        path: PathBuf,
        unified_diff: BoundedText,
    },
    ApprovalRequest {
        id: String,
        tool: String,
        details: BoundedText,
        options: Vec<ApprovalDecision>,
    },
    QuestionRequest {
        id: String,
        questions: Vec<Question>,
        schema_json: Option<String>,
    },
    RequestResolved {
        id: String,
    },
    Subagent {
        parent_tool_id: String,
        events: Vec<ChatEvent>,
    },
    TurnEnd {
        duration_ms: Option<u64>,
        turns: u64,
        usage: Usage,
        model: Option<String>,
        outcome: TurnOutcome,
    },
    Error {
        code: String,
        message: String,
        fatal: bool,
    },
    Diagnostic {
        stream: String,
        output: BoundedText,
    },
    ProcessExited {
        code: Option<i32>,
        expected: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_adapter_and_resume_strategy_round_trips_on_wire_and_postcard() {
        let pairs = [
            (AdapterKind::ClaudeStream, ResumeStrategy::ClaudeSession),
            (AdapterKind::CodexAppServer, ResumeStrategy::CodexThread),
            (AdapterKind::Acp, ResumeStrategy::AcpSession),
            (AdapterKind::CursorStreamJson, ResumeStrategy::CursorChat),
            (AdapterKind::PiRpc, ResumeStrategy::PiSession),
        ];
        for (adapter, resume) in pairs {
            let json = serde_json::to_string(&adapter).unwrap();
            assert_eq!(serde_json::from_str::<AdapterKind>(&json).unwrap(), adapter);
            let bytes = postcard::to_stdvec(&adapter).unwrap();
            assert_eq!(postcard::from_bytes::<AdapterKind>(&bytes).unwrap(), adapter);
            let json = serde_json::to_string(&resume).unwrap();
            assert_eq!(
                serde_json::from_str::<ResumeStrategy>(&json).unwrap(),
                resume
            );
            let bytes = postcard::to_stdvec(&resume).unwrap();
            assert_eq!(
                postcard::from_bytes::<ResumeStrategy>(&bytes).unwrap(),
                resume
            );
        }
        // Wire names are part of the persisted route contract.
        assert_eq!(
            serde_json::to_string(&AdapterKind::CursorStreamJson).unwrap(),
            "\"CursorStreamJson\""
        );
    }

    #[test]
    fn nested_events_and_configs_are_binary_durable() {
        let record = EventRecord {
            schema_version: CHAT_SCHEMA_VERSION,
            session_id: Uuid::nil(),
            incarnation: Uuid::nil(),
            sequence: 7,
            timestamp_ms: 1,
            native_session_id: Some("native".into()),
            turn_id: None,
            event: ChatEvent::Subagent {
                parent_tool_id: "tool".into(),
                events: vec![ChatEvent::TurnEnd {
                    duration_ms: None,
                    turns: 1,
                    usage: Usage {
                        cost_usd: Some("0.00123".into()),
                        ..Usage::default()
                    },
                    model: None,
                    outcome: TurnOutcome::Completed,
                }],
            },
        };
        let bytes = postcard::to_stdvec(&record).unwrap();
        assert_eq!(postcard::from_bytes::<EventRecord>(&bytes).unwrap(), record);
    }

    #[test]
    fn search_text_collects_messages_tools_and_questions_with_a_hard_bound() {
        let event = ChatEvent::UserMessage {
            id: "m1".into(),
            text: "fix the build".into(),
        };
        assert!(search_text(&event).contains("fix the build"));
        let huge = ChatEvent::AssistantText {
            id: "m2".into(),
            update: TextUpdate::Final,
            text: "x".repeat(MAX_CHAT_TEXT + 4096),
        };
        assert_eq!(search_text(&huge).len(), MAX_CHAT_TEXT);
        let nested = ChatEvent::Subagent {
            parent_tool_id: "tool".into(),
            events: vec![ChatEvent::QuestionRequest {
                id: "q".into(),
                questions: vec![Question {
                    id: "q1".into(),
                    text: "Which target?".into(),
                    options: vec![QuestionOption {
                        label: "release".into(),
                        description: "production".into(),
                    }],
                    multi: false,
                    free_text: true,
                    secret: false,
                }],
                schema_json: None,
            }],
        };
        let text = search_text(&nested);
        assert!(text.contains("Which target?"));
        assert!(text.contains("release"));
    }
}

// --- Daemon-owned chat records (M4 wire contract) -------------------------

pub const MAX_CHAT_MESSAGES: usize = 256;
pub const MAX_CHAT_TEXT: usize = 64 * 1024;
pub const MAX_CHAT_HISTORY: usize = 512;

/// One chat the daemon owns: a provider process, its transcript cursor and
/// its surface. `seq` is the last durable event sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: Uuid,
    pub worktree: crate::WorktreeId,
    pub provider: String,
    pub label: String,
    pub status: SessionState,
    pub status_source: StatusSource,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub permission_mode: PermissionMode,
    pub resume: Option<ResumeTarget>,
    pub seq: u64,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// Where a session status came from. Heuristics never approve anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusSource {
    Structured,
    Hook,
    Heuristic,
}

/// Durable events from `from` (inclusive) onward.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatEventPage {
    pub session: Uuid,
    pub incarnation: Uuid,
    pub events: Vec<EventRecord>,
    /// More events exist after this page.
    pub more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InboxKind {
    Approval,
    Question,
    Completed,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InboxState {
    Unread,
    Read,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: Uuid,
    pub session: Uuid,
    pub worktree: crate::WorktreeId,
    pub kind: InboxKind,
    pub state: InboxState,
    pub title: String,
    pub created_ms: u64,
}

/// One archived transcript hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatHit {
    pub session: Uuid,
    pub worktree: crate::WorktreeId,
    pub sequence: u64,
    pub snippet: String,
}

/// Plain text a durable event contributes to full-text search. It is bounded
/// by [`MAX_CHAT_TEXT`] so a pathological event cannot inflate the index.
pub fn search_text(event: &ChatEvent) -> String {
    struct Text(String);
    impl Text {
        fn push(&mut self, value: &str) {
            if self.0.len() >= MAX_CHAT_TEXT {
                return;
            }
            let separator = !self.0.is_empty();
            let mut remaining = MAX_CHAT_TEXT - self.0.len();
            if separator {
                self.0.push('\n');
                remaining -= 1;
            }
            let mut end = value.len().min(remaining);
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            self.0.push_str(&value[..end]);
        }
        fn bounded(&mut self, value: &BoundedText) {
            self.push(&value.text);
        }
    }
    fn walk(text: &mut Text, event: &ChatEvent, depth: usize) {
        if depth > 8 {
            return;
        }
        match event {
            ChatEvent::SessionReady(info) => {
                text.push(&info.resume.native_id);
                if let Some(model) = &info.model {
                    text.push(model);
                }
            }
            ChatEvent::StateChanged(_) => {}
            ChatEvent::UserMessage { text: value, .. }
            | ChatEvent::AssistantText { text: value, .. } => text.push(value),
            ChatEvent::StructuredOutput { json } => text.push(json),
            ChatEvent::Thinking { delta, .. } => text.push(delta),
            ChatEvent::ToolCall(call) => {
                text.push(&call.title);
                text.bounded(&call.input_summary);
                if let Some(output) = &call.output {
                    text.bounded(output);
                }
                if let Some(diff) = &call.diff {
                    text.bounded(diff);
                }
            }
            ChatEvent::ToolOutput { delta, .. } => text.bounded(delta),
            ChatEvent::FileEdit { path, unified_diff } => {
                text.push(&path.to_string_lossy());
                text.bounded(unified_diff);
            }
            ChatEvent::ApprovalRequest { tool, details, .. } => {
                text.push(tool);
                text.bounded(details);
            }
            ChatEvent::QuestionRequest { questions, .. } => {
                for question in questions {
                    text.push(&question.text);
                    for option in &question.options {
                        text.push(&option.label);
                        text.push(&option.description);
                    }
                }
            }
            ChatEvent::RequestResolved { .. } => {}
            ChatEvent::Subagent { events, .. } => {
                for event in events {
                    walk(text, event, depth + 1);
                }
            }
            ChatEvent::TurnEnd { model, .. } => {
                if let Some(model) = model {
                    text.push(model);
                }
            }
            ChatEvent::Error { message, .. } => text.push(message),
            ChatEvent::Diagnostic { output, .. } => text.bounded(output),
            ChatEvent::ProcessExited { .. } => {}
        }
    }
    let mut text = Text(String::new());
    walk(&mut text, event, 0);
    text.0
}
