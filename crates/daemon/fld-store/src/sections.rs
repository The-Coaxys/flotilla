//! Durable sidebar sections (schema v4). One bounded snapshot and revision
//! commit atomically; worktree removal prunes membership in its own transaction.
use super::*;
use fl_model::WorktreeId;
use fl_model::sections::{SectionKind, SectionMembership, SectionMutation, SectionsState};

const MAX_SECTIONS_BYTES: usize = 512 * 1024;
pub(super) const SCHEMA_V4: &str = "
    CREATE TABLE sections (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        state BLOB NOT NULL CHECK(length(state) BETWEEN 1 AND 524288)
    ) STRICT;
";

impl Database {
    pub fn sections_state(&self) -> Result<SectionsState> {
        let transaction = self.connection.unchecked_transaction()?;
        let state = load_sections(&transaction)?;
        validate_references(&state, &load_tree(&transaction)?)?;
        transaction.commit()?;
        Ok(state)
    }

    /// Compare-and-set every edit so two clients cannot silently overwrite
    /// section edits or resurrect a deleted definition.
    pub fn mutate_sections(
        &mut self,
        expected_rev: u64,
        mutation: SectionMutation,
    ) -> Result<SectionsState> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut state = load_sections(&transaction)?;
        if state.rev != expected_rev {
            return Err(StoreError::RevisionConflict {
                expected: state.rev,
                got: expected_rev,
            });
        }
        let tree = load_tree(&transaction)?;
        match mutation {
            SectionMutation::Upsert(section) => {
                section
                    .validate()
                    .map_err(|e| StoreError::Invalid(e.into()))?;
                if !tree.workspaces.iter().any(|w| w.id == section.workspace) {
                    return Err(StoreError::Missing("section workspace not found".into()));
                }
                if let Some(index) = state.sections.iter().position(|s| s.id == section.id) {
                    let old = &state.sections[index];
                    if old.workspace != section.workspace {
                        return Err(StoreError::Invalid(
                            "a section cannot move between workspaces".into(),
                        ));
                    }
                    if old.kind == SectionKind::Manual && section.kind != SectionKind::Manual {
                        for membership in &mut state.memberships {
                            if membership.manual == Some(section.id) {
                                membership.manual = None;
                            }
                        }
                    }
                    state.sections[index] = section;
                } else {
                    state.sections.push(section);
                }
            }
            SectionMutation::Delete(id) => {
                let index = state
                    .sections
                    .iter()
                    .position(|s| s.id == id)
                    .ok_or_else(|| StoreError::Missing("section not found".into()))?;
                state.sections.remove(index);
                for membership in &mut state.memberships {
                    if membership.manual == Some(id) {
                        membership.manual = None;
                    }
                }
            }
            SectionMutation::Assign { worktree, section } => {
                let workspace = worktree_workspace(&tree, worktree)?;
                if let Some(id) = section
                    && !state.sections.iter().any(|s| {
                        s.id == id && s.workspace == workspace && s.kind == SectionKind::Manual
                    })
                {
                    return Err(StoreError::Invalid(
                        "assignment requires a manual section in the worktree's workspace".into(),
                    ));
                }
                membership(&mut state, worktree).manual = section;
            }
            SectionMutation::Pin { worktree, pinned } => {
                worktree_workspace(&tree, worktree)?;
                membership(&mut state, worktree).pinned = pinned;
            }
        }
        state.memberships.retain(|m| m.pinned || m.manual.is_some());
        state.sections.sort_by_key(|s| (s.workspace, s.order, s.id));
        state.memberships.sort_by_key(|m| m.worktree);
        state
            .validate()
            .map_err(|e| StoreError::Invalid(e.into()))?;
        validate_references(&state, &tree)?;
        bump_and_write(&transaction, &mut state)?;
        transaction.commit()?;
        Ok(state)
    }
}

fn membership(state: &mut SectionsState, worktree: WorktreeId) -> &mut SectionMembership {
    let index = state
        .memberships
        .iter()
        .position(|m| m.worktree == worktree)
        .unwrap_or_else(|| {
            state.memberships.push(SectionMembership {
                worktree,
                manual: None,
                pinned: false,
            });
            state.memberships.len() - 1
        });
    &mut state.memberships[index]
}

fn worktree_workspace(
    tree: &fl_model::workspace::WorkspaceTree,
    id: WorktreeId,
) -> Result<WorkspaceId> {
    let worktree = tree
        .worktrees
        .iter()
        .find(|w| w.id == id)
        .ok_or_else(|| StoreError::Missing(format!("worktree {id} not found")))?;
    tree.projects
        .iter()
        .find(|p| p.id == worktree.project)
        .map(|p| p.workspace)
        .ok_or_else(|| corrupt("section membership worktree has no project"))
}

pub(super) fn validate_references(
    state: &SectionsState,
    tree: &fl_model::workspace::WorkspaceTree,
) -> Result<()> {
    for section in &state.sections {
        if !tree.workspaces.iter().any(|w| w.id == section.workspace) {
            return Err(corrupt("section refers to absent workspace"));
        }
    }
    for membership in &state.memberships {
        let workspace = worktree_workspace(tree, membership.worktree)
            .map_err(|_| corrupt("section membership refers to absent worktree"))?;
        if let Some(id) = membership.manual
            && !state
                .sections
                .iter()
                .any(|s| s.id == id && s.workspace == workspace && s.kind == SectionKind::Manual)
        {
            return Err(corrupt(
                "section assignment crosses workspace or references a non-manual section",
            ));
        }
    }
    Ok(())
}

pub(super) fn load_sections(connection: &Connection) -> Result<SectionsState> {
    let mut statement = connection.prepare("SELECT singleton,state FROM sections")?;
    let mut rows = statement.query([])?;
    let row = rows
        .next()?
        .ok_or_else(|| corrupt("missing section snapshot"))?;
    if row.get::<_, i64>(0)? != 1 {
        return Err(corrupt("invalid section singleton"));
    }
    let state: SectionsState = decode(row_blob(row, 1, MAX_SECTIONS_BYTES)?, "sections")?;
    state.validate().map_err(corrupt)?;
    if state.rev == 0 || rows.next()?.is_some() {
        return Err(corrupt("invalid section revision or extra snapshot"));
    }
    Ok(state)
}

fn bump_and_write(connection: &Connection, state: &mut SectionsState) -> Result<()> {
    state.rev = state
        .rev
        .checked_add(1)
        .ok_or_else(|| corrupt("section revision exhausted"))?;
    let bytes = postcard::to_stdvec(state)?;
    if bytes.len() > MAX_SECTIONS_BYTES {
        return Err(StoreError::Invalid("section snapshot exceeds bound".into()));
    }
    connection.execute("UPDATE sections SET state=?1 WHERE singleton=1", [bytes])?;
    Ok(())
}

pub(super) fn prune_memberships(connection: &Connection, removed: &[WorktreeId]) -> Result<()> {
    let mut state = load_sections(connection)?;
    let before = state.memberships.len();
    state.memberships.retain(|m| !removed.contains(&m.worktree));
    if before != state.memberships.len() {
        bump_and_write(connection, &mut state)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::sections::{Section, SectionFilter};
    use fl_model::workspace::{Project, ProjectKind, Worktree};
    use fl_model::{ProjectId, SectionId};
    fn setup(root: &Path) -> (Database, WorkspaceId, WorktreeId) {
        let mut db = Database::open(root).unwrap();
        let workspace = db.workspace_tree().unwrap().workspaces[0].id;
        let project = Project {
            id: ProjectId::new(),
            workspace,
            name: "Repo".into(),
            path: "/repo".into(),
            kind: ProjectKind::Git,
            default_branch: Some("main".into()),
            worktree_root: "/trees".into(),
            order: 0,
        };
        let worktree = Worktree {
            id: WorktreeId::new(),
            project: project.id,
            path: "/repo".into(),
            branch: Some("main".into()),
            target: None,
            label: "main".into(),
            is_primary: true,
            created_ms: 0,
            last_active_ms: 0,
        };
        db.add_project(project, std::slice::from_ref(&worktree))
            .unwrap();
        (db, workspace, worktree.id)
    }
    #[test]
    fn durable_cas_and_delete_preserves_pin() {
        let root = tempfile::tempdir().unwrap();
        let (mut db, workspace, worktree) = setup(root.path());
        let section = Section {
            id: SectionId::new(),
            workspace,
            name: "Mine".into(),
            order: 3,
            priority: 7,
            kind: SectionKind::Manual,
        };
        let s = db
            .mutate_sections(1, SectionMutation::Upsert(section.clone()))
            .unwrap();
        assert!(matches!(
            db.mutate_sections(1, SectionMutation::Delete(section.id)),
            Err(StoreError::RevisionConflict { .. })
        ));
        let s = db
            .mutate_sections(
                s.rev,
                SectionMutation::Assign {
                    worktree,
                    section: Some(section.id),
                },
            )
            .unwrap();
        let s = db
            .mutate_sections(
                s.rev,
                SectionMutation::Pin {
                    worktree,
                    pinned: true,
                },
            )
            .unwrap();
        drop(db);
        let mut db = Database::open(root.path()).unwrap();
        assert_eq!(db.sections_state().unwrap(), s);
        let s = db
            .mutate_sections(s.rev, SectionMutation::Delete(section.id))
            .unwrap();
        assert!(s.sections.is_empty());
        assert!(s.memberships[0].pinned);
        assert_eq!(s.memberships[0].manual, None);
        db.remove_project(db.workspace_tree().unwrap().projects[0].id)
            .unwrap();
        assert!(db.sections_state().unwrap().memberships.is_empty());
    }
    #[test]
    fn rejects_invalid_edits_atomically() {
        let root = tempfile::tempdir().unwrap();
        let (mut db, workspace, worktree) = setup(root.path());
        let section = Section {
            id: SectionId::new(),
            workspace,
            name: "Filters".into(),
            order: 0,
            priority: 0,
            kind: SectionKind::Filter(SectionFilter::default()),
        };
        let s = db
            .mutate_sections(1, SectionMutation::Upsert(section.clone()))
            .unwrap();
        assert!(
            db.mutate_sections(
                s.rev,
                SectionMutation::Assign {
                    worktree,
                    section: Some(section.id)
                }
            )
            .is_err()
        );
        assert!(
            db.mutate_sections(
                s.rev,
                SectionMutation::Pin {
                    worktree: WorktreeId::new(),
                    pinned: true
                }
            )
            .is_err()
        );
        let mut invalid = section;
        invalid.name.clear();
        assert!(
            db.mutate_sections(s.rev, SectionMutation::Upsert(invalid))
                .is_err()
        );
        assert_eq!(db.sections_state().unwrap(), s);
    }
    #[test]
    fn manual_conversion_and_workspace_boundary() {
        let root = tempfile::tempdir().unwrap();
        let (mut db, workspace, worktree) = setup(root.path());
        let mut section = Section {
            id: SectionId::new(),
            workspace,
            name: "Manual".into(),
            order: 0,
            priority: 0,
            kind: SectionKind::Manual,
        };
        let s = db
            .mutate_sections(1, SectionMutation::Upsert(section.clone()))
            .unwrap();
        let s = db
            .mutate_sections(
                s.rev,
                SectionMutation::Assign {
                    worktree,
                    section: Some(section.id),
                },
            )
            .unwrap();
        let s = db
            .mutate_sections(
                s.rev,
                SectionMutation::Pin {
                    worktree,
                    pinned: true,
                },
            )
            .unwrap();
        section.kind = SectionKind::Filter(SectionFilter::default());
        let s = db
            .mutate_sections(s.rev, SectionMutation::Upsert(section))
            .unwrap();
        assert_eq!(s.memberships[0].manual, None);
        assert!(s.memberships[0].pinned);
        let elsewhere = WorkspaceId::new();
        db.connection
            .execute(
                "INSERT INTO workspaces(id,name,ord) VALUES(?1,'Other',1)",
                [elsewhere.as_bytes().as_slice()],
            )
            .unwrap();
        let foreign = Section {
            id: SectionId::new(),
            workspace: elsewhere,
            name: "Other manual".into(),
            order: 0,
            priority: 0,
            kind: SectionKind::Manual,
        };
        let s = db
            .mutate_sections(s.rev, SectionMutation::Upsert(foreign.clone()))
            .unwrap();
        assert!(
            db.mutate_sections(
                s.rev,
                SectionMutation::Assign {
                    worktree,
                    section: Some(foreign.id)
                }
            )
            .is_err()
        );
        assert_eq!(db.sections_state().unwrap(), s);
    }
    #[test]
    fn migrating_v3_backs_up_and_keeps_tree() {
        let root = tempfile::tempdir().unwrap();
        let (db, _, _) = setup(root.path());
        let tree = db.workspace_tree().unwrap();
        db.connection
            .execute_batch(
                "DROP TABLE IF EXISTS chat_fts; DROP TABLE IF EXISTS chat_search; DROP TABLE IF EXISTS chat_inbox; \
                 DROP TABLE IF EXISTS chat_pending; DROP TABLE IF EXISTS chat_events; \
                 DROP TABLE IF EXISTS chat_sessions; DROP TABLE IF EXISTS chat_meta; \
                 DROP TABLE IF EXISTS routing; DROP TABLE IF EXISTS reviews; DROP TABLE IF EXISTS sections; \
                 PRAGMA user_version=3;",
            )
            .unwrap();
        drop(db);
        let db = Database::open(root.path()).unwrap();
        assert_eq!(db.workspace_tree().unwrap(), tree);
        assert_eq!(db.sections_state().unwrap().rev, 1);
        let prefix = format!("state.db.before-v3-to-v{SCHEMA_VERSION}-");
        let backups: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .collect();
        assert_eq!(backups.len(), 1);
    }
}
