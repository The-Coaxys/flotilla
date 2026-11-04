//! Terminal mirrors: the daemon's exact record stream applied to a local
//! engine off the UI thread and published as generation-tagged frames.
//!
//! One actor task per attached terminal incarnation owns the subscription,
//! the outstanding input and the snapshot state machine. The engine lives
//! behind a short-held lock so views can read text and history; parsing locks
//! it for at most one 16 KiB slice at a time. Frames are rendered by the
//! actor and published as `Arc`s together with a generation that increases
//! on every observable change.
use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use fl_model::{
    ExitStatus, IncarnationId, InputId, InputOwner, LogCursor, ProcessInfo, ProcessState, Size,
    SubscriptionId, TerminalId, TerminalRecord, TerminalRecordKind, TransferId, ViewId,
};
use fl_proto::{
    RecordBatch, SnapshotOffer, TerminalClientMsg, TerminalError, TerminalNotice, TerminalServerMsg,
};
use fl_term::{ENGINE_ID, Engine, HistoryLine, RenderFrame, TermMode, TextPoint};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::Core;
use crate::channels::{ChannelSet, Inbound, RouteKey, Routes, SnapshotResult};
use crate::clock::DaemonClock;
use crate::input::{AckOutcome, Discard, DiscardReason, INPUT_TTL, InputQueue};

/// Parsing holds the engine lock for at most this many bytes.
const FEED_SLICE: usize = 16 * 1024;
/// Records received while a snapshot transfers; beyond this, resubscribe.
const MAX_BUFFERED_BYTES: usize = 8 * 1024 * 1024;
const INPUT_RETRY: Duration = Duration::from_millis(50);
const RESYNC_MIN: Duration = Duration::from_millis(100);
const RESYNC_MAX: Duration = Duration::from_secs(5);
/// A cache write still running postpones the next one by this much.
const CACHE_RETRY: Duration = Duration::from_millis(200);
const EVENTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// The daemon runs a different child incarnation (`None`: unknown).
    Replaced { current: Option<IncarnationId> },
    /// The terminal no longer exists.
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorStatus {
    /// Cached or disconnected: the frame may lag the daemon.
    Stale,
    /// Connected and catching up (subscribing or installing a snapshot).
    Syncing,
    /// Applying the daemon's live record stream.
    Live,
    /// This incarnation is gone; the last frame stays readable.
    Ended(EndReason),
}

/// Published metadata of one mirror, replaced as a whole on every change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorState {
    pub terminal: TerminalId,
    pub incarnation: IncarnationId,
    pub status: MirrorStatus,
    /// Exclusive end of the daemon records reflected by the frame.
    pub cursor: Option<LogCursor>,
    pub owner: Option<InputOwner>,
    /// Canonical PTY echo state (predictive echo requires it).
    pub echo: bool,
    pub process: Option<ProcessInfo>,
    pub exit: Option<ExitStatus>,
    /// Typed input not yet acknowledged by the daemon (visibly pending).
    pub pending_input_bytes: usize,
    /// Control-link generation of the channels in use.
    pub link_generation: Option<u64>,
    pub snapshots_installed: u64,
    /// Reconnects that resumed from this mirror's own cursor.
    pub resumes: u64,
    /// Window title set by the program (OSC 0/2), part of the canonical
    /// engine state and therefore restored with every snapshot.
    pub title: Option<String>,
    /// Working directory reported by the shell (OSC 7), as a URI.
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorEvent {
    /// Every byte of `input` reached the PTY.
    InputAcknowledged {
        input: InputId,
        bytes: usize,
        echo: bool,
        cursor: LogCursor,
    },
    /// Input that will never be written (or only its `committed` prefix).
    InputDiscarded(Discard),
    /// The daemon refused a resize; `owner` holds the resize lease.
    ResizeRejected {
        owner: Option<InputOwner>,
    },
    Ended(EndReason),
    Error(String),
    /// The program's title or reported working directory changed.
    TitleChanged {
        title: Option<String>,
        cwd: Option<String>,
    },
    /// A bell or clipboard store from live output, delivered at most once per
    /// effect sequence even across reconnects.
    Notice {
        sequence: u64,
        notice: TerminalNotice,
    },
}

enum Command {
    Input { view: ViewId, bytes: Vec<u8> },
    Resize { view: ViewId, size: Size },
    Claim { view: ViewId },
    Visible { view: ViewId, visible: bool },
    Scroll(i32),
}

struct Published {
    generation: u64,
    frame: Arc<RenderFrame>,
    state: Arc<MirrorState>,
    wakers: Vec<Waker>,
}

struct MirrorShared {
    terminal: TerminalId,
    incarnation: IncarnationId,
    engine: Mutex<Option<Engine>>,
    published: Mutex<Published>,
    events: broadcast::Sender<MirrorEvent>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Keeps the actor alive; dropping the last one detaches the mirror.
pub(crate) struct Attachment {
    shared: Arc<MirrorShared>,
    commands: mpsc::UnboundedSender<Command>,
}

/// A view's handle to one terminal mirror. Clones share the view; every view
/// of the same terminal incarnation shares one mirror (and one viewport).
/// All methods are cheap and non-blocking, callable from the UI thread.
#[derive(Clone)]
pub struct MirrorHandle {
    attachment: Arc<Attachment>,
    view: ViewId,
}

impl std::fmt::Debug for MirrorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorHandle")
            .field("terminal", &self.terminal())
            .field("incarnation", &self.incarnation())
            .field("view", &self.view)
            .finish()
    }
}

impl MirrorHandle {
    pub(crate) fn new(attachment: Arc<Attachment>, view: ViewId) -> Self {
        let _ = attachment.commands.send(Command::Visible {
            view,
            visible: true,
        });
        Self { attachment, view }
    }

    fn shared(&self) -> &MirrorShared {
        &self.attachment.shared
    }

    fn command(&self, command: Command) {
        let _ = self.attachment.commands.send(command);
    }

    pub fn terminal(&self) -> TerminalId {
        self.shared().terminal
    }

    pub fn incarnation(&self) -> IncarnationId {
        self.shared().incarnation
    }

    pub fn view(&self) -> ViewId {
        self.view
    }

    /// Latest frame and its publication generation (an `Arc` clone).
    pub fn frame(&self) -> (u64, Arc<RenderFrame>) {
        let published = lock(&self.shared().published);
        (published.generation, published.frame.clone())
    }

    pub fn state(&self) -> Arc<MirrorState> {
        lock(&self.shared().published).state.clone()
    }

    pub fn generation(&self) -> u64 {
        lock(&self.shared().published).generation
    }

    /// Wake `waker` once after the next publication. Wakers that `will_wake`
    /// a registered one replace it.
    pub fn subscribe(&self, waker: &Waker) {
        let mut published = lock(&self.shared().published);
        register(&mut published.wakers, waker);
    }

    /// Resolves with the current generation once it exceeds `since`.
    pub async fn changed(&self, since: u64) -> u64 {
        poll_fn(|cx| {
            let mut published = lock(&self.shared().published);
            if published.generation > since {
                return Poll::Ready(published.generation);
            }
            register(&mut published.wakers, cx.waker());
            Poll::Pending
        })
        .await
    }

    pub fn events(&self) -> broadcast::Receiver<MirrorEvent> {
        self.shared().events.subscribe()
    }

    /// Already-encoded terminal input from this view, delivered in order and
    /// at most once. Never sent later than five seconds after this call.
    pub fn send_input(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.command(Command::Input {
                view: self.view,
                bytes,
            });
        }
    }

    /// Request a grid size for this view. Applied once this view holds the
    /// daemon's resize lease (claimed automatically when nobody holds it);
    /// the frame keeps the canonical size until the daemon approves.
    pub fn resize(&self, size: Size) {
        self.command(Command::Resize {
            view: self.view,
            size,
        });
    }

    /// Take the input-owner/resize lease for this view (e.g. on focus).
    pub fn claim(&self) {
        self.command(Command::Claim { view: self.view });
    }

    /// Hidden views stop live record delivery; a mirror whose views are all
    /// hidden is parked and catches up (possibly by snapshot) when shown.
    pub fn set_visible(&self, visible: bool) {
        self.command(Command::Visible {
            view: self.view,
            visible,
        });
    }

    /// Move the local viewport into (positive) or out of scrollback.
    pub fn scroll_display(&self, lines: i32) {
        self.command(Command::Scroll(lines));
    }

    pub fn modes(&self) -> TermMode {
        lock(&self.shared().published).frame.modes
    }

    pub fn text_between(&self, start: TextPoint, end: TextPoint) -> String {
        self.with_engine(|engine| engine.text_between(start, end))
            .unwrap_or_default()
    }

    pub fn history_page(&self, start: i32, count: u16) -> Vec<HistoryLine> {
        self.with_engine(|engine| engine.history_page(start, count))
            .unwrap_or_default()
    }

    pub fn stale(&self) -> bool {
        self.state().status != MirrorStatus::Live
    }

    pub fn exited(&self) -> Option<ExitStatus> {
        self.state().exit
    }

    /// Read the mirror engine under its lock; keep `f` short. `None` until a
    /// cached or daemon state exists.
    pub fn with_engine<R>(&self, f: impl FnOnce(&Engine) -> R) -> Option<R> {
        lock(&self.shared().engine).as_ref().map(f)
    }
}

fn register(wakers: &mut Vec<Waker>, waker: &Waker) {
    match wakers.iter_mut().find(|w| w.will_wake(waker)) {
        Some(existing) => existing.clone_from(waker),
        None => wakers.push(waker.clone()),
    }
}

/// Start the actor for one terminal incarnation.
pub(crate) fn spawn(
    core: &Arc<Core>,
    terminal: TerminalId,
    incarnation: IncarnationId,
) -> Arc<Attachment> {
    let (events, _) = broadcast::channel(EVENTS);
    let state = MirrorState {
        terminal,
        incarnation,
        status: MirrorStatus::Stale,
        cursor: None,
        owner: None,
        echo: false,
        process: None,
        exit: None,
        pending_input_bytes: 0,
        link_generation: None,
        snapshots_installed: 0,
        resumes: 0,
        title: None,
        cwd: None,
    };
    let shared = Arc::new(MirrorShared {
        terminal,
        incarnation,
        engine: Mutex::new(None),
        published: Mutex::new(Published {
            generation: 0,
            frame: Arc::new(RenderFrame::default()),
            state: Arc::new(state),
            wakers: Vec::new(),
        }),
        events,
    });
    let (commands_tx, commands) = mpsc::unbounded_channel();
    let key = (terminal, incarnation);
    let (mailbox, inbound) = Routes::mailbox();
    core.routes.insert(key, mailbox.clone());
    let actor = Actor {
        core: core.clone(),
        shared: shared.clone(),
        key,
        commands,
        mailbox,
        inbound,
        sets: core.sets.subscribe(),
        set: None,
        clock: None,
        cursor: None,
        sync: Sync::Down,
        force_snapshot: false,
        fetch: None,
        resync_failures: 0,
        resubscribe_at: None,
        ended: None,
        owner: None,
        echo: false,
        process: None,
        exit: None,
        views: HashMap::new(),
        parked: None,
        desired_size: None,
        resize_sent: None,
        claiming: false,
        input: InputQueue::default(),
        input_request: None,
        input_retry_at: None,
        outage: Some(Outage {
            since: Instant::now(),
            daemon_since: None,
        }),
        frame_dirty: false,
        state_dirty: true,
        spare: None,
        snapshots: 0,
        resumes: 0,
        last_notice: None,
        cache_due: None,
        cache_first: None,
        cache_write: None,
    };
    lock(&core.tasks).spawn_on(actor.run(), &core.runtime);
    Arc::new(Attachment {
        shared,
        commands: commands_tx,
    })
}

enum Sync {
    /// No subscription on the current channels.
    Down,
    Subscribing {
        request: u64,
        subscription: SubscriptionId,
    },
    Live {
        subscription: SubscriptionId,
    },
    /// Waiting for (or installing) a snapshot; later records are buffered.
    Awaiting {
        subscription: SubscriptionId,
        offer: Option<SnapshotOffer>,
        buffer: Vec<RecordBatch>,
        buffered: usize,
    },
}

impl Sync {
    fn subscription(&self) -> Option<SubscriptionId> {
        match self {
            Sync::Down => None,
            Sync::Subscribing { subscription, .. }
            | Sync::Live { subscription }
            | Sync::Awaiting { subscription, .. } => Some(*subscription),
        }
    }

    /// The daemon confirmed this incarnation on the current channels.
    fn subscribed(&self) -> bool {
        matches!(self, Sync::Live { .. } | Sync::Awaiting { .. })
    }
}

struct Outage {
    since: Instant,
    /// Daemon-clock estimate at `since`, when a clock sample existed.
    daemon_since: Option<u64>,
}

struct Actor {
    core: Arc<Core>,
    shared: Arc<MirrorShared>,
    key: RouteKey,
    commands: mpsc::UnboundedReceiver<Command>,
    mailbox: mpsc::Sender<Inbound>,
    inbound: mpsc::Receiver<Inbound>,
    sets: watch::Receiver<Option<Arc<ChannelSet>>>,
    set: Option<Arc<ChannelSet>>,
    clock: Option<DaemonClock>,
    /// Exact daemon position of the engine; `Some` iff an engine exists.
    cursor: Option<LogCursor>,
    sync: Sync,
    force_snapshot: bool,
    fetch: Option<(TransferId, oneshot::Receiver<SnapshotResult>)>,
    resync_failures: u32,
    resubscribe_at: Option<Instant>,
    ended: Option<EndReason>,
    owner: Option<InputOwner>,
    echo: bool,
    process: Option<ProcessInfo>,
    exit: Option<ExitStatus>,
    views: HashMap<ViewId, bool>,
    /// Parking state sent for the current subscription.
    parked: Option<bool>,
    desired_size: Option<(ViewId, Size)>,
    resize_sent: Option<Size>,
    claiming: bool,
    input: InputQueue,
    /// Request ID of the outstanding input on the current channels.
    input_request: Option<u64>,
    input_retry_at: Option<Instant>,
    outage: Option<Outage>,
    frame_dirty: bool,
    state_dirty: bool,
    /// A previously published frame no view still holds, reused for rendering.
    spare: Option<RenderFrame>,
    snapshots: u64,
    resumes: u64,
    /// Highest notice sequence already emitted for this incarnation.
    last_notice: Option<u64>,
    cache_due: Option<Instant>,
    cache_first: Option<Instant>,
    cache_write: Option<JoinHandle<()>>,
}

async fn snapshot_ready(
    fetch: &mut Option<(TransferId, oneshot::Receiver<SnapshotResult>)>,
) -> (TransferId, SnapshotResult) {
    match fetch {
        Some((transfer, result)) => {
            let result = result
                .await
                .unwrap_or_else(|_| Err("the bulk channel closed".into()));
            (*transfer, result)
        }
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

fn earliest(deadlines: impl IntoIterator<Item = Option<Instant>>) -> Option<Instant> {
    deadlines.into_iter().flatten().min()
}

impl Actor {
    async fn run(mut self) {
        self.load_cache().await;
        self.on_sets_changed().await;
        self.publish();
        loop {
            let timer = self.next_timer();
            tokio::select! {
                biased;
                _ = self.core.shutdown.cancelled() => break,
                command = self.commands.recv() => match command {
                    Some(command) => self.on_command(command).await,
                    None => break,
                },
                changed = self.sets.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    self.on_sets_changed().await;
                }
                Some(inbound) = self.inbound.recv() => self.on_inbound(inbound).await,
                (transfer, result) = snapshot_ready(&mut self.fetch) => {
                    self.fetch = None;
                    self.on_snapshot(transfer, result).await;
                }
                _ = sleep_until(timer) => self.on_timer().await,
            }
            // Apply a burst before rendering once.
            for _ in 0..64 {
                match self.inbound.try_recv() {
                    Ok(inbound) => self.on_inbound(inbound).await,
                    Err(_) => break,
                }
            }
            self.publish_if_dirty();
        }
        self.finish().await;
    }

    fn emit(&self, event: MirrorEvent) {
        let _ = self.shared.events.send(event);
    }

    fn discarded(&mut self, discard: Discard) {
        self.state_dirty = true;
        self.emit(MirrorEvent::InputDiscarded(discard));
    }

    fn next_timer(&self) -> Option<Instant> {
        let outage = self
            .outage
            .as_ref()
            .filter(|_| self.input.has_queued())
            .map(|outage| outage.since + INPUT_TTL);
        earliest([
            self.resubscribe_at,
            self.input_retry_at,
            outage,
            self.cache_due,
        ])
    }

    async fn on_timer(&mut self) {
        let now = Instant::now();
        if self.resubscribe_at.is_some_and(|at| at <= now) {
            self.resubscribe_at = None;
            self.subscribe().await;
        }
        if self.input_retry_at.is_some_and(|at| at <= now) {
            self.input_retry_at = None;
            self.pump_input().await;
        }
        if let Some(outage) = &self.outage {
            let duration = now.saturating_duration_since(outage.since);
            if duration >= INPUT_TTL
                && let Some(discard) = self
                    .input
                    .discard_queued(DiscardReason::Outage { duration })
            {
                self.discarded(discard);
            }
        }
        if self.cache_due.is_some_and(|at| at <= now) {
            self.save_cache(false).await;
        }
    }

    async fn on_command(&mut self, command: Command) {
        match command {
            Command::Input { view, bytes } => {
                let refused = match (&self.ended, &self.exit) {
                    (Some(_), _) => Some(DiscardReason::IncarnationEnded),
                    (None, Some(_)) => Some(DiscardReason::Exited),
                    (None, None) => None,
                };
                if let Some(reason) = refused {
                    let bytes = bytes.len();
                    return self.discarded(Discard {
                        bytes,
                        committed: 0,
                        reason,
                    });
                }
                match self
                    .input
                    .push(view, &bytes, Instant::now(), self.clock.as_ref())
                {
                    Ok(()) => {
                        self.state_dirty = true;
                        self.pump_input().await;
                    }
                    Err(discard) => self.discarded(discard),
                }
            }
            Command::Resize { view, size } => {
                if size.validate().is_err() {
                    return self.emit(MirrorEvent::Error(format!(
                        "unsupported terminal size {}x{}",
                        size.columns, size.rows
                    )));
                }
                self.desired_size = Some((view, size));
                self.resize_sent = None;
                self.apply_resize().await;
            }
            Command::Claim { view } => self.claim(view).await,
            Command::Visible { view, visible } => {
                self.views.insert(view, visible);
                self.sync_parked().await;
            }
            Command::Scroll(lines) => {
                if let Some(engine) = lock(&self.shared.engine).as_mut() {
                    engine.scroll(lines);
                    self.frame_dirty = true;
                }
            }
        }
    }

    async fn on_sets_changed(&mut self) {
        let next = self.sets.borrow_and_update().clone();
        let current = self.set.as_ref().map(|set| set.epoch);
        match next {
            Some(set) if Some(set.epoch) == current => {}
            Some(set) => {
                if self.set.is_some() {
                    self.link_down();
                }
                self.clock = Some(set.clock());
                self.set = Some(set);
                self.state_dirty = true;
                self.subscribe().await;
            }
            None => {
                if self.set.is_some() {
                    self.link_down();
                }
            }
        }
    }

    /// The epoch ended: everything bound to it is void. The outstanding input
    /// is kept for an exact resend; the outage clock starts.
    fn link_down(&mut self) {
        let now = Instant::now();
        self.set = None;
        self.sync = Sync::Down;
        self.fetch = None;
        self.input_request = None;
        self.parked = None;
        self.resize_sent = None;
        self.claiming = false;
        if self.outage.is_none() {
            self.outage = Some(Outage {
                since: now,
                daemon_since: self.clock.map(|clock| clock.lower_bound(now)),
            });
        }
        self.state_dirty = true;
    }

    async fn subscribe(&mut self) {
        if self.ended.is_some() {
            return;
        }
        let Some(set) = self.set.clone() else {
            return;
        };
        self.resubscribe_at = None;
        let request = set.request_id();
        let subscription = SubscriptionId::new();
        let from = if self.force_snapshot {
            None
        } else {
            self.cursor
        };
        self.sync = Sync::Subscribing {
            request,
            subscription,
        };
        self.parked = None;
        self.state_dirty = true;
        set.send(TerminalClientMsg::Subscribe {
            id: request,
            subscription,
            terminal: self.key.0,
            incarnation: self.key.1,
            from,
        })
        .await;
    }

    /// Drop the current stream and obtain a fresh snapshot after a backoff.
    async fn resync(&mut self, why: &str) {
        tracing::warn!(terminal = %self.key.0, "mirror resynchronizing: {why}");
        if let (Some(set), Some(subscription)) = (&self.set, self.sync.subscription()) {
            set.send(TerminalClientMsg::Unsubscribe {
                id: set.request_id(),
                subscription,
                terminal: self.key.0,
                incarnation: self.key.1,
            })
            .await;
        }
        self.sync = Sync::Down;
        self.fetch = None;
        self.input_request = None;
        self.force_snapshot = true;
        self.retry_subscribe();
    }

    fn retry_subscribe(&mut self) {
        self.resync_failures = self.resync_failures.saturating_add(1);
        let delay = RESYNC_MIN
            .saturating_mul(1 << self.resync_failures.min(8))
            .min(RESYNC_MAX);
        self.resubscribe_at = Some(Instant::now() + delay);
        self.state_dirty = true;
    }

    async fn on_inbound(&mut self, inbound: Inbound) {
        // Traffic of a former link generation or channel epoch is void.
        if self.set.as_ref().map(|set| set.epoch) != Some(inbound.epoch) || self.ended.is_some() {
            return;
        }
        match inbound.msg {
            TerminalServerMsg::Subscribed {
                id,
                subscription,
                incarnation,
                cursor,
                owner,
                echo,
                ..
            } => {
                if incarnation == self.key.1
                    && matches!(self.sync, Sync::Subscribing { request, subscription: s }
                        if request == id && s == subscription)
                {
                    self.on_subscribed(subscription, cursor, owner, echo).await;
                }
            }
            TerminalServerMsg::RecordBatch(batch) => {
                if Some(batch.subscription) == self.sync.subscription()
                    && batch.incarnation == self.key.1
                {
                    self.on_batch(batch).await;
                }
            }
            TerminalServerMsg::SnapshotOffer(offer) => {
                if Some(offer.subscription) == self.sync.subscription()
                    && offer.incarnation == self.key.1
                {
                    self.on_offer(offer);
                }
            }
            TerminalServerMsg::InputAck(ack) => {
                if ack.incarnation == self.key.1 {
                    self.on_ack(ack).await;
                }
            }
            TerminalServerMsg::OwnerChanged {
                id,
                incarnation,
                owner,
                ..
            } => {
                if incarnation == self.key.1 {
                    if id.is_some() {
                        self.claiming = false;
                    }
                    if self.owner != owner {
                        self.owner = owner;
                        self.resize_sent = None;
                        self.state_dirty = true;
                    }
                    self.apply_resize().await;
                }
            }
            TerminalServerMsg::ProcessChanged(info) => {
                if info.incarnation == self.key.1 {
                    if let ProcessState::Exited(status) | ProcessState::Draining(status) =
                        &info.state
                    {
                        self.exit = Some(*status);
                    }
                    self.echo = info.echo;
                    self.process = Some(info);
                    self.state_dirty = true;
                }
            }
            TerminalServerMsg::Error { id, error, .. } => self.on_error(id, error).await,
            TerminalServerMsg::Notice {
                incarnation,
                sequence,
                notice,
                ..
            } => {
                if incarnation == self.key.1 && self.last_notice.is_none_or(|last| sequence > last)
                {
                    self.last_notice = Some(sequence);
                    self.emit(MirrorEvent::Notice { sequence, notice });
                }
            }
            // Confirmations whose effects arrive as ordered records.
            TerminalServerMsg::Resized { .. }
            | TerminalServerMsg::Parked { .. }
            | TerminalServerMsg::Unsubscribed { .. } => {}
        }
    }

    async fn on_subscribed(
        &mut self,
        subscription: SubscriptionId,
        cursor: LogCursor,
        owner: Option<InputOwner>,
        echo: bool,
    ) {
        self.owner = owner;
        self.echo = echo;
        if !self.force_snapshot && self.cursor == Some(cursor) {
            self.sync = Sync::Live { subscription };
            self.resumes += 1;
            self.resync_failures = 0;
        } else {
            // The daemon offers a snapshot first; the old frame stays visible.
            self.sync = Sync::Awaiting {
                subscription,
                offer: None,
                buffer: Vec::new(),
                buffered: 0,
            };
        }
        self.force_snapshot = false;
        self.state_dirty = true;
        self.sync_parked().await;
        self.resume_input().await;
        self.apply_resize().await;
    }

    async fn on_batch(&mut self, batch: RecordBatch) {
        let failure = match &mut self.sync {
            Sync::Live { .. } => self.apply_batch(&batch).err(),
            Sync::Awaiting {
                buffer, buffered, ..
            } => {
                let bytes = batch_bytes(&batch);
                if *buffered + bytes > MAX_BUFFERED_BYTES {
                    Some("records outran the snapshot transfer".to_string())
                } else {
                    *buffered += bytes;
                    buffer.push(batch);
                    None
                }
            }
            Sync::Down | Sync::Subscribing { .. } => None,
        };
        if let Some(error) = failure {
            self.resync(&error).await;
        }
    }

    /// Apply contiguous records; prefixes already covered by an installed
    /// snapshot are skipped, gaps are errors.
    fn apply_batch(&mut self, batch: &RecordBatch) -> Result<(), String> {
        let Some(mut cursor) = self.cursor else {
            return Err("records arrived without a mirror state".into());
        };
        if batch.next.next_record <= cursor.next_record {
            return Ok(());
        }
        let start = batch.records.first().map_or(batch.next, |r| r.at);
        if start.next_record > cursor.next_record {
            return Err(format!(
                "record gap: mirror at {cursor:?}, batch starts at {start:?}"
            ));
        }
        for record in &batch.records {
            if record.at.next_record < cursor.next_record {
                continue;
            }
            if record.at != cursor {
                return Err(format!(
                    "record mismatch: mirror at {cursor:?}, record at {:?}",
                    record.at
                ));
            }
            self.apply_record(record)?;
            cursor = record.end().ok_or("record cursor overflow")?;
            self.cursor = Some(cursor);
        }
        self.engine_changed();
        Ok(())
    }

    fn apply_record(&mut self, record: &TerminalRecord) -> Result<(), String> {
        let engine_error = |e: fl_term::EngineError| e.to_string();
        match &record.kind {
            TerminalRecordKind::Output { bytes, echo } => {
                self.echo = *echo;
                let mut offset = record.at.next_byte;
                for slice in bytes.chunks(FEED_SLICE) {
                    let mut engine = lock(&self.shared.engine);
                    let engine = engine.as_mut().ok_or("no mirror engine")?;
                    engine.feed(offset, slice).map_err(engine_error)?;
                    offset += slice.len() as u64;
                }
            }
            TerminalRecordKind::Resize(size) => {
                let mut engine = lock(&self.shared.engine);
                let engine = engine.as_mut().ok_or("no mirror engine")?;
                engine.resize(*size).map_err(engine_error)?;
                self.resize_sent = None;
                self.state_dirty = true;
            }
            TerminalRecordKind::SyncFlush => {
                let mut engine = lock(&self.shared.engine);
                let engine = engine.as_mut().ok_or("no mirror engine")?;
                engine.flush_sync().map_err(engine_error)?;
            }
            TerminalRecordKind::Echo(echo) => {
                self.echo = *echo;
                self.state_dirty = true;
            }
            TerminalRecordKind::Exit(status) => {
                self.exit = Some(*status);
                self.state_dirty = true;
            }
        }
        Ok(())
    }

    fn engine_changed(&mut self) {
        self.frame_dirty = true;
        self.state_dirty = true;
        if self.core.cache.is_some() {
            let now = Instant::now();
            let first = *self.cache_first.get_or_insert(now);
            let due = (now + self.core.config.cache_debounce)
                .min(first + self.core.config.cache_max_delay);
            self.cache_due = Some(due);
        }
    }

    fn on_offer(&mut self, offer: SnapshotOffer) {
        if offer.engine_id != ENGINE_ID {
            return self.emit(MirrorEvent::Error(format!(
                "the daemon offered a snapshot of engine {}",
                offer.engine_id
            )));
        }
        let (Some(set), Some(subscription)) = (&self.set, self.sync.subscription()) else {
            return;
        };
        // A newer offer supersedes buffered records and any running transfer.
        self.fetch = Some((offer.transfer, set.snapshot(offer.clone())));
        self.sync = Sync::Awaiting {
            subscription,
            offer: Some(offer),
            buffer: Vec::new(),
            buffered: 0,
        };
        self.state_dirty = true;
    }

    async fn on_snapshot(&mut self, transfer: TransferId, result: SnapshotResult) {
        let Sync::Awaiting {
            subscription,
            offer: Some(offer),
            buffer,
            ..
        } = &mut self.sync
        else {
            return;
        };
        if offer.transfer != transfer {
            return;
        }
        match result {
            Ok(engine) => {
                let cursor = offer.cursor;
                let subscription = *subscription;
                let buffered = std::mem::take(buffer);
                // Atomic installation: the swap happens under the lock.
                *lock(&self.shared.engine) = Some(engine);
                self.cursor = Some(cursor);
                self.snapshots += 1;
                self.resync_failures = 0;
                self.sync = Sync::Live { subscription };
                self.engine_changed();
                for batch in buffered {
                    if let Err(error) = self.apply_batch(&batch) {
                        return self.resync(&error).await;
                    }
                }
                self.apply_resize().await;
            }
            Err(error) => {
                self.emit(MirrorEvent::Error(format!("snapshot transfer: {error}")));
                self.resync(&error).await;
            }
        }
    }

    async fn on_ack(&mut self, ack: fl_proto::InputAck) {
        match self.input.on_ack(&ack) {
            AckOutcome::Delivered(flight) => {
                self.input_request = None;
                self.echo = ack.echo;
                self.state_dirty = true;
                self.emit(MirrorEvent::InputAcknowledged {
                    input: flight.input,
                    bytes: flight.bytes.len(),
                    echo: ack.echo,
                    cursor: ack.cursor,
                });
                self.pump_input().await;
            }
            AckOutcome::Expired(discard) => {
                self.input_request = None;
                self.discarded(discard);
                self.pump_input().await;
            }
            AckOutcome::Partial => {
                // Same identity and bytes: the process host writes the suffix.
                self.input_request = None;
                self.pump_input().await;
            }
            AckOutcome::Ignored => {}
        }
    }

    async fn on_error(&mut self, id: Option<u64>, error: TerminalError) {
        let subscribe_request = match self.sync {
            Sync::Subscribing { request, .. } => Some(request),
            _ => None,
        };
        let for_subscription = id.is_some() && id == subscribe_request;
        let for_input = id.is_some() && id == self.input_request;
        match &error {
            TerminalError::WrongIncarnation { current } => {
                return self
                    .end(EndReason::Replaced {
                        current: Some(*current),
                    })
                    .await;
            }
            TerminalError::NotFound if for_subscription || for_input => {
                return self.end(EndReason::NotFound).await;
            }
            _ => {}
        }
        if for_subscription {
            if matches!(error, TerminalError::ResyncRequired { .. }) {
                self.force_snapshot = true;
            }
            if !matches!(error, TerminalError::Backpressured) {
                self.emit(MirrorEvent::Error(format!("subscribe: {error:?}")));
            }
            self.sync = Sync::Down;
            return self.retry_subscribe();
        }
        if for_input {
            self.input_request = None;
            match error {
                TerminalError::Backpressured => {
                    self.input_retry_at = Some(Instant::now() + INPUT_RETRY);
                }
                TerminalError::InvalidArgument { message }
                | TerminalError::Internal { message } => {
                    if let Some(discard) = self
                        .input
                        .reject_in_flight(DiscardReason::Rejected(message))
                    {
                        self.discarded(discard);
                    }
                    self.pump_input().await;
                }
                other => {
                    if let Some(discard) = self
                        .input
                        .reject_in_flight(DiscardReason::Rejected(format!("{other:?}")))
                    {
                        self.discarded(discard);
                    }
                    self.pump_input().await;
                }
            }
            return;
        }
        match error {
            TerminalError::NotOwner { owner } => {
                self.owner = owner;
                self.resize_sent = None;
                self.claiming = false;
                self.state_dirty = true;
                self.emit(MirrorEvent::ResizeRejected { owner });
            }
            // The daemon's worker for this terminal stopped streaming.
            TerminalError::Internal { message } if id.is_none() => {
                self.emit(MirrorEvent::Error(message));
                self.sync = Sync::Down;
                self.force_snapshot = true;
                self.retry_subscribe();
            }
            other => self.emit(MirrorEvent::Error(format!("{other:?}"))),
        }
    }

    /// This incarnation is gone: no input can ever be delivered.
    async fn end(&mut self, reason: EndReason) {
        if self.ended.is_some() {
            return;
        }
        for discard in self.input.discard_all(DiscardReason::IncarnationEnded) {
            self.discarded(discard);
        }
        self.ended = Some(reason.clone());
        self.sync = Sync::Down;
        self.fetch = None;
        self.input_request = None;
        self.resubscribe_at = None;
        self.input_retry_at = None;
        self.cache_due = None;
        self.cache_first = None;
        self.state_dirty = true;
        self.emit(MirrorEvent::Ended(reason));
        if let Some(cache) = self.core.cache.clone() {
            if let Some(write) = self.cache_write.take() {
                let _ = write.await;
            }
            let terminal = self.key.0;
            let _ = tokio::task::spawn_blocking(move || cache.remove_mirror(terminal)).await;
        }
    }

    /// After (re)subscribing on new channels: apply the outage rule, then
    /// resend the outstanding input exactly and continue with the queue.
    async fn resume_input(&mut self) {
        let now = Instant::now();
        if let Some(outage) = self.outage.take() {
            let local = now.saturating_duration_since(outage.since);
            let daemon = match (outage.daemon_since, self.clock) {
                (Some(since), Some(clock)) => {
                    Duration::from_nanos(clock.lower_bound(now).saturating_sub(since))
                }
                _ => Duration::ZERO,
            };
            let duration = local.max(daemon);
            if duration >= INPUT_TTL
                && let Some(discard) = self
                    .input
                    .discard_queued(DiscardReason::Outage { duration })
            {
                self.discarded(discard);
            }
        }
        self.input_request = None;
        self.pump_input().await;
    }

    async fn pump_input(&mut self) {
        if self.ended.is_some() || self.input_request.is_some() || !self.sync.subscribed() {
            return;
        }
        if self.input_retry_at.is_some_and(|at| at > Instant::now()) {
            return;
        }
        let Some(set) = self.set.clone() else {
            return;
        };
        let clock = set.clock();
        self.clock = Some(clock);
        let mut discards = Vec::new();
        let flight = match self.input.in_flight() {
            Some(flight) => Some(flight.clone()),
            None => self
                .input
                .next(
                    Instant::now(),
                    &clock,
                    set.client,
                    &self.core.sequence,
                    &mut discards,
                )
                .cloned(),
        };
        for discard in discards {
            self.discarded(discard);
        }
        let Some(flight) = flight else {
            return;
        };
        let request = set.request_id();
        self.input_request = Some(request);
        self.state_dirty = true;
        set.send(TerminalClientMsg::Input {
            id: request,
            terminal: self.key.0,
            incarnation: self.key.1,
            input: flight.input,
            view: flight.view,
            bytes: flight.bytes,
            deadline_ns: flight.deadline_ns,
        })
        .await;
    }

    fn current_size(&self) -> Option<Size> {
        lock(&self.shared.engine)
            .as_ref()
            .map(Engine::size)
            .or_else(|| self.process.as_ref().map(|process| process.size))
    }

    async fn claim(&mut self, view: ViewId) {
        let Some(set) = self.set.clone() else {
            return;
        };
        if !self.sync.subscribed() || self.ended.is_some() {
            return;
        }
        self.claiming = true;
        set.send(TerminalClientMsg::ClaimOwner {
            id: set.request_id(),
            terminal: self.key.0,
            incarnation: self.key.1,
            view,
        })
        .await;
    }

    /// Send the desired size once this view holds the resize lease; claim the
    /// lease when nobody holds it.
    async fn apply_resize(&mut self) {
        let (Some(set), Some((view, size))) = (self.set.clone(), self.desired_size) else {
            return;
        };
        if self.ended.is_some() || self.exit.is_some() || !self.sync.subscribed() {
            return;
        }
        if self.current_size() == Some(size) || self.resize_sent == Some(size) {
            return;
        }
        match self.owner {
            Some(owner) if owner.client == set.client && owner.view == view => {
                self.resize_sent = Some(size);
                set.send(TerminalClientMsg::Resize {
                    id: set.request_id(),
                    terminal: self.key.0,
                    incarnation: self.key.1,
                    owner,
                    size,
                })
                .await;
            }
            None if !self.claiming => self.claim(view).await,
            // Another view or client owns the size; render it letterboxed.
            _ => {}
        }
    }

    async fn sync_parked(&mut self) {
        let Some(subscription) = self.sync.subscription() else {
            return;
        };
        let Some(set) = self.set.clone() else {
            return;
        };
        if !self.sync.subscribed() {
            return;
        }
        let parked = !self.views.values().any(|visible| *visible);
        // A new subscription starts unparked.
        if self.parked.unwrap_or(false) == parked {
            self.parked = Some(parked);
            return;
        }
        self.parked = Some(parked);
        set.send(TerminalClientMsg::SetParked {
            id: set.request_id(),
            subscription,
            terminal: self.key.0,
            incarnation: self.key.1,
            parked,
        })
        .await;
    }

    fn status(&self) -> MirrorStatus {
        match (&self.ended, &self.set, &self.sync) {
            (Some(reason), _, _) => MirrorStatus::Ended(reason.clone()),
            (None, None, _) => MirrorStatus::Stale,
            (None, Some(_), Sync::Live { .. }) => MirrorStatus::Live,
            (None, Some(_), _) => MirrorStatus::Syncing,
        }
    }

    fn state(&self) -> MirrorState {
        let (title, cwd) = lock(&self.shared.engine)
            .as_ref()
            .map(|engine| {
                (
                    engine.title().map(str::to_owned),
                    engine.cwd().map(str::to_owned),
                )
            })
            .unwrap_or_default();
        MirrorState {
            terminal: self.key.0,
            incarnation: self.key.1,
            status: self.status(),
            cursor: self.cursor,
            owner: self.owner,
            echo: self.echo,
            process: self.process.clone(),
            exit: self.exit,
            pending_input_bytes: self.input.pending_bytes(),
            link_generation: self.set.as_ref().map(|set| set.generation),
            snapshots_installed: self.snapshots,
            resumes: self.resumes,
            title,
            cwd,
        }
    }

    fn publish_if_dirty(&mut self) {
        if self.frame_dirty || self.state_dirty {
            self.publish();
        }
    }

    fn publish(&mut self) {
        let frame = self.frame_dirty.then(|| {
            let mut frame = self.spare.take().unwrap_or_default();
            if let Some(engine) = lock(&self.shared.engine).as_ref() {
                engine.render_into(&mut frame);
            }
            Arc::new(frame)
        });
        let state = self.state();
        let wakers = {
            let mut published = lock(&self.shared.published);
            if frame.is_none() && *published.state == state {
                self.state_dirty = false;
                return;
            }
            published.generation += 1;
            if let Some(frame) = frame {
                let old = std::mem::replace(&mut published.frame, frame);
                // Reuse the previous frame's buffers once no view holds it.
                self.spare = Arc::try_unwrap(old).ok();
            }
            let retitled =
                (&published.state.title, &published.state.cwd) != (&state.title, &state.cwd);
            if retitled {
                self.emit(MirrorEvent::TitleChanged {
                    title: state.title.clone(),
                    cwd: state.cwd.clone(),
                });
            }
            published.state = Arc::new(state);
            std::mem::take(&mut published.wakers)
        };
        self.frame_dirty = false;
        self.state_dirty = false;
        for waker in wakers {
            waker.wake();
        }
    }

    async fn load_cache(&mut self) {
        let Some(cache) = self.core.cache.clone() else {
            return;
        };
        let terminal = self.key.0;
        let loaded = tokio::task::spawn_blocking(move || cache.load_mirror(terminal))
            .await
            .ok()
            .flatten();
        if let Some(cached) = loaded
            && cached.incarnation == self.key.1
        {
            *lock(&self.shared.engine) = Some(cached.engine);
            self.cursor = Some(cached.cursor);
            self.frame_dirty = true;
            self.state_dirty = true;
        }
    }

    /// Debounced checkpoint of the mirror state; `last` waits for the write.
    async fn save_cache(&mut self, last: bool) {
        let Some(cache) = self.core.cache.clone() else {
            return;
        };
        let (Some(cursor), None) = (self.cursor, &self.ended) else {
            self.cache_due = None;
            return;
        };
        if let Some(write) = &self.cache_write
            && !write.is_finished()
        {
            if !last {
                self.cache_due = Some(Instant::now() + CACHE_RETRY);
                return;
            }
            if let Some(write) = self.cache_write.take() {
                let _ = write.await;
            }
        }
        if self.cache_due.is_none() && !last {
            return;
        }
        let checkpoint = match lock(&self.shared.engine).as_ref().map(Engine::checkpoint) {
            Some(Ok(checkpoint)) => checkpoint,
            Some(Err(error)) => {
                tracing::warn!(terminal = %self.key.0, "mirror checkpoint failed: {error}");
                self.cache_due = None;
                return;
            }
            None => return,
        };
        self.cache_due = None;
        self.cache_first = None;
        let (terminal, incarnation) = self.key;
        let write = tokio::task::spawn_blocking(move || {
            if let Err(error) = cache.save_mirror(terminal, incarnation, cursor, checkpoint) {
                tracing::warn!(%terminal, "writing the mirror cache failed: {error}");
            }
        });
        if last {
            let _ = write.await;
        } else {
            self.cache_write = Some(write);
        }
    }

    async fn finish(mut self) {
        self.core.routes.remove(self.key, &self.mailbox);
        if self.ended.is_none()
            && let (Some(set), Some(subscription)) = (&self.set, self.sync.subscription())
            && !set.is_closed()
        {
            let unsubscribe = TerminalClientMsg::Unsubscribe {
                id: set.request_id(),
                subscription,
                terminal: self.key.0,
                incarnation: self.key.1,
            };
            let _ = tokio::time::timeout(Duration::from_secs(1), set.send(unsubscribe)).await;
        }
        for discard in self.input.discard_all(DiscardReason::Detached) {
            self.emit(MirrorEvent::InputDiscarded(discard));
        }
        if self.cache_due.is_some() {
            self.save_cache(true).await;
        } else if let Some(write) = self.cache_write.take() {
            let _ = write.await;
        }
        self.set = None;
        self.state_dirty = true;
        self.publish();
        let mut mirrors = lock(&self.core.mirrors);
        if mirrors
            .get(&self.key)
            .is_some_and(|attachment: &Weak<Attachment>| attachment.strong_count() == 0)
        {
            mirrors.remove(&self.key);
        }
    }
}

fn batch_bytes(batch: &RecordBatch) -> usize {
    batch
        .records
        .iter()
        .map(|record| match &record.kind {
            TerminalRecordKind::Output { bytes, .. } => bytes.len() + 48,
            _ => 48,
        })
        .sum()
}
