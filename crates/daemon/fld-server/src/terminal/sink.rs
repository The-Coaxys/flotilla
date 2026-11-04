//! Byte-accounted outbound queue of one terminal channel. Workers never block
//! on a subscriber: they observe its queued bytes and skip or replace output.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fl_model::ClientSessionId;
use fl_proto::TerminalServerMsg;
use fl_transport::conn::{ConnError, FrameWriter};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::bulk::SessionId;

/// Messages, not bytes: the byte bound is enforced by `queued`.
const CAPACITY: usize = 256;
/// Nominal cost of a small control message.
pub(crate) const SMALL: usize = 64;

pub(crate) struct Outbound {
    message: TerminalServerMsg,
    cost: usize,
}

#[derive(Clone)]
pub(crate) struct Sink {
    pub channel: u64,
    pub session: SessionId,
    pub client: ClientSessionId,
    tx: mpsc::Sender<Outbound>,
    queued: Arc<AtomicUsize>,
}

pub(crate) enum TrySend {
    Sent,
    Full,
    Closed,
}

impl Sink {
    pub(crate) fn new(
        channel: u64,
        session: SessionId,
        client: ClientSessionId,
        writer: FrameWriter,
        cancel: CancellationToken,
    ) -> (Self, tokio::task::JoinHandle<Result<(), ConnError>>) {
        let (tx, rx) = mpsc::channel(CAPACITY);
        let queued = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(write_loop(writer, rx, queued.clone(), cancel));
        (
            Self {
                channel,
                session,
                client,
                tx,
                queued,
            },
            task,
        )
    }

    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    pub(crate) fn try_send(&self, message: TerminalServerMsg, cost: usize) -> TrySend {
        self.queued.fetch_add(cost, Ordering::AcqRel);
        match self.tx.try_send(Outbound { message, cost }) {
            Ok(()) => TrySend::Sent,
            Err(error) => {
                self.queued.fetch_sub(cost, Ordering::AcqRel);
                match error {
                    mpsc::error::TrySendError::Full(_) => TrySend::Full,
                    mpsc::error::TrySendError::Closed(_) => TrySend::Closed,
                }
            }
        }
    }

    /// Request replies wait for capacity, which stops reading the requester.
    pub(crate) async fn send(&self, message: TerminalServerMsg) -> bool {
        self.queued.fetch_add(SMALL, Ordering::AcqRel);
        let sent = self
            .tx
            .send(Outbound {
                message,
                cost: SMALL,
            })
            .await
            .is_ok();
        if !sent {
            self.queued.fetch_sub(SMALL, Ordering::AcqRel);
        }
        sent
    }
}

async fn write_loop(
    mut writer: FrameWriter,
    mut rx: mpsc::Receiver<Outbound>,
    queued: Arc<AtomicUsize>,
    cancel: CancellationToken,
) -> Result<(), ConnError> {
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => break,
            next = rx.recv() => next,
        };
        let Some(outbound) = next else { break };
        let sent = tokio::select! {
            _ = cancel.cancelled() => break,
            sent = writer.send(&outbound.message) => sent,
        };
        queued.fetch_sub(outbound.cost, Ordering::AcqRel);
        if let Err(error) = sent {
            cancel.cancel();
            return Err(error);
        }
    }
    rx.close();
    let _ = writer.shutdown().await;
    Ok(())
}
