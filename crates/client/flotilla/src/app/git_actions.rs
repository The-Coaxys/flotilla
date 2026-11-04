//! The adaptive Git button: which action a worktree needs next, from its
//! git status and its pull request (spec §6, "Adaptive Git action button").
use fl_model::forge::{CheckStatus, ForgeState, MergeMethod, Mergeable, PrState, ReviewDecision};
use fl_model::workspace::GitSummary;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitAction {
    Commit,
    Push,
    /// Push and open a pull request into the target branch.
    CreatePr,
    /// Merge the target branch in (behind it, or conflicting).
    Update,
    MarkReady,
    Merge(MergeMethod),
    /// Nothing to do but look: running or failing checks, requested changes.
    Checks,
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
            Self::Push => "Push",
            Self::CreatePr => "Create PR",
            Self::Update => "Update branch",
            Self::MarkReady => "Mark ready",
            Self::Merge(MergeMethod::Squash) => "Squash and merge",
            Self::Merge(MergeMethod::Merge) => "Merge",
            Self::Merge(MergeMethod::Rebase) => "Rebase and merge",
            Self::Checks => "Checks",
            Self::AwaitingReview => "Awaiting review",
            Self::DeleteWorktree => "Delete worktree",
            Self::UpToDate => "Up to date",
        }
    }

    /// Whether clicking it does something (the others only inform).
    pub fn actionable(self) -> bool {
        !matches!(self, Self::AwaitingReview | Self::UpToDate)
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

/// The one next step, in the spec's priority order.
pub fn primary(cx: Context) -> GitAction {
    let pr = cx.forge.and_then(|forge| forge.pr.as_ref());
    if pr.is_some_and(|pr| pr.state == PrState::Merged) && !cx.is_primary {
        return GitAction::DeleteWorktree;
    }
    if cx.summary.dirty {
        return GitAction::Commit;
    }
    if cx.on_base {
        return if cx.summary.ahead > 0 {
            GitAction::Push
        } else {
            GitAction::UpToDate
        };
    }
    let open = pr.filter(|pr| pr.state == PrState::Open);
    let Some(pr) = open else {
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
        return GitAction::Update;
    }
    if let Some(CheckStatus::Failure | CheckStatus::Pending) = pr.checks_status() {
        return GitAction::Checks;
    }
    if pr.draft {
        return GitAction::MarkReady;
    }
    match pr.review {
        Some(ReviewDecision::ChangesRequested) => GitAction::Checks,
        Some(ReviewDecision::ReviewRequired) => GitAction::AwaitingReview,
        _ => match cx.forge.and_then(|forge| forge.methods.preferred()) {
            Some(method) => GitAction::Merge(method),
            None => GitAction::AwaitingReview,
        },
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
    if cx.summary.dirty {
        add(GitAction::Commit);
    }
    if cx.summary.ahead > 0 || (cx.summary.upstream.is_none() && !cx.on_base) {
        add(GitAction::Push);
    }
    if !cx.on_base && !cx.summary.dirty {
        add(GitAction::Update);
    }
    let pr = cx.forge.and_then(|forge| forge.pr.as_ref());
    match pr {
        Some(pr) if pr.state == PrState::Open => {
            if pr.draft {
                add(GitAction::MarkReady);
            }
            if let Some(forge) = cx.forge {
                for method in [MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase] {
                    if forge.methods.allows(method) {
                        add(GitAction::Merge(method));
                    }
                }
            }
            add(GitAction::Checks);
        }
        _ if !cx.on_base && (cx.has_changes || cx.summary.ahead > 0) => add(GitAction::CreatePr),
        _ => {}
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
        // Conflicts → update before anything else.
        let mut conflicting = pr(PrState::Open);
        conflicting.mergeable = Mergeable::Conflicting;
        conflicting.draft = true;
        let f = forge(Some(conflicting));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::Update);
        // Failing or running checks → checks.
        let mut failing = pr(PrState::Open);
        failing.checks[0].status = CheckStatus::Failure;
        let f = forge(Some(failing));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::Checks);
        // Reviews.
        let mut changes = pr(PrState::Open);
        changes.review = Some(ReviewDecision::ChangesRequested);
        let f = forge(Some(changes));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::Checks);
        let mut waiting = pr(PrState::Open);
        waiting.review = Some(ReviewDecision::ReviewRequired);
        let f = forge(Some(waiting));
        assert_eq!(primary(cx(&s, true, Some(&f))), GitAction::AwaitingReview);
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
