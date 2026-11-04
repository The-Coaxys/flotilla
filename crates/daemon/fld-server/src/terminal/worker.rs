//! One authoritative worker per terminal incarnation. It alone parses PTY
//! output, originates deterministic replies, checkpoints, and orders the exact
//! journal records forwarded to client mirror engines.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fl_model::{
    ClientSessionId, InputOwner, LogCursor, ProcessInfo, ProcessState, Size, SubscriptionId,
    TerminalRecord, TerminalRecordKind, ViewId,
};
use fl_prochost_proto::{ErrorCode, MAX_READ_BYTES, Request, Response, WriteId};
use fl_proto::validation::MAX_CLIPBOARD_BYTES;
use fl_proto::{
    ClipboardTarget, RecordBatch, SnapshotOffer, TERMINAL_PROTOCOL, TerminalError, TerminalNotice,
    TerminalServerMsg,
};
use fl_term::{ENGINE_ID, EffectKind, Engine};
use fld_store::TerminalKey;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::Shared;
use super::sink::{SMALL, Sink, TrySend};

/// A subscriber further behind than this receives a newer snapshot instead.
pub(crate) const LAG_LIMIT: usize = 512 * 1024;
const RING_BYTES: usize = 2 * 1024 * 1024;
const RING_RECORDS: usize = 4096;
const BATCH_PAYLOAD: usize = 384 * 1024;
const BATCH_RECORDS: usize = fl_proto::validation::MAX_RECORDS_PER_BATCH;
const RECORD_OVERHEAD: usize = 48;
const CHECKPOINT_BYTES: u64 = 1024 * 1024;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);
const MIN_CHECKPOINT_SPACING: Duration = Duration::from_secs(1);
const STALL_RETRY: Duration = Duration::from_millis(25);
/// Pause parsing (and therefore PTY reads) while replies wait for the PTY.
const MAX_PENDING_EFFECT_BYTES: usize = 1024 * 1024;
const COMMAND_CAPACITY: usize = 256;
/// Superseded checkpoint blobs are reclaimed after every this many checkpoints.
const GC_EVERY: u64 = 16;

pub(crate) enum Command {
    Subscribe {
        id: u64,
        subscription: SubscriptionId,
        from: Option<LogCursor>,
        sink: Sink,
    },
    Unsubscribe {
        id: u64,
        subscription: SubscriptionId,
        sink: Sink,
    },
    SetParked {
        id: u64,
        subscription: SubscriptionId,
        parked: bool,
        sink: Sink,
    },
    /// Remove every subscription of a closed channel.
    Detach { channel: u64 },
    /// Deliver once per channel, optionally skipping the requester's channel.
    Broadcast {
        except: Option<u64>,
        message: TerminalServerMsg,
    },
    Stop {
        checkpoint: bool,
        done: oneshot::Sender<()>,
    },
}

#[derive(Default)]
struct Ownership {
    owner: Option<InputOwner>,
    generation: u64,
}

/// Shared, cheaply readable view of one live terminal incarnation.
pub(crate) struct Terminal {
    pub key: TerminalKey,
    commands: mpsc::Sender<Command>,
    info: Mutex<ProcessInfo>,
    ownership: Mutex<Ownership>,
    abort: CancellationToken,
}

impl Terminal {
    pub(crate) fn info(&self) -> ProcessInfo {
        self.info.lock().expect("terminal info poisoned").clone()
    }

    pub(crate) fn owner(&self) -> Option<InputOwner> {
        self.ownership.lock().expect("ownership poisoned").owner
    }

    /// Returns the current lease and whether this call changed it. Lease
    /// generations are seeded from the boot clock, so a lease issued before a
    /// daemon restart is never reissued to a different holder.
    pub(crate) fn claim(&self, client: ClientSessionId, view: ViewId) -> (InputOwner, bool) {
        let mut state = self.ownership.lock().expect("ownership poisoned");
        if let Some(owner) = state.owner
            && owner.client == client
            && owner.view == view
        {
            return (owner, false);
        }
        let floor = fld_prochost::clock_ns().unwrap_or(0);
        state.generation = state.generation.saturating_add(1).max(floor);
        let owner = InputOwner {
            client,
            view,
            generation: state.generation,
        };
        state.owner = Some(owner);
        (owner, true)
    }

    pub(crate) fn check_owner(&self, owner: InputOwner) -> Result<(), Option<InputOwner>> {
        let current = self.owner();
        if current == Some(owner) {
            Ok(())
        } else {
            Err(current)
        }
    }

    /// Commands never wait: a saturated worker reports backpressure instead.
    pub(crate) fn command(&self, command: Command) -> Result<(), TerminalError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => TerminalError::Backpressured,
                mpsc::error::TrySendError::Closed(_) => TerminalError::NotFound,
            })
    }

    pub(crate) fn broadcast_owner(&self, except: Option<u64>, owner: Option<InputOwner>) {
        let _ = self.command(Command::Broadcast {
            except,
            message: TerminalServerMsg::OwnerChanged {
                id: None,
                terminal: self.key.terminal,
                incarnation: self.key.incarnation,
                owner,
            },
        });
    }

    /// Stop the worker, optionally after a final durable checkpoint.
    pub(crate) async fn stop(&self, checkpoint: bool) {
        let (done, stopped) = oneshot::channel();
        if self
            .commands
            .send(Command::Stop { checkpoint, done })
            .await
            .is_ok()
        {
            let _ = tokio::time::timeout(Duration::from_secs(10), stopped).await;
        }
        self.abort.cancel();
    }

    pub(crate) fn abort(&self) {
        self.abort.cancel();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.abort.cancel();
    }
}

struct Ring {
    records: VecDeque<TerminalRecord>,
    start: LogCursor,
    bytes: usize,
}

impl Ring {
    fn push(&mut self, record: TerminalRecord) {
        self.bytes += payload(&record);
        self.records.push_back(record);
        while self.bytes > RING_BYTES || self.records.len() > RING_RECORDS {
            let Some(old) = self.records.pop_front() else {
                break;
            };
            self.bytes -= payload(&old);
            self.start = old.end().expect("validated record cursor");
        }
    }

    fn index(&self, cursor: LogCursor) -> Option<usize> {
        let index = cursor.next_record.checked_sub(self.start.next_record)?;
        let index = usize::try_from(index).ok()?;
        match self.records.get(index) {
            Some(record) => (record.at == cursor).then_some(index),
            None => (index == self.records.len()
                && self
                    .records
                    .back()
                    .map_or(self.start, |r| r.end().expect("validated record cursor"))
                    == cursor)
                .then_some(index),
        }
    }

    fn batch(&self, index: usize) -> (Vec<TerminalRecord>, usize, LogCursor) {
        let mut records = Vec::new();
        let mut cost = 0;
        for record in self.records.range(index..) {
            let size = payload(record) + RECORD_OVERHEAD;
            if records.len() == BATCH_RECORDS
                || (!records.is_empty() && cost + size > BATCH_PAYLOAD)
            {
                break;
            }
            cost += size;
            records.push(record.clone());
        }
        let next = records
            .last()
            .map(|r| r.end().expect("validated record cursor"))
            .unwrap_or(self.start);
        (records, cost, next)
    }
}

fn payload(record: &TerminalRecord) -> usize {
    match &record.kind {
        TerminalRecordKind::Output { bytes, .. } => bytes.len(),
        _ => 0,
    }
}

struct Subscriber {
    id: SubscriptionId,
    sink: Sink,
    next: LogCursor,
    parked: bool,
    needs_snapshot: bool,
}

enum Pump {
    Done,
    Stalled,
    Closed,
}

enum ReadEvent {
    Records {
        records: Vec<TerminalRecord>,
        tail: LogCursor,
        state: ProcessState,
    },
    Failed(String),
}

struct Snapshot {
    cursor: LogCursor,
    bytes: Arc<Vec<u8>>,
    sha256: [u8; 32],
}

struct Worker {
    shared: Arc<Shared>,
    terminal: Arc<Terminal>,
    key: TerminalKey,
    engine: Option<Engine>,
    cursor: LogCursor,
    checkpointed: LogCursor,
    last_checkpoint: Instant,
    ring: Ring,
    subscribers: Vec<Subscriber>,
    retry_at: Option<Instant>,
    reading: bool,
    failed: Option<String>,
    snapshot: Option<Snapshot>,
    effects: mpsc::UnboundedSender<(u64, Vec<u8>)>,
    effect_bytes: Arc<AtomicUsize>,
}

/// Start a worker whose engine state is exactly `cursor` (restored checkpoint,
/// or a new engine at the journal start).
pub(crate) fn spawn(
    shared: Arc<Shared>,
    info: ProcessInfo,
    engine: Engine,
    cursor: LogCursor,
) -> Arc<Terminal> {
    let key = TerminalKey {
        terminal: info.terminal,
        incarnation: info.incarnation,
    };
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    let abort = shared.abort.child_token();
    let terminal = Arc::new(Terminal {
        key,
        commands,
        info: Mutex::new(info),
        ownership: Mutex::default(),
        abort: abort.clone(),
    });
    let (records_tx, records) = mpsc::channel(2);
    tokio::spawn(read_loop(
        shared.clone(),
        key,
        cursor,
        records_tx,
        abort.clone(),
    ));
    let (effects, effect_queue) = mpsc::unbounded_channel();
    let effect_bytes = Arc::new(AtomicUsize::new(0));
    tokio::spawn(effect_loop(
        shared.clone(),
        key,
        effect_queue,
        effect_bytes.clone(),
        abort.clone(),
    ));
    let worker = Worker {
        shared,
        terminal: terminal.clone(),
        key,
        engine: Some(engine),
        cursor,
        checkpointed: cursor,
        last_checkpoint: Instant::now(),
        ring: Ring {
            records: VecDeque::new(),
            start: cursor,
            bytes: 0,
        },
        subscribers: Vec::new(),
        retry_at: None,
        reading: true,
        failed: None,
        snapshot: None,
        effects,
        effect_bytes,
    };
    tokio::spawn(worker.run(receiver, records, abort));
    terminal
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

impl Worker {
    fn engine(&self) -> &Engine {
        self.engine
            .as_ref()
            .expect("engine is present outside checkpoints")
    }

    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut records: mpsc::Receiver<ReadEvent>,
        abort: CancellationToken,
    ) {
        let _abort_children = abort.clone().drop_guard();
        loop {
            let effects_saturated =
                self.effect_bytes.load(Ordering::Acquire) >= MAX_PENDING_EFFECT_BYTES;
            let effects_wait = (self.reading && effects_saturated)
                .then(|| Instant::now() + Duration::from_millis(10));
            tokio::select! {
                biased;
                _ = abort.cancelled() => return,
                command = commands.recv() => match command {
                    None => return,
                    Some(Command::Stop { checkpoint, done }) => {
                        if checkpoint {
                            self.checkpoint().await;
                        }
                        let _ = done.send(());
                        return;
                    }
                    Some(command) => self.command(command),
                },
                event = records.recv(), if self.reading && !effects_saturated => match event {
                    Some(event) => self.apply(event),
                    None => self.reading = false,
                },
                _ = sleep_until(self.retry_at) => self.pump_all(),
                _ = sleep_until(effects_wait) => {},
                _ = sleep_until(self.checkpoint_deadline()) => self.checkpoint().await,
            }
            let behind = self.cursor.next_byte - self.checkpointed.next_byte;
            if behind >= CHECKPOINT_BYTES
                && self.last_checkpoint.elapsed() >= MIN_CHECKPOINT_SPACING
            {
                self.checkpoint().await;
            }
        }
    }

    fn checkpoint_deadline(&self) -> Option<Instant> {
        (self.cursor != self.checkpointed && self.failed.is_none())
            .then(|| self.last_checkpoint + CHECKPOINT_INTERVAL)
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Subscribe {
                id,
                subscription,
                from,
                sink,
            } => self.subscribe(id, subscription, from, sink),
            Command::Unsubscribe {
                id,
                subscription,
                sink,
            } => {
                let before = self.subscribers.len();
                self.subscribers
                    .retain(|s| !(s.sink.channel == sink.channel && s.id == subscription));
                let message = if self.subscribers.len() < before {
                    TerminalServerMsg::Unsubscribed {
                        id,
                        subscription,
                        terminal: self.key.terminal,
                        incarnation: self.key.incarnation,
                    }
                } else {
                    self.error(Some(id), TerminalError::UnknownSubscription)
                };
                sink.try_send(message, SMALL);
            }
            Command::SetParked {
                id,
                subscription,
                parked,
                sink,
            } => {
                let Some(index) = self
                    .subscribers
                    .iter()
                    .position(|s| s.sink.channel == sink.channel && s.id == subscription)
                else {
                    sink.try_send(
                        self.error(Some(id), TerminalError::UnknownSubscription),
                        SMALL,
                    );
                    return;
                };
                self.subscribers[index].parked = parked;
                sink.try_send(
                    TerminalServerMsg::Parked {
                        id,
                        subscription,
                        terminal: self.key.terminal,
                        incarnation: self.key.incarnation,
                        parked,
                    },
                    SMALL,
                );
                if !parked {
                    self.pump_all();
                }
            }
            Command::Detach { channel } => self.subscribers.retain(|s| s.sink.channel != channel),
            Command::Broadcast { except, message } => {
                let mut seen = Vec::new();
                for subscriber in &self.subscribers {
                    let channel = subscriber.sink.channel;
                    if Some(channel) != except && !seen.contains(&channel) {
                        seen.push(channel);
                        subscriber.sink.try_send(message.clone(), SMALL);
                    }
                }
            }
            Command::Stop { .. } => unreachable!("handled by the worker loop"),
        }
    }

    fn error(&self, id: Option<u64>, error: TerminalError) -> TerminalServerMsg {
        TerminalServerMsg::Error {
            id,
            terminal: self.key.terminal,
            incarnation: self.key.incarnation,
            error,
        }
    }

    fn subscribe(
        &mut self,
        id: u64,
        subscription: SubscriptionId,
        from: Option<LogCursor>,
        sink: Sink,
    ) {
        if let Some(message) = &self.failed {
            let error = TerminalError::Internal {
                message: message.clone(),
            };
            sink.try_send(self.error(Some(id), error), SMALL);
            return;
        }
        if self
            .subscribers
            .iter()
            .any(|s| s.sink.channel == sink.channel && s.id == subscription)
        {
            let error = TerminalError::InvalidArgument {
                message: "subscription is already active".into(),
            };
            sink.try_send(self.error(Some(id), error), SMALL);
            return;
        }
        let resume = from.filter(|cursor| {
            cursor.next_byte <= self.cursor.next_byte
                && self.cursor.next_byte - cursor.next_byte <= LAG_LIMIT as u64
                && self.ring.index(*cursor).is_some()
        });
        let subscribed = TerminalServerMsg::Subscribed {
            id,
            subscription,
            terminal: self.key.terminal,
            incarnation: self.key.incarnation,
            cursor: resume.unwrap_or(self.cursor),
            owner: self.terminal.owner(),
            echo: self.terminal.info().echo,
        };
        if !matches!(sink.try_send(subscribed, SMALL), TrySend::Sent) {
            return;
        }
        sink.try_send(
            TerminalServerMsg::ProcessChanged(self.terminal.info()),
            SMALL,
        );
        self.subscribers.push(Subscriber {
            id: subscription,
            sink,
            next: resume.unwrap_or(self.cursor),
            parked: false,
            needs_snapshot: resume.is_none(),
        });
        self.pump_all();
    }

    fn apply(&mut self, event: ReadEvent) {
        let (records, tail, state) = match event {
            ReadEvent::Failed(message) => return self.fail(message),
            ReadEvent::Records {
                records,
                tail,
                state,
            } => (records, tail, state),
        };
        if self.failed.is_some() {
            return;
        }
        let start = self.cursor;
        let mut echo = None;
        let mut size = None;
        for record in records {
            if record.at != self.cursor {
                return self.fail(format!(
                    "journal gap: expected {:?}, received {:?}",
                    self.cursor, record.at
                ));
            }
            let Some(end) = record.end() else {
                return self.fail("journal cursor overflow".into());
            };
            if let Err(error) = self.feed(&record, &mut echo, &mut size) {
                return self.fail(format!("terminal engine rejected journal record: {error}"));
            }
            self.cursor = end;
            self.ring.push(record);
        }
        let changed = {
            let mut info = self.terminal.info.lock().expect("terminal info poisoned");
            let before = (info.state.clone(), info.size, info.echo);
            info.tail = tail;
            info.state = state;
            if let Some(echo) = echo {
                info.echo = echo;
            }
            if let Some(size) = size {
                info.size = size;
            }
            (before != (info.state.clone(), info.size, info.echo)).then(|| info.clone())
        };
        if let Some(info) = changed {
            let persisted = info.clone();
            self.shared.store.submit(move |store| {
                if let Err(error) = store.database.update_process(&persisted) {
                    tracing::warn!(terminal = %persisted.terminal, "persisting process state: {error}");
                }
            });
            for subscriber in &self.subscribers {
                subscriber
                    .sink
                    .try_send(TerminalServerMsg::ProcessChanged(info.clone()), SMALL);
            }
        }
        if self.cursor != start {
            self.pump_all();
        }
    }

    fn feed(
        &mut self,
        record: &TerminalRecord,
        echo: &mut Option<bool>,
        size: &mut Option<Size>,
    ) -> Result<(), fl_term::EngineError> {
        let engine = self
            .engine
            .as_mut()
            .expect("engine is present outside checkpoints");
        let effects = match &record.kind {
            TerminalRecordKind::Output { bytes, echo: e } => {
                *echo = Some(*e);
                engine.feed(record.at.next_byte, bytes)?
            }
            TerminalRecordKind::Resize(new) => {
                *size = Some(*new);
                engine.resize(*new)?
            }
            TerminalRecordKind::SyncFlush => engine.flush_sync()?,
            TerminalRecordKind::Echo(e) => {
                *echo = Some(*e);
                Vec::new()
            }
            TerminalRecordKind::Exit(_) => Vec::new(),
        };
        for effect in effects {
            let notice = match effect.kind {
                EffectKind::Reply(bytes) => {
                    self.effect_bytes.fetch_add(bytes.len(), Ordering::AcqRel);
                    let _ = self.effects.send((effect.sequence, bytes));
                    continue;
                }
                EffectKind::Bell => TerminalNotice::Bell,
                EffectKind::ClipboardStore { target, text } => {
                    if text.len() > MAX_CLIPBOARD_BYTES {
                        tracing::warn!(
                            terminal = %self.key.terminal,
                            bytes = text.len(),
                            "dropping an oversized OSC 52 clipboard store"
                        );
                        continue;
                    }
                    TerminalNotice::ClipboardStore {
                        target: match target {
                            fl_term::ClipboardTarget::Clipboard => ClipboardTarget::Clipboard,
                            fl_term::ClipboardTarget::Selection => ClipboardTarget::Selection,
                        },
                        text,
                    }
                }
                _ => continue,
            };
            self.notify(effect.sequence, notice);
        }
        Ok(())
    }

    /// Presentation notices go to every attached subscriber, parked ones
    /// included (a bell in a hidden tab is the point). A subscriber whose
    /// queue is full misses the notice rather than stalling the terminal.
    fn notify(&self, sequence: u64, notice: TerminalNotice) {
        let cost = SMALL
            + match &notice {
                TerminalNotice::ClipboardStore { text, .. } => text.len(),
                TerminalNotice::Bell => 0,
            };
        for subscriber in &self.subscribers {
            subscriber.sink.try_send(
                TerminalServerMsg::Notice {
                    terminal: self.key.terminal,
                    incarnation: self.key.incarnation,
                    sequence,
                    notice: notice.clone(),
                },
                cost,
            );
        }
    }

    fn fail(&mut self, message: String) {
        tracing::error!(terminal = %self.key.terminal, "terminal worker stopped: {message}");
        self.reading = false;
        for subscriber in &self.subscribers {
            let error = TerminalError::Internal {
                message: message.clone(),
            };
            subscriber.sink.try_send(self.error(None, error), SMALL);
        }
        self.failed = Some(message);
    }

    fn pump_all(&mut self) {
        let mut subscribers = std::mem::take(&mut self.subscribers);
        let mut stalled = false;
        subscribers.retain_mut(|subscriber| match self.pump(subscriber) {
            Pump::Done => true,
            Pump::Stalled => {
                stalled = true;
                true
            }
            Pump::Closed => false,
        });
        self.subscribers = subscribers;
        self.retry_at = stalled.then(|| Instant::now() + STALL_RETRY);
    }

    fn pump(&mut self, subscriber: &mut Subscriber) -> Pump {
        if subscriber.sink.is_closed() {
            return Pump::Closed;
        }
        if subscriber.parked || self.failed.is_some() {
            return Pump::Done;
        }
        loop {
            if !subscriber.needs_snapshot && subscriber.next == self.cursor {
                return Pump::Done;
            }
            if subscriber.sink.queued() > LAG_LIMIT {
                return Pump::Stalled;
            }
            let lag = self
                .cursor
                .next_byte
                .saturating_sub(subscriber.next.next_byte);
            let index = (!subscriber.needs_snapshot && lag <= LAG_LIMIT as u64)
                .then(|| self.ring.index(subscriber.next))
                .flatten();
            let Some(index) = index else {
                return self.offer_snapshot(subscriber);
            };
            let (records, cost, next) = self.ring.batch(index);
            let batch = TerminalServerMsg::RecordBatch(RecordBatch {
                subscription: subscriber.id,
                terminal: self.key.terminal,
                incarnation: self.key.incarnation,
                records,
                next,
            });
            match subscriber.sink.try_send(batch, cost) {
                TrySend::Sent => subscriber.next = next,
                TrySend::Full => return Pump::Stalled,
                TrySend::Closed => return Pump::Closed,
            }
        }
    }

    /// Replace a subscriber's pending history with the current exact state.
    fn offer_snapshot(&mut self, subscriber: &mut Subscriber) -> Pump {
        let snapshot = match self.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.fail(format!("encoding terminal snapshot: {error}"));
                return Pump::Done;
            }
        };
        let (bytes, sha256) = (snapshot.bytes.clone(), snapshot.sha256);
        let total_bytes = bytes.len() as u64;
        let transfer = self
            .shared
            .transfers
            .offer(subscriber.sink.session, bytes, sha256);
        let offer = TerminalServerMsg::SnapshotOffer(SnapshotOffer {
            transfer,
            subscription: subscriber.id,
            terminal: self.key.terminal,
            incarnation: self.key.incarnation,
            cursor: self.cursor,
            size: self.engine().size(),
            terminal_protocol: TERMINAL_PROTOCOL,
            engine_id: ENGINE_ID.into(),
            total_bytes,
            sha256,
        });
        match subscriber.sink.try_send(offer, SMALL) {
            TrySend::Sent => {
                subscriber.next = self.cursor;
                subscriber.needs_snapshot = false;
                Pump::Done
            }
            TrySend::Full => Pump::Stalled,
            TrySend::Closed => Pump::Closed,
        }
    }

    fn snapshot(&mut self) -> Result<&Snapshot, fl_term::EngineError> {
        if self
            .snapshot
            .as_ref()
            .is_none_or(|s| s.cursor != self.cursor)
        {
            let bytes = self.engine().checkpoint()?;
            let sha256 = Sha256::digest(&bytes).into();
            self.snapshot = Some(Snapshot {
                cursor: self.cursor,
                bytes: Arc::new(bytes),
                sha256,
            });
        }
        Ok(self.snapshot.as_ref().expect("snapshot was just cached"))
    }

    /// Durable checkpoint, then (and only then) journal reclamation.
    async fn checkpoint(&mut self) {
        if self.failed.is_some() || self.cursor == self.checkpointed {
            return;
        }
        let Some(engine) = self.engine.take() else {
            return;
        };
        let (key, cursor) = (self.key, self.cursor);
        let collect = self
            .shared
            .checkpoints
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(GC_EVERY);
        let result = self
            .shared
            .store
            .call(move |store| {
                let result = store.checkpoint(key, cursor, &engine);
                if collect && result.is_ok() {
                    super::collect_garbage(store);
                }
                (engine, result)
            })
            .await;
        self.last_checkpoint = Instant::now();
        let Ok((engine, result)) = result else {
            // The engine was dropped with the stopped store thread.
            self.reading = false;
            self.failed = Some("terminal state store stopped".into());
            return;
        };
        self.engine = Some(engine);
        if let Err(error) = result {
            tracing::warn!(terminal = %key.terminal, "terminal checkpoint failed: {error}");
            return;
        }
        self.checkpointed = cursor;
        let ack = Request::AckCheckpoint {
            terminal: key.terminal,
            incarnation: key.incarnation,
            through: cursor,
        };
        if let Err(error) = self.shared.prochost.request(&ack).await {
            // The next checkpoint acknowledges a later cursor, covering this one.
            tracing::warn!(terminal = %key.terminal, "journal acknowledgment failed: {error}");
        }
    }
}

async fn read_loop(
    shared: Arc<Shared>,
    key: TerminalKey,
    mut from: LogCursor,
    events: mpsc::Sender<ReadEvent>,
    abort: CancellationToken,
) {
    let mut backoff = Duration::from_millis(50);
    let mut last_state: Option<ProcessState> = None;
    loop {
        let request = Request::Read {
            terminal: key.terminal,
            incarnation: key.incarnation,
            from,
            max_bytes: MAX_READ_BYTES as u32,
            wait_ms: 1000,
        };
        let response = tokio::select! {
            _ = abort.cancelled() => return,
            response = shared.prochost.request(&request) => response,
        };
        let event = match response {
            Ok(Response::Records {
                records,
                next,
                tail,
                state,
            }) => {
                backoff = Duration::from_millis(50);
                let finished = records.is_empty()
                    && matches!(state, ProcessState::Exited(_) | ProcessState::Lost);
                let changed = last_state.as_ref() != Some(&state);
                from = next;
                if records.is_empty() && !changed {
                    if finished {
                        return;
                    }
                    continue;
                }
                last_state = Some(state.clone());
                let event = ReadEvent::Records {
                    records,
                    tail,
                    state,
                };
                let sent = tokio::select! {
                    _ = abort.cancelled() => return,
                    sent = events.send(event) => sent,
                };
                if sent.is_err() || finished {
                    return;
                }
                continue;
            }
            Ok(_) => ReadEvent::Failed("process host returned a non-record read response".into()),
            Err(error)
                if matches!(
                    error.code(),
                    Some(
                        ErrorCode::NotFound
                            | ErrorCode::WrongIncarnation
                            | ErrorCode::CursorExpired
                            | ErrorCode::CursorMismatch
                            | ErrorCode::InvalidArgument
                    )
                ) =>
            {
                ReadEvent::Failed(error.to_string())
            }
            Err(error) => {
                tracing::warn!(terminal = %key.terminal, "journal read failed, retrying: {error}");
                tokio::select! {
                    _ = abort.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => {},
                }
                backoff = (backoff * 2).min(Duration::from_secs(2));
                continue;
            }
        };
        let _ = events.send(event).await;
        return;
    }
}

/// Engine replies reach the PTY in sequence order through the prochost's
/// deduplicated boundary; replaying an older checkpoint never repeats them.
async fn effect_loop(
    shared: Arc<Shared>,
    key: TerminalKey,
    mut queue: mpsc::UnboundedReceiver<(u64, Vec<u8>)>,
    pending: Arc<AtomicUsize>,
    abort: CancellationToken,
) {
    loop {
        let next = tokio::select! {
            _ = abort.cancelled() => return,
            next = queue.recv() => next,
        };
        let Some((sequence, bytes)) = next else {
            return;
        };
        let length = bytes.len();
        let request = Request::Write {
            terminal: key.terminal,
            incarnation: key.incarnation,
            id: WriteId::Effect(sequence),
            bytes,
            deadline_ns: None,
        };
        let mut delay = Duration::from_millis(2);
        loop {
            let response = tokio::select! {
                _ = abort.cancelled() => return,
                response = shared.prochost.request(&request) => response,
            };
            match response {
                Ok(Response::Written { accepted, .. }) if accepted as usize >= length => break,
                Ok(Response::Written { .. }) => {}
                Ok(_) => break,
                Err(error)
                    if matches!(
                        error.code(),
                        Some(
                            ErrorCode::NotStopped
                                | ErrorCode::NotFound
                                | ErrorCode::WrongIncarnation
                                | ErrorCode::InvalidArgument
                                | ErrorCode::InputConflict
                        )
                    ) =>
                {
                    tracing::debug!(terminal = %key.terminal, sequence, "terminal reply dropped: {error}");
                    break;
                }
                Err(error) => {
                    tracing::warn!(terminal = %key.terminal, sequence, "terminal reply delayed: {error}")
                }
            }
            tokio::select! {
                _ = abort.cancelled() => return,
                _ = tokio::time::sleep(delay) => {},
            }
            delay = (delay * 2).min(Duration::from_secs(1));
        }
        pending.fetch_sub(length, Ordering::AcqRel);
    }
}
