//! Shared domain values. This crate has no terminal engine, transport or UI dependency.
pub mod forge;
pub mod layout;
pub mod review;
pub mod sections;
pub mod workspace;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! identity {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self { Self(Uuid::new_v4()) }
            pub const fn from_bytes(bytes: [u8; 16]) -> Self { Self(Uuid::from_bytes(bytes)) }
            pub const fn as_bytes(&self) -> &[u8; 16] { self.0.as_bytes() }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) }
        }
        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(value: &str) -> Result<Self, Self::Err> { value.parse().map(Self) }
        }
    )+};
}

identity!(
    WorkspaceId,
    ProjectId,
    WorktreeId,
    SectionId,
    TabId,
    ViewId,
    PaneId,
    SplitId,
    TerminalId,
    IncarnationId,
    OperationId,
    ClientSessionId,
    TransferId,
    ReviewThreadId,
    ReviewCommentId,
    SubscriptionId
);

pub const MAX_COLUMNS: u16 = 1024;
pub const MAX_SCREEN_ROWS: u16 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub columns: u16,
    pub rows: u16,
    /// Actual owner-supplied cell dimensions; zero means unknown.
    pub cell_width: u16,
    pub cell_height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("terminal geometry exceeds supported columns, rows or pixel dimensions")]
pub struct InvalidSize;

impl Size {
    pub const fn new(columns: u16, rows: u16) -> Self {
        Self {
            columns,
            rows,
            cell_width: 0,
            cell_height: 0,
        }
    }

    pub fn validate(self) -> Result<Self, InvalidSize> {
        if !(2..=MAX_COLUMNS).contains(&self.columns)
            || !(1..=MAX_SCREEN_ROWS).contains(&self.rows)
            || self.columns.checked_mul(self.cell_width).is_none()
            || self.rows.checked_mul(self.cell_height).is_none()
        {
            return Err(InvalidSize);
        }
        Ok(self)
    }
}

/// The exclusive end of consumed records and output bytes. Resize/control records
/// advance `next_record`, not `next_byte`; output advances both.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogCursor {
    pub next_record: u64,
    pub next_byte: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InputId {
    pub client: ClientSessionId,
    pub sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessState {
    Running,
    Backpressured {
        retained_bytes: u64,
        quota_bytes: u64,
        reason: String,
    },
    Draining(ExitStatus),
    Exited(ExitStatus),
    /// The host was restarted or its owning prochost is no longer available.
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub terminal: TerminalId,
    pub incarnation: IncarnationId,
    pub pid: u32,
    pub size: Size,
    pub argv: Vec<String>,
    pub cwd: String,
    pub state: ProcessState,
    pub tail: LogCursor,
    pub echo: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputOwner {
    pub client: ClientSessionId,
    pub view: ViewId,
    /// Daemon-issued lease generation; former owners cannot resize with stale leases.
    pub generation: u64,
}

/// Ordered canonical input to the daemon and each mirrored emulator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRecord {
    pub at: LogCursor,
    pub kind: TerminalRecordKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalRecordKind {
    Output { bytes: bytes::Bytes, echo: bool },
    Resize(Size),
    SyncFlush,
    Echo(bool),
    Exit(ExitStatus),
}

impl TerminalRecord {
    pub fn end(&self) -> Option<LogCursor> {
        let bytes = match &self.kind {
            TerminalRecordKind::Output { bytes, .. } => bytes.len() as u64,
            _ => 0,
        };
        Some(LogCursor {
            next_record: self.at.next_record.checked_add(1)?,
            next_byte: self.at.next_byte.checked_add(bytes)?,
        })
    }
}
