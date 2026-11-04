//! Review snapshots are part of workspaces_changes; mutation results land immediately.
use super::*;
use fl_model::review::{ReviewBundle, ReviewMutation, ReviewState};

impl Sync {
    pub async fn list_reviews(&self, worktree: WorktreeId) -> Result<ReviewState, CallError> {
        match self.call(Request::ListReviews { worktree }).await? {
            Response::Reviews(state) => {
                self.remember_reviews(&state);
                Ok(state)
            }
            _ => Err(CallError::Unexpected),
        }
    }
    pub async fn mutate_review(
        &self,
        worktree: WorktreeId,
        base_rev: u64,
        mutation: ReviewMutation,
    ) -> Result<ReviewState, CallError> {
        match self
            .call(Request::MutateReview {
                worktree,
                base_rev,
                mutation,
            })
            .await?
        {
            Response::Reviews(state) => {
                self.remember_reviews(&state);
                Ok(state)
            }
            _ => Err(CallError::Unexpected),
        }
    }
    /// A router can pass this exact snapshot-bound bundle to current/new/background chat.
    /// Bundle generation does not dispatch or change review status.
    pub async fn pending_review_bundle(
        &self,
        worktree: WorktreeId,
    ) -> Result<ReviewBundle, CallError> {
        match self.call(Request::PendingReviewBundle { worktree }).await? {
            Response::ReviewBundle(bundle) => Ok(bundle),
            _ => Err(CallError::Unexpected),
        }
    }
    fn remember_reviews(&self, state: &ReviewState) {
        workspaces::update(&self.workspaces_tx, |s| remember(s, state.clone()));
    }
}
pub(crate) fn remember(state: &mut WorkspacesState, reviews: ReviewState) {
    if state
        .reviews
        .get(&reviews.worktree)
        .is_none_or(|known| known.rev <= reviews.rev)
    {
        state.reviews.insert(reviews.worktree, reviews);
    }
}
pub(crate) async fn fetch(
    link: &ControlLink,
    state: &watch::Sender<Arc<WorkspacesState>>,
    worktree: WorktreeId,
) {
    let proto = link.daemon_proto();
    if proto.major == 0 && proto.minor < 4 {
        return;
    }
    if let Ok(Response::Reviews(reviews)) = link.rpc(Request::ListReviews { worktree }).await {
        workspaces::update(state, |s| remember(s, reviews));
    }
}
pub(crate) async fn fetch_all(
    link: &ControlLink,
    state: &watch::Sender<Arc<WorkspacesState>>,
    ids: &[WorktreeId],
) {
    use futures::StreamExt;
    futures::stream::iter(ids.iter().copied())
        .for_each_concurrent(8, |id| fetch(link, state, id))
        .await;
}
