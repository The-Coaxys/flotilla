//! Frozen independently from the app protocol: existing PTYs can outlive a daemon upgrade.
use fl_model::{IncarnationId, InputId, LogCursor, ProcessInfo, Size, TerminalId, TerminalRecord};
use fl_proto::WireMessage;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;

pub const VERSION: u16 = 1;
pub const KIND_CLIENT: u16 = 0x100;
pub const KIND_SERVER: u16 = 0x101;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_READ_BYTES: usize = 512 * 1024;
pub const MAX_READ_RECORDS: usize = 256;
pub const DEFAULT_QUOTA_BYTES: u64 = 256 * 1024 * 1024;
pub const MIN_QUOTA_BYTES: u64 = 128 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub terminal: TerminalId,
    pub incarnation: IncarnationId,
    /// Empty selects the remote user's real login shell with `-l`.
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub size: Size,
    pub quota_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WriteId {
    Client(InputId),
    /// Stable sequence from the authoritative engine, scoped to this incarnation.
    Effect(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg<'a> {
    Hello {
        version: u16,
    },
    /// Encoding can borrow the original request for retries without copying its
    /// payload. Deserialization always produces an owned request.
    Request {
        id: u64,
        request: Cow<'a, Request>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        version: u16,
        pid: u32,
        instance: IncarnationId,
    },
    Rejected(Error),
    Response {
        id: u64,
        result: Result<Response, Error>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    List,
    Spawn(SpawnSpec),
    Info {
        terminal: TerminalId,
        incarnation: IncarnationId,
    },
    Read {
        terminal: TerminalId,
        incarnation: IncarnationId,
        from: LogCursor,
        max_bytes: u32,
        /// Long-poll wait, capped at 1,000 ms. No data is discarded by a slow reader.
        wait_ms: u16,
    },
    Write {
        terminal: TerminalId,
        incarnation: IncarnationId,
        id: WriteId,
        /// Always the original complete input. Retries write only its uncommitted suffix.
        bytes: Vec<u8>,
        /// Required for client input, absent for deterministic engine replies.
        /// Linux CLOCK_BOOTTIME nanoseconds, checked at the actual write.
        deadline_ns: Option<u64>,
    },
    Resize {
        terminal: TerminalId,
        incarnation: IncarnationId,
        size: Size,
    },
    FlushSync {
        terminal: TerminalId,
        incarnation: IncarnationId,
    },
    /// Reclaim only records covered by this SQLite-durable daemon checkpoint.
    AckCheckpoint {
        terminal: TerminalId,
        incarnation: IncarnationId,
        through: LogCursor,
    },
    SetQuota {
        terminal: TerminalId,
        incarnation: IncarnationId,
        bytes: u64,
    },
    Terminate {
        terminal: TerminalId,
        incarnation: IncarnationId,
        grace_ms: u16,
    },
    /// Explicit close cleanup; permitted only after child exit and final output drain.
    Forget {
        terminal: TerminalId,
        incarnation: IncarnationId,
    },
    ShutdownIfIdle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Processes(Vec<ProcessInfo>),
    Process(ProcessInfo),
    Records {
        records: Vec<TerminalRecord>,
        next: LogCursor,
        tail: LogCursor,
        state: fl_model::ProcessState,
    },
    Written {
        /// Exclusive committed prefix of the original input, including prior attempts.
        accepted: u32,
        echo: bool,
        at: LogCursor,
        /// The remaining suffix expired and must never be retried.
        expired: bool,
    },
    Cursor(LogCursor),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    UnsupportedVersion,
    ExpectedHello,
    NotFound,
    WrongIncarnation,
    AlreadyExists,
    InvalidArgument,
    CursorExpired,
    CursorMismatch,
    InputConflict,
    NotStopped,
    Backpressured,
    Internal,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}
impl std::error::Error for Error {}

impl WireMessage for ClientMsg<'_> {
    const KIND: u16 = KIND_CLIENT;
    fn validate(&self) -> Result<(), fl_proto::ValidationError> {
        if let Self::Request { request, .. } = self {
            request
                .validate()
                .map_err(|e| fl_proto::ValidationError::InvalidPayload(e.to_string()))?;
        }
        Ok(())
    }
}
impl WireMessage for ServerMsg {
    const KIND: u16 = KIND_SERVER;
    fn validate(&self) -> Result<(), fl_proto::ValidationError> {
        if let Self::Response {
            result: Ok(response),
            ..
        } = self
        {
            response
                .validate()
                .map_err(|e| fl_proto::ValidationError::InvalidPayload(e.to_string()))?;
        }
        Ok(())
    }
}

impl SpawnSpec {
    pub fn validate(&self) -> Result<(), Error> {
        self.size.validate().map_err(invalid)?;
        fl_proto::validation::launch(&self.argv, &self.cwd).map_err(invalid)?;
        if self.quota_bytes < MIN_QUOTA_BYTES {
            return Err(invalid("journal quota must be at least 128 KiB"));
        }
        Ok(())
    }
}

impl Request {
    pub fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Spawn(spec) => spec.validate(),
            Self::Read {
                max_bytes, wait_ms, ..
            } if !(MAX_OUTPUT_BYTES..=MAX_READ_BYTES).contains(&(*max_bytes as usize))
                || *wait_ms > 1000 =>
            {
                Err(invalid(
                    "read requires a 64–512 KiB budget and at most 1,000 ms wait",
                ))
            }
            Self::Write { bytes, .. } if bytes.len() > MAX_INPUT_BYTES => {
                Err(invalid("input exceeds 64 KiB"))
            }
            Self::Write {
                id: WriteId::Client(_),
                deadline_ns: None,
                ..
            } => Err(invalid(
                "client input requires an absolute boot-clock deadline",
            )),
            Self::Write {
                id: WriteId::Effect(_),
                deadline_ns: Some(_),
                ..
            } => Err(invalid("deterministic engine replies cannot expire")),
            Self::Resize { size, .. } => size.validate().map(|_| ()).map_err(invalid),
            Self::SetQuota { bytes, .. } if *bytes < MIN_QUOTA_BYTES => {
                Err(invalid("journal quota must be at least 128 KiB"))
            }
            Self::Terminate { grace_ms, .. } if *grace_ms > 5000 => {
                Err(invalid("termination grace exceeds five seconds"))
            }
            _ => Ok(()),
        }
    }
}

impl Response {
    /// Validate journal continuity before a consumer applies any part of a batch.
    /// The caller additionally binds the first cursor to its requested cursor.
    pub fn validate(&self) -> Result<(), Error> {
        let Self::Records {
            records,
            next,
            tail,
            ..
        } = self
        else {
            return Ok(());
        };
        if records.len() > MAX_READ_RECORDS {
            return Err(invalid("too many records in one journal response"));
        }
        let mut cursor = records.first().map_or(*next, |record| record.at);
        let mut byte_count = 0usize;
        for record in records {
            if record.at != cursor {
                return Err(Error::new(
                    ErrorCode::CursorMismatch,
                    "journal batch is not contiguous",
                ));
            }
            match &record.kind {
                fl_model::TerminalRecordKind::Output { bytes, .. } => {
                    if bytes.len() > MAX_OUTPUT_BYTES {
                        return Err(invalid("output record exceeds 64 KiB"));
                    }
                    byte_count += bytes.len();
                }
                fl_model::TerminalRecordKind::Resize(size) => {
                    size.validate().map_err(invalid)?;
                }
                _ => {}
            }
            cursor = record
                .end()
                .ok_or_else(|| invalid("journal cursor overflow"))?;
        }
        if byte_count > MAX_READ_BYTES {
            return Err(invalid("journal response exceeds 512 KiB of output"));
        }
        if cursor != *next || next.next_record > tail.next_record || next.next_byte > tail.next_byte
        {
            return Err(Error::new(
                ErrorCode::CursorMismatch,
                "journal batch end does not match its watermark",
            ));
        }
        Ok(())
    }
}

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::InvalidArgument, message.to_string())
}

#[cfg(test)]
mod tests;
