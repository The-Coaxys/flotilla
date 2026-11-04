//! Terminal and bulk channels shared by every mirror of one client.
//!
//! For each published control link the connection task negotiates terminal
//! capabilities, samples the daemon clock and opens one hot terminal channel
//! and one bulk channel. Together they form an *epoch*: every inbound message
//! is tagged with it and mirrors drop traffic from any other epoch. When the
//! link generation ends (or a channel fails) the epoch is closed and a new one
//! is established on the next link.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fl_model::{ClientSessionId, IncarnationId, TerminalId};
use fl_proto::{
    BulkClientMsg, BulkServerMsg, ChannelRole, Request, Response, SnapshotOffer, TerminalClientMsg,
    TerminalServerMsg,
};
use fl_term::{ENGINE_ID, EffectMode, Engine};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter};
use flc_conn::broker::{ControlLink, LogicalChannel};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::SyncEvent;
use crate::clock::DaemonClock;

/// Daemon clock samples are refreshed this often to bound drift.
const CLOCK_REFRESH: Duration = Duration::from_secs(30);
const OUTBOUND: usize = 256;
const INBOUND: usize = 64;
const SNAPSHOT_REQUESTS: usize = 16;
const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// A server message for one mirror, tagged with the epoch that carried it.
pub(crate) struct Inbound {
    pub epoch: u64,
    pub msg: TerminalServerMsg,
}

pub(crate) type SnapshotResult = Result<Engine, String>;

struct SnapshotRequest {
    offer: SnapshotOffer,
    reply: oneshot::Sender<SnapshotResult>,
}

pub(crate) type RouteKey = (TerminalId, IncarnationId);

/// Mirror mailboxes by terminal incarnation.
#[derive(Default)]
pub(crate) struct Routes(Mutex<HashMap<RouteKey, mpsc::Sender<Inbound>>>);

impl Routes {
    pub fn mailbox() -> (mpsc::Sender<Inbound>, mpsc::Receiver<Inbound>) {
        mpsc::channel(INBOUND)
    }

    pub fn insert(&self, key: RouteKey, mailbox: mpsc::Sender<Inbound>) {
        self.0.lock().expect("routes poisoned").insert(key, mailbox);
    }

    /// Remove `key` only if it still routes to `mailbox`.
    pub fn remove(&self, key: RouteKey, mailbox: &mpsc::Sender<Inbound>) {
        let mut routes = self.0.lock().expect("routes poisoned");
        if routes.get(&key).is_some_and(|m| m.same_channel(mailbox)) {
            routes.remove(&key);
        }
    }

    fn get(&self, key: RouteKey) -> Option<mpsc::Sender<Inbound>> {
        self.0.lock().expect("routes poisoned").get(&key).cloned()
    }
}

/// One epoch of terminal channels. Mirrors hold it while it is current.
pub(crate) struct ChannelSet {
    pub epoch: u64,
    pub generation: u64,
    pub client: ClientSessionId,
    out: mpsc::Sender<TerminalClientMsg>,
    ids: AtomicU64,
    snapshots: mpsc::Sender<SnapshotRequest>,
    clock: Mutex<DaemonClock>,
    closed: CancellationToken,
}

impl ChannelSet {
    /// Request IDs are scoped to the channel.
    pub fn request_id(&self) -> u64 {
        self.ids.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn clock(&self) -> DaemonClock {
        *self.clock.lock().expect("clock poisoned")
    }

    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    /// Queue a message on the hot channel; false once the epoch has ended.
    pub async fn send(&self, msg: TerminalClientMsg) -> bool {
        tokio::select! {
            biased;
            _ = self.closed.cancelled() => false,
            sent = self.out.send(msg) => sent.is_ok(),
        }
    }

    /// Fetch, verify and restore the offered checkpoint on the bulk channel.
    pub fn snapshot(&self, offer: SnapshotOffer) -> oneshot::Receiver<SnapshotResult> {
        let (reply, result) = oneshot::channel();
        if let Err(error) = self.snapshots.try_send(SnapshotRequest { offer, reply }) {
            let request = match error {
                mpsc::error::TrySendError::Full(request)
                | mpsc::error::TrySendError::Closed(request) => request,
            };
            let _ = request
                .reply
                .send(Err("the bulk channel is unavailable".into()));
        }
        result
    }
}

enum OpenError {
    /// Waiting for another link cannot help.
    Incompatible(String),
    Failed(String),
}

/// Maintain channel epochs for every published link until `shutdown`.
pub(crate) async fn run(
    routes: Arc<Routes>,
    mut links: watch::Receiver<Option<ControlLink>>,
    sets: watch::Sender<Option<Arc<ChannelSet>>>,
    events: broadcast::Sender<SyncEvent>,
    shutdown: CancellationToken,
) {
    let mut epoch = 0;
    loop {
        let link = loop {
            if let Some(link) = links.borrow_and_update().clone()
                && !link.is_closed()
            {
                break link;
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                changed = links.changed() => if changed.is_err() {
                    shutdown.cancelled().await;
                    return;
                },
            }
        };
        let mut failures = 0u32;
        while !link.is_closed() && !shutdown.is_cancelled() {
            epoch += 1;
            match open(&routes, &link, epoch).await {
                Ok((set, mut tasks)) => {
                    failures = 0;
                    sets.send_replace(Some(set.clone()));
                    let _ = events.send(SyncEvent::Connected {
                        generation: link.generation(),
                    });
                    tokio::select! {
                        _ = shutdown.cancelled() => {}
                        _ = link.closed() => {}
                        _ = tasks.join_next() => {
                            tracing::debug!("terminal channels ended; reopening");
                        }
                    }
                    set.closed.cancel();
                    sets.send_replace(None);
                    tasks.abort_all();
                    let _ = events.send(SyncEvent::Disconnected);
                }
                Err(OpenError::Incompatible(daemon)) => {
                    let _ = events.send(SyncEvent::IncompatibleEngine { daemon });
                    tokio::select! {
                        _ = shutdown.cancelled() => {}
                        _ = link.closed() => {}
                    }
                }
                Err(OpenError::Failed(error)) => {
                    tracing::debug!("opening terminal channels failed: {error}");
                    let _ = events.send(SyncEvent::ChannelsFailed { error });
                    failures += 1;
                    let delay = RETRY_MIN
                        .saturating_mul(1 << failures.min(8))
                        .min(RETRY_MAX);
                    tokio::select! {
                        _ = shutdown.cancelled() => {}
                        _ = link.closed() => {}
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
        if shutdown.is_cancelled() {
            return;
        }
    }
}

async fn open(
    routes: &Arc<Routes>,
    link: &ControlLink,
    epoch: u64,
) -> Result<(Arc<ChannelSet>, JoinSet<()>), OpenError> {
    let failed = |e: &dyn std::fmt::Display| OpenError::Failed(e.to_string());
    let capabilities = match link.rpc(Request::Capabilities).await {
        Ok(Response::Capabilities(capabilities)) => capabilities,
        Ok(_) => return Err(OpenError::Failed("unexpected capabilities reply".into())),
        Err(error) => return Err(failed(&error)),
    };
    let received = Instant::now();
    if capabilities.engine_id != ENGINE_ID {
        return Err(OpenError::Incompatible(capabilities.engine_id));
    }
    for role in [ChannelRole::TerminalHot, ChannelRole::Bulk] {
        if !capabilities.roles.contains(&role) {
            return Err(OpenError::Incompatible(format!(
                "{} without the {role:?} channel",
                capabilities.engine_id
            )));
        }
    }
    let (hot, bulk) = tokio::try_join!(
        link.open_channel(ChannelRole::TerminalHot),
        link.open_channel(ChannelRole::Bulk)
    )
    .map_err(|e| failed(&e))?;
    let (out, outbound) = mpsc::channel(OUTBOUND);
    let (snapshots, requests) = mpsc::channel(SNAPSHOT_REQUESTS);
    let set = Arc::new(ChannelSet {
        epoch,
        generation: link.generation(),
        client: link.client_session_id(),
        out,
        ids: AtomicU64::new(0),
        snapshots,
        clock: Mutex::new(DaemonClock::new(capabilities.clock_ns, received)),
        closed: CancellationToken::new(),
    });
    let mut tasks = JoinSet::new();
    tasks.spawn(read_loop(hot.reader, routes.clone(), epoch));
    tasks.spawn(write_loop(hot.writer, outbound));
    tasks.spawn(bulk_loop(bulk, requests));
    tasks.spawn(clock_loop(link.clone(), set.clone()));
    Ok((set, tasks))
}

fn route_key(msg: &TerminalServerMsg) -> RouteKey {
    match msg {
        TerminalServerMsg::Subscribed {
            terminal,
            incarnation,
            ..
        }
        | TerminalServerMsg::Unsubscribed {
            terminal,
            incarnation,
            ..
        }
        | TerminalServerMsg::OwnerChanged {
            terminal,
            incarnation,
            ..
        }
        | TerminalServerMsg::Resized {
            terminal,
            incarnation,
            ..
        }
        | TerminalServerMsg::Parked {
            terminal,
            incarnation,
            ..
        }
        | TerminalServerMsg::Error {
            terminal,
            incarnation,
            ..
        }
        | TerminalServerMsg::Notice {
            terminal,
            incarnation,
            ..
        } => (*terminal, *incarnation),
        TerminalServerMsg::RecordBatch(batch) => (batch.terminal, batch.incarnation),
        TerminalServerMsg::SnapshotOffer(offer) => (offer.terminal, offer.incarnation),
        TerminalServerMsg::InputAck(ack) => (ack.terminal, ack.incarnation),
        TerminalServerMsg::ProcessChanged(info) => (info.terminal, info.incarnation),
    }
}

async fn read_loop(mut reader: FrameReader, routes: Arc<Routes>, epoch: u64) {
    loop {
        let msg = match reader.recv::<TerminalServerMsg>().await {
            Ok(msg) => msg,
            Err(ConnError::Closed) => return,
            Err(error) => {
                tracing::debug!("terminal channel failed: {error}");
                return;
            }
        };
        // A slow mirror applies backpressure to the channel; the daemon then
        // replaces that subscriber's backlog with a snapshot.
        if let Some(mailbox) = routes.get(route_key(&msg)) {
            let _ = mailbox.send(Inbound { epoch, msg }).await;
        }
    }
}

async fn write_loop(mut writer: FrameWriter, mut outbound: mpsc::Receiver<TerminalClientMsg>) {
    while let Some(msg) = outbound.recv().await {
        match writer.send(&msg).await {
            Ok(()) => {}
            // Mirrors validate requests first; never let one poison the channel.
            Err(ConnError::Msg(error)) => {
                tracing::warn!("dropping invalid terminal request: {error}")
            }
            Err(error) => {
                tracing::debug!("terminal channel write failed: {error}");
                return;
            }
        }
    }
}

async fn clock_loop(link: ControlLink, set: Arc<ChannelSet>) {
    let mut interval =
        tokio::time::interval_at(tokio::time::Instant::now() + CLOCK_REFRESH, CLOCK_REFRESH);
    loop {
        interval.tick().await;
        if let Ok(Response::Capabilities(capabilities)) = link.rpc(Request::Capabilities).await {
            *set.clock.lock().expect("clock poisoned") =
                DaemonClock::new(capabilities.clock_ns, Instant::now());
        }
    }
}

enum FetchError {
    /// The daemon refused or the bytes did not verify; the channel is intact.
    Refused(String),
    /// The bulk stream is unusable.
    Channel(String),
}

async fn bulk_loop(mut channel: LogicalChannel, mut requests: mpsc::Receiver<SnapshotRequest>) {
    while let Some(request) = requests.recv().await {
        // A newer offer superseded this one before it was fetched.
        if request.reply.is_closed() {
            continue;
        }
        match fetch(&mut channel, &request.offer).await {
            Ok(bytes) => {
                let offer = request.offer;
                let restored = tokio::task::spawn_blocking(move || restore(&offer, &bytes))
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                let _ = request.reply.send(restored);
            }
            Err(FetchError::Refused(error)) => {
                let _ = request.reply.send(Err(error));
            }
            Err(FetchError::Channel(error)) => {
                let _ = request.reply.send(Err(error));
                return;
            }
        }
    }
}

/// Assemble exactly the offered bytes and verify length and SHA-256 before
/// any decoding. The offer itself was validated (bounded size) on receipt.
async fn fetch(channel: &mut LogicalChannel, offer: &SnapshotOffer) -> Result<Vec<u8>, FetchError> {
    let broken = |e: &dyn std::fmt::Display| FetchError::Channel(format!("bulk channel: {e}"));
    channel
        .writer
        .send(&BulkClientMsg::Request {
            transfer: offer.transfer,
            offset: 0,
        })
        .await
        .map_err(|e| broken(&e))?;
    let mut bytes = Vec::new();
    loop {
        match channel
            .reader
            .recv::<BulkServerMsg>()
            .await
            .map_err(|e| broken(&e))?
        {
            BulkServerMsg::Chunk(chunk) => {
                chunk
                    .validate_for(offer.transfer, bytes.len() as u64, offer.total_bytes)
                    .map_err(|e| broken(&e))?;
                bytes.extend_from_slice(&chunk.bytes);
            }
            BulkServerMsg::Complete { transfer, sha256 } => {
                if transfer != offer.transfer {
                    return Err(broken(&"completion for another transfer"));
                }
                let digest: [u8; 32] = Sha256::digest(&bytes).into();
                if bytes.len() as u64 != offer.total_bytes
                    || sha256 != offer.sha256
                    || digest != offer.sha256
                {
                    return Err(FetchError::Refused(
                        "snapshot length or SHA-256 does not match its offer".into(),
                    ));
                }
                return Ok(bytes);
            }
            BulkServerMsg::Failure { transfer, error } if transfer == offer.transfer => {
                return Err(FetchError::Refused(format!(
                    "snapshot transfer failed: {error:?}"
                )));
            }
            BulkServerMsg::Failure { .. } => {
                return Err(broken(&"failure for another transfer"));
            }
        }
    }
}

fn restore(offer: &SnapshotOffer, bytes: &[u8]) -> SnapshotResult {
    let engine = Engine::restore(bytes, EffectMode::Mirror).map_err(|e| e.to_string())?;
    if engine.offset() != offer.cursor.next_byte || engine.size() != offer.size {
        return Err("snapshot state does not match its offer".into());
    }
    Ok(engine)
}
