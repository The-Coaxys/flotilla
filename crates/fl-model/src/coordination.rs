//! Durable coordination state shared by agents and the human (M6).
//!
//! Keys are versioned per scope. A version starts at 1 and strictly increases
//! for every successful write; a compare-and-set write names the version it
//! observed (0 means "create only"). Advisory locks carry an owner identity
//! and expire after a caller-selected TTL, so a crashed agent cannot hold one
//! forever.
use serde::{Deserialize, Serialize};

use crate::{WorkspaceId, WorktreeId};

/// Keys and owners stay short enough to log and display verbatim.
pub const MAX_COORDINATION_KEY_BYTES: usize = 256;
pub const MAX_COORDINATION_OWNER_BYTES: usize = 256;
/// One value is bounded like a large message, not like a file.
pub const MAX_COORDINATION_VALUE_BYTES: usize = 64 * 1024;
pub const MIN_LOCK_TTL_MS: u64 = 1_000;
pub const MAX_LOCK_TTL_MS: u64 = 24 * 60 * 60 * 1_000;

/// The namespace a coordination key or lock lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CoordinationScope {
    Global,
    Workspace(WorkspaceId),
    Worktree(WorktreeId),
}

impl CoordinationScope {
    /// Stable, human-readable name used by the CLI and errors.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace(_) => "workspace",
            Self::Worktree(_) => "worktree",
        }
    }
}

/// One durable key/value entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinationValue {
    pub scope: CoordinationScope,
    pub key: String,
    pub value: String,
    /// Starts at 1 and strictly increases on every successful write.
    pub version: u64,
    pub updated_ms: u64,
}

/// An advisory lock. Expired locks are reported as absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinationLock {
    pub scope: CoordinationScope,
    pub key: String,
    pub owner: String,
    pub acquired_ms: u64,
    pub expires_ms: u64,
}

/// One `get`: the value and any unexpired lock, either of which may be absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinationSnapshot {
    pub value: Option<CoordinationValue>,
    pub lock: Option<CoordinationLock>,
}

/// A provider hook the daemon may turn into chat facts. This is already
/// normalized by `fl hook`; the daemon never parses provider-specific JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEvent {
    /// The provider's hook name, e.g. `PreToolUse`, `Stop` or `notify`.
    pub name: String,
    pub tool: Option<String>,
    /// Provider-native tool-call identity when the hook supplies one.
    pub tool_id: Option<String>,
    /// Bounded detail text (tool input summary, message, …).
    pub detail: String,
    /// `Some(true)` only when the hook explicitly reports success.
    pub success: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookFactKind {
    /// The provider reported that it began working on a turn.
    TurnStarted,
    /// The provider reported that its turn ended. This never asserts an
    /// outcome: only structured adapter events may do that.
    TurnEnded,
    /// A notification with no state consequence.
    Notification,
}

/// A state fact that came from a provider hook rather than a structured
/// adapter event. It is persisted as a chat event so it appears in the
/// transcript, but its status source is always `Hook`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookFact {
    pub kind: HookFactKind,
    pub detail: String,
}
