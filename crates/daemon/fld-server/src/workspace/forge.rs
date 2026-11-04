//! Git actions (commit, push, update) and the forge (pull requests,
//! checks, reviews, merge) for worktrees, with polling that keeps the
//! forge state current: every 15 s for worktrees a client looked at in the
//! last two minutes, every 60 s for the rest.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fl_model::WorktreeId;
use fl_model::forge::{ForgeState, MergeMethod, MergeMethods, PrState};
use fl_model::workspace::{DaemonEvent, ProjectKind, Worktree};
use fl_proto::{RpcError, RpcErrorCode};
use fld_forge::{ForgeError, GitHub};

use super::{WorkspaceService, blocking, internal, invalid, now_ms, rpc};

const ACTIVE_POLL: Duration = Duration::from_secs(15);
const IDLE_POLL: Duration = Duration::from_secs(60);
/// A worktree counts as active this long after a client asked about it.
const ACTIVE_WINDOW: Duration = Duration::from_secs(120);
/// After a rate limit or outage, wait this long before polling again.
const BACKOFF: Duration = Duration::from_secs(300);

#[derive(Default)]
pub(crate) struct ForgeCache {
    states: HashMap<WorktreeId, ForgeState>,
    fetched: HashMap<WorktreeId, Instant>,
    requested: HashMap<WorktreeId, Instant>,
    backoff_until: Option<Instant>,
}

fn forge_rpc(error: ForgeError) -> RpcError {
    match error {
        ForgeError::Unavailable(message) => rpc(RpcErrorCode::Unsupported, message),
        ForgeError::Command { stderr, .. } => rpc(RpcErrorCode::InvalidArgument, stderr),
        ForgeError::Parse(message) => internal(message),
    }
}

/// Run blocking forge work off the async workers.
async fn forge_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ForgeError> + Send + 'static,
) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| internal(format!("background task failed: {error}")))?
        .map_err(forge_rpc)
}

/// What the forge needs about a worktree, read fresh from git.
struct Target {
    path: PathBuf,
    branch: Option<String>,
    /// The branch pull requests go into.
    base: Option<String>,
    github: Option<GitHub>,
}

impl WorkspaceService {
    async fn target(&self, id: WorktreeId) -> Result<(Worktree, Target), RpcError> {
        let (worktree, project) = self.worktree(id).await?;
        if project.kind != ProjectKind::Git {
            return Err(invalid("a plain folder has no git branch"));
        }
        let path = PathBuf::from(&worktree.path);
        let base = worktree.target.clone().or(project.default_branch.clone());
        let probe = path.clone();
        let (branch, repo) = blocking(move || {
            Ok((
                fld_git::repo::current_branch(&probe)?,
                fld_git::ops::github_repository(&probe)?,
            ))
        })
        .await?;
        let github = repo.map(|(owner, name)| GitHub::new(fld_forge::gh_path(), owner, name));
        Ok((
            worktree,
            Target {
                path,
                branch,
                base,
                github,
            },
        ))
    }

    // --- Git actions --------------------------------------------------------

    pub(crate) async fn git_commit(
        self: &Arc<Self>,
        id: WorktreeId,
        message: String,
        all: bool,
    ) -> Result<String, RpcError> {
        let (_, target) = self.target(id).await?;
        let path = target.path;
        let head = blocking(move || fld_git::ops::commit(&path, &message, all)).await?;
        self.refresh(id).await;
        Ok(head)
    }

    pub(crate) async fn git_push(self: &Arc<Self>, id: WorktreeId) -> Result<(), RpcError> {
        let (_, target) = self.target(id).await?;
        let path = target.path;
        blocking(move || fld_git::ops::push(&path)).await?;
        self.refresh(id).await;
        self.spawn_forge_refresh(id);
        Ok(())
    }

    pub(crate) async fn git_update(self: &Arc<Self>, id: WorktreeId) -> Result<(), RpcError> {
        let (_, target) = self.target(id).await?;
        let base = target
            .base
            .ok_or_else(|| invalid("this worktree has no target branch to update from"))?;
        let path = target.path;
        blocking(move || fld_git::ops::update(&path, &base)).await?;
        self.refresh(id).await;
        Ok(())
    }

    // --- Forge ----------------------------------------------------------------

    /// Fetch, cache and (when changed) publish the forge state.
    pub(crate) async fn forge_status(
        self: &Arc<Self>,
        id: WorktreeId,
    ) -> Result<ForgeState, RpcError> {
        self.forge
            .lock()
            .expect("forge cache poisoned")
            .requested
            .insert(id, Instant::now());
        self.fetch_forge(id).await
    }

    async fn fetch_forge(self: &Arc<Self>, id: WorktreeId) -> Result<ForgeState, RpcError> {
        if self.forge_backed_off() {
            return Err(internal(
                "GitHub rate limit backoff is active; retry after five minutes",
            ));
        }
        let (_, target) = self.target(id).await?;
        let unavailable = |reason: &str| ForgeState {
            worktree: id,
            unavailable: Some(reason.to_string()),
            pr: None,
            methods: MergeMethods::default(),
            fetched_ms: now_ms(),
        };
        let state = match (&target.github, &target.branch) {
            (None, _) => unavailable("origin is not a GitHub repository"),
            (_, None) => unavailable("the worktree is on a detached HEAD"),
            (Some(github), Some(branch)) => {
                let (github, branch) = (github.clone(), branch.clone());
                match forge_blocking(move || github.fetch(&branch)).await {
                    Ok((pr, methods)) => ForgeState {
                        worktree: id,
                        unavailable: None,
                        pr,
                        methods,
                        fetched_ms: now_ms(),
                    },
                    Err(error) if error.code == RpcErrorCode::Unsupported => {
                        unavailable(&error.message)
                    }
                    Err(error) => {
                        if error.message.contains("rate limit") {
                            self.forge
                                .lock()
                                .expect("forge cache poisoned")
                                .backoff_until = Some(Instant::now() + BACKOFF);
                        }
                        return Err(error);
                    }
                }
            }
        };
        self.record_forge(state.clone());
        Ok(state)
    }

    fn record_forge(&self, state: ForgeState) {
        let id = state.worktree;
        let changed = {
            let mut cache = self.forge.lock().expect("forge cache poisoned");
            cache.fetched.insert(id, Instant::now());
            let same = cache.states.get(&id).is_some_and(|known| {
                ForgeState {
                    fetched_ms: state.fetched_ms,
                    ..known.clone()
                } == state
            });
            cache.states.insert(id, state.clone());
            !same
        };
        if changed {
            self.publish(DaemonEvent::ForgeChanged(state));
        }
    }

    fn spawn_forge_refresh(self: &Arc<Self>, id: WorktreeId) {
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(error) = service.fetch_forge(id).await {
                tracing::debug!(worktree = %id, "forge refresh failed: {}", error.message);
            }
        });
    }

    async fn pull_request(
        self: &Arc<Self>,
        id: WorktreeId,
    ) -> Result<(GitHub, u64, MergeMethods), RpcError> {
        let state = self.fetch_forge(id).await?;
        if let Some(reason) = state.unavailable {
            return Err(rpc(RpcErrorCode::Unsupported, reason));
        }
        let pr = state
            .pr
            .filter(|pr| pr.state == PrState::Open)
            .ok_or_else(|| invalid("this branch has no open pull request"))?;
        let (_, target) = self.target(id).await?;
        let github = target
            .github
            .expect("a forge state implies a GitHub origin");
        Ok((github, pr.number, state.methods))
    }

    pub(crate) async fn create_pull_request(
        self: &Arc<Self>,
        id: WorktreeId,
        title: String,
        body: String,
        draft: bool,
    ) -> Result<ForgeState, RpcError> {
        let (_, target) = self.target(id).await?;
        let github = target.github.ok_or_else(|| {
            rpc(
                RpcErrorCode::Unsupported,
                "origin is not a GitHub repository",
            )
        })?;
        let branch = target
            .branch
            .ok_or_else(|| invalid("cannot open a pull request from a detached HEAD"))?;
        let base = target
            .base
            .ok_or_else(|| invalid("this worktree has no target branch"))?;
        if branch == base {
            return Err(invalid(format!(
                "the worktree is on {base} itself; create a task branch first"
            )));
        }
        // The branch must exist on the remote first.
        let path = target.path.clone();
        blocking(move || fld_git::ops::push(&path)).await?;
        let cwd = target.path;
        forge_blocking(move || github.create(&cwd, &base, &branch, &title, &body, draft)).await?;
        self.refresh(id).await;
        self.fetch_forge(id).await
    }

    pub(crate) async fn mark_ready(
        self: &Arc<Self>,
        id: WorktreeId,
    ) -> Result<ForgeState, RpcError> {
        let (github, number, _) = self.pull_request(id).await?;
        forge_blocking(move || github.mark_ready(number)).await?;
        self.fetch_forge(id).await
    }

    pub(crate) async fn merge_pull_request(
        self: &Arc<Self>,
        id: WorktreeId,
        expected_number: u64,
        method: MergeMethod,
    ) -> Result<ForgeState, RpcError> {
        let (github, number, methods) = self.pull_request(id).await?;
        if number != expected_number {
            return Err(rpc(
                RpcErrorCode::RevisionConflict,
                "the branch's pull request changed; refresh and confirm the merge again",
            ));
        }
        if !methods.allows(method) {
            return Err(invalid("the repository does not allow that merge method"));
        }
        forge_blocking(move || github.merge(number, method)).await?;
        self.fetch_forge(id).await
    }

    /// Poll forge state for every git worktree with an upstream branch.
    pub(crate) fn start_forge_polling(self: &Arc<Self>) {
        let service = self.clone();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(ACTIVE_POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tick.tick() => {}
                }
                if service.forge_backed_off() {
                    continue;
                }
                let Ok(tree) = service.tree().await else {
                    continue;
                };
                for worktree in &tree.worktrees {
                    if service.forge_backed_off() {
                        break;
                    }
                    let git = tree
                        .projects
                        .iter()
                        .any(|p| p.id == worktree.project && p.kind == ProjectKind::Git);
                    if !git
                        || !service.forge_due(worktree.id)
                        || !has_upstream(&worktree.path).await
                    {
                        continue;
                    }
                    if let Err(error) = service.fetch_forge(worktree.id).await {
                        tracing::debug!(worktree = %worktree.id, "forge poll failed: {}", error.message);
                    }
                    if cancel.is_cancelled() {
                        return;
                    }
                }
            }
        });
    }

    fn forge_backed_off(&self) -> bool {
        self.forge
            .lock()
            .expect("forge cache poisoned")
            .backoff_until
            .is_some_and(|until| Instant::now() < until)
    }

    fn forge_due(&self, id: WorktreeId) -> bool {
        let cache = self.forge.lock().expect("forge cache poisoned");
        let now = Instant::now();
        let active = cache
            .requested
            .get(&id)
            .is_some_and(|at| now.duration_since(*at) < ACTIVE_WINDOW);
        let interval = if active { ACTIVE_POLL } else { IDLE_POLL };
        cache
            .fetched
            .get(&id)
            .is_none_or(|at| now.duration_since(*at) >= interval - Duration::from_millis(500))
    }

    pub(crate) fn forget_forge(&self, id: WorktreeId) {
        let mut cache = self.forge.lock().expect("forge cache poisoned");
        cache.states.remove(&id);
        cache.fetched.remove(&id);
        cache.requested.remove(&id);
    }
}

/// Branches never pushed cannot have a pull request; skip them when polling.
async fn has_upstream(path: &str) -> bool {
    let path = Path::new(path).to_path_buf();
    tokio::task::spawn_blocking(move || fld_git::ops::upstream(&path).ok().flatten().is_some())
        .await
        .unwrap_or(false)
}
