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
mod review;
mod workspaces;

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use fl_model::forge::{ForgeState, MergeMethod};
use fl_model::layout::LayoutOp;
use fl_model::sections::{SectionMutation, SectionsState};
use fl_model::workspace::{
    DirListing, FileContent, FileDiff, LayoutScope, Project, ScriptKind, ScriptState, Worktree,
    WorktreeStatus,
};
use fl_model::{
    IncarnationId, OperationId, ProcessInfo, ProjectId, TerminalId, ViewId, WorkspaceId, WorktreeId,
};
pub use fl_proto::{ClipboardTarget, TerminalNotice};
use fl_proto::{Request, Response};
use flc_conn::broker::{ControlLink, RpcCallError};
pub use input::{Discard, DiscardReason};
pub use layout::{LayoutApplyError, LayoutTicket, PendingLayoutOp, WorkspaceState};
pub use mirror::{EndReason, MirrorEvent, MirrorHandle, MirrorState, MirrorStatus};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
pub use workspaces::WorkspacesState;

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
    links: watch::Receiver<Option<ControlLink>>,
    home: LayoutHandle,
    layouts: Mutex<HashMap<LayoutScope, LayoutHandle>>,
    registry: workspaces::LayoutRegistry,
    workspaces: watch::Receiver<Arc<WorkspacesState>>,
    /// Shared with the workspace task, so forge and sections request
    /// results land in the mirror immediately.
    workspaces_tx: Arc<watch::Sender<Arc<WorkspacesState>>>,
    events: broadcast::Sender<SyncEvent>,
}

/// One layout scope (the host's home layout or a worktree's): its published
/// state and its operation queue. Cheap to clone.
#[derive(Clone)]
pub struct LayoutHandle {
    scope: LayoutScope,
    workspace: watch::Receiver<Arc<WorkspaceState>>,
    commands: mpsc::UnboundedSender<layout::LayoutCommand>,
}

impl std::fmt::Debug for LayoutHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutHandle")
            .field("scope", &self.scope)
            .finish()
    }
}

impl LayoutHandle {
    pub fn scope(&self) -> LayoutScope {
        self.scope
    }

    pub fn workspace(&self) -> Arc<WorkspaceState> {
        self.workspace.borrow().clone()
    }

    pub fn workspace_changes(&self) -> watch::Receiver<Arc<WorkspaceState>> {
        self.workspace.clone()
    }

    /// Queue a layout operation. It is persisted first, then reconciled with
    /// the daemon; `LayoutTicket` resolves when the daemon answers.
    pub fn apply(&self, op: LayoutOp) -> LayoutTicket {
        let operation = OperationId::new();
        let (reply, result) = tokio::sync::oneshot::channel();
        // The layout task lives as long as the Sync that created it.
        let _ = self.commands.send(layout::LayoutCommand::Apply {
            operation,
            op,
            reply,
        });
        LayoutTicket { operation, result }
    }

    /// Ask for an immediate process/layout refresh.
    pub fn refresh(&self) {
        let _ = self.commands.send(layout::LayoutCommand::Refresh);
    }
}

/// Why a daemon request could not be answered.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("not connected to the daemon")]
    Offline,
    #[error(transparent)]
    Rpc(#[from] RpcCallError),
    #[error("unexpected daemon reply")]
    Unexpected,
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
        let channels_task = channels::run(
            core.routes.clone(),
            links.clone(),
            core.sets.clone(),
            events.clone(),
            core.shutdown.clone(),
        );
        let registry = workspaces::LayoutRegistry::default();
        let (workspaces_tx, workspaces) = watch::channel(Arc::new(WorkspacesState::default()));
        let workspaces_tx = Arc::new(workspaces_tx);
        let workspaces_task = workspaces::run(
            links.clone(),
            workspaces_tx.clone(),
            registry.clone(),
            core.shutdown.clone(),
        );
        {
            let mut tasks = lock(&core.tasks);
            tasks.spawn_on(channels_task, &core.runtime);
            tasks.spawn_on(workspaces_task, &core.runtime);
        }
        let home = spawn_layout(&core, &links, &events, &registry, LayoutScope::Home);
        Self {
            core,
            links,
            home,
            layouts: Mutex::new(HashMap::new()),
            registry,
            workspaces,
            workspaces_tx,
            events,
        }
    }

    fn remember_forge(&self, state: &ForgeState) {
        workspaces::update(&self.workspaces_tx, |s| {
            s.forges.insert(state.worktree, state.clone());
        });
    }

    fn remember_sections(&self, state: &SectionsState) {
        let links = self.links.borrow();
        if links
            .as_ref()
            .is_some_and(|link| workspaces::supports_sections(link.daemon_proto()))
        {
            workspaces::remember_sections(&self.workspaces_tx, state.clone());
        }
    }

    /// The layout of `scope`; its sync task starts on first use and runs for
    /// the session.
    pub fn layout(&self, scope: LayoutScope) -> LayoutHandle {
        if scope == LayoutScope::Home {
            return self.home.clone();
        }
        lock(&self.layouts)
            .entry(scope)
            .or_insert_with(|| {
                spawn_layout(&self.core, &self.links, &self.events, &self.registry, scope)
            })
            .clone()
    }

    /// Projects, worktrees, live status, scripts and sidebar sections.
    pub fn workspaces(&self) -> Arc<WorkspacesState> {
        self.workspaces.borrow().clone()
    }

    pub fn workspaces_changes(&self) -> watch::Receiver<Arc<WorkspacesState>> {
        self.workspaces.clone()
    }

    /// One request on the current link. Runs on the session's Tokio
    /// runtime, so it can be awaited from any executor (the UI's included).
    pub async fn call(&self, req: Request) -> Result<Response, CallError> {
        let link = self
            .links
            .borrow()
            .clone()
            .filter(|link| !link.is_closed())
            .ok_or(CallError::Offline)?;
        if matches!(&req, Request::ListSections | Request::MutateSections { .. })
            && !workspaces::supports_sections(link.daemon_proto())
        {
            return Err(CallError::Rpc(RpcCallError::Remote(fl_proto::RpcError {
                code: fl_proto::RpcErrorCode::Unsupported,
                message: "sidebar sections require daemon protocol 0.4".into(),
            })));
        }
        self.core
            .runtime
            .spawn(async move { link.rpc(req).await })
            .await
            .map_err(|_| CallError::Offline)?
            .map_err(CallError::from)
    }

    /// Fetch the daemon's section definitions, assignments and pins, and
    /// immediately publish the snapshot unless the mirror is already newer.
    pub async fn list_sections(&self) -> Result<SectionsState, CallError> {
        match self.call(Request::ListSections).await? {
            Response::SectionsState(state) => {
                self.remember_sections(&state);
                Ok(state)
            }
            _ => Err(CallError::Unexpected),
        }
    }

    /// Apply one revision-checked section mutation. Conflicts refresh the
    /// mirror before returning the error; the mutation is never retried.
    pub async fn mutate_sections(
        &self,
        expected_rev: u64,
        mutation: SectionMutation,
    ) -> Result<SectionsState, CallError> {
        match self
            .call(Request::MutateSections {
                expected_rev,
                mutation,
            })
            .await
        {
            Ok(Response::SectionsState(state)) => {
                self.remember_sections(&state);
                Ok(state)
            }
            Ok(_) => Err(CallError::Unexpected),
            Err(error) => {
                if matches!(
                    &error,
                    CallError::Rpc(error)
                        if error.code() == Some(fl_proto::RpcErrorCode::RevisionConflict)
                ) {
                    let _ = self.list_sections().await;
                }
                Err(error)
            }
        }
    }

    pub async fn list_dir(&self, path: String) -> Result<DirListing, CallError> {
        match self.call(Request::ListDir { path }).await? {
            Response::DirListing(listing) => Ok(listing),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn add_project(
        &self,
        workspace: WorkspaceId,
        path: String,
    ) -> Result<Project, CallError> {
        match self.call(Request::AddProject { workspace, path }).await? {
            Response::Project(project) => Ok(project),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn clone_project(
        &self,
        workspace: WorkspaceId,
        url: String,
        dest: String,
    ) -> Result<Project, CallError> {
        match self
            .call(Request::CloneProject {
                workspace,
                url,
                dest,
            })
            .await?
        {
            Response::Project(project) => Ok(project),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn remove_project(&self, project: ProjectId) -> Result<(), CallError> {
        match self.call(Request::RemoveProject { project }).await? {
            Response::Done => Ok(()),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn create_worktree(
        &self,
        project: ProjectId,
        branch: Option<String>,
        base: Option<String>,
    ) -> Result<Worktree, CallError> {
        match self
            .call(Request::CreateWorktree {
                project,
                branch,
                base,
            })
            .await?
        {
            Response::Worktree(worktree) => Ok(worktree),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn delete_worktree(
        &self,
        worktree: WorktreeId,
        force: bool,
    ) -> Result<(), CallError> {
        match self
            .call(Request::DeleteWorktree { worktree, force })
            .await?
        {
            Response::Done => Ok(()),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn worktree_status(&self, worktree: WorktreeId) -> Result<WorktreeStatus, CallError> {
        match self.call(Request::WorktreeStatus { worktree }).await? {
            Response::WorktreeStatus(status) => Ok(status),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn read_file(&self, path: String, max_bytes: u32) -> Result<FileContent, CallError> {
        match self.call(Request::ReadFile { path, max_bytes }).await? {
            Response::FileContent(content) => Ok(content),
            _ => Err(CallError::Unexpected),
        }
    }

    /// Returns the new file's SHA-256. Fails with a revision conflict when
    /// `expected_sha256` no longer matches the file on disk.
    pub async fn write_file(
        &self,
        path: String,
        bytes: Vec<u8>,
        expected_sha256: Option<[u8; 32]>,
    ) -> Result<[u8; 32], CallError> {
        match self
            .call(Request::WriteFile {
                path,
                bytes,
                expected_sha256,
            })
            .await?
        {
            Response::Written(sha) => Ok(sha),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn file_diff(
        &self,
        worktree: WorktreeId,
        path: String,
    ) -> Result<FileDiff, CallError> {
        match self.call(Request::FileDiff { worktree, path }).await? {
            Response::FileDiff(diff) => Ok(diff),
            _ => Err(CallError::Unexpected),
        }
    }

    /// Fetch the worktree's forge state now (and mark it active, so the
    /// daemon polls it every 15 s for a while).
    pub async fn forge_status(&self, worktree: WorktreeId) -> Result<ForgeState, CallError> {
        match self.call(Request::ForgeStatus { worktree }).await? {
            Response::ForgeState(state) => {
                self.remember_forge(&state);
                Ok(state)
            }
            _ => Err(CallError::Unexpected),
        }
    }

    /// Commit (everything with `all`); returns the new HEAD.
    pub async fn git_commit(
        &self,
        worktree: WorktreeId,
        message: String,
        all: bool,
    ) -> Result<String, CallError> {
        match self
            .call(Request::GitCommit {
                worktree,
                message,
                all,
            })
            .await?
        {
            Response::Committed { head } => Ok(head),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn git_push(&self, worktree: WorktreeId) -> Result<(), CallError> {
        match self.call(Request::GitPush { worktree }).await? {
            Response::Done => Ok(()),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn git_update(&self, worktree: WorktreeId) -> Result<(), CallError> {
        match self.call(Request::GitUpdate { worktree }).await? {
            Response::Done => Ok(()),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn create_pull_request(
        &self,
        worktree: WorktreeId,
        title: String,
        body: String,
        draft: bool,
    ) -> Result<ForgeState, CallError> {
        match self
            .call(Request::CreatePullRequest {
                worktree,
                title,
                body,
                draft,
            })
            .await?
        {
            Response::ForgeState(state) => {
                self.remember_forge(&state);
                Ok(state)
            }
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn mark_ready(&self, worktree: WorktreeId) -> Result<ForgeState, CallError> {
        match self
            .call(Request::MarkPullRequestReady { worktree })
            .await?
        {
            Response::ForgeState(state) => {
                self.remember_forge(&state);
                Ok(state)
            }
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn merge_pull_request(
        &self,
        worktree: WorktreeId,
        number: u64,
        method: MergeMethod,
    ) -> Result<ForgeState, CallError> {
        match self
            .call(Request::MergePullRequest {
                worktree,
                number,
                method,
            })
            .await?
        {
            Response::ForgeState(state) => {
                self.remember_forge(&state);
                Ok(state)
            }
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn run_script(
        &self,
        worktree: WorktreeId,
        kind: ScriptKind,
    ) -> Result<ScriptState, CallError> {
        match self.call(Request::RunScript { worktree, kind }).await? {
            Response::ScriptState(state) => Ok(state),
            _ => Err(CallError::Unexpected),
        }
    }

    pub async fn stop_script(
        &self,
        worktree: WorktreeId,
        kind: ScriptKind,
    ) -> Result<ScriptState, CallError> {
        match self.call(Request::StopScript { worktree, kind }).await? {
            Response::ScriptState(state) => Ok(state),
            _ => Err(CallError::Unexpected),
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

    /// The home layout's state (see [`Sync::layout`] for worktrees).
    pub fn workspace(&self) -> Arc<WorkspaceState> {
        self.home.workspace()
    }

    pub fn workspace_changes(&self) -> watch::Receiver<Arc<WorkspaceState>> {
        self.home.workspace_changes()
    }

    pub fn events(&self) -> broadcast::Receiver<SyncEvent> {
        self.events.subscribe()
    }

    /// Queue an operation on the home layout.
    pub fn apply(&self, op: LayoutOp) -> LayoutTicket {
        self.home.apply(op)
    }

    /// Ask every layout for an immediate refresh (e.g. on window focus).
    pub fn refresh(&self) {
        self.home.refresh();
        for handle in lock(&self.layouts).values() {
            handle.refresh();
        }
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

fn spawn_layout(
    core: &Arc<Core>,
    links: &watch::Receiver<Option<ControlLink>>,
    events: &broadcast::Sender<SyncEvent>,
    registry: &workspaces::LayoutRegistry,
    scope: LayoutScope,
) -> LayoutHandle {
    let (initial, pending) = layout::initial_state(core.cache.as_ref(), scope);
    let (state, workspace) = watch::channel(Arc::new(initial.clone()));
    let (commands, receiver) = mpsc::unbounded_channel();
    lock(registry).insert(scope, commands.clone());
    let task = layout::run(
        scope,
        links.clone(),
        receiver,
        state,
        initial,
        pending,
        events.clone(),
        core.cache.clone(),
        core.config.layout_refresh,
        core.shutdown.clone(),
    );
    lock(&core.tasks).spawn_on(task, &core.runtime);
    LayoutHandle {
        scope,
        workspace,
        commands,
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
