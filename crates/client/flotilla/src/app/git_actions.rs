//! The adaptive Git button: which action a worktree needs next, from its
//! git status and its pull request (spec §6, "Adaptive Git action button").
//!
//! Direct actions run a daemon Git/forge call. Agent actions (`task`) are
//! routed through Fast/Thorough routes and prepared or dispatched by the
//! Routes UI. The classifier stays pure so its priority order is testable.
use fl_model::forge::{CheckStatus, ForgeState, MergeMethod, Mergeable, PrState, ReviewDecision};
use fl_model::routing::GitTask;
use fl_model::workspace::GitSummary;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitAction {
    /// Open the manual commit dialog.
    Commit,
    /// Routed: write a commit message, commit and push.
    CommitPush,
    Push,
    /// Push and open a pull request into the target branch.
    CreatePr,
    /// Merge the target branch in (behind it).
    Update,
    MarkReady,
    Merge(MergeMethod),
    /// Routed: resolve conflicts with the target branch.
    Resolve,
    /// Routed: find and fix the failing checks.
    FixCi,
    /// Routed: make the changes reviewers requested.
    FixChanges,
    /// Routed: address unresolved review threads / requested changes.
    FixComments,
    /// Routed: investigate failing checks without changing code.
    Investigate,
    /// Open the checks panel.
    Checks,
    /// Ask the forge for fresh checks and review state.
    RefreshChecks,
    /// Waiting on reviewers.
    AwaitingReview,
    DeleteWorktree,
    /// Nothing to do.
    UpToDate,
}

impl GitAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Commit => "Commit",
            Self::CommitPush => "Commit & push",
            Self::Push => "Push",
            Self::CreatePr => "Create PR",
            Self::Update => "Update branch",
            Self::MarkReady => "Mark ready",
            Self::Merge(MergeMethod::Squash) => "Squash and merge",
            Self::Merge(MergeMethod::Merge) => "Merge",
            Self::Merge(MergeMethod::Rebase) => "Rebase and merge",
            Self::Resolve => "Resolve",
            Self::FixCi => "Fix CI",
            Self::FixChanges => "Fix changes",
            Self::FixComments => "Fix comments",
            Self::Investigate => "Investigate",
            Self::Checks => "Checks",
            Self::RefreshChecks => "Refresh checks",
            Self::AwaitingReview => "Awaiting review",
            Self::DeleteWorktree => "Delete worktree",
            Self::UpToDate => "Up to date",
        }
    }

    /// Whether clicking it does something (the others only inform).
    pub fn actionable(self) -> bool {
        !matches!(self, Self::AwaitingReview | Self::UpToDate)
    }

    /// The routed agent task this action dispatches, if it is an agent action.
    ///
    /// `Commit` and `CreatePr` keep their existing manual dialogs; the new
    /// `Commit & push` and the priority-5 actions dispatch through routes.
    pub fn task(self) -> Option<GitTask> {
        Some(match self {
            Self::CommitPush => GitTask::CommitPush,
            Self::Resolve => GitTask::Resolve,
            Self::FixCi => GitTask::FixCi,
            Self::FixChanges => GitTask::FixChanges,
            Self::FixComments => GitTask::FixComments,
            Self::Investigate => GitTask::Investigate,
            _ => return None,
        })
    }
}

/// What the worktree is, for choosing actions.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub summary: &'a GitSummary,
    /// Changed lines or files against the merge-base with the target.
    pub has_changes: bool,
    /// The worktree is on the project's default branch.
    pub on_base: bool,
    pub is_primary: bool,
    pub forge: Option<&'a ForgeState>,
}

fn open_pr(cx: Context) -> Option<&fl_model::forge::PullRequest> {
    cx.forge
        .and_then(|forge| forge.pr.as_ref())
        .filter(|pr| pr.state == PrState::Open)
}

fn checks_fail(pr: &fl_model::forge::PullRequest) -> bool {
    pr.checks_status() == Some(CheckStatus::Failure)
}

fn checks_pending(pr: &fl_model::forge::PullRequest) -> bool {
    pr.checks_status() == Some(CheckStatus::Pending)
}

/// Reviewers asked for changes, or threads are still unresolved.
fn needs_comment_work(pr: &fl_model::forge::PullRequest) -> bool {
    pr.review == Some(ReviewDecision::ChangesRequested) || pr.unresolved_threads > 0
}

/// The one next step, in the spec's priority order.
pub fn primary(cx: Context) -> GitAction {
    let pr = cx.forge.and_then(|forge| forge.pr.as_ref());
    if pr.is_some_and(|pr| pr.state == PrState::Merged) && !cx.is_primary {
        return GitAction::DeleteWorktree;
    }
    // Conflicted files cannot be committed; resolving is the only next step.
    if cx.summary.conflicted && !cx.on_base {
        return GitAction::Resolve;
    }
    if cx.summary.dirty {
        return if cx.on_base {
            GitAction::CommitPush
        } else {
            GitAction::Commit
        };
    }
    if cx.on_base {
        return if cx.summary.ahead > 0 {
            GitAction::Push
        } else {
            GitAction::UpToDate
        };
    }
    let Some(pr) = open_pr(cx) else {
        return if cx.has_changes || cx.summary.ahead > 0 {
            GitAction::CreatePr
        } else {
            GitAction::UpToDate
        };
    };
    if cx.summary.ahead > 0 || cx.summary.upstream.is_none() {
        return GitAction::Push;
    }
    if pr.mergeable == Mergeable::Conflicting {
        return GitAction::Resolve;
    }
    if checks_fail(pr) {
        return GitAction::FixCi;
    }
    if needs_comment_work(pr) {
        return GitAction::FixComments;
    }
    if checks_pending(pr) {
        return GitAction::Checks;
    }
    if cx.summary.behind > 0 {
        return GitAction::Update;
    }
    if pr.draft {
        return GitAction::MarkReady;
    }
    if pr.review == Some(ReviewDecision::ReviewRequired) {
        return GitAction::AwaitingReview;
    }
    match cx.forge.and_then(|forge| forge.methods.preferred()) {
        Some(method) => GitAction::Merge(method),
        None => GitAction::AwaitingReview,
    }
}

/// Every action that applies now, for the dropdown (primary first).
pub fn available(cx: Context) -> Vec<GitAction> {
    let mut actions = vec![primary(cx)];
    let mut add = |action: GitAction| {
        if !actions.contains(&action) {
            actions.push(action);
        }
    };
    if cx.summary.conflicted && !cx.on_base {
        add(GitAction::Resolve);
    }
    if cx.summary.dirty {
        add(GitAction::Commit);
        add(GitAction::CommitPush);
    }
    if cx.summary.ahead > 0 || (cx.summary.upstream.is_none() && !cx.on_base) {
        add(GitAction::Push);
    }
    if !cx.on_base && !cx.summary.dirty {
        add(GitAction::Update);
    }
    match open_pr(cx) {
        Some(pr) => {
            if pr.draft {
                add(GitAction::MarkReady);
            }
            if pr.mergeable == Mergeable::Conflicting {
                add(GitAction::Resolve);
            }
            if checks_fail(pr) {
                add(GitAction::FixCi);
                // The non-mutating alternative to fixing the checks.
                add(GitAction::Investigate);
            }
            if checks_pending(pr) {
                add(GitAction::Checks);
            }
            if needs_comment_work(pr) {
                add(GitAction::FixComments);
            }
            if pr.review == Some(ReviewDecision::ChangesRequested) {
                add(GitAction::FixChanges);
            }
            if let Some(forge) = cx.forge {
                for method in [MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase] {
                    if forge.methods.allows(method) {
                        add(GitAction::Merge(method));
                    }
                }
            }
            add(GitAction::RefreshChecks);
        }
        None if !cx.on_base && (cx.has_changes || cx.summary.ahead > 0) => {
            add(GitAction::CreatePr);
        }
        None => {}
    }
    if !cx.is_primary {
        add(GitAction::DeleteWorktree);
    }
    // Informational states only appear as the primary label.
    let first = actions[0];
    actions.retain(|action| action.actionable() || *action == first);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::WorktreeId;
    use fl_model::forge::{Check, MergeMethods, PullRequest};

    fn summary(dirty: bool, ahead: u32, upstream: bool) -> GitSummary {
        GitSummary {
            branch: Some("feature".into()),
            upstream: upstream.then(|| "origin/feature".into()),
            ahead,
            dirty,
            ..GitSummary::default()
        }
    }

    fn pr(state: PrState) -> PullRequest {
        PullRequest {
            number: 1,
            url: "https://github.com/a/b/pull/1".into(),
            title: "t".into(),
            state,
            draft: false,
            base: "main".into(),
            head: "feature".into(),
            mergeable: Mergeable::Mergeable,
            review: None,
            checks: vec![Check {
                name: "ci".into(),
                status: CheckStatus::Success,
                url: None,
            }],
            unresolved_threads: 0,
            check_rollup: None,
        }
    }

    fn forge(pr: Option<PullRequest>) -> ForgeState {
        ForgeState {
            worktree: WorktreeId::new(),
            unavailable: None,
            pr,
            methods: MergeMethods {
                squash: true,
                merge: true,
                rebase: false,
            },
            fetched_ms: 0,
        }
    }

    fn cx<'a>(s: &'a GitSummary, changes: bool, forge: Option<&'a ForgeState>) -> Context<'a> {
        Context {
            summary: s,
            has_changes: changes,
            on_base: false,
            is_primary: false,
            forge,
        }
    }

    #[test]
    fn the_task_branch_loop_in_order() {
        // Edits → commit.
        let s = summary(true, 0, false);
        assert_eq!(primary(cx(&s, true, None)), GitAction::Commit);
        // Committed, never pushed, no PR → create PR (which pushes).
        let s = summary(false, 0, false);
        assert_eq!(
            primary(cx(&s, true, Some(&forge(None)))),
            GitAction::CreatePr
        );
        // PR open, checks green, draft → ready.
        let s = summary(false, 0, true);
        let mut open = pr(PrState::Open);
        open.draft = true;
        let f = forge(Some(open.clone()));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::MarkReady);
        // Ready and approved → merge with the preferred method.
        open.draft = false;
        open.review = Some(ReviewDecision::Approved);
        let f = forge(Some(open.clone()));
        assert_eq!(
            primary(cx(&s, true, Some(&f))),
            GitAction::Merge(MergeMethod::Squash)
        );
        // Merged → delete the worktree.
        let f = forge(Some(pr(PrState::Merged)));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::DeleteWorktree);
    }

    #[test]
    fn blockers_on_an_open_pull_request_take_precedence() {
        let s = summary(false, 0, true);
        // Local commits not on the PR yet → push.
        let ahead = summary(false, 2, true);
        let f = forge(Some(pr(PrState::Open)));
        assert_eq!(primary(cx(&ahead, true, Some(&f))), GitAction::Push);
        // Conflicts → resolve with an agent, with the direct update still listed.
        let mut conflicting = pr(PrState::Open);
        conflicting.mergeable = Mergeable::Conflicting;
        conflicting.draft = true;
        let f = forge(Some(conflicting));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::Resolve);
        assert!(available(cx(&s, true, Some(&f))).contains(&GitAction::Update));
        // Failing checks → fix them, and investigate is the alternate.
        let mut failing = pr(PrState::Open);
        failing.checks[0].status = CheckStatus::Failure;
        let f = forge(Some(failing));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::FixCi);
        assert!(available(cx(&s, true, Some(&f))).contains(&GitAction::Investigate));
        // Running checks → show the checks panel.
        let mut pending = pr(PrState::Open);
        pending.checks[0].status = CheckStatus::Pending;
        let f = forge(Some(pending.clone()));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::Checks);
        // Requested changes outrank the informational checks state.
        pending.review = Some(ReviewDecision::ChangesRequested);
        let f = forge(Some(pending));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::FixComments);
        // Changes requested → fix the comments; both agent actions are offered.
        let mut changes = pr(PrState::Open);
        changes.review = Some(ReviewDecision::ChangesRequested);
        let f = forge(Some(changes));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::FixComments);
        let actions = available(cx(&s, true, Some(&f)));
        assert!(actions.contains(&GitAction::FixChanges));
        assert!(actions.contains(&GitAction::FixComments));
        assert!(actions.contains(&GitAction::RefreshChecks));
        // Unresolved threads alone also call for comment work.
        let mut threads = pr(PrState::Open);
        threads.unresolved_threads = 2;
        let f = forge(Some(threads));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::FixComments);
        assert!(!available(cx(&s, true, Some(&f))).contains(&GitAction::FixChanges));
        // Merely waiting on reviewers stays informational.
        let mut waiting = pr(PrState::Open);
        waiting.review = Some(ReviewDecision::ReviewRequired);
        let f = forge(Some(waiting));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::AwaitingReview);
    }

    #[test]
    fn behind_the_base_branch_updates_after_blockers() {
        let mut behind = summary(false, 0, true);
        behind.behind = 3;
        let f = forge(Some(pr(PrState::Open)));
        assert_eq!(primary(cx(&behind, true, Some(&f))), GitAction::Update);
        // A conflict still outranks the update.
        let mut conflicting = pr(PrState::Open);
        conflicting.mergeable = Mergeable::Conflicting;
        let f = forge(Some(conflicting));
        assert_eq!(primary(cx(&behind, true, Some(&f))), GitAction::Resolve);
    }

    #[test]
    fn the_base_branch_and_quiet_branches() {
        let ahead = summary(false, 1, true);
        let mut on_base = cx(&ahead, false, None);
        on_base.on_base = true;
        assert_eq!(primary(on_base), GitAction::Push);
        let s = summary(false, 0, true);
        let mut clean = cx(&s, false, None);
        clean.on_base = true;
        assert_eq!(primary(clean), GitAction::UpToDate);
        // A task branch without changes has nothing to open a PR for.
        let s = summary(false, 0, false);
        assert_eq!(
            primary(cx(&s, false, Some(&forge(None)))),
            GitAction::UpToDate
        );
        // A merged PR on the primary checkout never offers deletion.
        let s = summary(false, 0, true);
        let f = forge(Some(pr(PrState::Merged)));
        let mut primary_checkout = cx(&s, false, Some(&f));
        primary_checkout.is_primary = true;
        assert_ne!(primary(primary_checkout), GitAction::DeleteWorktree);
        assert!(!available(primary_checkout).contains(&GitAction::DeleteWorktree));
    }

    #[test]
    fn a_dirty_base_branch_commits_and_pushes() {
        let dirty = summary(true, 0, true);
        let mut base_cx = cx(&dirty, false, None);
        base_cx.on_base = true;
        assert_eq!(primary(base_cx), GitAction::CommitPush);
        let actions = available(base_cx);
        assert!(actions.contains(&GitAction::Commit));
        assert!(actions.contains(&GitAction::CommitPush));
        // A dirty task branch still commits locally first.
        let dirty = summary(true, 0, false);
        assert_eq!(primary(cx(&dirty, true, None)), GitAction::Commit);
    }

    #[test]
    fn conflicted_files_resolve_before_committing() {
        let mut dirty = summary(true, 0, false);
        dirty.conflicted = true;
        assert_eq!(primary(cx(&dirty, true, None)), GitAction::Resolve);
        let actions = available(cx(&dirty, true, None));
        assert_eq!(actions[0], GitAction::Resolve);
        assert!(actions.contains(&GitAction::Commit));
    }

    #[test]
    fn agent_actions_map_to_routed_tasks_and_direct_ones_stay_local() {
        assert_eq!(GitAction::CommitPush.task(), Some(GitTask::CommitPush));
        assert_eq!(GitAction::Resolve.task(), Some(GitTask::Resolve));
        assert_eq!(GitAction::FixCi.task(), Some(GitTask::FixCi));
        assert_eq!(GitAction::FixChanges.task(), Some(GitTask::FixChanges));
        assert_eq!(GitAction::FixComments.task(), Some(GitTask::FixComments));
        assert_eq!(GitAction::Investigate.task(), Some(GitTask::Investigate));
        for direct in [
            GitAction::Commit,
            GitAction::Push,
            GitAction::CreatePr,
            GitAction::Update,
            GitAction::MarkReady,
            GitAction::Merge(MergeMethod::Squash),
            GitAction::Checks,
            GitAction::RefreshChecks,
            GitAction::DeleteWorktree,
        ] {
            assert_eq!(direct.task(), None, "{direct:?} must not be routed");
        }
    }

    #[test]
    fn the_dropdown_lists_allowed_merge_methods_only() {
        let s = summary(false, 0, true);
        let mut open = pr(PrState::Open);
        open.draft = true;
        let f = forge(Some(open));
        let actions = available(cx(&s, true, Some(&f)));
        assert_eq!(actions[0], GitAction::MarkReady);
        assert!(actions.contains(&GitAction::Merge(MergeMethod::Squash)));
        assert!(actions.contains(&GitAction::Merge(MergeMethod::Merge)));
        assert!(!actions.contains(&GitAction::Merge(MergeMethod::Rebase)));
        assert!(actions.contains(&GitAction::DeleteWorktree));
        assert!(!actions.contains(&GitAction::CreatePr));
    }
}
