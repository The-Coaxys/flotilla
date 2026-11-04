//! Workspaces, projects, worktrees and their git state (M2).
//!
//! Paths are absolute UTF-8 paths on the daemon host. Every collection here
//! is bounded by `fl-proto` validation before it crosses the wire.
use serde::{Deserialize, Serialize};

use crate::{ProjectId, TerminalId, WorkspaceId, WorktreeId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub name: String,
    /// Sidebar order, ascending.
    pub order: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectKind {
    Git,
    /// A plain directory: files and terminals, no branch workflows.
    Folder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub workspace: WorkspaceId,
    pub name: String,
    /// The primary checkout (the repository's main worktree).
    pub path: String,
    pub kind: ProjectKind,
    /// The branch new worktrees start from and diff against, e.g. `main`.
    pub default_branch: Option<String>,
    /// Directory that holds this project's created worktrees.
    pub worktree_root: String,
    pub order: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub id: WorktreeId,
    pub project: ProjectId,
    pub path: String,
    /// `None` on a detached HEAD or a folder project.
    pub branch: Option<String>,
    /// The branch this worktree diffs against (merge-base target).
    pub target: Option<String>,
    /// Display name; defaults to the branch or directory name.
    pub label: String,
    /// The project's main checkout. It cannot be deleted from Flotilla.
    pub is_primary: bool,
    pub created_ms: u64,
    pub last_active_ms: u64,
}

/// Everything the sidebar shows, in one consistent snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTree {
    /// Bumped on every structural change; clients drop older snapshots.
    pub rev: u64,
    pub workspaces: Vec<WorkspaceInfo>,
    pub projects: Vec<Project>,
    pub worktrees: Vec<Worktree>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Path relative to the worktree root.
    pub path: String,
    /// Source path of a rename or copy.
    pub old_path: Option<String>,
    pub kind: ChangeKind,
    /// Line counts against the merge-base with the target; `None` for binary.
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSummary {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Totals over `files` (the sidebar's +/− counts).
    pub added: u64,
    pub removed: u64,
    /// Uncommitted changes in the working tree or index.
    pub dirty: bool,
    pub conflicted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatus {
    pub worktree: WorktreeId,
    pub summary: GitSummary,
    /// Changes against the merge-base with the target, including
    /// uncommitted and untracked files, sorted by path.
    pub files: Vec<FileChange>,
    /// Daemon wall clock at computation, milliseconds since the epoch.
    pub at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified_ms: u64,
    /// Matched by the worktree's `.gitignore` (shown dimmed, never hidden).
    pub ignored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirListing {
    /// Canonical absolute path that was listed.
    pub path: String,
    /// Directories first, then files, each sorted case-insensitively.
    pub entries: Vec<DirEntry>,
    /// More entries existed than the listing limit allows.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub bytes: Vec<u8>,
    /// SHA-256 of the complete file (also when `truncated`); writes use it
    /// to refuse overwriting a file that changed since it was read.
    pub sha256: [u8; 32],
    pub size: u64,
    pub truncated: bool,
    /// Contains NUL bytes in its first 8 KiB.
    pub binary: bool,
}

/// Both sides of one changed file: `old` at the merge-base with the target
/// (absent for added/untracked files), `new` in the working tree (absent
/// for deleted files).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub worktree: WorktreeId,
    pub path: String,
    pub old: Option<FileContent>,
    pub new: Option<FileContent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScriptKind {
    Setup,
    /// A named run target from `.flotilla/config.json`; `None` is the default.
    Run(Option<String>),
    Teardown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScriptRunState {
    Idle,
    Running,
    Exited { code: Option<i32> },
}

/// Scripts run as ordinary daemon terminals, so their output survives
/// reconnects and daemon restarts like any shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptState {
    pub worktree: WorktreeId,
    pub kind: ScriptKind,
    pub terminal: Option<TerminalId>,
    pub state: ScriptRunState,
}

/// The caller of one `fl` invocation, resolved from `FLOTILLA_SESSION_ID`
/// and the working directory (protocol 0.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerContext {
    /// Absolute working directory supplied by the client.
    pub cwd: String,
    /// Longest worktree path containing `cwd`; `None` outside every worktree.
    pub worktree: Option<Worktree>,
    pub project: Option<Project>,
    /// The chat session named by `FLOTILLA_SESSION_ID`, when it still exists.
    pub session: Option<crate::chat::ChatSession>,
}

/// Configured scripts of one worktree, as parsed from `.flotilla/config.json`
/// for `fl project scripts check` (protocol 0.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectScriptCheck {
    pub worktree: WorktreeId,
    pub config_path: String,
    /// The config read or parse failure; `None` when the file is absent or valid.
    pub error: Option<String>,
    pub setup: bool,
    pub teardown: bool,
    /// Run target names that have at least one command, sorted by name.
    pub runs: Vec<String>,
}

/// Where a layout lives: the host's home layout (M1) or one worktree's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayoutScope {
    Home,
    Worktree(WorktreeId),
}

/// Pushed by the daemon to every authenticated session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonEvent {
    WorkspaceTreeChanged {
        rev: u64,
    },
    WorktreeStatus(WorktreeStatus),
    ScriptChanged(ScriptState),
    LayoutChanged {
        scope: LayoutScope,
        rev: u64,
    },
    /// The forge state of a worktree changed (polled or after an action).
    ForgeChanged(crate::forge::ForgeState),
    /// App-managed reviews changed; fetch the bounded snapshot.
    ReviewChanged {
        worktree: WorktreeId,
        rev: u64,
    },
    /// Section definitions, assignments or pins changed (protocol 0.4).
    SectionsChanged(crate::sections::SectionsState),
    /// Fast/Thorough routes or prompt templates changed (protocol 0.4).
    RoutingChanged(crate::routing::RoutingState),
    /// A routed Git task started, advanced or finished (protocol 0.4).
    TaskDispatchChanged(crate::routing::TaskDispatch),
    /// A chat session was created, reconnected, or changed status
    /// (protocol 0.4).
    ChatSessionChanged(crate::chat::ChatSession),
    /// Durable chat events were appended; fetch them with `GetChatEvents`
    /// from `sequence` (protocol 0.4). Events are hints: a reconnect pages
    /// from the last durable sequence instead of trusting this.
    ChatEventAppended {
        session: uuid::Uuid,
        worktree: WorktreeId,
        incarnation: uuid::Uuid,
        /// The newest durable sequence in this append batch.
        sequence: u64,
    },
    /// Inbox items were added, read, or dismissed (protocol 0.4).
    InboxChanged,
    /// A coordination value or lock changed (protocol 0.4). `version` is 0
    /// for a deletion; watchers re-read the key instead of trusting this.
    CoordinationChanged {
        scope: crate::coordination::CoordinationScope,
        key: String,
        version: u64,
    },
}
