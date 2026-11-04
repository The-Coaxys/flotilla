//! Durable daemon state. Filesystem/SQLite work belongs off runtime/UI workers.
pub mod checkpoint;
pub mod database;
mod store;
use fl_model::{
    IncarnationId, LogCursor, OperationId, ProcessInfo, Size, TerminalId, WorktreeId,
    layout::Layout,
};
use fl_prochost_proto::SpawnSpec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub use store::Store;

pub type BlobHash = [u8; 32];
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalKey {
    pub terminal: TerminalId,
    pub incarnation: IncarnationId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRef {
    pub hash: BlobHash,
    pub cursor: LogCursor,
    pub size: Size,
    pub engine_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalEntry {
    pub spec: SpawnSpec,
    pub desired: bool,
    pub worktree: Option<WorktreeId>,
    pub process: Option<ProcessInfo>,
    pub launch_error: Option<String>,
    pub checkpoint: Option<CheckpointRef>,
}
#[derive(Debug, Clone)]
pub struct LaunchDefaults {
    pub cwd: String,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub quota_bytes: u64,
}
pub struct AppliedLayout {
    pub layout: Layout,
    pub spawned: Option<TerminalEntry>,
    pub closed: smallvec::SmallVec<[TerminalKey; 4]>,
    pub replayed: bool,
}
/// Worktrees forgotten in one commit and the terminal intents they closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedWorktrees {
    pub rev: u64,
    pub worktrees: Vec<WorktreeId>,
    pub closed: Vec<TerminalKey>,
}
/// One worktree script and its current terminal incarnation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptEntry {
    pub worktree: WorktreeId,
    pub kind: String,
    pub key: TerminalKey,
}
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid durable state input: {0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Codec(#[from] postcard::Error),
    #[error(transparent)]
    Layout(#[from] fl_model::layout::LayoutError),
    #[error(transparent)]
    Engine(#[from] fl_term::EngineError),
    #[error("terminal not found: {0}")]
    NotFound(TerminalId),
    #[error("wrong terminal incarnation")]
    WrongIncarnation,
    #[error("layout revision conflict: expected {expected}, got {got}")]
    RevisionConflict { expected: u64, got: u64 },
    #[error("operation ID reused with different payload: {0}")]
    OperationConflict(OperationId),
    #[error("corrupt durable state: {0}")]
    Corrupt(String),
    #[error("database schema {0} is newer than supported")]
    FutureSchema(u32),
    #[error("stale checkpoint cursor")]
    StaleCheckpoint,
    #[error("terminal is still desired by its layout")]
    TerminalActive,
    /// A workspace, project or worktree that does not exist.
    #[error("{0}")]
    Missing(String),
    /// A project or worktree path that is already known.
    #[error("{0}")]
    Duplicate(String),
    /// A compare-and-set or advisory-lock conflict, with the reason.
    #[error("{0}")]
    Conflict(String),
    /// The caller does not own the coordination lock it tried to release.
    #[error("{0}")]
    NotOwner(String),
}
pub type Result<T, E = StoreError> = std::result::Result<T, E>;
