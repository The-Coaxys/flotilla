//! Durable branch groups (schema v10).
//!
//! A group is one root worktree plus its children. Membership is stored
//! separately from the M2 worktree row so the wire shape of `Worktree` stays
//! unchanged; removing a worktree prunes its membership, and removing the root
//! drops the group so the remaining children become standalone worktrees.
use super::*;
use fl_model::group::{
    BranchGroup, MAX_GROUP_BASE_BYTES, MAX_GROUP_LABEL_BYTES, MAX_GROUP_MEMBERS,
};
use fl_model::workspace::Worktree;

pub(super) const SCHEMA_V10: &str = "
    CREATE TABLE worktree_groups (
        root BLOB PRIMARY KEY NOT NULL CHECK(length(root) = 16),
        project BLOB NOT NULL CHECK(length(project) = 16),
        label TEXT NOT NULL CHECK(length(label) BETWEEN 1 AND 1024),
        base TEXT CHECK(base IS NULL OR length(base) BETWEEN 1 AND 1024),
        created_ms INTEGER NOT NULL CHECK(created_ms >= 0)
    ) STRICT;
    CREATE TABLE worktree_group_members (
        worktree BLOB PRIMARY KEY NOT NULL CHECK(length(worktree) = 16),
        root BLOB NOT NULL REFERENCES worktree_groups(root) ON DELETE CASCADE,
        ord INTEGER NOT NULL CHECK(ord >= 0)
    ) STRICT;
    CREATE INDEX worktree_group_members_by_root ON worktree_group_members(root, ord);
";

impl Database {
    /// The group `worktree` belongs to, whether it is the root or a child.
    pub fn worktree_group(&self, worktree: WorktreeId) -> Result<Option<BranchGroup>> {
        let transaction = self.connection.unchecked_transaction()?;
        let root: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT root FROM worktree_group_members WHERE worktree=?1",
                [worktree.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        let root = root
            .map(|bytes| fixed_bytes::<16>(&bytes, "branch group root"))
            .transpose()?;
        let group = match root {
            Some(root) => Some(load_group(&transaction, WorktreeId::from_bytes(root))?),
            None => None,
        };
        transaction.commit()?;
        Ok(group)
    }

    /// Make `worktree` the root of its own group unless it already belongs to
    /// one. The boolean reports whether a new group was recorded.
    pub fn register_group_root(&mut self, worktree: WorktreeId) -> Result<(BranchGroup, bool)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(root) = member_root(&transaction, worktree)? {
            let group = load_group(&transaction, root)?;
            transaction.commit()?;
            return Ok((group, false));
        }
        let tree = load_tree(&transaction)?;
        let row = tree
            .worktrees
            .iter()
            .find(|candidate| candidate.id == worktree)
            .ok_or_else(|| missing_worktree(worktree))?;
        let group = BranchGroup {
            root: worktree,
            project: row.project,
            label: group_label(row)?,
            base: group_base(row),
            members: vec![worktree],
        };
        group
            .validate()
            .map_err(|error| StoreError::Invalid(error.to_owned()))?;
        transaction.execute(
            "INSERT INTO worktree_groups(root,project,label,base,created_ms) VALUES(?1,?2,?3,?4,?5)",
            params![
                worktree.as_bytes().as_slice(),
                row.project.as_bytes().as_slice(),
                group.label,
                group.base,
                sql_ms(row.created_ms)?,
            ],
        )?;
        insert_member(&transaction, worktree, worktree, 0)?;
        check_integrity(&transaction)?;
        transaction.commit()?;
        Ok((group, true))
    }

    /// Append a child to `root`'s group. The same worktree cannot join twice,
    /// and a group never exceeds [`MAX_GROUP_MEMBERS`] worktrees.
    pub fn add_group_member(
        &mut self,
        root: WorktreeId,
        worktree: WorktreeId,
    ) -> Result<BranchGroup> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut group = load_group(&transaction, root)?;
        if let Some(existing) = member_root(&transaction, worktree)? {
            return Err(StoreError::Duplicate(format!(
                "worktree {worktree} already belongs to branch group {existing}"
            )));
        }
        if group.members.len() >= MAX_GROUP_MEMBERS {
            return Err(StoreError::Invalid(format!(
                "branch group {root} already has {MAX_GROUP_MEMBERS} worktrees"
            )));
        }
        let tree = load_tree(&transaction)?;
        let row = tree
            .worktrees
            .iter()
            .find(|candidate| candidate.id == worktree)
            .ok_or_else(|| missing_worktree(worktree))?;
        if row.project != group.project {
            return Err(StoreError::Invalid(
                "a branch group child must belong to the group's project".into(),
            ));
        }
        let ord = i64::try_from(group.members.len())
            .map_err(|_| StoreError::Invalid("invalid branch group size".into()))?;
        insert_member(&transaction, root, worktree, ord)?;
        group.members.push(worktree);
        group
            .validate()
            .map_err(|error| StoreError::Invalid(error.to_owned()))?;
        check_integrity(&transaction)?;
        transaction.commit()?;
        Ok(group)
    }
}

fn group_label(worktree: &Worktree) -> Result<String> {
    let label = if worktree.label.is_empty() {
        worktree
            .branch
            .clone()
            .unwrap_or_else(|| "branch group".to_owned())
    } else {
        worktree.label.clone()
    };
    validate_text("branch group label", &label, MAX_GROUP_LABEL_BYTES)
        .map_err(StoreError::Invalid)?;
    Ok(label)
}

fn group_base(worktree: &Worktree) -> Option<String> {
    worktree
        .target
        .clone()
        .filter(|base| !base.is_empty() && base.len() <= MAX_GROUP_BASE_BYTES)
}

fn missing_worktree(id: WorktreeId) -> StoreError {
    StoreError::Missing(format!("worktree {id} not found"))
}

fn member_root(connection: &Connection, worktree: WorktreeId) -> Result<Option<WorktreeId>> {
    let root: Option<Vec<u8>> = connection
        .query_row(
            "SELECT root FROM worktree_group_members WHERE worktree=?1",
            [worktree.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    root.map(|bytes| fixed_bytes::<16>(&bytes, "branch group root"))
        .transpose()
        .map(|root| root.map(WorktreeId::from_bytes))
}

fn insert_member(
    connection: &Connection,
    root: WorktreeId,
    worktree: WorktreeId,
    ord: i64,
) -> Result<()> {
    connection.execute(
        "INSERT INTO worktree_group_members(worktree,root,ord) VALUES(?1,?2,?3)",
        params![
            worktree.as_bytes().as_slice(),
            root.as_bytes().as_slice(),
            ord
        ],
    )?;
    Ok(())
}

fn load_group(connection: &Connection, root: WorktreeId) -> Result<BranchGroup> {
    let row = connection
        .query_row(
            "SELECT root,project,label,base FROM worktree_groups WHERE root=?1",
            [root.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::Missing(format!("branch group {root} not found")))?;
    let mut members = Vec::new();
    {
        let mut statement = connection.prepare(&format!(
            "SELECT worktree FROM worktree_group_members WHERE root=?1 ORDER BY ord,worktree LIMIT {}",
            MAX_GROUP_MEMBERS + 1
        ))?;
        let mut rows = statement.query([root.as_bytes().as_slice()])?;
        while let Some(row) = rows.next()? {
            members.push(fixed_bytes::<16>(
                row_blob(row, 0, 16)?,
                "branch group member",
            )?);
        }
    }
    if members.len() > MAX_GROUP_MEMBERS {
        return Err(corrupt("branch group exceeds its member limit"));
    }
    let group = BranchGroup {
        root: WorktreeId::from_bytes(fixed_bytes::<16>(&row.0, "branch group root")?),
        project: fl_model::ProjectId::from_bytes(fixed_bytes::<16>(
            &row.1,
            "branch group project",
        )?),
        label: row.2,
        base: row.3,
        members: members.into_iter().map(WorktreeId::from_bytes).collect(),
    };
    group.validate().map_err(corrupt)?;
    Ok(group)
}

/// Drop group membership for forgotten worktrees. A removed root drops its
/// group with every membership; the surviving children become standalone.
pub(super) fn prune_groups(connection: &Connection, ids: &[WorktreeId]) -> Result<()> {
    for id in ids {
        connection.execute(
            "DELETE FROM worktree_groups WHERE root=?1",
            [id.as_bytes().as_slice()],
        )?;
        connection.execute(
            "DELETE FROM worktree_group_members WHERE worktree=?1",
            [id.as_bytes().as_slice()],
        )?;
    }
    check_integrity(connection)
}

/// Cross-check every group: its root and members exist, belong to one
/// project, and are not shared with another group.
fn check_integrity(connection: &Connection) -> Result<()> {
    let dummy: i64 = connection.query_row(
        "SELECT count(*) FROM worktree_group_members m
         LEFT JOIN worktrees w ON w.id=m.worktree
         LEFT JOIN worktree_groups g ON g.root=m.root
         WHERE w.id IS NULL OR g.root IS NULL",
        [],
        |row| row.get(0),
    )?;
    if dummy != 0 {
        return Err(corrupt(
            "branch group refers to an absent worktree or group",
        ));
    }
    let foreign: i64 = connection.query_row(
        "SELECT count(*) FROM worktree_group_members m
         JOIN worktrees w ON w.id=m.worktree
         JOIN worktree_groups g ON g.root=m.root
         WHERE w.project != g.project",
        [],
        |row| row.get(0),
    )?;
    if foreign != 0 {
        return Err(corrupt("branch group member belongs to another project"));
    }
    let roots: i64 = connection.query_row(
        "SELECT count(*) FROM worktree_groups g
         LEFT JOIN worktree_group_members m ON m.worktree=g.root
         WHERE m.worktree IS NULL",
        [],
        |row| row.get(0),
    )?;
    if roots != 0 {
        return Err(corrupt("branch group root is not a member of its group"));
    }
    Ok(())
}

pub(super) fn validate_integrity(connection: &Connection) -> Result<()> {
    check_integrity(connection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::workspace::{Project, ProjectKind};

    fn fixture() -> (tempfile::TempDir, Database, Project, Worktree) {
        let temporary = tempfile::tempdir().unwrap();
        let mut database = Database::open(temporary.path()).unwrap();
        let workspace = database.workspace_tree().unwrap().workspaces[0].id;
        let project = Project {
            id: fl_model::ProjectId::new(),
            workspace,
            name: "repo".into(),
            path: "/srv/repo".into(),
            kind: ProjectKind::Git,
            default_branch: Some("main".into()),
            worktree_root: "/srv/repo-worktrees".into(),
            order: 0,
        };
        let primary = worktree(&project, "/srv/repo", true);
        database
            .add_project(project.clone(), std::slice::from_ref(&primary))
            .unwrap();
        (temporary, database, project, primary)
    }

    fn worktree(project: &Project, path: &str, is_primary: bool) -> Worktree {
        Worktree {
            id: WorktreeId::new(),
            project: project.id,
            path: path.into(),
            branch: Some(if is_primary {
                "main".into()
            } else {
                "child".into()
            }),
            target: Some("main".into()),
            label: if is_primary {
                "main".into()
            } else {
                "child".into()
            },
            is_primary,
            created_ms: 1,
            last_active_ms: 1,
        }
    }

    #[test]
    fn groups_register_once_and_prune_with_their_worktrees() {
        let (_temporary, mut database, project, primary) = fixture();
        let (group, created) = database.register_group_root(primary.id).unwrap();
        assert!(created);
        assert_eq!(group.members, vec![primary.id]);
        assert_eq!(group.base.as_deref(), Some("main"));
        let (again, created) = database.register_group_root(primary.id).unwrap();
        assert!(!created);
        assert_eq!(again.members, vec![primary.id]);

        let child = worktree(&project, "/srv/repo-worktrees/child", false);
        database.add_worktree(&child).unwrap();
        let group = database.add_group_member(primary.id, child.id).unwrap();
        assert_eq!(group.members, vec![primary.id, child.id]);
        assert!(matches!(
            database.add_group_member(primary.id, child.id),
            Err(StoreError::Duplicate(_))
        ));
        let found = database.worktree_group(child.id).unwrap().unwrap();
        assert_eq!(found.root, primary.id);
        assert_eq!(found.label, "main");

        database.remove_worktree(child.id).unwrap();
        let found = database.worktree_group(primary.id).unwrap().unwrap();
        assert_eq!(found.members, vec![primary.id]);

        let second = worktree(&project, "/srv/repo-worktrees/second", false);
        database.add_worktree(&second).unwrap();
        database.add_group_member(primary.id, second.id).unwrap();
        database.remove_worktree(primary.id).unwrap_err();
        let removed = database.remove_project(project.id).unwrap();
        assert!(removed.worktrees.contains(&primary.id));
        assert!(database.worktree_group(second.id).unwrap().is_none());
    }

    #[test]
    fn a_group_never_accepts_a_foreign_project_member() {
        let (_temporary, mut database, project, primary) = fixture();
        database.register_group_root(primary.id).unwrap();
        let workspace = database.workspace_tree().unwrap().workspaces[0].id;
        let other = Project {
            id: fl_model::ProjectId::new(),
            workspace,
            name: "other".into(),
            path: "/srv/other".into(),
            kind: ProjectKind::Git,
            default_branch: Some("main".into()),
            worktree_root: "/srv/other-worktrees".into(),
            order: 1,
        };
        let foreign = worktree(&other, "/srv/other", true);
        database
            .add_project(other, std::slice::from_ref(&foreign))
            .unwrap();
        let err = database
            .add_group_member(primary.id, foreign.id)
            .unwrap_err();
        assert!(err.to_string().contains("project"), "{err}");
        assert!(database.worktree_group(primary.id).unwrap().is_some());
        let _ = project;
    }
}
