//! App-managed reviews: no forge posting and no implicit action dispatch.
use super::*;
use fl_model::review::{ReviewBundle, ReviewMutation, ReviewState};

impl WorkspaceService {
    pub(crate) async fn list_reviews(&self, worktree: WorktreeId) -> Result<ReviewState, RpcError> {
        self.store(move |store| store.database.list_reviews(worktree))
            .await
    }

    /// Revision-checked insertion supports agent-origin findings from the future CLI.
    pub(crate) async fn mutate_review(
        &self,
        worktree: WorktreeId,
        base_rev: u64,
        mutation: ReviewMutation,
    ) -> Result<ReviewState, RpcError> {
        mutation.validate(worktree).map_err(invalid)?;
        let state = self
            .store(move |store| {
                store
                    .database
                    .mutate_review(worktree, base_rev, mutation, now_ms())
            })
            .await?;
        self.publish(DaemonEvent::ReviewChanged {
            worktree,
            rev: state.rev,
        });
        Ok(state)
    }

    pub(crate) async fn pending_review_bundle(
        &self,
        worktree: WorktreeId,
    ) -> Result<ReviewBundle, RpcError> {
        Ok(self.list_reviews(worktree).await?.pending_bundle())
    }
}
