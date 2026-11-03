//! Session-scoped checkpoint transfers served on the bulk channel. Offers are
//! held only briefly and within explicit count/byte bounds.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fl_model::TransferId;
use fl_proto::{BulkChunk, BulkClientMsg, BulkError, BulkServerMsg};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter};

use crate::channels::ChannelLease;

const TRANSFER_LIFETIME: Duration = Duration::from_secs(60);
const MAX_SESSION_TRANSFERS: usize = 16;
const MAX_TRANSFER_BYTES: usize = 256 * 1024 * 1024;
const CHUNK_BYTES: usize = 256 * 1024;

pub(crate) type SessionId = [u8; 16];

struct Transfer {
    session: SessionId,
    bytes: Arc<Vec<u8>>,
    sha256: [u8; 32],
    expires: Instant,
    created: Instant,
}

#[derive(Default)]
pub(crate) struct Transfers {
    state: Mutex<HashMap<TransferId, Transfer>>,
}

impl Transfers {
    /// Register an offer. The oldest offers are evicted to stay within bounds;
    /// a client requesting an evicted transfer receives `NotFound` and resubscribes.
    pub(crate) fn offer(
        &self,
        session: SessionId,
        bytes: Arc<Vec<u8>>,
        sha256: [u8; 32],
    ) -> TransferId {
        let now = Instant::now();
        let mut state = self.state.lock().expect("transfer registry poisoned");
        state.retain(|_, transfer| transfer.expires > now);
        let shares = |state: &HashMap<TransferId, Transfer>| {
            let mut seen: Vec<*const Vec<u8>> = Vec::new();
            state
                .values()
                .filter(|transfer| {
                    let pointer = Arc::as_ptr(&transfer.bytes);
                    let new = !seen.contains(&pointer);
                    seen.push(pointer);
                    new
                })
                .map(|transfer| transfer.bytes.len())
                .sum::<usize>()
        };
        loop {
            let session_count = state.values().filter(|t| t.session == session).count();
            let over_bytes = shares(&state) + bytes.len() > MAX_TRANSFER_BYTES;
            if session_count < MAX_SESSION_TRANSFERS && !over_bytes {
                break;
            }
            let oldest = state
                .iter()
                .filter(|(_, t)| over_bytes || t.session == session)
                .min_by_key(|(_, t)| t.created)
                .map(|(id, _)| *id);
            match oldest {
                Some(id) => {
                    state.remove(&id);
                }
                None => break,
            }
        }
        let id = TransferId::new();
        state.insert(
            id,
            Transfer {
                session,
                bytes,
                sha256,
                expires: now + TRANSFER_LIFETIME,
                created: now,
            },
        );
        id
    }

    fn get(
        &self,
        session: SessionId,
        transfer: TransferId,
    ) -> Result<(Arc<Vec<u8>>, [u8; 32]), BulkError> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("transfer registry poisoned");
        state.retain(|_, transfer| transfer.expires > now);
        let found = state.get_mut(&transfer).ok_or(BulkError::NotFound)?;
        if found.session != session {
            return Err(BulkError::WrongSession);
        }
        found.expires = now + TRANSFER_LIFETIME;
        Ok((found.bytes.clone(), found.sha256))
    }

    fn cancel(&self, session: SessionId, transfer: TransferId) -> Result<(), BulkError> {
        let mut state = self.state.lock().expect("transfer registry poisoned");
        match state.get(&transfer) {
            None => Err(BulkError::NotFound),
            Some(found) if found.session != session => Err(BulkError::WrongSession),
            Some(_) => {
                state.remove(&transfer);
                Ok(())
            }
        }
    }
}

pub(crate) async fn serve(
    transfers: Arc<Transfers>,
    lease: ChannelLease,
    mut reader: FrameReader,
    mut writer: FrameWriter,
) -> Result<(), ConnError> {
    loop {
        let message = tokio::select! {
            _ = lease.cancel.cancelled() => break,
            message = reader.recv::<BulkClientMsg>() => message,
        };
        match message {
            Ok(BulkClientMsg::Request { transfer, offset }) => {
                let found = transfers.get(lease.session, transfer);
                let (bytes, sha256) = match found {
                    Ok(found) => found,
                    Err(error) => {
                        writer
                            .send(&BulkServerMsg::Failure { transfer, error })
                            .await?;
                        continue;
                    }
                };
                let Ok(start) = usize::try_from(offset) else {
                    writer
                        .send(&BulkServerMsg::Failure {
                            transfer,
                            error: BulkError::InvalidOffset,
                        })
                        .await?;
                    continue;
                };
                if start > bytes.len() {
                    writer
                        .send(&BulkServerMsg::Failure {
                            transfer,
                            error: BulkError::InvalidOffset,
                        })
                        .await?;
                    continue;
                }
                for (index, chunk) in bytes[start..].chunks(CHUNK_BYTES).enumerate() {
                    let message = BulkServerMsg::Chunk(BulkChunk {
                        transfer,
                        offset: (start + index * CHUNK_BYTES) as u64,
                        bytes: chunk.to_vec(),
                    });
                    tokio::select! {
                        _ = lease.cancel.cancelled() => return Ok(()),
                        sent = writer.send(&message) => sent?,
                    }
                }
                writer
                    .send(&BulkServerMsg::Complete { transfer, sha256 })
                    .await?;
            }
            Ok(BulkClientMsg::Cancel { transfer }) => {
                let error = transfers
                    .cancel(lease.session, transfer)
                    .err()
                    .unwrap_or(BulkError::Cancelled);
                writer
                    .send(&BulkServerMsg::Failure { transfer, error })
                    .await?;
            }
            Err(ConnError::Closed) => break,
            Err(error) => return Err(error),
        }
    }
    let _ = writer.shutdown().await;
    Ok(())
}
