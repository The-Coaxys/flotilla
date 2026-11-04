//! Daemon terminal service: durable layout effects, reconciliation with the
//! independent process host, authoritative workers and logical channels.
pub(crate) mod bulk;
mod channel;
mod prochost;
mod sink;
mod store;
mod worker;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fl_model::layout::{Layout, LayoutOp};
use fl_model::{IncarnationId, OperationId, ProcessInfo, ProcessState, TerminalId};
use fl_prochost_proto::{ErrorCode, Request, Response};
use fl_proto::{
    CHANNEL_SCHEMA, Capabilities, ChannelRole, RpcError, RpcErrorCode, TERMINAL_PROTOCOL,
    TerminalError,
};
use fl_term::{ENGINE_ID, EffectMode};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter};
use fld_store::{LaunchDefaults, StoreError, TerminalEntry, TerminalKey};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::FlotillaHome;
use crate::channels::ChannelLease;
use bulk::Transfers;
use prochost::{Prochost, ProchostError};
use store::StoreHandle;
use worker::Terminal;

const TERMINATE_GRACE_MS: u16 = 2000;
const RETIRE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Shared {
    pub(crate) store: StoreHandle,
    pub(crate) prochost: Prochost,
    pub(crate) transfers: Arc<Transfers>,
    /// Checkpoints taken by this daemon; paces superseded-blob collection.
    pub(crate) checkpoints: std::sync::atomic::AtomicU64,
    /// Cancels every worker without a final checkpoint (crash-equivalent).
    pub(crate) abort: CancellationToken,
}

pub struct TerminalService {
    pub(crate) shared: Arc<Shared>,
    launch: LaunchDefaults,
    terminals: Mutex<HashMap<TerminalId, Arc<Terminal>>>,
    /// Visible launch/restore failures of desired terminals.
    failures: Mutex<HashMap<TerminalId, (IncarnationId, String)>>,
    /// Serializes layout commits with their spawn/close effects.
    layout: tokio::sync::Mutex<()>,
    retiring: Mutex<JoinSet<()>>,
}

fn key_of(entry: &TerminalEntry) -> TerminalKey {
    TerminalKey {
        terminal: entry.spec.terminal,
        incarnation: entry.spec.incarnation,
    }
}

/// Wire text is bounded and NUL-free.
pub(crate) fn bounded(message: String) -> String {
    let mut message = message.replace('\0', " ");
    if message.len() > 4000 {
        let mut end = 4000;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    message
}

fn rpc(code: RpcErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: bounded(message.into()),
    }
}

fn store_rpc(error: StoreError) -> RpcError {
    let code = match &error {
        StoreError::RevisionConflict { .. } => RpcErrorCode::RevisionConflict,
        StoreError::OperationConflict(_) | StoreError::Layout(_) | StoreError::Invalid(_) => {
            RpcErrorCode::InvalidArgument
        }
        StoreError::NotFound(_) => RpcErrorCode::NotFound,
        StoreError::WrongIncarnation => RpcErrorCode::WrongIncarnation,
        _ => RpcErrorCode::Internal,
    };
    rpc(code, error.to_string())
}

fn stopped() -> RpcError {
    rpc(RpcErrorCode::Internal, "terminal state store has stopped")
}

impl TerminalService {
    /// Opens durable state off the async workers, ensures the process host and
    /// reconciles every recorded terminal before any client can attach.
    pub(crate) async fn start(
        home: &FlotillaHome,
        prochost_executable: Option<PathBuf>,
        launch: LaunchDefaults,
    ) -> std::io::Result<Arc<Self>> {
        let store = StoreHandle::open(
            home.state_dir().join("workspace"),
            home.run_dir().join("workspace.lock"),
        )
        .await?;
        let prochost = Prochost::new(home.root().to_path_buf(), prochost_executable);
        prochost.client().await.map_err(std::io::Error::other)?;
        let service = Arc::new(Self {
            shared: Arc::new(Shared {
                store,
                prochost,
                transfers: Arc::default(),
                checkpoints: Default::default(),
                abort: CancellationToken::new(),
            }),
            launch,
            terminals: Mutex::default(),
            failures: Mutex::default(),
            layout: tokio::sync::Mutex::new(()),
            retiring: Mutex::new(JoinSet::new()),
        });
        service.reconcile().await?;
        service.shared.store.submit(collect_garbage);
        Ok(service)
    }

    async fn reconcile(self: &Arc<Self>) -> std::io::Result<()> {
        let entries = self
            .shared
            .store
            .call(|store| store.database.list_terminals())
            .await
            .map_err(std::io::Error::other)?
            .map_err(|error| std::io::Error::other(format!("listing terminals: {error}")))?;
        let processes = match self.shared.prochost.request(&Request::List).await {
            Ok(Response::Processes(processes)) => processes,
            Ok(_) => {
                return Err(std::io::Error::other(
                    "unexpected process-host list response",
                ));
            }
            Err(error) => return Err(std::io::Error::other(error)),
        };
        let mut processes: HashMap<TerminalId, ProcessInfo> = processes
            .into_iter()
            .map(|info| (info.terminal, info))
            .collect();
        for entry in entries {
            let key = key_of(&entry);
            let process = processes.remove(&key.terminal);
            if !entry.desired {
                // A closed intent: finish termination, journal cleanup and removal.
                self.retire_later(key, true);
                continue;
            }
            match process {
                Some(info)
                    if info.incarnation == key.incarnation && info.state != ProcessState::Lost =>
                {
                    self.attach(info).await;
                }
                Some(info) if info.incarnation == key.incarnation => {
                    tracing::warn!(terminal = %key.terminal, "terminal process was lost; relaunching");
                    self.relaunch(key, true).await;
                }
                other => {
                    if let Some(stale) = other {
                        let stale = TerminalKey {
                            terminal: stale.terminal,
                            incarnation: stale.incarnation,
                        };
                        tracing::warn!(terminal = %key.terminal, "retiring stale process-host incarnation");
                        retire(&self.shared, stale, false).await;
                    }
                    if entry.process.is_none() && entry.checkpoint.is_none() {
                        // Committed but never (successfully) spawned: same incarnation.
                        self.launch(entry).await;
                    } else {
                        tracing::warn!(terminal = %key.terminal, "terminal process is missing; relaunching");
                        self.relaunch(key, false).await;
                    }
                }
            }
        }
        for info in processes.into_values() {
            tracing::warn!(terminal = %info.terminal, "retiring process with no recorded terminal");
            self.retire_later(
                TerminalKey {
                    terminal: info.terminal,
                    incarnation: info.incarnation,
                },
                false,
            );
        }
        Ok(())
    }

    fn fail(&self, key: TerminalKey, message: String) {
        tracing::error!(terminal = %key.terminal, "{message}");
        self.failures
            .lock()
            .expect("terminal failures poisoned")
            .insert(key.terminal, (key.incarnation, bounded(message)));
    }

    /// Restore the authoritative engine (checkpoint + journal suffix) for a
    /// retained process incarnation and start its worker.
    async fn attach(self: &Arc<Self>, info: ProcessInfo) {
        let key = TerminalKey {
            terminal: info.terminal,
            incarnation: info.incarnation,
        };
        let persisted = info.clone();
        let restored = self
            .shared
            .store
            .call(move |store| {
                store.database.update_process(&persisted)?;
                store.restore(key, EffectMode::Authoritative)
            })
            .await;
        match restored {
            Ok(Ok((engine, cursor))) => {
                let terminal = worker::spawn(self.shared.clone(), info, engine, cursor);
                self.failures
                    .lock()
                    .expect("terminal failures poisoned")
                    .remove(&key.terminal);
                if let Some(previous) = self
                    .terminals
                    .lock()
                    .expect("terminal registry poisoned")
                    .insert(key.terminal, terminal)
                {
                    previous.abort();
                }
            }
            Ok(Err(error)) => self.fail(key, format!("restoring terminal state failed: {error}")),
            Err(error) => self.fail(key, error.to_string()),
        }
    }

    async fn launch(self: &Arc<Self>, entry: TerminalEntry) {
        let key = key_of(&entry);
        let had_error = entry.launch_error.is_some();
        match self
            .shared
            .prochost
            .request(&Request::Spawn(entry.spec))
            .await
        {
            Ok(Response::Process(info)) => {
                if had_error {
                    self.shared.store.submit(move |store| {
                        let _ = store.database.set_launch_error(key, None);
                    });
                }
                self.attach(info).await;
            }
            Ok(_) => self.fail(key, "unexpected process-host spawn response".into()),
            Err(error) => {
                let message = bounded(format!("terminal launch failed: {error}"));
                let recorded = message.clone();
                self.shared.store.submit(move |store| {
                    if let Err(error) = store.database.set_launch_error(key, Some(&recorded)) {
                        tracing::warn!("recording launch error: {error}");
                    }
                });
                self.fail(key, message);
            }
        }
    }

    /// Replace a lost child with a new incarnation of the same desired terminal.
    async fn relaunch(self: &Arc<Self>, old: TerminalKey, retained: bool) {
        let relaunched = self
            .shared
            .store
            .call(move |store| store.database.relaunch_terminal(old))
            .await;
        match relaunched {
            Ok(Ok(entry)) => {
                if retained {
                    // The process host retains one incarnation per terminal ID.
                    retire(&self.shared, old, false).await;
                }
                self.launch(entry).await;
            }
            Ok(Err(error)) => self.fail(old, format!("terminal relaunch failed: {error}")),
            Err(error) => self.fail(old, error.to_string()),
        }
    }

    fn retire_later(&self, key: TerminalKey, remove_record: bool) {
        let shared = self.shared.clone();
        let mut retiring = self.retiring.lock().expect("retire tasks poisoned");
        while retiring.try_join_next().is_some() {}
        retiring.spawn(async move { retire(&shared, key, remove_record).await });
    }

    async fn close(self: &Arc<Self>, key: TerminalKey) {
        let terminal = {
            let mut terminals = self.terminals.lock().expect("terminal registry poisoned");
            match terminals.get(&key.terminal) {
                Some(found) if found.key == key => terminals.remove(&key.terminal),
                _ => None,
            }
        };
        if let Some(terminal) = terminal {
            terminal.stop(false).await;
        }
        self.failures
            .lock()
            .expect("terminal failures poisoned")
            .remove(&key.terminal);
        // Signal now; waiting for exit, journal cleanup and removal continue
        // in the background (and again at startup if interrupted).
        let _ = self
            .shared
            .prochost
            .request(&Request::Terminate {
                terminal: key.terminal,
                incarnation: key.incarnation,
                grace_ms: TERMINATE_GRACE_MS,
            })
            .await;
        self.retire_later(key, true);
    }

    pub(crate) fn lookup(
        &self,
        terminal: TerminalId,
        incarnation: IncarnationId,
    ) -> Result<Arc<Terminal>, TerminalError> {
        if let Some(found) = self
            .terminals
            .lock()
            .expect("terminal registry poisoned")
            .get(&terminal)
        {
            return if found.key.incarnation == incarnation {
                Ok(found.clone())
            } else {
                Err(TerminalError::WrongIncarnation {
                    current: found.key.incarnation,
                })
            };
        }
        match self
            .failures
            .lock()
            .expect("terminal failures poisoned")
            .get(&terminal)
        {
            Some((current, _)) if *current != incarnation => {
                Err(TerminalError::WrongIncarnation { current: *current })
            }
            Some((_, message)) => Err(TerminalError::Internal {
                message: message.clone(),
            }),
            None => Err(TerminalError::NotFound),
        }
    }

    pub(crate) fn capabilities(&self) -> Result<Capabilities, RpcError> {
        let clock_ns = fld_prochost::clock_ns()
            .map_err(|error| rpc(RpcErrorCode::Internal, format!("boot clock: {error}")))?;
        Ok(Capabilities {
            terminal_protocol: TERMINAL_PROTOCOL,
            engine_id: ENGINE_ID.into(),
            channel_schema: CHANNEL_SCHEMA,
            roles: vec![
                ChannelRole::TerminalHot,
                ChannelRole::TerminalBackground,
                ChannelRole::Bulk,
            ],
            clock_ns,
        })
    }

    pub(crate) async fn get_layout(&self) -> Result<Layout, RpcError> {
        self.shared
            .store
            .call(|store| store.database.default_layout())
            .await
            .map_err(|_| stopped())?
            .map_err(store_rpc)
    }

    /// Commit, then execute the committed spawn/close effects. Effects run in
    /// a detached task: a dropped control session cannot strand them halfway.
    pub(crate) async fn apply_layout(
        self: &Arc<Self>,
        operation: OperationId,
        base_rev: u64,
        op: LayoutOp,
    ) -> Result<Layout, RpcError> {
        let service = self.clone();
        tokio::spawn(async move {
            let _serial = service.layout.lock().await;
            let defaults = service.launch.clone();
            let applied = service
                .shared
                .store
                .call(move |store| {
                    store
                        .database
                        .apply_layout(operation, base_rev, op, &defaults)
                })
                .await
                .map_err(|_| stopped())?
                .map_err(store_rpc)?;
            for key in applied.closed {
                service.close(key).await;
            }
            if let Some(entry) = applied.spawned {
                service.launch(entry).await;
            }
            Ok(applied.layout)
        })
        .await
        .map_err(|error| {
            rpc(
                RpcErrorCode::Internal,
                format!("layout task failed: {error}"),
            )
        })?
    }

    pub(crate) fn process_info(
        &self,
        terminal: TerminalId,
        incarnation: IncarnationId,
    ) -> Result<ProcessInfo, RpcError> {
        match self.lookup(terminal, incarnation) {
            Ok(found) => Ok(found.info()),
            Err(TerminalError::WrongIncarnation { current }) => Err(rpc(
                RpcErrorCode::WrongIncarnation,
                format!("current incarnation is {current}"),
            )),
            Err(TerminalError::Internal { message }) => Err(rpc(RpcErrorCode::Internal, message)),
            Err(_) => Err(rpc(RpcErrorCode::NotFound, "terminal not found")),
        }
    }

    pub(crate) fn list_processes(&self) -> Vec<ProcessInfo> {
        let terminals = self.terminals.lock().expect("terminal registry poisoned");
        let mut list: Vec<_> = terminals.values().map(|terminal| terminal.info()).collect();
        list.sort_by_key(|info| info.terminal);
        list
    }

    pub(crate) async fn serve_channel(
        self: &Arc<Self>,
        lease: ChannelLease,
        reader: FrameReader,
        writer: FrameWriter,
    ) -> Result<(), ConnError> {
        match lease.role {
            ChannelRole::TerminalHot | ChannelRole::TerminalBackground => {
                channel::serve(self.clone(), lease, reader, writer).await
            }
            ChannelRole::Bulk => {
                bulk::serve(self.shared.transfers.clone(), lease, reader, writer).await
            }
        }
    }

    /// Final durable checkpoints, then stop workers and an idle managed host.
    pub(crate) async fn shutdown(&self) {
        let terminals: Vec<_> = self
            .terminals
            .lock()
            .expect("terminal registry poisoned")
            .drain()
            .map(|(_, terminal)| terminal)
            .collect();
        let mut stopping = JoinSet::new();
        for terminal in terminals {
            stopping.spawn(async move { terminal.stop(true).await });
        }
        while stopping.join_next().await.is_some() {}
        let retiring = std::mem::take(&mut *self.retiring.lock().expect("retire tasks poisoned"));
        drop(retiring);
        self.shared.abort.cancel();
        self.shared.prochost.shutdown_if_idle().await;
    }

    /// Crash-equivalent stop used when the daemon handle is dropped.
    pub(crate) fn abort(&self) {
        self.shared.abort.cancel();
        self.retiring
            .lock()
            .expect("retire tasks poisoned")
            .abort_all();
        self.terminals
            .lock()
            .expect("terminal registry poisoned")
            .clear();
    }
}

/// Terminate (if still running), wait for exit and final drain, release the
/// journal, then optionally remove the closed intent.
async fn retire(shared: &Shared, key: TerminalKey, remove_record: bool) {
    let deadline = tokio::time::Instant::now() + RETIRE_TIMEOUT;
    let mut terminated = false;
    loop {
        let info = Request::Info {
            terminal: key.terminal,
            incarnation: key.incarnation,
        };
        match shared.prochost.request(&info).await {
            Ok(Response::Process(info))
                if matches!(info.state, ProcessState::Exited(_) | ProcessState::Lost) =>
            {
                let forget = Request::Forget {
                    terminal: key.terminal,
                    incarnation: key.incarnation,
                };
                match shared.prochost.request(&forget).await {
                    Ok(_) => break,
                    Err(error) if is_gone(&error) => break,
                    Err(error) => tracing::debug!("forget pending: {error}"),
                }
            }
            Ok(_) if !terminated => {
                let terminate = Request::Terminate {
                    terminal: key.terminal,
                    incarnation: key.incarnation,
                    grace_ms: TERMINATE_GRACE_MS,
                };
                terminated = shared.prochost.request(&terminate).await.is_ok();
            }
            Ok(_) => {}
            Err(error) if is_gone(&error) => break,
            Err(error) => tracing::debug!("waiting for terminal exit: {error}"),
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(terminal = %key.terminal, "terminal did not exit; cleanup resumes at next start");
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if remove_record {
        let removed = shared
            .store
            .call(move |store| {
                let removed = store.database.remove_terminal(key);
                if removed.is_ok() {
                    collect_garbage(store);
                }
                removed
            })
            .await;
        match removed {
            Ok(Ok(())) | Ok(Err(StoreError::NotFound(_))) => {}
            Ok(Err(error)) => {
                tracing::warn!(terminal = %key.terminal, "removing closed terminal: {error}")
            }
            Err(error) => tracing::warn!("{error}"),
        }
    }
}

/// Remove checkpoint blobs no longer referenced by any terminal record.
fn collect_garbage(store: &mut fld_store::Store) {
    match store.collect_checkpoint_garbage() {
        Ok(0) => {}
        Ok(removed) => tracing::debug!(removed, "reclaimed superseded terminal checkpoints"),
        Err(error) => tracing::warn!("checkpoint garbage collection failed: {error}"),
    }
}

fn is_gone(error: &ProchostError) -> bool {
    matches!(
        error.code(),
        Some(ErrorCode::NotFound | ErrorCode::WrongIncarnation)
    )
}
