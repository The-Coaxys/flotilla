//! Workspaces, projects, worktrees and script ownership (schema v3).
//!
//! Every structural change bumps one tree revision in the same transaction,
//! so a client snapshot with a higher revision always reflects a later commit.
use super::*;
use crate::{RemovedWorktrees, ScriptEntry};
use fl_model::ProjectId;
use fl_model::workspace::{Project, ProjectKind, WorkspaceInfo, WorkspaceTree, Worktree};

/// Matches the wire bound in `fl_proto::validation`.
pub(crate) const MAX_WORKTREES: usize = 4096;
const MAX_PROJECTS: usize = 512;
const MAX_WORKSPACES: usize = 64;
const MAX_SCRIPT_KIND_BYTES: usize = 4200;

impl Database {
    pub fn workspace_tree(&self) -> Result<WorkspaceTree> {
        let transaction = self.connection.unchecked_transaction()?;
        let tree = load_tree(&transaction)?;
        transaction.commit()?;
        Ok(tree)
    }

    /// One worktree and its project.
    pub fn worktree(&self, id: WorktreeId) -> Result<(Worktree, Project)> {
        let tree = self.workspace_tree()?;
        let worktree = tree
            .worktrees
            .into_iter()
            .find(|worktree| worktree.id == id)
            .ok_or_else(|| missing_worktree(id))?;
        let project = tree
            .projects
            .into_iter()
            .find(|project| project.id == worktree.project)
            .ok_or_else(|| corrupt("worktree refers to an absent project"))?;
        Ok((worktree, project))
    }

    /// Insert a project with its initial worktrees (each with an empty
    /// layout). The project's sidebar order is assigned here.
    pub fn add_project(
        &mut self,
        mut project: Project,
        worktrees: &[Worktree],
    ) -> Result<(Project, u64)> {
        validate_project(&project).map_err(StoreError::Invalid)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tree = load_tree(&transaction)?;
        if !tree.workspaces.iter().any(|w| w.id == project.workspace) {
            return Err(StoreError::Missing(format!(
                "workspace {} not found",
                project.workspace
            )));
        }
        if tree.projects.len() >= MAX_PROJECTS {
            return Err(StoreError::Invalid("too many projects".into()));
        }
        if let Some(existing) = tree.projects.iter().find(|p| p.path == project.path) {
            return Err(StoreError::Duplicate(format!(
                "{} is already added as project {}",
                project.path, existing.name
            )));
        }
        if tree.worktrees.len() + worktrees.len() > MAX_WORKTREES {
            return Err(StoreError::Invalid("too many worktrees".into()));
        }
        project.order = tree
            .projects
            .iter()
            .filter(|p| p.workspace == project.workspace)
            .map(|p| p.order.saturating_add(1))
            .max()
            .unwrap_or(0);
        if worktrees.iter().filter(|w| w.is_primary).count() != 1 {
            return Err(StoreError::Invalid(
                "a project needs exactly one primary worktree".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO projects(id,workspace,name,path,kind,default_branch,worktree_root,ord) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                project.id.as_bytes().as_slice(),
                project.workspace.as_bytes().as_slice(),
                project.name,
                project.path,
                kind_text(project.kind),
                project.default_branch,
                project.worktree_root,
                i64::from(project.order),
            ],
        )?;
        for worktree in worktrees {
            if worktree.project != project.id {
                return Err(StoreError::Invalid(
                    "worktree belongs to another project".into(),
                ));
            }
            insert_worktree(&transaction, &tree, worktree, project.workspace)?;
        }
        let rev = bump_tree(&transaction)?;
        transaction.commit()?;
        Ok((project, rev))
    }

    /// Forget a project: its worktrees, layouts and scripts. Their desired
    /// terminals become closed intents, returned for the caller to retire.
    pub fn remove_project(&mut self, project: ProjectId) -> Result<RemovedWorktrees> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tree = load_tree(&transaction)?;
        if !tree.projects.iter().any(|p| p.id == project) {
            return Err(StoreError::Missing(format!("project {project} not found")));
        }
        let ids: Vec<_> = tree
            .worktrees
            .iter()
            .filter(|w| w.project == project)
            .map(|w| w.id)
            .collect();
        let closed = delete_worktrees(&transaction, &ids)?;
        transaction.execute(
            "DELETE FROM projects WHERE id=?1",
            [project.as_bytes().as_slice()],
        )?;
        let rev = bump_tree(&transaction)?;
        transaction.commit()?;
        Ok(RemovedWorktrees {
            rev,
            worktrees: ids,
            closed,
        })
    }

    pub fn add_worktree(&mut self, worktree: &Worktree) -> Result<u64> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tree = load_tree(&transaction)?;
        let project = tree
            .projects
            .iter()
            .find(|p| p.id == worktree.project)
            .ok_or_else(|| {
                StoreError::Missing(format!("project {} not found", worktree.project))
            })?;
        if worktree.is_primary {
            return Err(StoreError::Invalid(
                "a project has exactly one primary worktree".into(),
            ));
        }
        if tree.worktrees.len() >= MAX_WORKTREES {
            return Err(StoreError::Invalid("too many worktrees".into()));
        }
        insert_worktree(&transaction, &tree, worktree, project.workspace)?;
        let rev = bump_tree(&transaction)?;
        transaction.commit()?;
        Ok(rev)
    }

    /// Forget one linked worktree (the primary goes with its project).
    pub fn remove_worktree(&mut self, id: WorktreeId) -> Result<RemovedWorktrees> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tree = load_tree(&transaction)?;
        let worktree = tree
            .worktrees
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| missing_worktree(id))?;
        if worktree.is_primary {
            return Err(StoreError::Invalid(
                "the primary worktree is removed with its project".into(),
            ));
        }
        let closed = delete_worktrees(&transaction, &[id])?;
        let rev = bump_tree(&transaction)?;
        transaction.commit()?;
        Ok(RemovedWorktrees {
            rev,
            worktrees: vec![id],
            closed,
        })
    }

    pub fn set_worktree_label(&mut self, id: WorktreeId, label: &str) -> Result<(Worktree, u64)> {
        validate_text("label", label, MAX_TEXT_BYTES).map_err(StoreError::Invalid)?;
        self.update_worktree(id, |worktree| {
            worktree.label = label.to_owned();
            true
        })
        .map(|(worktree, rev)| (worktree, rev.unwrap_or_default()))
    }

    /// Record the branch a worktree currently has checked out. Returns the new
    /// tree revision when it changed.
    pub fn set_worktree_branch(
        &mut self,
        id: WorktreeId,
        branch: Option<&str>,
    ) -> Result<Option<u64>> {
        if let Some(branch) = branch {
            validate_text("branch", branch, MAX_TEXT_BYTES).map_err(StoreError::Invalid)?;
        }
        self.update_worktree(id, |worktree| {
            if worktree.branch.as_deref() == branch {
                return false;
            }
            worktree.branch = branch.map(str::to_owned);
            true
        })
        .map(|(_, rev)| rev)
    }

    /// Record the merge-base target a worktree diffs against. Returns the new
    /// tree revision when it changed.
    pub fn set_worktree_target(
        &mut self,
        id: WorktreeId,
        target: Option<&str>,
    ) -> Result<Option<u64>> {
        if let Some(target) = target {
            validate_text("target branch", target, MAX_TEXT_BYTES).map_err(StoreError::Invalid)?;
        }
        self.update_worktree(id, |worktree| {
            if worktree.target.as_deref() == target {
                return false;
            }
            worktree.target = target.map(str::to_owned);
            true
        })
        .map(|(_, rev)| rev)
    }

    fn update_worktree(
        &mut self,
        id: WorktreeId,
        update: impl FnOnce(&mut Worktree) -> bool,
    ) -> Result<(Worktree, Option<u64>)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tree = load_tree(&transaction)?;
        let mut worktree = tree
            .worktrees
            .into_iter()
            .find(|w| w.id == id)
            .ok_or_else(|| missing_worktree(id))?;
        if !update(&mut worktree) {
            transaction.commit()?;
            return Ok((worktree, None));
        }
        transaction.execute(
            "UPDATE worktrees SET label=?2,branch=?3 WHERE id=?1",
            params![id.as_bytes().as_slice(), worktree.label, worktree.branch],
        )?;
        let rev = bump_tree(&transaction)?;
        transaction.commit()?;
        Ok((worktree, Some(rev)))
    }

    /// Commit a new script terminal for `(worktree, kind)`. A previous run of
    /// the same script becomes a closed intent, returned for retirement.
    pub fn start_script(
        &mut self,
        worktree: WorktreeId,
        kind: &str,
        spec: SpawnSpec,
    ) -> Result<(TerminalEntry, Option<TerminalKey>)> {
        validate_text("script kind", kind, MAX_SCRIPT_KIND_BYTES).map_err(StoreError::Invalid)?;
        if kind.is_empty() {
            return Err(StoreError::Invalid("empty script kind".into()));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tree = load_tree(&transaction)?;
        if !tree.worktrees.iter().any(|w| w.id == worktree) {
            return Err(missing_worktree(worktree));
        }
        let mut state = load_state(&transaction)?;
        if state
            .terminals
            .iter()
            .any(|entry| entry.spec.terminal == spec.terminal)
        {
            return Err(LayoutError::DuplicateTerminal(spec.terminal).into());
        }
        if state.terminals.len() == MAX_TERMINALS {
            return Err(LayoutError::TooManyTerminals.into());
        }
        let entry = TerminalEntry {
            spec,
            desired: true,
            worktree: Some(worktree),
            process: None,
            launch_error: None,
            checkpoint: None,
        };
        validate_entry(&entry).map_err(StoreError::Invalid)?;
        transaction.execute(
            "INSERT INTO terminals(terminal, incarnation, desired, entry) VALUES (?1, ?2, 1, ?3)",
            params![
                entry.spec.terminal.as_bytes().as_slice(),
                entry.spec.incarnation.as_bytes().as_slice(),
                encode_entry(&entry)?
            ],
        )?;
        let previous = state
            .scripts
            .iter()
            .find(|script| script.worktree == worktree && script.kind == kind)
            .map(|script| script.terminal);
        let replaced = match previous {
            Some(terminal) => {
                transaction.execute(
                    "UPDATE scripts SET terminal=?3 WHERE worktree=?1 AND kind=?2",
                    params![
                        worktree.as_bytes().as_slice(),
                        kind,
                        entry.spec.terminal.as_bytes().as_slice()
                    ],
                )?;
                Some(close_entry(&transaction, &mut state.terminals, terminal)?)
            }
            None => {
                transaction.execute(
                    "INSERT INTO scripts(worktree,kind,terminal) VALUES(?1,?2,?3)",
                    params![
                        worktree.as_bytes().as_slice(),
                        kind,
                        entry.spec.terminal.as_bytes().as_slice()
                    ],
                )?;
                None
            }
        };
        load_state(&transaction)?;
        transaction.commit()?;
        Ok((entry, replaced))
    }

    /// Forget the script's terminal (a closed intent the caller retires).
    pub fn close_script(
        &mut self,
        worktree: WorktreeId,
        kind: &str,
    ) -> Result<Option<TerminalKey>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut state = load_state(&transaction)?;
        let Some(terminal) = state
            .scripts
            .iter()
            .find(|script| script.worktree == worktree && script.kind == kind)
            .map(|script| script.terminal)
        else {
            transaction.commit()?;
            return Ok(None);
        };
        transaction.execute(
            "DELETE FROM scripts WHERE worktree=?1 AND kind=?2",
            params![worktree.as_bytes().as_slice(), kind],
        )?;
        let key = close_entry(&transaction, &mut state.terminals, terminal)?;
        transaction.commit()?;
        Ok(Some(key))
    }

    /// Every script terminal, in worktree and kind order.
    pub fn scripts(&self) -> Result<Vec<ScriptEntry>> {
        let transaction = self.connection.unchecked_transaction()?;
        let state = load_state(&transaction)?;
        transaction.commit()?;
        state
            .scripts
            .into_iter()
            .map(|script| {
                let entry = state
                    .terminals
                    .iter()
                    .find(|entry| entry.spec.terminal == script.terminal)
                    .ok_or_else(|| corrupt("script refers to an absent terminal"))?;
                Ok(ScriptEntry {
                    worktree: script.worktree,
                    kind: script.kind,
                    key: key_of(entry),
                })
            })
            .collect()
    }
}

fn missing_worktree(id: WorktreeId) -> StoreError {
    StoreError::Missing(format!("worktree {id} not found"))
}

fn kind_text(kind: ProjectKind) -> &'static str {
    match kind {
        ProjectKind::Git => "git",
        ProjectKind::Folder => "folder",
    }
}

fn validate_path(field: &str, path: &str) -> std::result::Result<(), String> {
    validate_text(field, path, MAX_TEXT_BYTES)?;
    if !path.starts_with('/') {
        return Err(format!("{field} is not absolute"));
    }
    Ok(())
}

fn validate_project(project: &Project) -> std::result::Result<(), String> {
    validate_text("project name", &project.name, MAX_TEXT_BYTES)?;
    if project.name.is_empty() {
        return Err("empty project name".into());
    }
    validate_path("project path", &project.path)?;
    validate_path("worktree root", &project.worktree_root)?;
    if let Some(branch) = &project.default_branch {
        validate_text("default branch", branch, MAX_TEXT_BYTES)?;
    }
    Ok(())
}

fn validate_worktree(worktree: &Worktree) -> std::result::Result<(), String> {
    validate_path("worktree path", &worktree.path)?;
    validate_text("label", &worktree.label, MAX_TEXT_BYTES)?;
    for branch in [&worktree.branch, &worktree.target].into_iter().flatten() {
        validate_text("branch", branch, MAX_TEXT_BYTES)?;
    }
    Ok(())
}

fn insert_worktree(
    connection: &Connection,
    tree: &WorkspaceTree,
    worktree: &Worktree,
    workspace: WorkspaceId,
) -> Result<()> {
    validate_worktree(worktree).map_err(StoreError::Invalid)?;
    if tree.worktrees.iter().any(|w| w.path == worktree.path) {
        return Err(StoreError::Duplicate(format!(
            "{} is already a known worktree",
            worktree.path
        )));
    }
    connection.execute(
        "INSERT INTO worktrees(id,project,path,branch,target,label,is_primary,created_ms,last_active_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            worktree.id.as_bytes().as_slice(),
            worktree.project.as_bytes().as_slice(),
            worktree.path,
            worktree.branch,
            worktree.target,
            worktree.label,
            worktree.is_primary,
            sql_ms(worktree.created_ms)?,
            sql_ms(worktree.last_active_ms)?,
        ],
    )?;
    let layout = Layout::new(workspace, ViewId::new());
    connection.execute(
        "INSERT INTO worktree_layouts(worktree,state) VALUES(?1,?2)",
        params![worktree.id.as_bytes().as_slice(), encode_layout(&layout)?],
    )?;
    Ok(())
}

fn close_entry(
    connection: &Connection,
    terminals: &mut [TerminalEntry],
    terminal: TerminalId,
) -> Result<TerminalKey> {
    let entry = terminals
        .iter_mut()
        .find(|entry| entry.spec.terminal == terminal)
        .ok_or_else(|| corrupt("owner refers to an absent terminal"))?;
    entry.desired = false;
    write_entry(connection, entry)?;
    Ok(key_of(entry))
}

/// Delete worktrees with their layouts and scripts; their desired terminals
/// become closed intents.
fn delete_worktrees(connection: &Connection, ids: &[WorktreeId]) -> Result<Vec<TerminalKey>> {
    let mut state = load_state(connection)?;
    let owned: Vec<TerminalId> = state
        .terminals
        .iter()
        .filter(|entry| entry.desired && entry.worktree.is_some_and(|w| ids.contains(&w)))
        .map(|entry| entry.spec.terminal)
        .collect();
    super::sections::prune_memberships(connection, ids)?;
    for id in ids {
        let id = id.as_bytes().as_slice();
        connection.execute("DELETE FROM scripts WHERE worktree=?1", [id])?;
        connection.execute("DELETE FROM worktree_layouts WHERE worktree=?1", [id])?;
        connection.execute("DELETE FROM worktrees WHERE id=?1", [id])?;
    }
    let mut closed = Vec::with_capacity(owned.len());
    for terminal in owned {
        closed.push(close_entry(connection, &mut state.terminals, terminal)?);
    }
    // The remaining state must still be consistent before commit.
    load_state(connection)?;
    Ok(closed)
}

fn bump_tree(connection: &Connection) -> Result<u64> {
    let rev = tree_rev(connection)?
        .checked_add(1)
        .ok_or_else(|| StoreError::Invalid("workspace tree revision overflow".into()))?;
    connection.execute(
        "UPDATE workspace_meta SET tree_rev=?1 WHERE singleton=1",
        [postcard::to_stdvec(&rev)?],
    )?;
    Ok(rev)
}

fn tree_rev(connection: &Connection) -> Result<u64> {
    let mut statement = connection.prepare("SELECT singleton,tree_rev FROM workspace_meta")?;
    let mut rows = statement.query([])?;
    let row = rows
        .next()?
        .ok_or_else(|| corrupt("workspace tree revision is absent"))?;
    if row.get::<_, i64>(0)? != 1 {
        return Err(corrupt("invalid workspace tree singleton"));
    }
    let rev = decode(row_blob(row, 1, 10)?, "workspace tree revision")?;
    if rows.next()?.is_some() {
        return Err(corrupt("more than one workspace tree revision"));
    }
    Ok(rev)
}

fn id_column<const N: usize>(row: &Row<'_>, column: usize, field: &str) -> Result<[u8; N]> {
    fixed_bytes(row_blob(row, column, N)?, field)
}

fn u32_column(row: &Row<'_>, column: usize) -> Result<u32> {
    u32::try_from(row.get::<_, i64>(column)?).map_err(|_| corrupt("invalid persisted order"))
}

fn ms_column(row: &Row<'_>, column: usize) -> Result<u64> {
    u64::try_from(row.get::<_, i64>(column)?).map_err(|_| corrupt("invalid persisted timestamp"))
}

/// Load and validate the complete tree; every reference must resolve.
pub(crate) fn load_tree(connection: &Connection) -> Result<WorkspaceTree> {
    let rev = tree_rev(connection)?;
    let mut workspaces = Vec::new();
    {
        let mut statement = connection.prepare(&format!(
            "SELECT id,name,ord FROM workspaces ORDER BY ord,id LIMIT {}",
            MAX_WORKSPACES + 1
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if workspaces.len() == MAX_WORKSPACES {
                return Err(corrupt("persisted workspace count exceeds its bound"));
            }
            workspaces.push(WorkspaceInfo {
                id: WorkspaceId::from_bytes(id_column(row, 0, "workspace ID")?),
                name: row.get(1)?,
                order: u32_column(row, 2)?,
            });
        }
    }
    let mut projects = Vec::new();
    {
        let mut statement = connection.prepare(&format!(
            "SELECT id,workspace,name,path,kind,default_branch,worktree_root,ord FROM projects ORDER BY ord,path LIMIT {}",
            MAX_PROJECTS + 1
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if projects.len() == MAX_PROJECTS {
                return Err(corrupt("persisted project count exceeds its bound"));
            }
            let kind = match row.get::<_, String>(4)?.as_str() {
                "git" => ProjectKind::Git,
                "folder" => ProjectKind::Folder,
                _ => return Err(corrupt("invalid persisted project kind")),
            };
            let project = Project {
                id: ProjectId::from_bytes(id_column(row, 0, "project ID")?),
                workspace: WorkspaceId::from_bytes(id_column(row, 1, "workspace ID")?),
                name: row.get(2)?,
                path: row.get(3)?,
                kind,
                default_branch: row.get(5)?,
                worktree_root: row.get(6)?,
                order: u32_column(row, 7)?,
            };
            validate_project(&project)
                .map_err(|error| corrupt(format!("invalid persisted project: {error}")))?;
            if !workspaces.iter().any(|w| w.id == project.workspace) {
                return Err(corrupt("project refers to an absent workspace"));
            }
            projects.push(project);
        }
    }
    let mut worktrees = Vec::new();
    {
        let mut statement = connection.prepare(&format!(
            "SELECT id,project,path,branch,target,label,is_primary,created_ms,last_active_ms FROM worktrees ORDER BY project,is_primary DESC,created_ms,path LIMIT {}",
            MAX_WORKTREES + 1
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if worktrees.len() == MAX_WORKTREES {
                return Err(corrupt("persisted worktree count exceeds its bound"));
            }
            let worktree = Worktree {
                id: WorktreeId::from_bytes(id_column(row, 0, "worktree ID")?),
                project: ProjectId::from_bytes(id_column(row, 1, "project ID")?),
                path: row.get(2)?,
                branch: row.get(3)?,
                target: row.get(4)?,
                label: row.get(5)?,
                is_primary: match row.get::<_, i64>(6)? {
                    0 => false,
                    1 => true,
                    _ => return Err(corrupt("invalid worktree primary flag")),
                },
                created_ms: ms_column(row, 7)?,
                last_active_ms: ms_column(row, 8)?,
            };
            validate_worktree(&worktree)
                .map_err(|error| corrupt(format!("invalid persisted worktree: {error}")))?;
            if !projects.iter().any(|p| p.id == worktree.project) {
                return Err(corrupt("worktree refers to an absent project"));
            }
            worktrees.push(worktree);
        }
    }
    for project in &projects {
        let primaries = worktrees
            .iter()
            .filter(|w| w.project == project.id && w.is_primary)
            .count();
        if primaries != 1 {
            return Err(corrupt(format!(
                "project {} has {primaries} primary worktrees",
                project.id
            )));
        }
    }
    let layouts: i64 = connection.query_row(
        "SELECT count(*) FROM worktree_layouts WHERE worktree IN (SELECT id FROM worktrees)",
        [],
        |row| row.get(0),
    )?;
    let all_layouts: i64 =
        connection.query_row("SELECT count(*) FROM worktree_layouts", [], |row| {
            row.get(0)
        })?;
    if layouts != all_layouts || layouts as usize != worktrees.len() {
        return Err(corrupt("worktree layouts disagree with worktrees"));
    }
    // Sort projects by workspace order, then their own order.
    projects.sort_by_key(|p| {
        (
            workspaces
                .iter()
                .position(|w| w.id == p.workspace)
                .unwrap_or(usize::MAX),
            p.order,
        )
    });
    Ok(WorkspaceTree {
        rev,
        workspaces,
        projects,
        worktrees,
    })
}
