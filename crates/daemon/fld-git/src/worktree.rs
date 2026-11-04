//! Creating, preparing and deleting linked worktrees.
use std::ffi::OsStr;
use std::path::{Component, Path};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::cmd::{self, command_error, line};
use crate::repo::branch_exists;
use crate::watch::IGNORED_DIRS;
use crate::{GitError, Result};

/// File at the primary checkout's root listing (gitignore syntax) the
/// untracked files copied into every new worktree, e.g. `.env`.
pub const WORKTREE_INCLUDE_FILE: &str = ".worktreeinclude";

const ADJECTIVES: &[&str] = &[
    "amber", "bold", "brisk", "calm", "clever", "crisp", "daring", "eager", "fancy", "gentle",
    "glad", "golden", "happy", "humble", "jolly", "keen", "lively", "lucky", "mellow", "nimble",
    "proud", "quick", "quiet", "rapid", "shiny", "silent", "steady", "sunny", "swift", "tidy",
    "vivid", "witty",
];

const NOUNS: &[&str] = &[
    "anchor",
    "beacon",
    "breeze",
    "canyon",
    "comet",
    "coral",
    "delta",
    "ember",
    "falcon",
    "fjord",
    "forest",
    "galaxy",
    "harbor",
    "island",
    "lagoon",
    "lantern",
    "meadow",
    "nebula",
    "orbit",
    "otter",
    "pebble",
    "pine",
    "quartz",
    "reef",
    "river",
    "sparrow",
    "summit",
    "tide",
    "valley",
    "willow",
    "zephyr",
    "lighthouse",
];

/// A fresh branch name for a new worktree:
/// `flotilla/<adjective>-<noun>-<4 lowercase hex>`.
pub fn temporary_branch_name() -> String {
    let mut random = [0u8; 4];
    if getrandom::fill(&mut random).is_err() {
        // Uniqueness is re-checked by `add_worktree`; the clock is enough.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        random = nanos.to_le_bytes();
    }
    let adjective = ADJECTIVES[usize::from(random[0]) % ADJECTIVES.len()];
    let noun = NOUNS[usize::from(random[1]) % NOUNS.len()];
    format!(
        "flotilla/{adjective}-{noun}-{:02x}{:02x}",
        random[2], random[3]
    )
}

/// A directory name for `branch`: `/` and everything outside
/// `[A-Za-z0-9._-]` become `-`, runs of `-` collapse, leading and trailing
/// `-`/`.` are trimmed; an empty result is `worktree`.
pub fn slug(branch: &str) -> String {
    let mut out = String::with_capacity(branch.len());
    for c in branch.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let trimmed = out.trim_matches(['-', '.']);
    if trimmed.is_empty() {
        "worktree".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Checks `name` with `git check-ref-format --branch`.
pub fn validate_branch_name(repo: &Path, name: &str) -> Result<()> {
    let invalid = || GitError::Invalid(format!("`{name}` is not a valid branch name"));
    // `-x` would be read as an option, `@{-1}` is expanded by `--branch`
    // and `HEAD` is refused by `git branch`.
    if name.is_empty() || cmd::is_option_like(name) || name.contains("@{") || name == "HEAD" {
        return Err(invalid());
    }
    let out = cmd::output(repo, &["check-ref-format", "--branch", name])?;
    if out.status.success() {
        Ok(())
    } else if matches!(out.status.code(), Some(1 | 128)) {
        Err(invalid())
    } else {
        Err(command_error(&["check-ref-format", "--branch", name], &out))
    }
}

/// Creates a worktree at `path` on a new branch `branch` starting at `base`.
/// The new branch does not track `base`.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if path.symlink_metadata().is_ok() {
        return Err(GitError::Invalid(format!(
            "{} already exists",
            path.display()
        )));
    }
    validate_branch_name(repo, branch)?;
    if branch_exists(repo, branch)? {
        return Err(GitError::Invalid(format!(
            "branch `{branch}` already exists"
        )));
    }
    if cmd::resolve_commit(repo, base)?.is_none() {
        return Err(GitError::NotFound(format!(
            "`{base}` does not name a commit"
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| GitError::io(format!("cannot create {}", parent.display()), e))?;
    }
    let args: [&OsStr; 7] = [
        "worktree".as_ref(),
        "add".as_ref(),
        "--no-track".as_ref(),
        "-b".as_ref(),
        branch.as_ref(),
        path.as_os_str(),
        base.as_ref(),
    ];
    cmd::run(repo, &args)?;
    Ok(())
}

/// Whether a `.worktreeinclude` pattern names the directory `name`, so that
/// an otherwise skipped heavy directory must be walked.
fn patterns_name(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| {
        p.trim_start_matches('!')
            .split('/')
            .any(|component| component == name)
    })
}

/// Copies the untracked files matched by the primary checkout's
/// `.worktreeinclude` into `worktree`, skipping files that already exist
/// there. Returns the copied paths relative to the root, sorted.
pub fn copy_includes(primary: &Path, worktree: &Path) -> Result<Vec<String>> {
    let include = primary.join(WORKTREE_INCLUDE_FILE);
    let text = match std::fs::read_to_string(&include) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(GitError::io(
                format!("cannot read {}", include.display()),
                e,
            ));
        }
    };
    let mut builder = GitignoreBuilder::new(primary);
    let mut patterns = Vec::new();
    for raw in text.lines() {
        let pattern = raw.trim_end();
        if pattern.is_empty() || pattern.starts_with('#') {
            continue;
        }
        builder.add_line(None, pattern).map_err(|e| {
            GitError::Invalid(format!(
                "{}: invalid pattern `{pattern}`: {e}",
                include.display()
            ))
        })?;
        patterns.push(pattern.to_owned());
    }
    let matcher = builder
        .build()
        .map_err(|e| GitError::Invalid(format!("{}: {e}", include.display())))?;
    let mut copied = Vec::new();
    let mut walker = IncludeWalker {
        primary,
        worktree,
        matcher: &matcher,
        patterns: &patterns,
        copied: &mut copied,
    };
    walker.walk(Path::new(""))?;
    copied.sort();
    Ok(copied)
}

struct IncludeWalker<'a> {
    primary: &'a Path,
    worktree: &'a Path,
    matcher: &'a Gitignore,
    patterns: &'a [String],
    copied: &'a mut Vec<String>,
}

impl IncludeWalker<'_> {
    fn walk(&mut self, relative: &Path) -> Result<()> {
        let dir = self.primary.join(relative);
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| GitError::io(format!("cannot read {}", dir.display()), e))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| GitError::io(format!("cannot read {}", dir.display()), e))?;
            let name = entry.file_name();
            let child = relative.join(&name);
            let file_type = entry
                .file_type()
                .map_err(|e| GitError::io(format!("cannot inspect {}", child.display()), e))?;
            if file_type.is_dir() {
                if name == ".git" {
                    continue;
                }
                let name = name.to_string_lossy();
                if IGNORED_DIRS.contains(&name.as_ref()) && !patterns_name(self.patterns, &name) {
                    continue;
                }
                self.walk(&child)?;
            } else if file_type.is_file()
                && self
                    .matcher
                    .matched_path_or_any_parents(&child, false)
                    .is_ignore()
            {
                self.copy(&child)?;
            }
        }
        Ok(())
    }

    fn copy(&mut self, relative: &Path) -> Result<()> {
        let dest = self.worktree.join(relative);
        if dest.symlink_metadata().is_ok() {
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| GitError::io(format!("cannot create {}", parent.display()), e))?;
        }
        let source = self.primary.join(relative);
        // `fs::copy` also copies the permission bits.
        std::fs::copy(&source, &dest).map_err(|e| {
            GitError::io(
                format!("cannot copy {} to {}", source.display(), dest.display()),
                e,
            )
        })?;
        self.copied.push(slash_path(relative));
        Ok(())
    }
}

fn slash_path(relative: &Path) -> String {
    relative
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Runs `git submodule update --init --recursive` when the worktree has a
/// `.gitmodules`; returns whether it ran.
pub fn init_submodules(worktree: &Path) -> Result<bool> {
    if !worktree.join(".gitmodules").is_file() {
        return Ok(false);
    }
    cmd::run(worktree, &["submodule", "update", "--init", "--recursive"])?;
    Ok(true)
}

/// What deleting a worktree would lose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeleteCheck {
    /// Uncommitted changes, including untracked files.
    pub dirty: bool,
    /// Commits on HEAD that are on no remote branch (nor on `target`).
    pub unpushed: u32,
}

/// Reports whether `worktree` has uncommitted changes and how many commits
/// would exist nowhere else once it is gone.
pub fn delete_check(worktree: &Path, target: Option<&str>) -> Result<DeleteCheck> {
    let status = cmd::run(
        worktree,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )?;
    let dirty = !crate::status::parse_porcelain_v2(&status)?
        .entries
        .is_empty();
    if cmd::resolve_commit(worktree, "HEAD")?.is_none() {
        return Ok(DeleteCheck { dirty, unpushed: 0 });
    }
    let mut args = vec!["rev-list", "--count", "HEAD", "--not", "--remotes"];
    let resolved_target = match target {
        Some(t) => cmd::resolve_commit(worktree, t)?,
        None => None,
    };
    if let Some(oid) = resolved_target.as_deref() {
        args.push(oid);
    }
    let out = cmd::run(worktree, &args)?;
    let count = line(&out);
    let unpushed = count.parse().map_err(|_| {
        GitError::Parse(format!(
            "unexpected `git rev-list --count` output `{count}`"
        ))
    })?;
    Ok(DeleteCheck { dirty, unpushed })
}

/// Removes the linked worktree at `path`. `force` also discards local
/// changes and overrides a lock. A directory that is already gone is pruned.
pub fn remove_worktree(repo: &Path, path: &Path, force: bool) -> Result<()> {
    if path.symlink_metadata().is_err() {
        cmd::run(repo, &["worktree", "prune"])?;
        return Ok(());
    }
    let mut args: Vec<&OsStr> = vec!["worktree".as_ref(), "remove".as_ref()];
    if force {
        args.extend([OsStr::new("--force"), OsStr::new("--force")]);
    }
    args.push(path.as_os_str());
    cmd::run(repo, &args)?;
    Ok(())
}

/// Deletes local `branch`. With `force` it always goes; otherwise only when
/// it is fully merged into `target` (or `origin/<target>`, else the primary
/// checkout's HEAD). Returns whether it was deleted; a missing branch is
/// `Ok(false)`.
pub fn delete_branch(repo: &Path, branch: &str, target: Option<&str>, force: bool) -> Result<bool> {
    if branch.is_empty() || cmd::is_option_like(branch) || !branch_exists(repo, branch)? {
        return Ok(false);
    }
    if !force {
        let mut into = None;
        if let Some(t) = target {
            for candidate in [t.to_owned(), format!("origin/{t}")] {
                if let Some(oid) = cmd::resolve_commit(repo, &candidate)? {
                    into = Some(oid);
                    break;
                }
            }
        }
        let into = match into {
            Some(oid) => oid,
            None => match cmd::resolve_commit(repo, "HEAD")? {
                Some(oid) => oid,
                None => return Ok(false),
            },
        };
        let full = format!("refs/heads/{branch}");
        let args = ["merge-base", "--is-ancestor", full.as_str(), into.as_str()];
        let out = cmd::output(repo, &args)?;
        match out.status.code() {
            Some(0) => {}
            Some(1) => return Ok(false),
            _ => return Err(command_error(&args, &out)),
        }
    }
    cmd::run(repo, &["branch", "-D", branch])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_branch_names() {
        assert_eq!(
            slug("flotilla/brisk-otter-1a2b"),
            "flotilla-brisk-otter-1a2b"
        );
        assert_eq!(slug("feature//x y"), "feature-x-y");
        assert_eq!(slug("--a__b..c--"), "a__b..c");
        assert_eq!(slug(".hidden/"), "hidden");
        assert_eq!(slug("ünïcode"), "n-code");
        assert_eq!(slug("///"), "worktree");
        assert_eq!(slug(""), "worktree");
        assert_eq!(slug("..."), "worktree");
    }

    #[test]
    fn temporary_branch_names_are_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let name = temporary_branch_name();
            let rest = name.strip_prefix("flotilla/").expect(&name);
            let parts: Vec<&str> = rest.split('-').collect();
            assert_eq!(parts.len(), 3, "{name}");
            assert!(ADJECTIVES.contains(&parts[0]), "{name}");
            assert!(NOUNS.contains(&parts[1]), "{name}");
            assert_eq!(parts[2].len(), 4, "{name}");
            assert!(
                parts[2]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{name}"
            );
            assert_eq!(slug(&name), name.replace('/', "-"));
            seen.insert(name);
        }
        assert!(seen.len() > 1);
    }

    #[test]
    fn pattern_names_heavy_dirs() {
        let patterns = vec!["node_modules/.cache/keep".to_owned(), "!target".to_owned()];
        assert!(patterns_name(&patterns, "node_modules"));
        assert!(patterns_name(&patterns, "target"));
        assert!(!patterns_name(&patterns, "dist"));
        assert!(!patterns_name(
            &["node_modules_x".to_owned()],
            "node_modules"
        ));
    }
}
