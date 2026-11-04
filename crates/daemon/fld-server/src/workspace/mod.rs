//! Workspaces, projects and worktrees (M2): repository services over the
//! `git` CLI, worktree-scoped layouts, files, live status and scripts.
//!
//! Durable structure lives in `fld-store` (schema v3). Structural changes
//! are serialized and each one publishes `WorkspaceTreeChanged`.
mod coordination;
mod forge;
mod review;
mod routing;
mod scripts;
mod sections;
mod status;
mod team;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fl_model::layout::{Layout, LayoutOp};
use fl_model::workspace::{
    CallerContext, DaemonEvent, DirListing, FileContent, FileDiff, LayoutScope, Project,
    ProjectKind, WorkspaceInfo, WorkspaceTree, Worktree, WorktreeStatus,
};
use fl_model::{OperationId, ProjectId, WorkspaceId, WorktreeId};
use fl_proto::validation::{MAX_DIR_ENTRIES, MAX_FILE_BYTES};
use fl_proto::{RpcError, RpcErrorCode};
use fld_git::GitError;
use fld_store::{LaunchDefaults, RemovedWorktrees, Store, StoreError};
use tokio_util::sync::CancellationToken;

use crate::FlotillaHome;
use crate::chat::ChatService;
use crate::events::EventBus;
use crate::terminal::{TerminalService, bounded, stopped_rpc, store_rpc};

/// Each side of a diff stays below half a wire frame.
const MAX_DIFF_SIDE_BYTES: usize = MAX_FILE_BYTES - 1024 * 1024;

pub(crate) struct WorkspaceService {
    terminals: Arc<TerminalService>,
    events: Arc<EventBus>,
    chat: Arc<ChatService>,
    home: FlotillaHome,
    /// Serializes structural changes (projects and worktrees) end to end,
    /// including their git operations.
    structure: tokio::sync::Mutex<()>,
    /// One watcher task per watched worktree.
    watchers: Mutex<HashMap<WorktreeId, CancellationToken>>,
    /// Last published status per worktree (only changes are published).
    statuses: Mutex<HashMap<WorktreeId, WorktreeStatus>>,
    forge: Mutex<forge::ForgeCache>,
    /// Routed dispatches of this daemon lifetime.
    dispatches: Mutex<HashMap<fl_model::TaskRunId, fl_model::routing::TaskDispatch>>,
    /// Cancels a pending dispatch when its worktree is removed.
    dispatch_cancels: Mutex<HashMap<fl_model::TaskRunId, CancellationToken>>,
    /// Cached provider discovery; handshakes are expensive and change slowly.
    providers: tokio::sync::Mutex<Option<(std::time::Instant, Vec<fld_agent::ProviderDiscovery>)>>,
    /// Serializes team role bookkeeping and run settlement.
    teams: tokio::sync::Mutex<()>,
    cancel: CancellationToken,
}

pub(crate) fn rpc(code: RpcErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: bounded(message.into()),
    }
}

fn internal(message: impl Into<String>) -> RpcError {
    rpc(RpcErrorCode::Internal, message)
}

fn invalid(message: impl Into<String>) -> RpcError {
    rpc(RpcErrorCode::InvalidArgument, message)
}

pub(crate) fn git_rpc(error: GitError) -> RpcError {
    let code = match &error {
        GitError::NotFound(_) => RpcErrorCode::NotFound,
        GitError::Invalid(_) | GitError::Refused(_) => RpcErrorCode::InvalidArgument,
        GitError::Conflict { .. } => RpcErrorCode::RevisionConflict,
        GitError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            RpcErrorCode::NotFound
        }
        _ => RpcErrorCode::Internal,
    };
    rpc(code, error.to_string())
}

/// Run blocking git/file work off the async workers.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, GitError> + Send + 'static,
) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| internal(format!("background task failed: {error}")))?
        .map_err(git_rpc)
}

fn utf8(path: &Path) -> Result<String, RpcError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("{} is not valid UTF-8", path.display())))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

impl WorkspaceService {
    /// Reconcile stored worktrees with the filesystem, then watch every
    /// worktree of a git project and resume monitoring live scripts.
    pub(crate) async fn start(
        home: FlotillaHome,
        terminals: Arc<TerminalService>,
        events: Arc<EventBus>,
        chat: Arc<ChatService>,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, RpcError> {
        let service = Arc::new(Self {
            terminals,
            events,
            chat,
            home,
            structure: tokio::sync::Mutex::new(()),
            watchers: Mutex::default(),
            statuses: Mutex::default(),
            forge: Mutex::default(),
            dispatches: Mutex::default(),
            dispatch_cancels: Mutex::default(),
            providers: tokio::sync::Mutex::new(None),
            teams: tokio::sync::Mutex::new(()),
            cancel,
        });
        service.reconcile().await?;
        service.clone().resume_scripts().await?;
        service.start_forge_polling();
        service.watch_team_notices();
        Ok(service)
    }

    async fn reconcile(self: &Arc<Self>) -> Result<(), RpcError> {
        let _structure = self.structure.lock().await;
        let tree = self.tree().await?;
        for worktree in &tree.worktrees {
            if worktree.is_primary || Path::new(&worktree.path).exists() {
                continue;
            }
            tracing::warn!(path = %worktree.path, "worktree directory vanished; forgetting it");
            let id = worktree.id;
            let removed = self
                .store(move |store| store.database.remove_worktree(id))
                .await?;
            self.forget(removed).await;
        }
        let tree = self.tree().await?;
        for worktree in &tree.worktrees {
            let git = tree
                .projects
                .iter()
                .any(|p| p.id == worktree.project && p.kind == ProjectKind::Git);
            if git {
                self.watch(worktree.id);
            }
        }
        Ok(())
    }

    pub(crate) fn publish(&self, event: DaemonEvent) {
        self.events.publish(event);
    }

    fn tree_changed(&self, rev: u64) {
        self.publish(DaemonEvent::WorkspaceTreeChanged { rev });
    }

    async fn store<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, RpcError> {
        self.terminals
            .shared
            .store
            .call(job)
            .await
            .map_err(|_| stopped_rpc())?
            .map_err(store_rpc)
    }

    pub(crate) async fn tree(&self) -> Result<WorkspaceTree, RpcError> {
        self.store(|store| store.database.workspace_tree()).await
    }

    pub(crate) async fn worktree(&self, id: WorktreeId) -> Result<(Worktree, Project), RpcError> {
        self.store(move |store| store.database.worktree(id)).await
    }

    /// Resolve `fl`'s caller from its working directory and optional chat
    /// session. A missing session is not an error: stale `FLOTILLA_SESSION_ID`
    /// values must not break commands that do not need one.
    pub(crate) async fn resolve_caller(
        &self,
        cwd: String,
        session: Option<uuid::Uuid>,
    ) -> Result<CallerContext, RpcError> {
        let tree = self.tree().await?;
        let cwd_path = Path::new(&cwd);
        let worktree = tree
            .worktrees
            .iter()
            .filter(|worktree| cwd_path.starts_with(Path::new(&worktree.path)))
            .max_by_key(|worktree| worktree.path.len())
            .cloned();
        let session = match session {
            Some(id) => self.chat.session(id).await.ok(),
            None => None,
        };
        let project = worktree
            .as_ref()
            .and_then(|worktree| tree.projects.iter().find(|p| p.id == worktree.project))
            .cloned();
        Ok(CallerContext {
            cwd,
            worktree,
            project,
            session,
        })
    }

    /// Chat history across every worktree, newest first, bounded by `limit`.
    pub(crate) async fn chat_history(
        &self,
        limit: u16,
    ) -> Result<Vec<fl_model::chat::ChatSession>, RpcError> {
        let mut sessions = self
            .store(|store| store.database.all_chat_sessions())
            .await?;
        sessions.reverse();
        sessions.truncate(usize::from(limit));
        Ok(sessions)
    }

    pub(crate) async fn selected_workspace(&self) -> Result<Option<WorkspaceInfo>, RpcError> {
        self.store(|store| store.database.selected_workspace())
            .await
    }

    pub(crate) async fn select_workspace(
        &self,
        workspace: WorkspaceId,
    ) -> Result<WorkspaceInfo, RpcError> {
        self.store(move |store| store.database.set_selected_workspace(workspace))
            .await
    }

    /// Stop watchers, drop cached state and retire the closed terminals of
    /// worktrees that were just forgotten, then announce the new tree.
    async fn forget(&self, removed: RemovedWorktrees) {
        for id in &removed.worktrees {
            self.unwatch(*id);
            self.statuses.lock().expect("statuses poisoned").remove(id);
            self.forget_forge(*id);
        }
        self.terminals.close_all(removed.closed).await;
        self.tree_changed(removed.rev);
        // Membership pruning commits with deletion; publish its new revision
        // separately without changing the M2 WorkspaceTree wire shape.
        match self.sections_state().await {
            Ok(state) => self.publish(DaemonEvent::SectionsChanged(state)),
            Err(error) => {
                tracing::warn!(message = %error.message, "section snapshot after worktree removal failed")
            }
        }
    }

    /// Launch policy for terminals of one worktree: its directory as cwd and
    /// the `FLOTILLA_*` variables on top of the daemon's defaults.
    fn worktree_defaults(&self, worktree: &Worktree, project: &Project) -> LaunchDefaults {
        let mut defaults = self.terminals.launch_defaults().clone();
        defaults.cwd = worktree.path.clone();
        for (key, value) in [
            ("FLOTILLA_ROOT_PATH", &project.path),
            ("FLOTILLA_WORKTREE_PATH", &worktree.path),
            ("FLOTILLA_WORKTREE_NAME", &worktree.label),
        ] {
            defaults.env.insert(key.into(), value.clone());
        }
        defaults
    }

    // --- Projects -----------------------------------------------------------

    pub(crate) async fn add_project(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        path: String,
    ) -> Result<Project, RpcError> {
        let _structure = self.structure.lock().await;
        self.add_project_locked(workspace, path).await
    }

    async fn add_project_locked(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        path: String,
    ) -> Result<Project, RpcError> {
        let discovered = blocking(move || discover_project(Path::new(&path))).await?;
        let tree = self.tree().await?;
        let id = ProjectId::new();
        let name = dir_name(&discovered.path);
        let mut root = self
            .home
            .root()
            .join("worktrees")
            .join(fld_git::worktree::slug(&name));
        let root_text = utf8(&root)?;
        if tree.projects.iter().any(|p| p.worktree_root == root_text) {
            root = self.home.root().join("worktrees").join(format!(
                "{}-{}",
                fld_git::worktree::slug(&name),
                &id.to_string()[..8]
            ));
        }
        let project_path = utf8(&discovered.path)?;
        let project = Project {
            id,
            workspace,
            name,
            path: project_path.clone(),
            kind: discovered.kind,
            default_branch: discovered.default_branch.clone(),
            worktree_root: match discovered.kind {
                ProjectKind::Git => utf8(&root)?,
                ProjectKind::Folder => project_path.clone(),
            },
            order: 0,
        };
        let now = now_ms();
        let mut worktrees = Vec::with_capacity(discovered.worktrees.len());
        for (index, found) in discovered.worktrees.iter().enumerate() {
            let path = utf8(&found.path)?;
            worktrees.push(Worktree {
                id: WorktreeId::new(),
                project: id,
                label: found
                    .branch
                    .clone()
                    .unwrap_or_else(|| dir_name(&found.path)),
                path,
                branch: found.branch.clone(),
                target: discovered.default_branch.clone(),
                is_primary: index == 0,
                created_ms: now,
                last_active_ms: now,
            });
        }
        let inserted = worktrees.clone();
        let (project, rev) = self
            .store(move |store| store.database.add_project(project, &inserted))
            .await?;
        if project.kind == ProjectKind::Git {
            for worktree in &worktrees {
                self.watch(worktree.id);
            }
        }
        self.tree_changed(rev);
        Ok(project)
    }

    pub(crate) async fn clone_project(
        self: &Arc<Self>,
        workspace: WorkspaceId,
        url: String,
        dest: String,
    ) -> Result<Project, RpcError> {
        let _structure = self.structure.lock().await;
        let target = dest.clone();
        blocking(move || fld_git::repo::clone(&url, Path::new(&target))).await?;
        self.add_project_locked(workspace, dest).await
    }

    /// Forget a project; its files stay untouched.
    pub(crate) async fn remove_project(&self, project: ProjectId) -> Result<(), RpcError> {
        let _structure = self.structure.lock().await;
        let tree = self.tree().await?;
        let worktrees: Vec<WorktreeId> = tree
            .worktrees
            .iter()
            .filter(|worktree| worktree.project == project)
            .map(|worktree| worktree.id)
            .collect();
        // Stop providers and routed tasks before the cascade deletes their
        // sessions and dispatches.
        for worktree in &worktrees {
            self.cancel_dispatches_for(*worktree);
            self.chat.close_worktree(*worktree).await?;
        }
        let removed = self
            .store(move |store| store.database.remove_project(project))
            .await?;
        self.forget(removed).await;
        Ok(())
    }

    // --- Worktrees ----------------------------------------------------------

    pub(crate) async fn create_worktree(
        self: &Arc<Self>,
        project: ProjectId,
        branch: Option<String>,
        base: Option<String>,
    ) -> Result<Worktree, RpcError> {
        let _structure = self.structure.lock().await;
        let tree = self.tree().await?;
        let project = tree
            .projects
            .into_iter()
            .find(|p| p.id == project)
            .ok_or_else(|| {
                rpc(
                    RpcErrorCode::NotFound,
                    format!("project {project} not found"),
                )
            })?;
        if project.kind != ProjectKind::Git {
            return Err(invalid(format!(
                "{} is a folder project; worktrees need a git repository",
                project.name
            )));
        }
        let primary = PathBuf::from(&project.path);
        let branch = match branch {
            Some(branch) => {
                let (repo, name) = (primary.clone(), branch.clone());
                blocking(move || fld_git::worktree::validate_branch_name(&repo, &name)).await?;
                branch
            }
            None => fld_git::worktree::temporary_branch_name(),
        };
        let start = base
            .clone()
            .or_else(|| project.default_branch.clone())
            .unwrap_or_else(|| "HEAD".into());
        let root = PathBuf::from(&project.worktree_root);
        let slug = fld_git::worktree::slug(&branch);
        let mut path = root.join(&slug);
        let mut suffix = 2;
        while path.exists() || tree.worktrees.iter().any(|w| Path::new(&w.path) == path) {
            if suffix > 1000 {
                return Err(invalid(format!(
                    "no free worktree directory for {slug} in {}",
                    root.display()
                )));
            }
            path = root.join(format!("{slug}-{suffix}"));
            suffix += 1;
        }
        {
            let (repo, path, branch, start) =
                (primary.clone(), path.clone(), branch.clone(), start);
            blocking(move || {
                fld_git::worktree::add_worktree(&repo, &path, &branch, &start)?;
                let prepared = fld_git::worktree::copy_includes(&repo, &path)
                    .and_then(|_| fld_git::worktree::init_submodules(&path));
                if let Err(error) = prepared {
                    // Never leave a half-prepared worktree or its new branch.
                    if let Err(cleanup) = fld_git::worktree::remove_worktree(&repo, &path, true) {
                        tracing::warn!("removing failed worktree: {cleanup}");
                    }
                    if let Err(cleanup) =
                        fld_git::worktree::delete_branch(&repo, &branch, None, true)
                    {
                        tracing::warn!("deleting failed worktree branch: {cleanup}");
                    }
                    return Err(error);
                }
                Ok(())
            })
            .await?;
        }
        let canonical = utf8(
            &std::fs::canonicalize(&path)
                .map_err(|error| internal(format!("resolving {}: {error}", path.display())))?,
        )?;
        let now = now_ms();
        let worktree = Worktree {
            id: WorktreeId::new(),
            project: project.id,
            path: canonical,
            label: branch.clone(),
            branch: Some(branch),
            target: base.or_else(|| project.default_branch.clone()),
            is_primary: false,
            created_ms: now,
            last_active_ms: now,
        };
        let inserted = worktree.clone();
        let rev = self
            .store(move |store| store.database.add_worktree(&inserted))
            .await?;
        self.watch(worktree.id);
        self.tree_changed(rev);
        // Setup runs as a script terminal; the worktree exists either way.
        if let Err(error) = self
            .clone()
            .run_configured(&worktree, &project, fl_model::workspace::ScriptKind::Setup)
            .await
        {
            tracing::warn!(worktree = %worktree.path, "setup script not started: {}", error.message);
        }
        Ok(worktree)
    }

    pub(crate) async fn delete_worktree(
        self: &Arc<Self>,
        id: WorktreeId,
        force: bool,
    ) -> Result<(), RpcError> {
        let _structure = self.structure.lock().await;
        let (worktree, project) = self.worktree(id).await?;
        if worktree.is_primary {
            return Err(invalid(
                "the primary checkout cannot be deleted; remove the project instead",
            ));
        }
        let path = PathBuf::from(&worktree.path);
        let primary = PathBuf::from(&project.path);
        if path.exists() {
            if !force {
                let (dir, target) = (path.clone(), worktree.target.clone());
                let check =
                    blocking(move || fld_git::worktree::delete_check(&dir, target.as_deref()))
                        .await?;
                if check.dirty {
                    return Err(invalid(format!(
                        "{} has uncommitted or untracked changes; delete with force to discard them",
                        worktree.label
                    )));
                }
                if check.unpushed > 0 {
                    return Err(invalid(format!(
                        "{} has {} commit(s) that are not on any remote or on {}; delete with force to discard them",
                        worktree.label,
                        check.unpushed,
                        worktree.target.as_deref().unwrap_or("the target branch")
                    )));
                }
            }
            // A provider or routed task must not outlive the worktree.
            self.cancel_dispatches_for(id);
            self.chat.close_worktree(id).await?;
            self.clone().teardown(&worktree, &project).await;
            self.unwatch(id);
            let (repo, dir) = (primary.clone(), path.clone());
            blocking(move || fld_git::worktree::remove_worktree(&repo, &dir, force)).await?;
        } else {
            self.cancel_dispatches_for(id);
            self.chat.close_worktree(id).await?;
            self.unwatch(id);
            let (repo, dir) = (primary.clone(), path.clone());
            blocking(move || fld_git::worktree::remove_worktree(&repo, &dir, true)).await?;
        }
        if let Some(branch) = worktree.branch.clone() {
            let target = worktree.target.clone();
            let deleted = blocking(move || {
                fld_git::worktree::delete_branch(&primary, &branch, target.as_deref(), force)
            })
            .await?;
            if !deleted {
                tracing::info!(branch = ?worktree.branch, "kept unmerged branch of deleted worktree");
            }
        }
        let removed = self
            .store(move |store| store.database.remove_worktree(id))
            .await?;
        self.forget(removed).await;
        Ok(())
    }

    pub(crate) async fn set_label(
        &self,
        id: WorktreeId,
        label: String,
    ) -> Result<Worktree, RpcError> {
        let _structure = self.structure.lock().await;
        let (worktree, rev) = self
            .store(move |store| store.database.set_worktree_label(id, &label))
            .await?;
        self.tree_changed(rev);
        Ok(worktree)
    }

    /// Rename the branch checked out in a worktree, then record it durably.
    pub(crate) async fn rename_branch(
        &self,
        id: WorktreeId,
        branch: String,
    ) -> Result<Worktree, RpcError> {
        let _structure = self.structure.lock().await;
        let (worktree, project) = self.worktree(id).await?;
        if project.kind != ProjectKind::Git {
            return Err(invalid(format!(
                "{} is a folder project; it has no branch to rename",
                project.name
            )));
        }
        let (primary, name) = (PathBuf::from(&project.path), branch.clone());
        blocking(move || fld_git::worktree::validate_branch_name(&primary, &name)).await?;
        let (path, name) = (PathBuf::from(&worktree.path), branch.clone());
        blocking(move || fld_git::ops::rename_branch(&path, &name)).await?;
        let rev = self
            .store(move |store| store.database.set_worktree_branch(id, Some(&branch)))
            .await?;
        if let Some(rev) = rev {
            self.tree_changed(rev);
        }
        self.worktree(id).await.map(|(worktree, _)| worktree)
    }

    /// Change the branch this worktree diffs against.
    pub(crate) async fn set_target(
        &self,
        id: WorktreeId,
        target: Option<String>,
    ) -> Result<Worktree, RpcError> {
        let _structure = self.structure.lock().await;
        let (_, project) = self.worktree(id).await?;
        if project.kind != ProjectKind::Git {
            return Err(invalid(format!(
                "{} is a folder project; it has no target branch",
                project.name
            )));
        }
        if let Some(target) = &target {
            let (primary, name) = (PathBuf::from(&project.path), target.clone());
            blocking(move || fld_git::worktree::validate_branch_name(&primary, &name)).await?;
        }
        let rev = self
            .store(move |store| store.database.set_worktree_target(id, target.as_deref()))
            .await?;
        if let Some(rev) = rev {
            self.tree_changed(rev);
        }
        self.worktree(id).await.map(|(worktree, _)| worktree)
    }

    // --- Layouts ------------------------------------------------------------

    pub(crate) async fn scoped_layout(&self, scope: LayoutScope) -> Result<Layout, RpcError> {
        self.terminals.get_scoped_layout(scope).await
    }

    pub(crate) async fn apply_scoped_layout(
        &self,
        scope: LayoutScope,
        operation: OperationId,
        base_rev: u64,
        op: LayoutOp,
    ) -> Result<Layout, RpcError> {
        let defaults = match scope {
            LayoutScope::Home => self.terminals.launch_defaults().clone(),
            LayoutScope::Worktree(id) => {
                let (worktree, project) = self.worktree(id).await?;
                self.worktree_defaults(&worktree, &project)
            }
        };
        let layout = self
            .terminals
            .apply_scoped_layout(scope, operation, base_rev, op, defaults)
            .await?;
        self.publish(DaemonEvent::LayoutChanged {
            scope,
            rev: layout.rev,
        });
        Ok(layout)
    }

    // --- Files --------------------------------------------------------------

    pub(crate) async fn list_dir(&self, path: String) -> Result<DirListing, RpcError> {
        blocking(move || fld_git::files::list_dir(Path::new(&path), MAX_DIR_ENTRIES)).await
    }

    pub(crate) async fn read_file(
        &self,
        path: String,
        max_bytes: u32,
    ) -> Result<FileContent, RpcError> {
        let max = (max_bytes as usize).min(MAX_FILE_BYTES);
        blocking(move || fld_git::files::read_file(Path::new(&path), max)).await
    }

    pub(crate) async fn write_file(
        &self,
        path: String,
        bytes: Vec<u8>,
        expected: Option<[u8; 32]>,
    ) -> Result<[u8; 32], RpcError> {
        blocking(move || fld_git::files::write_file(Path::new(&path), &bytes, expected)).await
    }

    pub(crate) async fn file_diff(
        &self,
        id: WorktreeId,
        relative: String,
    ) -> Result<FileDiff, RpcError> {
        let (worktree, project) = self.worktree(id).await?;
        if project.kind != ProjectKind::Git {
            return Err(invalid(format!("{} is not a git project", project.name)));
        }
        // A rename's old side lives under its source path.
        let cached = self
            .statuses
            .lock()
            .expect("statuses poisoned")
            .get(&id)
            .cloned();
        let status = match cached {
            Some(status) => status,
            None => self.compute_status(&worktree, &project).await?,
        };
        let old_relative = status
            .files
            .iter()
            .find(|file| file.path == relative)
            .and_then(|file| file.old_path.clone());
        let dir = PathBuf::from(&worktree.path);
        let target = worktree.target.clone();
        let path = relative.clone();
        let (old, new) = blocking(move || {
            let base = fld_git::status::diff_base(&dir, target.as_deref())?;
            fld_git::files::diff_sides(
                &dir,
                base.as_deref(),
                &path,
                old_relative.as_deref(),
                MAX_DIFF_SIDE_BYTES,
            )
        })
        .await?;
        Ok(FileDiff {
            worktree: id,
            path: relative,
            old,
            new,
        })
    }
}

struct DiscoveredProject {
    path: PathBuf,
    kind: ProjectKind,
    default_branch: Option<String>,
    /// The primary checkout first.
    worktrees: Vec<fld_git::repo::WorktreeEntry>,
}

/// Resolve an added directory into a project: its git repository's main
/// checkout with every live worktree, or the plain folder itself.
fn discover_project(path: &Path) -> Result<DiscoveredProject, GitError> {
    let canonical = std::fs::canonicalize(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            GitError::NotFound(format!("{} does not exist", path.display()))
        } else {
            GitError::Io {
                context: format!("resolving {}", path.display()),
                source,
            }
        }
    })?;
    if !canonical.is_dir() {
        return Err(GitError::Invalid(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }
    let Some(repo) = fld_git::repo::discover(&canonical)? else {
        return Ok(DiscoveredProject {
            worktrees: vec![fld_git::repo::WorktreeEntry {
                path: canonical.clone(),
                head: None,
                branch: None,
                detached: false,
                bare: false,
                locked: false,
                prunable: false,
            }],
            path: canonical,
            kind: ProjectKind::Folder,
            default_branch: None,
        });
    };
    let mut worktrees: Vec<_> = fld_git::repo::list_worktrees(&repo.toplevel)?
        .into_iter()
        .filter(|entry| !entry.bare && !entry.prunable && entry.path.is_dir())
        .filter_map(|mut entry| {
            entry.path = std::fs::canonicalize(&entry.path).ok()?;
            Some(entry)
        })
        .collect();
    let main = worktrees
        .first()
        .map(|entry| entry.path.clone())
        .ok_or_else(|| {
            GitError::Invalid(format!(
                "{} has no checked-out worktree",
                canonical.display()
            ))
        })?;
    worktrees.dedup_by(|a, b| a.path == b.path);
    let default_branch = fld_git::repo::default_branch(&main)?;
    Ok(DiscoveredProject {
        path: main,
        kind: ProjectKind::Git,
        default_branch,
        worktrees,
    })
}
