//! Pull requests, checks and reviews as the daemon reads them from the
//! repository's forge (GitHub through the `gh` CLI in M3).
use serde::{Deserialize, Serialize};

use crate::WorktreeId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckStatus {
    Pending,
    Success,
    Failure,
    /// Neutral, skipped or cancelled: neither blocks nor passes.
    Neutral,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeMethods {
    pub squash: bool,
    pub merge: bool,
    pub rebase: bool,
}

impl MergeMethods {
    /// The repository's preferred method: squash, then merge, then rebase.
    pub fn preferred(self) -> Option<MergeMethod> {
        if self.squash {
            Some(MergeMethod::Squash)
        } else if self.merge {
            Some(MergeMethod::Merge)
        } else if self.rebase {
            Some(MergeMethod::Rebase)
        } else {
            None
        }
    }

    pub fn allows(self, method: MergeMethod) -> bool {
        match method {
            MergeMethod::Squash => self.squash,
            MergeMethod::Merge => self.merge,
            MergeMethod::Rebase => self.rebase,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub state: PrState,
    pub draft: bool,
    pub base: String,
    pub head: String,
    pub mergeable: Mergeable,
    pub review: Option<ReviewDecision>,
    pub checks: Vec<Check>,
    pub unresolved_threads: u32,
}

impl PullRequest {
    /// Rolled-up check state: any failure, else any pending, else success
    /// (`None` without checks).
    pub fn checks_status(&self) -> Option<CheckStatus> {
        if self.checks.is_empty() {
            return None;
        }
        let any = |status| self.checks.iter().any(|c| c.status == status);
        Some(if any(CheckStatus::Failure) {
            CheckStatus::Failure
        } else if any(CheckStatus::Pending) {
            CheckStatus::Pending
        } else {
            CheckStatus::Success
        })
    }
}

/// The forge's view of one worktree's branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeState {
    pub worktree: WorktreeId,
    /// Why the forge cannot be used here (no `gh`, not signed in, no
    /// GitHub remote, detached HEAD); `None` when it can.
    pub unavailable: Option<String>,
    /// The newest pull request whose head is this branch.
    pub pr: Option<PullRequest>,
    pub methods: MergeMethods,
    /// Daemon wall clock of the last successful fetch.
    pub fetched_ms: u64,
}
