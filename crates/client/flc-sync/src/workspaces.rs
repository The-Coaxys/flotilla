//! The daemon's workspaces as the client sees them: the project/worktree
//! tree, live git status per worktree and script states, kept current from
//! pushed daemon events and refetched on every new link.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fl_model::WorktreeId;
use fl_model::forge::ForgeState;
use fl_model::workspace::{DaemonEvent, LayoutScope, ScriptState, WorkspaceTree, WorktreeStatus};
use fl_proto::{Request, Response};
use flc_conn::broker::ControlLink;
use futures::StreamExt;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::layout::LayoutCommand;

/// Concurrent per-worktree status/script requests after a (re)connect.
const FETCH_CONCURRENCY: usize = 8;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspacesState {
    /// The last tree the daemon reported; kept while offline.
    pub tree: Option<WorkspaceTree>,
    pub statuses: HashMap<WorktreeId, WorktreeStatus>,
    pub scripts: HashMap<WorktreeId, Vec<ScriptState>>,
    /// Forge state of worktrees the daemon has reported (pushed when it
    /// changes; request [`crate::Sync::forge_status`] for a fresh one).
    pub forges: HashMap<WorktreeId, ForgeState>,
    /// Connected and following daemon events.
    pub live: bool,
}

impl WorkspacesState {
    pub fn worktree(&self, id: WorktreeId) -> Option<&fl_model::workspace::Worktree> {
        self.tree.as_ref()?.worktrees.iter().find(|w| w.id == id)
    }
}

pub(crate) type LayoutRegistry =
    Arc<Mutex<HashMap<LayoutScope, mpsc::UnboundedSender<LayoutCommand>>>>;

pub(crate) async fn run(
    mut links: watch::Receiver<Option<ControlLink>>,
    state: Arc<watch::Sender<Arc<WorkspacesState>>>,
    layouts: LayoutRegistry,
    shutdown: CancellationToken,
) {
    loop {
        let link = links.borrow_and_update().clone().filter(|l| !l.is_closed());
        if let Some(link) = link {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                () = follow(&link, &state, &layouts) => {}
                changed = links.changed() => if changed.is_err() { return },
            }
        } else {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                changed = links.changed() => if changed.is_err() { return },
            }
        }
        update(&state, |s| s.live = false);
    }
}

pub(crate) fn update(
    state: &watch::Sender<Arc<WorkspacesState>>,
    f: impl FnOnce(&mut WorkspacesState),
) {
    state.send_if_modified(|current| {
        let mut next = (**current).clone();
        f(&mut next);
        if next == **current {
            false
        } else {
            *current = Arc::new(next);
            true
        }
    });
}

/// Follow one link until it closes.
async fn follow(
    link: &ControlLink,
    state: &watch::Sender<Arc<WorkspacesState>>,
    layouts: &LayoutRegistry,
) {
    // Subscribe first so nothing that happens during the fetch is missed.
    let mut events = link.events();
    if !resync(link, state).await {
        // An older daemon without workspaces: nothing to follow.
        link.closed().await;
        return;
    }
    loop {
        let event = tokio::select! {
            () = link.closed() => return,
            event = events.recv() => event,
        };
        match event {
            Ok(DaemonEvent::WorkspaceTreeChanged { rev }) => {
                let current = state.borrow().tree.as_ref().map(|tree| tree.rev);
                if current.is_none_or(|current| rev > current) {
                    refetch_tree(link, state).await;
                }
            }
            Ok(DaemonEvent::WorktreeStatus(status)) => {
                update(state, |s| {
                    s.statuses.insert(status.worktree, status);
                });
            }
            Ok(DaemonEvent::ScriptChanged(script)) => update(state, |s| {
                let list = s.scripts.entry(script.worktree).or_default();
                match list.iter_mut().find(|known| known.kind == script.kind) {
                    Some(known) => *known = script,
                    None => list.push(script),
                }
            }),
            Ok(DaemonEvent::ForgeChanged(forge)) => update(state, |s| {
                s.forges.insert(forge.worktree, forge);
            }),
            Ok(DaemonEvent::LayoutChanged { scope, .. }) => {
                if let Some(commands) = layouts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&scope)
                {
                    let _ = commands.send(LayoutCommand::Refresh);
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                resync(link, state).await;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn fetch_tree(link: &ControlLink) -> Option<WorkspaceTree> {
    match link.rpc(Request::GetWorkspaceTree).await {
        Ok(Response::WorkspaceTree(tree)) => Some(tree),
        _ => None,
    }
}

/// Fetch the tree plus every worktree's status and scripts.
async fn resync(link: &ControlLink, state: &watch::Sender<Arc<WorkspacesState>>) -> bool {
    let Some(tree) = fetch_tree(link).await else {
        return false;
    };
    let ids: Vec<WorktreeId> = tree.worktrees.iter().map(|w| w.id).collect();
    let (statuses, scripts) = fetch_details(link, &ids).await;
    update(state, |s| {
        s.tree = Some(tree);
        s.statuses = statuses;
        s.scripts = scripts;
        s.live = true;
    });
    true
}

/// A structural change: adopt the new tree, fetch details for worktrees we
/// have none for, and forget removed ones.
async fn refetch_tree(link: &ControlLink, state: &watch::Sender<Arc<WorkspacesState>>) {
    let Some(tree) = fetch_tree(link).await else {
        return;
    };
    let known = state.borrow().statuses.keys().copied().collect::<Vec<_>>();
    let fresh: Vec<WorktreeId> = tree
        .worktrees
        .iter()
        .map(|w| w.id)
        .filter(|id| !known.contains(id))
        .collect();
    let (statuses, scripts) = fetch_details(link, &fresh).await;
    update(state, |s| {
        // Never let a slow reply replace a newer tree.
        if s.tree
            .as_ref()
            .is_some_and(|current| current.rev > tree.rev)
        {
            return;
        }
        let live: Vec<WorktreeId> = tree.worktrees.iter().map(|w| w.id).collect();
        s.statuses.retain(|id, _| live.contains(id));
        s.scripts.retain(|id, _| live.contains(id));
        s.forges.retain(|id, _| live.contains(id));
        s.statuses.extend(statuses);
        s.scripts.extend(scripts);
        s.tree = Some(tree);
    });
}

type Details = (
    HashMap<WorktreeId, WorktreeStatus>,
    HashMap<WorktreeId, Vec<ScriptState>>,
);

async fn fetch_details(link: &ControlLink, ids: &[WorktreeId]) -> Details {
    let results: Vec<_> = futures::stream::iter(ids.iter().copied())
        .map(|worktree| async move {
            let status = match link.rpc(Request::WorktreeStatus { worktree }).await {
                Ok(Response::WorktreeStatus(status)) => Some(status),
                _ => None,
            };
            let scripts = match link.rpc(Request::ListScripts { worktree }).await {
                Ok(Response::Scripts(scripts)) => Some(scripts),
                _ => None,
            };
            (worktree, status, scripts)
        })
        .buffer_unordered(FETCH_CONCURRENCY)
        .collect()
        .await;
    let mut statuses = HashMap::new();
    let mut scripts = HashMap::new();
    for (id, status, list) in results {
        if let Some(status) = status {
            statuses.insert(id, status);
        }
        if let Some(list) = list {
            scripts.insert(id, list);
        }
    }
    (statuses, scripts)
}
