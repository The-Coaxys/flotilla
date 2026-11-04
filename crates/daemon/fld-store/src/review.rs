//! Schema V5: independent app-managed review snapshots, atomically revisioned.
use super::*;
use fl_model::review::{MAX_REVIEW_BYTES, ReviewMutation, ReviewState};
use rusqlite::OptionalExtension;

pub(crate) const SCHEMA_V5: &str = "
    CREATE TABLE reviews (
        worktree BLOB PRIMARY KEY NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
        state BLOB NOT NULL CHECK(length(state) BETWEEN 1 AND 557056)
    ) STRICT;
";

impl Database {
    pub fn list_reviews(&self, worktree: WorktreeId) -> Result<ReviewState> {
        let transaction = self.connection.unchecked_transaction()?;
        let state = load_reviews(&transaction, worktree)?;
        transaction.commit()?;
        Ok(state)
    }

    /// The same service API is used by the native client and future review-add CLI.
    /// `base_rev` binds every mutation to a previously fetched worktree snapshot.
    pub fn mutate_review(
        &mut self,
        worktree: WorktreeId,
        base_rev: u64,
        mutation: ReviewMutation,
        now_ms: u64,
    ) -> Result<ReviewState> {
        mutation
            .validate(worktree)
            .map_err(|e| StoreError::Invalid(e.into()))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut state = load_reviews(&transaction, worktree)?;
        if state.rev != base_rev {
            return Err(StoreError::RevisionConflict {
                expected: state.rev,
                got: base_rev,
            });
        }
        state
            .apply(mutation, now_ms)
            .map_err(|e| StoreError::Invalid(e.into()))?;
        let bytes = postcard::to_stdvec(&state)?;
        if bytes.len() > MAX_REVIEW_BYTES + 32768 {
            return Err(StoreError::Invalid(
                "review snapshot exceeds byte limit".into(),
            ));
        }
        transaction.execute("INSERT INTO reviews(worktree,state) VALUES(?1,?2) ON CONFLICT(worktree) DO UPDATE SET state=excluded.state",
            params![worktree.as_bytes().as_slice(), bytes])?;
        transaction.commit()?;
        Ok(state)
    }
}
fn load_reviews(connection: &Connection, worktree: WorktreeId) -> Result<ReviewState> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM worktrees WHERE id=?1)",
        [worktree.as_bytes().as_slice()],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(StoreError::Missing(format!(
            "worktree {worktree} not found"
        )));
    }
    let bytes: Option<Vec<u8>> = connection
        .query_row(
            "SELECT state FROM reviews WHERE worktree=?1",
            [worktree.as_bytes().as_slice()],
            |r| r.get(0),
        )
        .optional()?;
    let Some(bytes) = bytes else {
        return Ok(ReviewState::empty(worktree));
    };
    if bytes.len() > MAX_REVIEW_BYTES + 32768 {
        return Err(corrupt("oversized review snapshot"));
    }
    let state: ReviewState = postcard::from_bytes(&bytes)?;
    state.validate().map_err(corrupt)?;
    if state.worktree != worktree {
        return Err(corrupt("review worktree identity mismatch"));
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::ProjectId;
    use fl_model::review::{ReviewAnchor, ReviewOrigin, ReviewStatus};
    use fl_model::workspace::{Project, ProjectKind, Worktree};
    fn fixture(db: &mut Database) -> WorktreeId {
        let workspace = db.workspace_tree().unwrap().workspaces[0].id;
        let p = ProjectId::new();
        let w = WorktreeId::new();
        db.add_project(
            Project {
                id: p,
                workspace,
                name: "review".into(),
                path: "/repo".into(),
                kind: ProjectKind::Git,
                default_branch: Some("main".into()),
                worktree_root: "/trees".into(),
                order: 0,
            },
            &[Worktree {
                id: w,
                project: p,
                path: "/repo".into(),
                branch: Some("main".into()),
                target: Some("main".into()),
                label: "main".into(),
                is_primary: true,
                created_ms: 0,
                last_active_ms: 0,
            }],
        )
        .unwrap();
        w
    }
    #[test]
    fn reviews_survive_restart_and_conflicts_are_atomic() {
        let root = tempfile::tempdir().unwrap();
        let mut db = Database::open(root.path()).unwrap();
        let w = fixture(&mut db);
        let first = db
            .mutate_review(
                w,
                0,
                ReviewMutation::Add {
                    anchor: ReviewAnchor {
                        worktree: w,
                        path: "a.rs".into(),
                        lines: None,
                    },
                    origin: ReviewOrigin::User,
                    body: "Fix error".into(),
                },
                10,
            )
            .unwrap();
        let thread = first.threads[0].id;
        assert!(matches!(
            db.mutate_review(w, 0, ReviewMutation::Dismiss { thread }, 11),
            Err(StoreError::RevisionConflict {
                expected: 1,
                got: 0
            })
        ));
        drop(db);
        let mut db = Database::open(root.path()).unwrap();
        assert_eq!(db.list_reviews(w).unwrap(), first);
        let replied = db
            .mutate_review(
                w,
                1,
                ReviewMutation::Reply {
                    thread,
                    origin: ReviewOrigin::Agent,
                    body: "Working".into(),
                },
                12,
            )
            .unwrap();
        assert_eq!(replied.threads[0].comments.len(), 2);
        let fixed = db
            .mutate_review(
                w,
                2,
                ReviewMutation::SetStatus {
                    thread,
                    status: ReviewStatus::Resolved,
                },
                13,
            )
            .unwrap();
        drop(db);
        let mut db = Database::open(root.path()).unwrap();
        assert_eq!(db.list_reviews(w).unwrap(), fixed);
        db.remove_project(db.worktree(w).unwrap().1.id).unwrap();
        assert!(matches!(db.list_reviews(w), Err(StoreError::Missing(_))));
    }
}
