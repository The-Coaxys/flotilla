//! Layout synchronization against the daemon's authoritative revision.
//!
//! Operations carry stable `OperationId`s and are applied strictly in order,
//! one request at a time. A request whose reply was lost is first repeated
//! exactly (same ID and base revision): the daemon deduplicates a committed
//! operation before checking revisions, so a `RevisionConflict` proves the
//! operation was not committed. Only then is it rebased onto a freshly
//! fetched layout, provided it still applies; otherwise it is rejected with
//! an explicit event. Unacknowledged operations persist in the offline cache
//! and are reconciled the same way after a restart.
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use fl_model::layout::{Layout, LayoutOp};
use fl_model::workspace::LayoutScope;
use fl_model::{OperationId, ProcessInfo};
use fl_proto::{Request, Response, RpcErrorCode};
use flc_conn::broker::{ControlLink, RpcCallError};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::SyncEvent;
use crate::cache::{Cache, PersistedOp};

/// Published workspace state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkspaceState {
    /// Last layout acknowledged by the daemon (or loaded from the cache).
    pub layout: Option<Layout>,
    /// `layout` with every pending operation applied locally, for display.
    pub projected: Option<Layout>,
    /// `layout` came from the offline cache and is not yet confirmed.
    pub stale: bool,
    /// Operations not yet acknowledged, in submission order.
    pub pending: Vec<PendingLayoutOp>,
    /// Terminal processes at the last refresh (incarnations for attaching).
    pub processes: Vec<ProcessInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLayoutOp {
    pub operation: OperationId,
    pub op: LayoutOp,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutApplyError {
    #[error("the operation does not apply to the current layout: {0}")]
    Invalid(String),
    #[error("the daemon rejected the operation: {0}")]
    Rejected(String),
    #[error("the sync client stopped before the operation was acknowledged")]
    Stopped,
}

pub(crate) enum LayoutCommand {
    Apply {
        operation: OperationId,
        op: LayoutOp,
        reply: oneshot::Sender<Result<Layout, LayoutApplyError>>,
    },
    Refresh,
}

/// Resolves when the daemon acknowledged or rejected one submitted operation.
pub struct LayoutTicket {
    pub operation: OperationId,
    pub(crate) result: oneshot::Receiver<Result<Layout, LayoutApplyError>>,
}

impl LayoutTicket {
    pub async fn wait(self) -> Result<Layout, LayoutApplyError> {
        self.result.await.unwrap_or(Err(LayoutApplyError::Stopped))
    }
}

struct Queued {
    persisted: PersistedOp,
    reply: Option<oneshot::Sender<Result<Layout, LayoutApplyError>>>,
}

enum Interrupted {
    LinkLost,
    Shutdown,
}

struct LayoutTask {
    scope: LayoutScope,
    layout: Option<Layout>,
    stale: bool,
    queue: VecDeque<Queued>,
    processes: Vec<ProcessInfo>,
    state: watch::Sender<Arc<WorkspaceState>>,
    events: broadcast::Sender<SyncEvent>,
    cache: Option<Cache>,
    shutdown: CancellationToken,
}

pub(crate) fn initial_state(
    cache: Option<&Cache>,
    scope: LayoutScope,
) -> (WorkspaceState, Vec<PersistedOp>) {
    let (layout, pending) = cache
        .and_then(|cache| cache.load_workspace(scope))
        .unwrap_or((None, Vec::new()));
    let state = WorkspaceState {
        projected: project(layout.as_ref(), pending.iter().map(|p| &p.op)),
        stale: layout.is_some(),
        layout,
        pending: pending
            .iter()
            .map(|p| PendingLayoutOp {
                operation: p.operation,
                op: p.op.clone(),
            })
            .collect(),
        processes: Vec::new(),
    };
    (state, pending)
}

fn project<'a>(layout: Option<&Layout>, ops: impl Iterator<Item = &'a LayoutOp>) -> Option<Layout> {
    let mut projected = layout?.clone();
    for op in ops {
        // An operation that no longer applies is skipped in the projection;
        // reconciliation decides its fate.
        let mut next = projected.clone();
        if next.apply(op.clone()).is_ok() {
            projected = next;
        }
    }
    Some(projected)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    scope: LayoutScope,
    mut links: watch::Receiver<Option<ControlLink>>,
    mut commands: mpsc::UnboundedReceiver<LayoutCommand>,
    state: watch::Sender<Arc<WorkspaceState>>,
    initial: WorkspaceState,
    pending: Vec<PersistedOp>,
    events: broadcast::Sender<SyncEvent>,
    cache: Option<Cache>,
    refresh: Duration,
    shutdown: CancellationToken,
) {
    let mut task = LayoutTask {
        scope,
        layout: initial.layout,
        stale: initial.stale,
        queue: pending
            .into_iter()
            .map(|persisted| Queued {
                persisted,
                reply: None,
            })
            .collect(),
        processes: Vec::new(),
        state,
        events,
        cache,
        shutdown,
    };
    let mut needs_refresh = true;
    loop {
        let link = links.borrow_and_update().clone().filter(|l| !l.is_closed());
        if let Some(link) = &link {
            let synced = async {
                if needs_refresh {
                    task.refresh(link).await?;
                    needs_refresh = false;
                }
                task.drain(link).await
            };
            match synced.await {
                Ok(()) => {}
                Err(Interrupted::Shutdown) => break,
                Err(Interrupted::LinkLost) => needs_refresh = true,
            }
        }
        let periodic = async {
            match &link {
                Some(link) if !link.is_closed() => tokio::time::sleep(refresh).await,
                _ => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = task.shutdown.cancelled() => break,
            command = commands.recv() => match command {
                Some(LayoutCommand::Apply { operation, op, reply }) => {
                    task.queue.push_back(Queued {
                        persisted: PersistedOp { operation, base_rev: None, op },
                        reply: Some(reply),
                    });
                    task.persist().await;
                    task.publish();
                }
                Some(LayoutCommand::Refresh) => needs_refresh = true,
                None => break,
            },
            changed = links.changed() => {
                if changed.is_err() {
                    break;
                }
                needs_refresh = true;
            }
            _ = periodic => needs_refresh = true,
        }
    }
    task.persist().await;
    for queued in task.queue.drain(..) {
        if let Some(reply) = queued.reply {
            let _ = reply.send(Err(LayoutApplyError::Stopped));
        }
    }
}

impl LayoutTask {
    async fn rpc(&self, link: &ControlLink, req: Request) -> Result<Response, RpcCallError> {
        tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => Err(RpcCallError::LinkClosed),
            result = link.rpc(req) => result,
        }
    }

    fn interrupted(&self, link: &ControlLink) -> Interrupted {
        if self.shutdown.is_cancelled() {
            Interrupted::Shutdown
        } else if link.is_closed() {
            Interrupted::LinkLost
        } else {
            // A timeout on a live link: retry after the next wake-up.
            Interrupted::LinkLost
        }
    }

    async fn refresh(&mut self, link: &ControlLink) -> Result<(), Interrupted> {
        let request = match self.scope {
            LayoutScope::Home => Request::GetLayout,
            scope => Request::GetScopedLayout { scope },
        };
        let layout = match self.rpc(link, request).await {
            Ok(Response::Layout(layout)) => layout,
            Ok(_) => return Err(Interrupted::LinkLost),
            Err(_) => return Err(self.interrupted(link)),
        };
        let listed = match self.rpc(link, Request::ListProcesses).await {
            Ok(Response::Processes(processes)) => {
                self.processes = processes;
                true
            }
            _ => false,
        };
        self.accept(layout);
        // Every scope shares one mirror cache: keep the mirrors of terminals
        // the daemon still runs, whichever layout they belong to.
        if listed && let Some(cache) = self.cache.clone() {
            let live: Vec<_> = self.processes.iter().map(|p| p.terminal).collect();
            let _ = tokio::task::spawn_blocking(move || {
                cache.retain_mirrors(|terminal| live.contains(&terminal))
            })
            .await;
        }
        self.persist().await;
        self.publish();
        Ok(())
    }

    /// Adopt a daemon layout unless an older reply overtook a newer one.
    fn accept(&mut self, layout: Layout) {
        let newer = self
            .layout
            .as_ref()
            .is_none_or(|current| self.stale || layout.rev >= current.rev);
        if newer {
            self.layout = Some(layout);
            self.stale = false;
        }
    }

    /// Submit queued operations in order until the queue is empty.
    async fn drain(&mut self, link: &ControlLink) -> Result<(), Interrupted> {
        while let Some(front) = self.queue.front_mut() {
            let base_rev = match front.persisted.base_rev {
                Some(rev) => rev,
                None => {
                    let rev = self.layout.as_ref().map_or(0, |layout| layout.rev);
                    front.persisted.base_rev = Some(rev);
                    self.persist().await;
                    rev
                }
            };
            let front = self.queue.front().expect("checked above");
            let (operation, op) = (front.persisted.operation, front.persisted.op.clone());
            let request = match self.scope {
                LayoutScope::Home => Request::ApplyLayout {
                    operation,
                    base_rev,
                    op,
                },
                scope => Request::ApplyScopedLayout {
                    scope,
                    operation,
                    base_rev,
                    op,
                },
            };
            match self.rpc(link, request).await {
                Ok(Response::Layout(layout)) => {
                    self.accept(layout.clone());
                    // A spawn or close changes the process list; publish it so
                    // the UI can attach the new terminal's incarnation at once.
                    if let Ok(Response::Processes(processes)) =
                        self.rpc(link, Request::ListProcesses).await
                    {
                        self.processes = processes;
                    }
                    let done = self.queue.pop_front().expect("checked above");
                    if let Some(reply) = done.reply {
                        let _ = reply.send(Ok(layout));
                    }
                    self.persist().await;
                    self.publish();
                }
                Ok(_) => {
                    self.reject(LayoutApplyError::Rejected("unexpected reply".into()))
                        .await
                }
                Err(error) if error.code() == Some(RpcErrorCode::RevisionConflict) => {
                    // Not committed: rebase onto the current layout if it still applies.
                    self.refresh(link).await?;
                    let op = self
                        .queue
                        .front()
                        .expect("checked above")
                        .persisted
                        .op
                        .clone();
                    let mut probe = self.layout.clone().ok_or(Interrupted::LinkLost)?;
                    match probe.apply(op) {
                        Ok(_) => {
                            let rev = self.layout.as_ref().map(|layout| layout.rev);
                            self.queue
                                .front_mut()
                                .expect("checked above")
                                .persisted
                                .base_rev = rev;
                            self.persist().await;
                        }
                        Err(error) => {
                            self.reject(LayoutApplyError::Invalid(error.to_string()))
                                .await
                        }
                    }
                }
                Err(RpcCallError::Remote(error)) => {
                    self.reject(LayoutApplyError::Rejected(error.message)).await
                }
                Err(RpcCallError::Timeout(_)) if !link.is_closed() => {
                    // The exact request is repeated; the daemon deduplicates it.
                    if self.shutdown.is_cancelled() {
                        return Err(Interrupted::Shutdown);
                    }
                }
                Err(_) => return Err(self.interrupted(link)),
            }
        }
        Ok(())
    }

    async fn reject(&mut self, error: LayoutApplyError) {
        let Some(rejected) = self.queue.pop_front() else {
            return;
        };
        tracing::warn!(
            operation = %rejected.persisted.operation,
            "layout operation rejected: {error}"
        );
        let _ = self.events.send(SyncEvent::LayoutRejected {
            operation: rejected.persisted.operation,
            op: rejected.persisted.op,
            error: error.clone(),
        });
        if let Some(reply) = rejected.reply {
            let _ = reply.send(Err(error));
        }
        self.persist().await;
        self.publish();
    }

    async fn persist(&self) {
        let Some(cache) = self.cache.clone() else {
            return;
        };
        let layout = self.layout.clone();
        let pending: Vec<PersistedOp> = self.queue.iter().map(|q| q.persisted.clone()).collect();
        let scope = self.scope;
        let saved = tokio::task::spawn_blocking(move || {
            cache.save_workspace(scope, layout.as_ref(), &pending)
        })
        .await;
        if let Ok(Err(error)) = saved {
            tracing::warn!("writing the workspace cache failed: {error}");
        }
    }

    fn publish(&self) {
        let pending: Vec<PendingLayoutOp> = self
            .queue
            .iter()
            .map(|q| PendingLayoutOp {
                operation: q.persisted.operation,
                op: q.persisted.op.clone(),
            })
            .collect();
        let state = WorkspaceState {
            projected: project(self.layout.as_ref(), pending.iter().map(|p| &p.op)),
            layout: self.layout.clone(),
            stale: self.stale,
            pending,
            processes: self.processes.clone(),
        };
        self.state.send_if_modified(|current| {
            if **current == state {
                false
            } else {
                *current = Arc::new(state);
                true
            }
        });
    }
}
