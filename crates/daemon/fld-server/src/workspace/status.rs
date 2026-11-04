//! Live git status: one filesystem watcher per worktree of a git project.
//! Status is recomputed off the async workers after each debounced change
//! (and on the watcher's poll fallback); only changed results are published.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fl_model::WorktreeId;
use fl_model::workspace::{
    DaemonEvent, GitSummary, Project, ProjectKind, Worktree, WorktreeStatus,
};
use fl_proto::RpcError;
use fl_proto::validation::MAX_CHANGED_FILES;
use fld_git::watch::{WatchConfig, Watcher};

use super::{WorkspaceService, blocking, now_ms};

impl WorkspaceService {
    /// Start watching one worktree (idempotent).
    pub(super) fn watch(self: &Arc<Self>, id: WorktreeId) {
        let cancel = self.cancel.child_token();
        {
            let mut watchers = self.watchers.lock().expect("watchers poisoned");
            if watchers.contains_key(&id) {
                return;
            }
            watchers.insert(id, cancel.clone());
        }
        let service = self.clone();
        tokio::spawn(async move {
            service.watch_loop(id, cancel.clone()).await;
            // A loop that ended on its own (not by `unwatch`) frees the slot,
            // so a later `watch` can retry. A cancelled one was already
            // removed and may have been replaced.
            if !cancel.is_cancelled() {
                service
                    .watchers
                    .lock()
                    .expect("watchers poisoned")
                    .remove(&id);
            }
        });
    }

    pub(super) fn unwatch(&self, id: WorktreeId) {
        if let Some(token) = self.watchers.lock().expect("watchers poisoned").remove(&id) {
            token.cancel();
        }
    }

    async fn watch_loop(
        self: &Arc<Self>,
        id: WorktreeId,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let path = match self.worktree(id).await {
            Ok((worktree, _)) => PathBuf::from(worktree.path),
            Err(error) => {
                tracing::debug!(worktree = %id, "not watching: {}", error.message);
                return;
            }
        };
        let dir = path.clone();
        let repo = match blocking(move || fld_git::repo::discover(&dir)).await {
            Ok(Some(repo)) => repo,
            Ok(None) => {
                tracing::warn!(path = %path.display(), "worktree is no longer a git checkout; not watching");
                return;
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), "not watching worktree: {}", error.message);
                return;
            }
        };
        let (watcher, mut signals) = match Watcher::start(
            &path,
            &repo.git_dir,
            &repo.common_dir,
            WatchConfig::default(),
        ) {
            Ok(started) => started,
            Err(error) => {
                tracing::warn!(path = %path.display(), "not watching worktree: {error}");
                return;
            }
        };
        self.refresh(id).await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                signal = signals.recv() => match signal {
                    Some(()) => self.refresh(id).await,
                    None => break,
                },
            }
        }
        drop(watcher);
    }

    /// Recompute and publish one worktree's status if it changed; also keep
    /// the recorded branch in step with the checkout.
    pub(super) async fn refresh(&self, id: WorktreeId) {
        let Ok((worktree, project)) = self.worktree(id).await else {
            return;
        };
        match self.compute_status(&worktree, &project).await {
            Ok(status) => {
                if project.kind == ProjectKind::Git
                    && status.summary.branch != worktree.branch
                    && Path::new(&worktree.path).is_dir()
                {
                    let branch = status.summary.branch.clone();
                    match self
                        .store(move |store| {
                            store.database.set_worktree_branch(id, branch.as_deref())
                        })
                        .await
                    {
                        Ok(Some(rev)) => self.tree_changed(rev),
                        Ok(None) => {}
                        Err(error) => tracing::debug!("recording branch: {}", error.message),
                    }
                }
                self.record(status);
            }
            Err(error) => {
                tracing::debug!(path = %worktree.path, "status failed: {}", error.message)
            }
        }
    }

    /// Cache `status`; publish it when it differs from the previous result.
    fn record(&self, status: WorktreeStatus) {
        let changed = {
            let mut statuses = self.statuses.lock().expect("statuses poisoned");
            let changed = statuses.get(&status.worktree).is_none_or(|previous| {
                previous.summary != status.summary || previous.files != status.files
            });
            if changed {
                statuses.insert(status.worktree, status.clone());
            }
            changed
        };
        if changed {
            self.publish(DaemonEvent::WorktreeStatus(status));
        }
    }

    pub(super) async fn compute_status(
        &self,
        worktree: &Worktree,
        project: &Project,
    ) -> Result<WorktreeStatus, RpcError> {
        let (summary, files) = match project.kind {
            ProjectKind::Folder => (GitSummary::default(), Vec::new()),
            ProjectKind::Git => {
                let path = PathBuf::from(&worktree.path);
                let target = worktree.target.clone();
                blocking(move || {
                    fld_git::status::status(&path, target.as_deref(), MAX_CHANGED_FILES)
                })
                .await?
            }
        };
        Ok(WorktreeStatus {
            worktree: worktree.id,
            summary,
            files,
            at_ms: now_ms(),
        })
    }

    /// A fresh status (published to every session if it changed).
    pub(crate) async fn worktree_status(&self, id: WorktreeId) -> Result<WorktreeStatus, RpcError> {
        let (worktree, project) = self.worktree(id).await?;
        let status = self.compute_status(&worktree, &project).await?;
        self.record(status.clone());
        Ok(status)
    }
}
