//! Per-incarnation terminal streams. All request IDs are scoped to the bound
//! channel. Subscription IDs must not be reused within a server session.
//! Receivers must additionally validate session/incarnation/subscription and
//! owner generations against authoritative state; these are not wire checks.
use crate::{
    WireMessage,
    channel::TERMINAL_PROTOCOL,
    validation::{self, *},
};
use fl_model::{
    IncarnationId, InputId, InputOwner, LogCursor, ProcessInfo, Size, SubscriptionId, TerminalId,
    TerminalRecord, TerminalRecordKind, TransferId, ViewId,
};
use serde::{Deserialize, Serialize};

pub const KIND_TERMINAL_CLIENT: u16 = 10;
pub const KIND_TERMINAL_SERVER: u16 = 11;

/// Postcard discriminants 0..=5 in declaration order. Append only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalClientMsg {
    Subscribe {
        id: u64,
        subscription: SubscriptionId,
        terminal: TerminalId,
        incarnation: IncarnationId,
        from: Option<LogCursor>,
    },
    Unsubscribe {
        id: u64,
        subscription: SubscriptionId,
        terminal: TerminalId,
        incarnation: IncarnationId,
    },
    Input {
        id: u64,
        terminal: TerminalId,
        incarnation: IncarnationId,
        input: InputId,
        view: ViewId,
        bytes: Vec<u8>,
        /// Absolute daemon boot-clock deadline; expired bytes never enter a PTY.
        deadline_ns: u64,
    },
    ClaimOwner {
        id: u64,
        terminal: TerminalId,
        incarnation: IncarnationId,
        view: ViewId,
    },
    Resize {
        id: u64,
        terminal: TerminalId,
        incarnation: IncarnationId,
        owner: InputOwner,
        size: Size,
    },
    /// Parked subscriptions retain identity but stop hot record delivery.
    SetParked {
        id: u64,
        subscription: SubscriptionId,
        terminal: TerminalId,
        incarnation: IncarnationId,
        parked: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordBatch {
    pub subscription: SubscriptionId,
    pub terminal: TerminalId,
    pub incarnation: IncarnationId,
    pub records: Vec<TerminalRecord>,
    /// Exclusive end of this batch, not the daemon's possibly newer tail.
    pub next: LogCursor,
}

impl RecordBatch {
    /// Checks internal contiguity and overflow. The receiver must compare the
    /// first record's `at` (or empty batch's `next`) with its own applied cursor.
    pub fn validate(&self) -> Result<(), ValidationError> {
        validation::bound("record count", self.records.len(), MAX_RECORDS_PER_BATCH)?;
        let mut cursor = self.records.first().map_or(self.next, |record| record.at);
        for record in &self.records {
            if record.at != cursor {
                return Err(ValidationError::Invalid("record contiguity"));
            }
            match &record.kind {
                TerminalRecordKind::Output { bytes, .. } => {
                    validation::bound("output bytes", bytes.len(), MAX_OUTPUT_BYTES)?
                }
                TerminalRecordKind::Resize(size) => validation::size(*size)?,
                _ => {}
            }
            cursor = record
                .end()
                .ok_or(ValidationError::Invalid("record cursor overflow"))?;
        }
        if cursor != self.next {
            return Err(ValidationError::Invalid("batch next cursor"));
        }
        let encoded = postcard::experimental::serialized_size(self)
            .map_err(|_| ValidationError::Invalid("record serialization"))?;
        // Server enum's RecordBatch discriminant is one byte (index 2).
        validation::bound(
            "serialized record batch",
            encoded.saturating_add(1),
            MAX_RECORD_BATCH_BYTES,
        )
    }

    pub fn validate_from(&self, expected: LogCursor) -> Result<(), ValidationError> {
        self.validate()?;
        if self.records.first().map_or(self.next, |record| record.at) != expected {
            return Err(ValidationError::Invalid("record start cursor"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotOffer {
    pub transfer: TransferId,
    pub subscription: SubscriptionId,
    pub terminal: TerminalId,
    pub incarnation: IncarnationId,
    /// Resume log records at this exclusive checkpoint cursor.
    pub cursor: LogCursor,
    pub size: Size,
    pub terminal_protocol: u16,
    pub engine_id: String,
    pub total_bytes: u64,
    pub sha256: [u8; 32],
}

impl SnapshotOffer {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validation::size(self.size)?;
        validation::schema(
            "terminal protocol",
            self.terminal_protocol,
            TERMINAL_PROTOCOL,
        )?;
        validation::engine(&self.engine_id)?;
        if self.total_bytes == 0 {
            return Err(ValidationError::Invalid("empty checkpoint"));
        }
        validation::bound_u64("checkpoint bytes", self.total_bytes, MAX_CHECKPOINT_BYTES)
    }
}

/// Only committed PTY bytes may appear here, never transport receipt. Partial
/// writes cumulatively advance `accepted_bytes` for the same InputId; the
/// receiver retains/retries the suffix and must match its original byte length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputAck {
    pub id: u64,
    pub terminal: TerminalId,
    pub incarnation: IncarnationId,
    pub input: InputId,
    /// Exclusive committed prefix in this input, not a global byte count.
    pub accepted_bytes: u32,
    /// Canonical echo state and committed log cursor at acknowledgment time.
    pub echo: bool,
    pub cursor: LogCursor,
    /// Uncommitted suffix was discarded at its original deadline.
    pub expired: bool,
}

impl InputAck {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validation::bound(
            "input accepted prefix",
            self.accepted_bytes as usize,
            MAX_INPUT_BYTES,
        )
    }

    pub fn validate_for(
        &self,
        input: InputId,
        sent_bytes: usize,
        previous_prefix: u32,
    ) -> Result<(), ValidationError> {
        self.validate()?;
        validation::bound("sent input bytes", sent_bytes, MAX_INPUT_BYTES)?;
        if self.input != input
            || self.accepted_bytes as usize > sent_bytes
            || self.accepted_bytes < previous_prefix
        {
            return Err(ValidationError::Invalid("input acknowledgment prefix"));
        }
        Ok(())
    }
}

/// Typed terminal failures, discriminants 0..=8 in declaration order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalError {
    NotFound,
    WrongIncarnation {
        current: IncarnationId,
    },
    NotOwner {
        owner: Option<InputOwner>,
    },
    /// Requested history is no longer retained; obtain a fresh snapshot.
    ResyncRequired {
        earliest: LogCursor,
    },
    UnknownSubscription,
    InvalidArgument {
        message: String,
    },
    InputSequence {
        next_sequence: u64,
    },
    Internal {
        message: String,
    },
    Backpressured,
}

impl TerminalError {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::InvalidArgument { message } | Self::Internal { message } => {
                validation::text("terminal error", message, MAX_TEXT_BYTES)
            }
            _ => Ok(()),
        }
    }
}

/// Which local clipboard an OSC 52 store addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardTarget {
    Clipboard,
    /// The X11-style primary selection; Windows has none and ignores it.
    Selection,
}

/// Presentation effects the authoritative engine observed in live output.
/// They are sent once, to the subscribers attached at that moment; a client
/// that attaches later sees the resulting screen, never a replayed notice.
/// Discriminants 0..=1 in declaration order. Append only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalNotice {
    Bell,
    /// OSC 52 store. The client applies it only with the host's consent.
    ClipboardStore {
        target: ClipboardTarget,
        text: String,
    },
}

impl TerminalNotice {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Bell => Ok(()),
            Self::ClipboardStore { text, .. } => {
                validation::bound("clipboard bytes", text.len(), MAX_CLIPBOARD_BYTES)
            }
        }
    }
}

/// Discriminants 0..=10 in declaration order. Append only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalServerMsg {
    Subscribed {
        id: u64,
        subscription: SubscriptionId,
        terminal: TerminalId,
        incarnation: IncarnationId,
        cursor: LogCursor,
        owner: Option<InputOwner>,
        echo: bool,
    },
    Unsubscribed {
        id: u64,
        subscription: SubscriptionId,
        terminal: TerminalId,
        incarnation: IncarnationId,
    },
    RecordBatch(RecordBatch),
    SnapshotOffer(SnapshotOffer),
    InputAck(InputAck),
    OwnerChanged {
        id: Option<u64>,
        terminal: TerminalId,
        incarnation: IncarnationId,
        owner: Option<InputOwner>,
    },
    Resized {
        id: u64,
        terminal: TerminalId,
        incarnation: IncarnationId,
        size: Size,
        cursor: LogCursor,
    },
    Parked {
        id: u64,
        subscription: SubscriptionId,
        terminal: TerminalId,
        incarnation: IncarnationId,
        parked: bool,
    },
    ProcessChanged(ProcessInfo),
    Error {
        id: Option<u64>,
        terminal: TerminalId,
        incarnation: IncarnationId,
        error: TerminalError,
    },
    /// `sequence` is the engine's effect sequence: stable within the
    /// incarnation, so a client drops a notice it has already applied.
    Notice {
        terminal: TerminalId,
        incarnation: IncarnationId,
        sequence: u64,
        notice: TerminalNotice,
    },
}

impl WireMessage for TerminalClientMsg {
    const KIND: u16 = KIND_TERMINAL_CLIENT;
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Input { bytes, .. } => {
                validation::bound("input bytes", bytes.len(), MAX_INPUT_BYTES)
            }
            Self::Resize { size, .. } => validation::size(*size),
            _ => Ok(()),
        }
    }
}

impl WireMessage for TerminalServerMsg {
    const KIND: u16 = KIND_TERMINAL_SERVER;
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::RecordBatch(value) => value.validate(),
            Self::SnapshotOffer(value) => value.validate(),
            Self::InputAck(value) => value.validate(),
            Self::Resized { size, .. } => validation::size(*size),
            Self::ProcessChanged(value) => validation::process(value),
            Self::Error { error, .. } => error.validate(),
            Self::Notice { notice, .. } => notice.validate(),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Frame, MsgError};
    use fl_model::ClientSessionId;

    fn batch(records: Vec<TerminalRecord>) -> RecordBatch {
        let next = records
            .last()
            .map_or(LogCursor::default(), |record| record.end().unwrap());
        RecordBatch {
            subscription: SubscriptionId::from_bytes([1; 16]),
            terminal: TerminalId::from_bytes([2; 16]),
            incarnation: IncarnationId::from_bytes([3; 16]),
            records,
            next,
        }
    }

    fn output(at: LogCursor, len: usize) -> TerminalRecord {
        TerminalRecord {
            at,
            kind: TerminalRecordKind::Output {
                bytes: vec![0; len].into(),
                echo: true,
            },
        }
    }

    #[test]
    fn input_limit_is_enforced_on_send_and_receive() {
        let mut message = TerminalClientMsg::Input {
            id: 1,
            terminal: TerminalId::from_bytes([1; 16]),
            incarnation: IncarnationId::from_bytes([2; 16]),
            input: InputId {
                client: ClientSessionId::from_bytes([3; 16]),
                sequence: 1,
            },
            view: ViewId::from_bytes([4; 16]),
            bytes: vec![0; MAX_INPUT_BYTES],
            deadline_ns: 1,
        };
        assert!(message.to_frame().is_ok());
        let TerminalClientMsg::Input { bytes, .. } = &mut message else {
            unreachable!()
        };
        bytes.push(0);
        assert!(matches!(message.to_frame(), Err(MsgError::Validation(_))));
        let frame = Frame {
            kind: KIND_TERMINAL_CLIENT,
            payload: postcard::to_stdvec(&message).unwrap().into(),
        };
        assert!(matches!(
            TerminalClientMsg::from_frame(&frame),
            Err(MsgError::Validation(_))
        ));
    }

    #[test]
    fn output_limit_and_serialized_batch_limit_are_distinct() {
        let mut records = Vec::new();
        let mut cursor = LogCursor::default();
        for _ in 0..7 {
            let record = output(cursor, MAX_OUTPUT_BYTES);
            cursor = record.end().unwrap();
            records.push(record);
        }
        assert!(batch(records.clone()).validate().is_ok());
        records.push(output(cursor, MAX_OUTPUT_BYTES));
        let too_large = TerminalServerMsg::RecordBatch(batch(records));
        assert!(matches!(
            too_large.to_frame(),
            Err(MsgError::Validation(ValidationError::Limit {
                field: "serialized record batch",
                ..
            }))
        ));
        let frame = Frame {
            kind: KIND_TERMINAL_SERVER,
            payload: postcard::to_stdvec(&too_large).unwrap().into(),
        };
        assert!(matches!(
            TerminalServerMsg::from_frame(&frame),
            Err(MsgError::Validation(_))
        ));
        assert!(matches!(
            batch(vec![output(LogCursor::default(), MAX_OUTPUT_BYTES + 1)]).validate(),
            Err(ValidationError::Limit {
                field: "output bytes",
                ..
            })
        ));
    }

    #[test]
    fn control_records_count_toward_batch_limit_and_cursor() {
        let records = (0..=MAX_RECORDS_PER_BATCH)
            .map(|index| TerminalRecord {
                at: LogCursor {
                    next_record: index as u64,
                    next_byte: 0,
                },
                kind: TerminalRecordKind::SyncFlush,
            })
            .collect::<Vec<_>>();
        assert!(
            batch(records[..MAX_RECORDS_PER_BATCH].to_vec())
                .validate()
                .is_ok()
        );
        assert!(matches!(
            batch(records).validate(),
            Err(ValidationError::Limit {
                field: "record count",
                ..
            })
        ));
    }

    #[test]
    fn batches_reject_gaps_wrong_exclusive_end_and_cursor_overflow() {
        let first = output(LogCursor::default(), 3);
        let mut value = batch(vec![first]);
        assert!(value.validate_from(LogCursor::default()).is_ok());
        value.next.next_byte += 1;
        assert!(value.validate().is_err());
        value.next.next_byte -= 1;
        assert!(
            value
                .validate_from(LogCursor {
                    next_record: 1,
                    next_byte: 0
                })
                .is_err()
        );
        value.records.push(output(
            LogCursor {
                next_record: 2,
                next_byte: 3,
            },
            1,
        ));
        assert!(value.validate().is_err());
        for at in [
            LogCursor {
                next_record: u64::MAX,
                next_byte: 0,
            },
            LogCursor {
                next_record: 0,
                next_byte: u64::MAX,
            },
        ] {
            let record = output(at, 1);
            let value = RecordBatch {
                records: vec![record],
                ..batch(Vec::new())
            };
            assert!(value.validate().is_err());
        }
    }

    #[test]
    fn input_ack_matches_committed_prefix_not_transport_receipt() {
        let input = InputId {
            client: ClientSessionId::from_bytes([3; 16]),
            sequence: 7,
        };
        let mut ack = InputAck {
            id: 1,
            terminal: TerminalId::from_bytes([1; 16]),
            incarnation: IncarnationId::from_bytes([2; 16]),
            input,
            accepted_bytes: 3,
            echo: false,
            cursor: LogCursor::default(),
            expired: false,
        };
        assert!(ack.validate_for(input, 8, 0).is_ok());
        assert!(ack.validate_for(input, 8, 4).is_err());
        assert!(
            ack.validate_for(
                InputId {
                    sequence: 8,
                    ..input
                },
                8,
                0
            )
            .is_err()
        );
        assert!(ack.validate_for(input, 2, 0).is_err());
        ack.accepted_bytes = MAX_INPUT_BYTES as u32 + 1;
        assert!(ack.validate().is_err());
    }

    #[test]
    fn snapshots_reject_unknown_schema_geometry_and_length() {
        let mut offer = SnapshotOffer {
            transfer: TransferId::from_bytes([1; 16]),
            subscription: SubscriptionId::from_bytes([2; 16]),
            terminal: TerminalId::from_bytes([3; 16]),
            incarnation: IncarnationId::from_bytes([4; 16]),
            cursor: LogCursor::default(),
            size: Size::new(80, 24),
            terminal_protocol: TERMINAL_PROTOCOL,
            engine_id: "test-engine".into(),
            total_bytes: MAX_CHECKPOINT_BYTES,
            sha256: [0; 32],
        };
        assert!(offer.validate().is_ok());
        offer.terminal_protocol += 1;
        assert!(matches!(
            offer.validate(),
            Err(ValidationError::Schema { .. })
        ));
        offer.terminal_protocol = TERMINAL_PROTOCOL;
        offer.size.columns = 1;
        assert!(offer.validate().is_err());
        offer.size = Size::new(80, 24);
        offer.total_bytes += 1;
        assert!(offer.validate().is_err());
        offer.total_bytes = 0;
        assert!(offer.validate().is_err());
    }

    fn notice(text: String) -> TerminalServerMsg {
        TerminalServerMsg::Notice {
            terminal: TerminalId::from_bytes([1; 16]),
            incarnation: IncarnationId::from_bytes([2; 16]),
            sequence: 7,
            notice: TerminalNotice::ClipboardStore {
                target: ClipboardTarget::Clipboard,
                text,
            },
        }
    }

    /// A clipboard store is bounded on both sides, and the new message is
    /// appended after every existing server discriminant.
    #[test]
    fn clipboard_notices_are_bounded_and_appended() {
        let fits = notice("é".repeat(MAX_CLIPBOARD_BYTES / 2));
        let frame = fits.to_frame().unwrap();
        assert_eq!(frame.payload[0], 10, "Notice is discriminant 10");
        assert_eq!(TerminalServerMsg::from_frame(&frame).unwrap(), fits);

        let too_large = notice("x".repeat(MAX_CLIPBOARD_BYTES + 1));
        assert!(matches!(
            too_large.to_frame(),
            Err(MsgError::Validation(ValidationError::Limit {
                field: "clipboard bytes",
                ..
            }))
        ));
        let frame = Frame {
            kind: KIND_TERMINAL_SERVER,
            payload: postcard::to_stdvec(&too_large).unwrap().into(),
        };
        assert!(matches!(
            TerminalServerMsg::from_frame(&frame),
            Err(MsgError::Validation(_))
        ));
    }
}
