//! Client session state: terminal mirrors, exactly-once input, layout
//! reconciliation and the offline cache.
//!
//! The UI thread only touches `MirrorHandle` and `Sync` methods: they clone an
//! `Arc`, wake a task, or read one short-held lock. Parsing, network IO, disk
//! writes and reconnection live on the caller's Tokio runtime and off the UI.
mod cache;
mod channels;
mod clock;
mod input;
mod layout;
mod mirror;

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use fl_model::layout::LayoutOp;
use fl_model::{IncarnationId, OperationId, ProcessInfo, TerminalId, ViewId};
pub use input::{Discard, DiscardReason};
pub use layout::{LayoutApplyError, LayoutTicket, PendingLayoutOp, WorkspaceState};
pub use mirror::{EndReason, MirrorEvent, MirrorHandle, MirrorState, MirrorStatus};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use cache::Cache;
use channels::{ChannelSet, Routes};

/// Session-wide notifications, in addition to each mirror's own events.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    /// A new control link is negotiated and channels are open.
    Connected { generation: u64 },
    /// The control link ended; mirrors keep their last frames, marked stale.
    Disconnected,
    /// The daemon runs a different terminal engine; exact mirrors are refused.
    IncompatibleEngine { daemon: String },
    /// Channels could not be established on this link.
    ChannelsFailed { error: String },
    /// A layout operation was rejected by the daemon and dropped.
    LayoutRejected {
        operation: OperationId,
        op: LayoutOp,
        error: LayoutApplyError,
    },
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Debounced mirror-cache write delay after the last change.
    pub cache_debounce: Duration,
    /// Upper bound on that debounce while output keeps arriving.
    pub cache_max_delay: Duration,
    /// Periodic process-list refresh for offline reattachment.
    pub layout_refresh: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache_debounce: Duration::from_millis(500),
            cache_max_delay: Duration::from_secs(3),
            layout_refresh: Duration::from_secs(5),
        }
    }
}

/// Shared session state behind every handle. One per client session.
pub(crate) struct Core {
    pub(crate) runtime: Handle,
    pub(crate) tasks: Mutex<JoinSet<()>>,
    pub(crate) mirrors: Mutex<HashMap<(TerminalId, IncarnationId), Weak<mirror::Attachment>>>,
    pub(crate) routes: Arc<Routes>,
    pub(crate) sets: watch::Sender<Option<Arc<ChannelSet>>>,
    pub(crate) cache: Option<Cache>,
    pub(crate) config: Config,
    pub(crate) sequence: AtomicU64,
    pub(crate) shutdown: CancellationToken,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Owns the sync tasks for one client session.
///
/// `links` is published by the connection supervisor: each new authenticated
/// control link replaces the previous one, and mirrors, channels and layout
/// reconcile against that generation automatically.
pub struct Sync {
    core: Arc<Core>,
    workspace: watch::Receiver<Arc<WorkspaceState>>,
    layout: mpsc::UnboundedSender<layout::LayoutCommand>,
    events: broadcast::Sender<SyncEvent>,
}

impl std::fmt::Debug for Sync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sync")
            .field("mirrors", &lock(&self.core.mirrors).len())
            .finish()
    }
}

impl Sync {
    /// `cache_dir` enables the offline cache; the directory is created on the
    /// first write. Must be called within a Tokio runtime context.
    pub fn new(
        links: watch::Receiver<Option<flc_conn::broker::ControlLink>>,
        cache_dir: Option<std::path::PathBuf>,
        config: Config,
    ) -> Self {
        let core = Arc::new(Core {
            runtime: Handle::current(),
            tasks: Mutex::new(JoinSet::new()),
            mirrors: Mutex::new(HashMap::new()),
            routes: Arc::new(Routes::default()),
            sets: watch::channel(None).0,
            cache: cache_dir.map(Cache::new),
            config,
            sequence: AtomicU64::new(0),
            shutdown: CancellationToken::new(),
        });
        let (events, _) = broadcast::channel(256);
        let (initial, pending) = layout::initial_state(core.cache.as_ref());
        let (workspace_tx, workspace) = watch::channel(Arc::new(initial.clone()));
        let (layout_tx, layout_rx) = mpsc::unbounded_channel();
        let channels_task = channels::run(
            core.routes.clone(),
            links.clone(),
            core.sets.clone(),
            events.clone(),
            core.shutdown.clone(),
        );
        let layout_task = layout::run(
            links,
            layout_rx,
            workspace_tx,
            initial,
            pending,
            events.clone(),
            core.cache.clone(),
            core.config.layout_refresh,
            core.shutdown.clone(),
        );
        {
            let mut tasks = lock(&core.tasks);
            tasks.spawn_on(channels_task, &core.runtime);
            tasks.spawn_on(layout_task, &core.runtime);
        }
        Self {
            core,
            workspace,
            layout: layout_tx,
            events,
        }
    }

    /// One mirror per terminal incarnation, shared by every view. Mirrors
    /// survive link changes; a replaced incarnation ends its mirror.
    pub fn attach(
        &self,
        terminal: TerminalId,
        incarnation: IncarnationId,
        view: ViewId,
    ) -> MirrorHandle {
        let mirrors = lock(&self.core.mirrors);
        if let Some(attachment) = mirrors
            .get(&(terminal, incarnation))
            .and_then(Weak::upgrade)
        {
            return MirrorHandle::new(attachment, view);
        }
        drop(mirrors);
        let attachment = mirror::spawn(&self.core, terminal, incarnation);
        lock(&self.core.mirrors).insert((terminal, incarnation), Arc::downgrade(&attachment));
        MirrorHandle::new(attachment, view)
    }

    pub fn workspace(&self) -> Arc<WorkspaceState> {
        self.workspace.borrow().clone()
    }

    pub fn workspace_changes(&self) -> watch::Receiver<Arc<WorkspaceState>> {
        self.workspace.clone()
    }

    pub fn events(&self) -> broadcast::Receiver<SyncEvent> {
        self.events.subscribe()
    }

    /// Queue a layout operation. It is persisted first, then reconciled with
    /// the daemon; `LayoutTicket` resolves when the daemon answers.
    pub fn apply(&self, op: LayoutOp) -> LayoutTicket {
        let operation = OperationId::new();
        let (reply, result) = tokio::sync::oneshot::channel();
        // The layout task is alive for as long as this handle exists.
        let _ = self.layout.send(layout::LayoutCommand::Apply {
            operation,
            op,
            reply,
        });
        LayoutTicket { operation, result }
    }

    /// Ask for an immediate process/layout refresh (e.g. on window focus).
    pub fn refresh(&self) {
        let _ = self.layout.send(layout::LayoutCommand::Refresh);
    }

    /// Stop all sync work. Mirrors keep their last published frames.
    pub async fn shutdown(&self) {
        self.core.shutdown.cancel();
        let mut tasks = {
            let mut tasks = lock(&self.core.tasks);
            std::mem::replace(&mut *tasks, JoinSet::new())
        };
        while tasks.join_next().await.is_some() {}
    }
}

impl Drop for Sync {
    fn drop(&mut self) {
        self.core.shutdown.cancel();
        lock(&self.core.tasks).abort_all();
    }
}

impl WorkspaceState {
    /// Terminals the UI can attach to, from the last process listing.
    pub fn attachable(&self) -> impl Iterator<Item = &ProcessInfo> {
        self.processes.iter()
    }
}
