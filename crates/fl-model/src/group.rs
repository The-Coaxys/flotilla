//! Branch groups and their shared context (spec §6).
//!
//! A branch group is one root worktree plus lazily created children. The root
//! holds `FEATURE.md`, `AGENTS.md`/`CLAUDE.md` and a `context/` directory;
//! a child receives a copy of that context when it is created, so an edit in
//! any member shows up in the group's diff against the group base.
//!
//! Every collection here is bounded before it crosses the wire.
use crate::workspace::{FileChange, Worktree};
use crate::{ProjectId, WorktreeId};
use serde::{Deserialize, Serialize};

/// Most worktrees one branch group may hold, root included.
pub const MAX_GROUP_MEMBERS: usize = 64;
/// Most shared-context entries one root may report.
pub const MAX_SHARED_ENTRIES: usize = 256;
/// Longest relative shared-context path.
pub const MAX_SHARED_PATH_BYTES: usize = 1024;
/// Changed files listed per member of a grouped diff.
pub const MAX_GROUP_CHILD_FILES: usize = 512;
/// Changed files listed across a whole grouped diff.
pub const MAX_GROUP_FILES: usize = 4000;
/// Longest worktree-relative path in a grouped diff.
pub const MAX_GROUP_PATH_BYTES: usize = 4096;
pub const MAX_GROUP_LABEL_BYTES: usize = 1024;
pub const MAX_GROUP_BASE_BYTES: usize = 1024;

/// The file a branch group always carries for its feature description.
pub const SHARED_FEATURE: &str = "FEATURE.md";
/// Repository guidance read by agents, in either spelling.
pub const SHARED_AGENTS: &str = "AGENTS.md";
pub const SHARED_CLAUDE: &str = "CLAUDE.md";
/// Directory for free-form context a child inherits.
pub const SHARED_CONTEXT_DIR: &str = "context";

/// What a group root keeps and what a child received from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedContext {
    /// Absolute path of the worktree this snapshot describes.
    pub root: String,
    /// Shared paths present, relative and sorted.
    pub present: Vec<String>,
    /// Required shared paths that are absent. `AGENTS.md` is reported once
    /// when neither `AGENTS.md` nor `CLAUDE.md` exists.
    pub missing: Vec<String>,
}

impl SharedContext {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.present.len() > MAX_SHARED_ENTRIES || self.missing.len() > MAX_SHARED_ENTRIES {
            return Err("too many shared-context entries");
        }
        for path in self.present.iter().chain(self.missing.iter()) {
            if path.is_empty() || path.len() > MAX_SHARED_PATH_BYTES || path.contains('\0') {
                return Err("invalid shared-context path");
            }
        }
        Ok(())
    }
}

/// One group root, its base and its members (the root first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchGroup {
    pub root: WorktreeId,
    pub project: ProjectId,
    pub label: String,
    /// The branch every member diffs against, from the root's target.
    pub base: Option<String>,
    /// The root, then children in creation order.
    pub members: Vec<WorktreeId>,
}

impl BranchGroup {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.members.is_empty() || self.members.len() > MAX_GROUP_MEMBERS {
            return Err("invalid branch group size");
        }
        if self.members[0] != self.root {
            return Err("branch group members must start with the root");
        }
        if self
            .members
            .iter()
            .enumerate()
            .any(|(index, id)| self.members[..index].contains(id))
        {
            return Err("duplicate branch group member");
        }
        if self.label.is_empty() || self.label.len() > MAX_GROUP_LABEL_BYTES {
            return Err("invalid branch group label");
        }
        if let Some(base) = &self.base
            && (base.is_empty() || base.len() > MAX_GROUP_BASE_BYTES || base.contains('\0'))
        {
            return Err("invalid branch group base");
        }
        Ok(())
    }

    pub fn children(&self) -> &[WorktreeId] {
        self.members.get(1..).unwrap_or_default()
    }
}

/// One member's contribution to the group diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupChildDiff {
    pub worktree: WorktreeId,
    pub label: String,
    pub branch: Option<String>,
    pub is_root: bool,
    /// Line totals against the group base.
    pub added: u64,
    pub removed: u64,
    pub dirty: bool,
    pub conflicted: bool,
    pub files: Vec<FileChange>,
}

/// The repository summary of a branch group, per member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupDiff {
    pub group: BranchGroup,
    /// Totals over every member, against the group base.
    pub added: u64,
    pub removed: u64,
    pub changed_files: usize,
    /// Root first, then children in creation order.
    pub children: Vec<GroupChildDiff>,
    /// Daemon wall clock at computation, milliseconds since the epoch.
    pub at_ms: u64,
}

impl GroupDiff {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.group.validate()?;
        if self.children.len() != self.group.members.len() {
            return Err("group diff does not cover every member");
        }
        let mut changed = 0usize;
        for (index, child) in self.children.iter().enumerate() {
            if child.worktree != self.group.members[index] {
                return Err("group diff members are out of order");
            }
            if child.label.len() > MAX_GROUP_LABEL_BYTES {
                return Err("invalid group diff label");
            }
            if child.files.len() > MAX_GROUP_CHILD_FILES {
                return Err("too many group diff files");
            }
            changed += child.files.len();
            for file in &child.files {
                if file.path.len() > MAX_GROUP_PATH_BYTES || file.path.contains('\0') {
                    return Err("invalid group diff path");
                }
            }
        }
        if changed > MAX_GROUP_FILES || changed != self.changed_files {
            return Err("group diff file totals disagree");
        }
        Ok(())
    }
}

/// A lazily created child and the shared context it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeChild {
    pub worktree: Worktree,
    /// False when the group already had its child and this call returned it.
    pub created: bool,
    pub shared: SharedContext,
}

impl WorktreeChild {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.shared.validate()
    }
}
