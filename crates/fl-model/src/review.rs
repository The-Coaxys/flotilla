//! App-owned findings, independent of forge review APIs. Lines are 1-based inclusive.
use crate::{ReviewCommentId, ReviewThreadId, WorktreeId};
use serde::{Deserialize, Serialize};

pub const MAX_THREADS: usize = 128;
pub const MAX_COMMENTS: usize = 16;
pub const MAX_COMMENT_BYTES: usize = 4096;
pub const MAX_REVIEW_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAnchor {
    pub worktree: WorktreeId,
    /// Canonical repository-relative slash-separated path; never an absolute host path.
    pub path: String,
    pub lines: Option<LineRange>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewOrigin {
    User,
    Agent,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewTriage {
    Undecided,
    ShouldFix,
    Dismissed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStatus {
    Open,
    Pending,
    Fix,
    Resolved,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: ReviewCommentId,
    pub origin: ReviewOrigin,
    pub body: String,
    pub created_ms: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    pub id: ReviewThreadId,
    pub anchor: ReviewAnchor,
    pub origin: ReviewOrigin,
    pub triage: ReviewTriage,
    pub status: ReviewStatus,
    pub comments: Vec<ReviewComment>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewState {
    pub worktree: WorktreeId,
    pub rev: u64,
    pub threads: Vec<ReviewThread>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewMutation {
    Add {
        anchor: ReviewAnchor,
        origin: ReviewOrigin,
        body: String,
    },
    Reply {
        thread: ReviewThreadId,
        origin: ReviewOrigin,
        body: String,
    },
    ShouldFix {
        thread: ReviewThreadId,
    },
    Dismiss {
        thread: ReviewThreadId,
    },
    Reopen {
        thread: ReviewThreadId,
    },
    SetStatus {
        thread: ReviewThreadId,
        status: ReviewStatus,
    },
}
/// Snapshot-bound input for the action router. Creating/copying a bundle never marks it sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewBundle {
    pub worktree: WorktreeId,
    pub rev: u64,
    pub threads: Vec<ReviewThreadId>,
    pub prompt: String,
}

pub fn validate_body(body: &str) -> Result<(), &'static str> {
    if body.trim().is_empty() || body.len() > MAX_COMMENT_BYTES || body.contains('\0') {
        return Err("review comment must contain 1–4096 bytes of nonempty text without NUL");
    }
    Ok(())
}
impl ReviewAnchor {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.path.is_empty()
            || self.path.len() > 4096
            || self.path.contains(['\\', '\0', ':'])
            || self.path.chars().any(char::is_control)
            || self
                .path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err("review anchor must be a normalized relative file path");
        }
        if self
            .lines
            .is_some_and(|r| r.start == 0 || r.end < r.start || r.end > 10_000_000)
        {
            return Err("review lines must be an ordered 1-based inclusive range");
        }
        Ok(())
    }
}
impl ReviewThread {
    pub fn is_pending(&self) -> bool {
        self.status == ReviewStatus::Pending
            && self.triage != ReviewTriage::Dismissed
            && (self.origin == ReviewOrigin::User || self.triage == ReviewTriage::ShouldFix)
    }
}
impl ReviewMutation {
    pub fn validate(&self, worktree: WorktreeId) -> Result<(), &'static str> {
        match self {
            Self::Add { anchor, body, .. } => {
                anchor.validate()?;
                if anchor.worktree != worktree {
                    return Err("review anchor belongs to another worktree");
                }
                validate_body(body)
            }
            Self::Reply { body, .. } => validate_body(body),
            _ => Ok(()),
        }
    }
}
impl ReviewState {
    pub fn empty(worktree: WorktreeId) -> Self {
        Self {
            worktree,
            rev: 0,
            threads: Vec::new(),
        }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.threads.len() > MAX_THREADS {
            return Err("too many review threads");
        }
        let mut bytes = 0;
        for (index, thread) in self.threads.iter().enumerate() {
            thread.anchor.validate()?;
            if thread.anchor.worktree != self.worktree
                || self.threads[..index].iter().any(|t| t.id == thread.id)
            {
                return Err("invalid review thread identity");
            }
            if thread.comments.is_empty() || thread.comments.len() > MAX_COMMENTS {
                return Err("invalid review comment count");
            }
            if thread.triage == ReviewTriage::Dismissed && thread.status != ReviewStatus::Resolved {
                return Err("dismissed review must be resolved");
            }
            bytes += thread.anchor.path.len() + 128;
            for (i, comment) in thread.comments.iter().enumerate() {
                validate_body(&comment.body)?;
                if thread.comments[..i].iter().any(|c| c.id == comment.id) {
                    return Err("duplicate review comment");
                }
                bytes += comment.body.len() + 64;
            }
        }
        if bytes > MAX_REVIEW_BYTES {
            return Err("review state exceeds byte limit");
        }
        Ok(())
    }
    pub fn apply(&mut self, mutation: ReviewMutation, now_ms: u64) -> Result<(), &'static str> {
        mutation.validate(self.worktree)?;
        // Work on a candidate: rejected bounds/transitions never partially mutate state.
        let mut next = self.clone();
        match mutation {
            ReviewMutation::Add {
                anchor,
                origin,
                body,
            } => next.threads.push(ReviewThread {
                id: ReviewThreadId::new(),
                anchor,
                origin,
                triage: ReviewTriage::Undecided,
                status: if origin == ReviewOrigin::User {
                    ReviewStatus::Pending
                } else {
                    ReviewStatus::Open
                },
                comments: vec![ReviewComment {
                    id: ReviewCommentId::new(),
                    origin,
                    body,
                    created_ms: now_ms,
                }],
            }),
            mutation => {
                let id = match &mutation {
                    ReviewMutation::Reply { thread, .. }
                    | ReviewMutation::ShouldFix { thread }
                    | ReviewMutation::Dismiss { thread }
                    | ReviewMutation::Reopen { thread }
                    | ReviewMutation::SetStatus { thread, .. } => *thread,
                    ReviewMutation::Add { .. } => unreachable!(),
                };
                let thread = next
                    .threads
                    .iter_mut()
                    .find(|t| t.id == id)
                    .ok_or("review thread not found")?;
                match mutation {
                    ReviewMutation::Reply { origin, body, .. } => {
                        thread.comments.push(ReviewComment {
                            id: ReviewCommentId::new(),
                            origin,
                            body,
                            created_ms: now_ms,
                        })
                    }
                    ReviewMutation::ShouldFix { .. } => {
                        thread.triage = ReviewTriage::ShouldFix;
                        thread.status = ReviewStatus::Pending;
                    }
                    ReviewMutation::Dismiss { .. } => {
                        thread.triage = ReviewTriage::Dismissed;
                        thread.status = ReviewStatus::Resolved;
                    }
                    ReviewMutation::Reopen { .. } => {
                        thread.triage = ReviewTriage::Undecided;
                        thread.status = if thread.origin == ReviewOrigin::User {
                            ReviewStatus::Pending
                        } else {
                            ReviewStatus::Open
                        };
                    }
                    ReviewMutation::SetStatus { status, .. } => {
                        if thread.triage == ReviewTriage::Dismissed {
                            return Err("reopen dismissed finding before updating its status");
                        }
                        thread.status = status;
                    }
                    ReviewMutation::Add { .. } => unreachable!(),
                }
            }
        }
        next.rev = next.rev.checked_add(1).ok_or("review revision exhausted")?;
        next.validate()?;
        *self = next;
        Ok(())
    }
    pub fn pending_bundle(&self) -> ReviewBundle {
        use std::fmt::Write;
        let mut bundle = ReviewBundle {
            worktree: self.worktree,
            rev: self.rev,
            threads: Vec::new(),
            prompt: String::new(),
        };
        for thread in &self.threads {
            if !thread.is_pending() {
                continue;
            }
            bundle.threads.push(thread.id);
            let _ = writeln!(
                bundle.prompt,
                "Review thread {} — {}{}",
                thread.id,
                thread.anchor.path,
                thread
                    .anchor
                    .lines
                    .map(|r| format!(":{}-{}", r.start, r.end))
                    .unwrap_or_default()
            );
            for comment in &thread.comments {
                let _ = writeln!(bundle.prompt, "{:?}: {}", comment.origin, comment.body);
            }
            bundle.prompt.push('\n');
        }
        bundle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn add(worktree: WorktreeId, origin: ReviewOrigin) -> ReviewMutation {
        ReviewMutation::Add {
            anchor: ReviewAnchor {
                worktree,
                path: "src/main.rs".into(),
                lines: Some(LineRange { start: 2, end: 4 }),
            },
            origin,
            body: "Handle failure".into(),
        }
    }
    #[test]
    fn anchors_reject_traversal_absolute_and_invalid_lines() {
        let mut a = ReviewAnchor {
            worktree: WorktreeId::new(),
            path: "src/a.rs".into(),
            lines: None,
        };
        for path in [
            "/etc/passwd",
            "../a",
            "a/../b",
            "a//b",
            "C:/a",
            "a\\b",
            "a/./b",
            "a\nfile",
        ] {
            a.path = path.into();
            assert!(a.validate().is_err(), "{path}");
        }
        a.path = "src/a.rs".into();
        for r in [
            LineRange { start: 0, end: 1 },
            LineRange { start: 4, end: 2 },
        ] {
            a.lines = Some(r);
            assert!(a.validate().is_err());
        }
    }
    #[test]
    fn triage_replies_status_and_bundle() {
        let w = WorktreeId::new();
        let mut s = ReviewState::empty(w);
        s.apply(add(w, ReviewOrigin::Agent), 1).unwrap();
        let id = s.threads[0].id;
        assert!(s.pending_bundle().threads.is_empty());
        s.apply(ReviewMutation::ShouldFix { thread: id }, 2)
            .unwrap();
        s.apply(
            ReviewMutation::Reply {
                thread: id,
                origin: ReviewOrigin::User,
                body: "Agreed".into(),
            },
            3,
        )
        .unwrap();
        assert!(s.pending_bundle().prompt.contains("Agreed"));
        s.apply(ReviewMutation::Dismiss { thread: id }, 4).unwrap();
        assert!(s.pending_bundle().threads.is_empty());
        assert!(
            s.apply(
                ReviewMutation::SetStatus {
                    thread: id,
                    status: ReviewStatus::Fix
                },
                5
            )
            .is_err()
        );
        s.apply(ReviewMutation::Reopen { thread: id }, 6).unwrap();
        assert_eq!(s.threads[0].status, ReviewStatus::Open);
        s.apply(ReviewMutation::ShouldFix { thread: id }, 7)
            .unwrap();
        s.apply(
            ReviewMutation::SetStatus {
                thread: id,
                status: ReviewStatus::Fix,
            },
            8,
        )
        .unwrap();
        assert!(s.pending_bundle().threads.is_empty());
        s.apply(
            ReviewMutation::SetStatus {
                thread: id,
                status: ReviewStatus::Resolved,
            },
            9,
        )
        .unwrap();
        assert_eq!(s.threads[0].comments.len(), 2);
    }
    #[test]
    fn rejected_mutations_preserve_revision_and_comments() {
        let w = WorktreeId::new();
        let mut s = ReviewState::empty(w);
        assert!(
            s.apply(add(WorktreeId::new(), ReviewOrigin::User), 0)
                .is_err()
        );
        assert_eq!(s.rev, 0);
        s.apply(add(w, ReviewOrigin::User), 1).unwrap();
        assert_eq!(s.pending_bundle().threads.len(), 1);
        let id = s.threads[0].id;
        for n in 1..MAX_COMMENTS {
            s.apply(
                ReviewMutation::Reply {
                    thread: id,
                    origin: ReviewOrigin::Agent,
                    body: format!("Reply {n}"),
                },
                2,
            )
            .unwrap();
        }
        let before = s.clone();
        assert!(
            s.apply(
                ReviewMutation::Reply {
                    thread: id,
                    origin: ReviewOrigin::User,
                    body: "Over limit".into()
                },
                3
            )
            .is_err()
        );
        assert_eq!(s, before);
        assert!(
            s.apply(
                ReviewMutation::Reply {
                    thread: id,
                    origin: ReviewOrigin::User,
                    body: " ".into()
                },
                4
            )
            .is_err()
        );
        assert_eq!(s, before);
        s.rev = u64::MAX;
        let before = s.clone();
        assert!(s.apply(ReviewMutation::Dismiss { thread: id }, 5).is_err());
        assert_eq!(s, before);
    }
}
