use super::*;
use fl_model::{ProcessState, TerminalRecordKind};

fn batch(records: Vec<TerminalRecord>, next: LogCursor) -> Response {
    Response::Records {
        records,
        next,
        tail: next,
        state: ProcessState::Running,
    }
}

#[test]
fn discontinuous_byte_offsets_are_rejected_before_batch_application() {
    let response = batch(
        vec![
            TerminalRecord {
                at: LogCursor::default(),
                kind: TerminalRecordKind::Output {
                    bytes: b"ab".as_slice().into(),
                    echo: true,
                },
            },
            TerminalRecord {
                at: LogCursor {
                    next_record: 1,
                    next_byte: 1,
                },
                kind: TerminalRecordKind::Resize(Size::new(80, 24)),
            },
        ],
        LogCursor {
            next_record: 2,
            next_byte: 1,
        },
    );
    assert_eq!(
        response.validate().unwrap_err().code,
        ErrorCode::CursorMismatch
    );
}

#[test]
fn same_offset_controls_must_still_advance_the_record_watermark() {
    let response = batch(
        vec![TerminalRecord {
            at: LogCursor {
                next_record: 7,
                next_byte: 23,
            },
            kind: TerminalRecordKind::SyncFlush,
        }],
        LogCursor {
            next_record: 7,
            next_byte: 23,
        },
    );
    assert_eq!(
        response.validate().unwrap_err().code,
        ErrorCode::CursorMismatch
    );
}

#[test]
fn hostile_record_sizes_and_overflow_cannot_reach_a_terminal_consumer() {
    let response = batch(
        vec![TerminalRecord {
            at: LogCursor::default(),
            kind: TerminalRecordKind::Output {
                bytes: vec![b'x'; MAX_OUTPUT_BYTES + 1].into(),
                echo: true,
            },
        }],
        LogCursor {
            next_record: 1,
            next_byte: MAX_OUTPUT_BYTES as u64 + 1,
        },
    );
    assert_eq!(
        response.validate().unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    let response = batch(
        vec![TerminalRecord {
            at: LogCursor {
                next_record: u64::MAX,
                next_byte: 0,
            },
            kind: TerminalRecordKind::Echo(false),
        }],
        LogCursor {
            next_record: u64::MAX,
            next_byte: 0,
        },
    );
    assert_eq!(
        response.validate().unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn undersized_read_budget_cannot_leave_a_reader_stuck_on_one_record() {
    let request = Request::Read {
        terminal: TerminalId::new(),
        incarnation: IncarnationId::new(),
        from: LogCursor::default(),
        max_bytes: MAX_OUTPUT_BYTES as u32 - 1,
        wait_ms: 1000,
    };
    assert_eq!(
        request.validate().unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}
