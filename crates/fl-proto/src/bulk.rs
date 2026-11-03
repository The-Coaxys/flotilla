//! Checkpoint bytes travel only in bounded bulk chunks. The receiver verifies
//! the bound server session, offer/subscription/incarnation, contiguous offsets,
//! exact total length and SHA-256 before decoding a checkpoint. No allocation
//! should be based on a peer length until SnapshotOffer::validate succeeds.
use crate::{
    WireMessage,
    validation::{self, *},
};
use fl_model::TransferId;
use serde::{Deserialize, Serialize};

pub const KIND_BULK_CLIENT: u16 = 12;
pub const KIND_BULK_SERVER: u16 = 13;

/// Discriminants 0, 1. Each request resumes the same offered transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BulkClientMsg {
    Request { transfer: TransferId, offset: u64 },
    Cancel { transfer: TransferId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkChunk {
    pub transfer: TransferId,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl BulkChunk {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.bytes.is_empty() {
            return Err(ValidationError::Invalid("empty bulk chunk"));
        }
        validation::bound("bulk chunk bytes", self.bytes.len(), MAX_BULK_CHUNK_BYTES)?;
        let end = self
            .offset
            .checked_add(self.bytes.len() as u64)
            .ok_or(ValidationError::Invalid("bulk offset overflow"))?;
        validation::bound_u64("bulk end offset", end, MAX_CHECKPOINT_BYTES)
    }

    /// Validates against the receiver's already-validated offer and progress.
    /// Hash verification remains mandatory at Complete, before engine restore.
    pub fn validate_for(
        &self,
        transfer: TransferId,
        expected_offset: u64,
        total_bytes: u64,
    ) -> Result<(), ValidationError> {
        self.validate()?;
        validation::bound_u64("checkpoint bytes", total_bytes, MAX_CHECKPOINT_BYTES)?;
        if self.transfer != transfer
            || self.offset != expected_offset
            || self.offset + self.bytes.len() as u64 > total_bytes
        {
            return Err(ValidationError::Invalid("bulk transfer progress"));
        }
        Ok(())
    }
}

/// Discriminants 0..=5. Authorization/session expiry is distinct from missing data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BulkError {
    NotFound,
    ExpiredCapability,
    WrongSession,
    InvalidOffset,
    Cancelled,
    Internal { message: String },
}

/// Discriminants 0, 1, 2. Complete repeats the offer digest; it is not proof
/// of integrity until the receiver hashes the exact assembled bytes itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BulkServerMsg {
    Chunk(BulkChunk),
    Complete {
        transfer: TransferId,
        sha256: [u8; 32],
    },
    Failure {
        transfer: TransferId,
        error: BulkError,
    },
}

impl WireMessage for BulkClientMsg {
    const KIND: u16 = KIND_BULK_CLIENT;
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Request { offset, .. } => {
                validation::bound_u64("bulk request offset", *offset, MAX_CHECKPOINT_BYTES)
            }
            Self::Cancel { .. } => Ok(()),
        }
    }
}

impl WireMessage for BulkServerMsg {
    const KIND: u16 = KIND_BULK_SERVER;
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Chunk(value) => value.validate(),
            Self::Failure {
                error: BulkError::Internal { message },
                ..
            } => validation::text("bulk error", message, MAX_TEXT_BYTES),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Frame, MsgError};

    fn chunk(length: usize) -> BulkChunk {
        BulkChunk {
            transfer: TransferId::from_bytes([1; 16]),
            offset: 0,
            bytes: vec![0; length],
        }
    }

    #[test]
    fn chunk_limit_is_enforced_on_send_and_receive() {
        assert!(
            BulkServerMsg::Chunk(chunk(MAX_BULK_CHUNK_BYTES))
                .to_frame()
                .is_ok()
        );
        let message = BulkServerMsg::Chunk(chunk(MAX_BULK_CHUNK_BYTES + 1));
        assert!(matches!(message.to_frame(), Err(MsgError::Validation(_))));
        let frame = Frame {
            kind: KIND_BULK_SERVER,
            payload: postcard::to_stdvec(&message).unwrap().into(),
        };
        assert!(matches!(
            BulkServerMsg::from_frame(&frame),
            Err(MsgError::Validation(_))
        ));
        assert!(chunk(0).validate().is_err());
    }

    #[test]
    fn progress_rejects_overlap_gaps_wrong_transfer_and_truncation() {
        let mut value = chunk(4);
        let transfer = value.transfer;
        assert!(value.validate_for(transfer, 0, 4).is_ok());
        assert!(value.validate_for(transfer, 1, 4).is_err());
        assert!(
            value
                .validate_for(TransferId::from_bytes([2; 16]), 0, 4)
                .is_err()
        );
        assert!(value.validate_for(transfer, 0, 3).is_err());
        value.offset = 4;
        assert!(value.validate_for(transfer, 0, 8).is_err());
        assert!(value.validate_for(transfer, 4, 8).is_ok());
    }

    #[test]
    fn offsets_cannot_wrap_or_exceed_checkpoint_limit() {
        let mut value = chunk(1);
        value.offset = MAX_CHECKPOINT_BYTES - 1;
        assert!(value.validate().is_ok());
        value.offset += 1;
        assert!(value.validate().is_err());
        value.offset = u64::MAX;
        assert!(value.validate().is_err());
        assert!(
            BulkClientMsg::Request {
                transfer: value.transfer,
                offset: MAX_CHECKPOINT_BYTES
            }
            .validate()
            .is_ok()
        );
        assert!(
            BulkClientMsg::Request {
                transfer: value.transfer,
                offset: MAX_CHECKPOINT_BYTES + 1
            }
            .validate()
            .is_err()
        );
    }
}
