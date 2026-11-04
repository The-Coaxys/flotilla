//! Branch groups: shared context materialization and grouped diffs (spec §6).
//!
//! A group root holds `FEATURE.md`, `AGENTS.md`/`CLAUDE.md` and `context/`.
//! `ensure-child` creates the group's one child lazily, copies the shared
//! context into it, and is idempotent. Grouped diffs are computed by the
//! daemon from the group base (the root's target, else the project default)
//! so the client only renders the numbers.
use std::path::{Path, PathBuf};

use super::{WorkspaceService, blocking, internal, invalid, now_ms, rpc};
use fl_model::WorktreeId;
use fl_model::group::{
    BranchGroup, GroupChildDiff, GroupDiff, MAX_GROUP_CHILD_FILES, SHARED_AGENTS, SHARED_CLAUDE,
    SHARED_CONTEXT_DIR, SHARED_FEATURE, SharedContext, WorktreeChild,
};
use fl_model::workspace::ProjectKind;
use fl_proto::validation::MAX_FILE_BYTES;
use fl_proto::{RpcError, RpcErrorCode};

/// Shared context never copies more than this many files or bytes.
const MAX_SHARED_FILES: usize = 256;
const MAX_SHARED_BYTES: u64 = 16 * 1024 * 1024;

const FEATURE_TEMPLATE: &str = "\
# Feature

<!-- One paragraph: what this branch group builds and why. -->
";

const GUIDANCE_TEMPLATE: &str = "\
# Repository guidance

This worktree belongs to a Flotilla branch group. Its root holds `FEATURE.md`,
this guidance file and a `context/` directory; every child worktree starts with
a copy of them.

- Record the feature and its acceptance criteria in `FEATURE.md`.
- Put durable notes, plans and references under `context/`.
- Commit the context with the work when it helps reviewers.
";

impl WorkspaceService {
    /// Register the group root when needed, create its child lazily, and copy
    /// the shared context into the child. Calling it again returns the same
    /// child with `created: false`.
    pub(crate) async fn ensure_child(
        self: &std::sync::Arc<Self>,
        id: WorktreeId,
    ) -> Result<WorktreeChild, RpcError> {
        let _structure = self.structure.lock().await;
        let (worktree, project) = self.worktree(id).await?;
        if project.kind != ProjectKind::Git {
            return Err(invalid(format!(
                "{} is a folder project; branch groups need a git repository",
                project.name
            )));
        }
        let root_path = PathBuf::from(&worktree.path);
        if !root_path.is_dir() {
            return Err(rpc(
                RpcErrorCode::NotFound,
                format!("{} is missing from disk", worktree.path),
            ));
        }
        // The root's skeleton is created once; existing files are untouched.
        let root_dir = root_path.clone();
        let materialized = tokio::task::spawn_blocking(move || materialize(&root_dir))
            .await
            .map_err(|error| internal(format!("background task failed: {error}")))?
            .map_err(|error| internal(format!("preparing shared context: {error}")))?;
        for path in &materialized {
            tracing::info!(path = %path, "created shared context entry");
        }
        // The first call designates the caller as the group root; a call from
        // a child resolves to that child's existing group.
        let group = self
            .store(move |store| store.database.register_group_root(id))
            .await
            .map(|(group, _)| group)?;
        if let Some(child) = group.members.get(1) {
            let child = self.worktree(*child).await?.0;
            let shared = shared_snapshot(Path::new(&child.path)).await?;
            return Ok(WorktreeChild {
                worktree: child,
                created: false,
                shared,
            });
        }
        let child = self
            .create_worktree_locked(
                project.id,
                None,
                group
                    .base
                    .clone()
                    .or_else(|| project.default_branch.clone()),
            )
            .await?;
        let registered = self
            .store(move |store| store.database.add_group_member(group.root, child.id))
            .await?;
        debug_assert_eq!(registered.root, group.root);
        // Copy the root's context into the child, never overwriting it.
        let root_for_copy = root_path.clone();
        let child_path = PathBuf::from(&child.path);
        let copied =
            tokio::task::spawn_blocking(move || copy_shared_context(&root_for_copy, &child_path))
                .await
                .map_err(|error| internal(format!("background task failed: {error}")))?
                .map_err(|error| internal(format!("copying shared context: {error}")))?;
        for path in &copied {
            tracing::info!(path = %path, child = %child.path, "copied shared context");
        }
        let shared = shared_snapshot(Path::new(&child.path)).await?;
        Ok(WorktreeChild {
            worktree: child,
            created: true,
            shared,
        })
    }

    /// The branch group a worktree belongs to, normalized to its root.
    pub(crate) async fn worktree_group(&self, id: WorktreeId) -> Result<BranchGroup, RpcError> {
        let (worktree, _) = self.worktree(id).await?;
        match self
            .store(move |store| store.database.worktree_group(id))
            .await?
        {
            Some(group) => Ok(group),
            None => Err(rpc(
                RpcErrorCode::NotFound,
                format!(
                    "{} is not part of a branch group; run `fl worktree ensure-child`",
                    worktree.label
                ),
            )),
        }
    }

    /// The group's diff against its base: a repository summary plus every
    /// member's changed files.
    pub(crate) async fn group_diff(&self, id: WorktreeId) -> Result<GroupDiff, RpcError> {
        let (_, project) = self.worktree(id).await?;
        if project.kind != ProjectKind::Git {
            return Err(invalid(format!(
                "{} is a folder project; it has no diff",
                project.name
            )));
        }
        let group = self.worktree_group(id).await?;
        let tree = self.tree().await?;
        let base = group
            .base
            .clone()
            .or_else(|| project.default_branch.clone());
        let mut children = Vec::with_capacity(group.members.len());
        let (mut added, mut removed, mut changed) = (0u64, 0u64, 0usize);
        for member in &group.members {
            let worktree = tree
                .worktrees
                .iter()
                .find(|candidate| candidate.id == *member)
                .ok_or_else(|| {
                    rpc(
                        RpcErrorCode::NotFound,
                        format!("branch group member {member} is not a known worktree"),
                    )
                })?;
            let path = PathBuf::from(&worktree.path);
            let target = base.clone();
            let (summary, files) = blocking(move || {
                fld_git::status::status(&path, target.as_deref(), MAX_GROUP_CHILD_FILES)
            })
            .await?;
            added += summary.added;
            removed += summary.removed;
            changed += files.len();
            children.push(GroupChildDiff {
                worktree: worktree.id,
                label: worktree.label.clone(),
                branch: worktree.branch.clone(),
                is_root: worktree.id == group.root,
                added: summary.added,
                removed: summary.removed,
                dirty: summary.dirty,
                conflicted: summary.conflicted,
                files,
            });
        }
        Ok(GroupDiff {
            group,
            added,
            removed,
            changed_files: changed,
            children,
            at_ms: now_ms(),
        })
    }
}

/// How a worktree's own shared context looks right now.
async fn shared_snapshot(root: &Path) -> Result<SharedContext, RpcError> {
    let root = root.to_path_buf();
    let snapshot = tokio::task::spawn_blocking(move || snapshot(&root))
        .await
        .map_err(|error| internal(format!("background task failed: {error}")))?
        .map_err(|error| internal(format!("reading shared context: {error}")))?;
    Ok(snapshot)
}

fn snapshot(root: &Path) -> std::io::Result<SharedContext> {
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for name in [SHARED_FEATURE, SHARED_AGENTS, SHARED_CLAUDE] {
        let path = root.join(name);
        if path.is_file() {
            present.push(name.to_owned());
        }
    }
    if root.join(SHARED_CONTEXT_DIR).is_dir() {
        present.push(SHARED_CONTEXT_DIR.to_owned());
        for entry in read_relative(root, Path::new(SHARED_CONTEXT_DIR), MAX_SHARED_FILES)? {
            present.push(entry);
        }
    } else {
        missing.push(SHARED_CONTEXT_DIR.to_owned());
    }
    if !root.join(SHARED_FEATURE).is_file() {
        missing.push(SHARED_FEATURE.to_owned());
    }
    if !root.join(SHARED_AGENTS).is_file() && !root.join(SHARED_CLAUDE).is_file() {
        missing.push(SHARED_AGENTS.to_owned());
    }
    present.sort();
    missing.sort();
    Ok(SharedContext {
        root: root.display().to_string(),
        present,
        missing,
    })
}

/// Create the root's shared-context skeleton, never replacing a file.
fn materialize(root: &Path) -> std::io::Result<Vec<String>> {
    let mut created = Vec::new();
    for (name, text) in [
        (SHARED_FEATURE, FEATURE_TEMPLATE),
        (SHARED_AGENTS, GUIDANCE_TEMPLATE),
    ] {
        let path = root.join(name);
        // Either spelling of agent guidance satisfies the skeleton.
        if name == SHARED_AGENTS && root.join(SHARED_CLAUDE).exists() {
            continue;
        }
        if !path.exists() {
            std::fs::write(&path, text)?;
            created.push(name.to_owned());
        }
    }
    let context = root.join(SHARED_CONTEXT_DIR);
    if !context.is_dir() {
        std::fs::create_dir_all(&context)?;
        created.push(SHARED_CONTEXT_DIR.to_owned());
    }
    Ok(created)
}

/// Copy the root's shared context into a fresh child. Existing child files
/// win: materialization never overwrites work. Returns copied paths.
fn copy_shared_context(root: &Path, child: &Path) -> std::io::Result<Vec<String>> {
    let mut copied = Vec::new();
    let mut bytes = 0u64;
    for name in [SHARED_FEATURE, SHARED_AGENTS, SHARED_CLAUDE] {
        let source = root.join(name);
        if !source.is_file() {
            continue;
        }
        bytes += copy_file(&source, &child.join(name))?;
        copied.push(name.to_owned());
    }
    let context = root.join(SHARED_CONTEXT_DIR);
    if !context.is_dir() {
        return Ok(copied);
    }
    let target = child.join(SHARED_CONTEXT_DIR);
    std::fs::create_dir_all(&target)?;
    for relative in read_relative(root, Path::new(SHARED_CONTEXT_DIR), MAX_SHARED_FILES)? {
        let source = root.join(&relative);
        let destination = child.join(&relative);
        if destination.is_dir() {
            continue;
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        bytes += copy_file(&source, &destination)?;
        copied.push(relative);
    }
    if bytes > MAX_SHARED_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!(
                "shared context exceeds {} MiB",
                MAX_SHARED_BYTES / 1024 / 1024
            ),
        ));
    }
    Ok(copied)
}

fn copy_file(source: &Path, destination: &Path) -> std::io::Result<u64> {
    if destination.exists() {
        return Ok(0);
    }
    let metadata = std::fs::metadata(source)?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("{} is too large to copy", source.display()),
        ));
    }
    std::fs::copy(source, destination)
}

/// Regular files below `root/dir`, relative and sorted. Symlinks, special
/// files and nested directories are skipped; the walk is entry-bounded.
fn read_relative(root: &Path, dir: &Path, limit: usize) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(relative_dir) = stack.pop() {
        let mut entries: Vec<_> =
            std::fs::read_dir(root.join(&relative_dir))?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let relative = relative_dir.join(entry.file_name());
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                stack.push(relative);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            if out.len() >= limit {
                return Ok(out);
            }
            out.push(relative.to_string_lossy().into_owned());
        }
    }
    out.sort();
    Ok(out)
}
