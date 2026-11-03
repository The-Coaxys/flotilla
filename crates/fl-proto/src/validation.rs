//! Semantic wire bounds, enforced by `WireMessage` at both boundaries.
//! These checks do not authorize sessions, incarnations, leases or transfers.
use fl_model::{ProcessInfo, ProcessState, Size};

pub const MAX_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_RECORDS_PER_BATCH: usize = 256;
/// Entire postcard-encoded RecordBatch server message, including its envelope.
pub const MAX_RECORD_BATCH_BYTES: usize = 512 * 1024;
pub const MAX_BULK_CHUNK_BYTES: usize = 512 * 1024;
pub const MAX_CHANNEL_LIFETIME_MS: u32 = 60_000;
pub const MAX_ENGINE_ID_BYTES: usize = 128;
/// Matches the terminal checkpoint codec's 512 MiB encoded-byte ceiling,
/// without taking a dependency on the engine.
pub const MAX_CHECKPOINT_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_PROCESS_COUNT: usize = 128;
pub const MAX_ARG_COUNT: usize = 256;
pub const MAX_TEXT_BYTES: usize = 4096;
pub const MAX_ARGV_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("{field} exceeds {max} (got {actual})")]
    Limit {
        field: &'static str,
        max: u64,
        actual: u64,
    },
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
    #[error("invalid {0}")]
    Invalid(&'static str),
    #[error("unsupported {field}: {got} (expected {expected})")]
    Schema {
        field: &'static str,
        expected: u16,
        got: u16,
    },
}

pub(crate) fn bound(field: &'static str, actual: usize, max: usize) -> Result<(), ValidationError> {
    bound_u64(field, actual as u64, max as u64)
}

pub(crate) fn bound_u64(field: &'static str, actual: u64, max: u64) -> Result<(), ValidationError> {
    if actual > max {
        Err(ValidationError::Limit { field, max, actual })
    } else {
        Ok(())
    }
}

pub(crate) fn schema(field: &'static str, got: u16, expected: u16) -> Result<(), ValidationError> {
    if got != expected {
        Err(ValidationError::Schema {
            field,
            got,
            expected,
        })
    } else {
        Ok(())
    }
}

pub(crate) fn text(field: &'static str, value: &str, max: usize) -> Result<(), ValidationError> {
    bound(field, value.len(), max)?;
    if value.contains('\0') {
        return Err(ValidationError::Invalid(field));
    }
    Ok(())
}

pub(crate) fn engine(value: &str) -> Result<(), ValidationError> {
    text("engine_id", value, MAX_ENGINE_ID_BYTES)?;
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(ValidationError::Invalid("engine_id"));
    }
    Ok(())
}

pub(crate) fn size(value: Size) -> Result<(), ValidationError> {
    value
        .validate()
        .map(|_| ())
        .map_err(|_| ValidationError::Invalid("size"))
}

/// Launch bounds also apply before spawning a child, so its metadata remains
/// representable on every control stream after a successful launch.
pub fn launch(argv: &[String], cwd: &str) -> Result<(), ValidationError> {
    bound("argv count", argv.len(), MAX_ARG_COUNT)?;
    let mut total = 0usize;
    for arg in argv {
        text("argv argument", arg, MAX_TEXT_BYTES)?;
        total += arg.len();
    }
    bound("argv bytes", total, MAX_ARGV_BYTES)?;
    text("cwd", cwd, MAX_TEXT_BYTES)
}

pub(crate) fn process(value: &ProcessInfo) -> Result<(), ValidationError> {
    size(value.size)?;
    launch(&value.argv, &value.cwd)?;
    if let ProcessState::Backpressured {
        reason,
        quota_bytes,
        ..
    } = &value.state
    {
        text("backpressure reason", reason, MAX_TEXT_BYTES)?;
        if *quota_bytes == 0 {
            return Err(ValidationError::Invalid("retention quota"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::{IncarnationId, LogCursor, TerminalId};

    fn info() -> ProcessInfo {
        ProcessInfo {
            terminal: TerminalId::from_bytes([1; 16]),
            incarnation: IncarnationId::from_bytes([2; 16]),
            pid: 1,
            size: Size::new(80, 24),
            argv: vec!["sh".into()],
            cwd: "/".into(),
            state: ProcessState::Running,
            tail: LogCursor::default(),
            echo: true,
        }
    }

    #[test]
    fn process_metadata_cannot_bypass_wire_geometry_and_string_bounds() {
        let mut value = info();
        assert!(process(&value).is_ok());
        value.size.cell_width = u16::MAX;
        assert!(process(&value).is_err());
        value.size = Size::new(80, 24);
        value.argv = vec!["x".repeat(MAX_TEXT_BYTES); MAX_ARGV_BYTES / MAX_TEXT_BYTES];
        assert!(process(&value).is_ok());
        value.argv.push("x".into());
        assert!(matches!(
            process(&value),
            Err(ValidationError::Limit {
                field: "argv bytes",
                ..
            })
        ));
        value.argv = vec!["x".into(); MAX_ARG_COUNT + 1];
        assert!(matches!(
            process(&value),
            Err(ValidationError::Limit {
                field: "argv count",
                ..
            })
        ));
        value.argv.clear();
        value.cwd = "/bad\0path".into();
        assert!(process(&value).is_err());
    }

    #[test]
    fn process_list_rejects_duplicate_terminal_identity() {
        let value = info();
        assert!(
            crate::Response::Processes(vec![value.clone(), value])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn capabilities_reject_unknown_schema_duplicate_roles_and_invalid_engine() {
        let mut value = crate::Capabilities {
            terminal_protocol: crate::TERMINAL_PROTOCOL,
            channel_schema: crate::CHANNEL_SCHEMA,
            engine_id: "engine".into(),
            roles: vec![crate::ChannelRole::TerminalHot],
            clock_ns: 1,
        };
        assert!(value.validate().is_ok());
        value.terminal_protocol += 1;
        assert!(matches!(
            value.validate(),
            Err(ValidationError::Schema { .. })
        ));
        value.terminal_protocol = crate::TERMINAL_PROTOCOL;
        value.roles.push(crate::ChannelRole::TerminalHot);
        assert!(value.validate().is_err());
        value.roles.pop();
        value.engine_id = " \n".into();
        assert!(value.validate().is_err());
        value.engine_id = "x".repeat(MAX_ENGINE_ID_BYTES + 1);
        assert!(value.validate().is_err());
    }
}
