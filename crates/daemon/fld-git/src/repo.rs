//! Repository discovery, branches, worktree listing and cloning.
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::cmd::{self, command_error, line};
use crate::{GitError, Result};

/// Where a work tree's repository lives. All paths are absolute and canonical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// Root of the work tree containing the discovered path.
    pub toplevel: PathBuf,
    /// This work tree's git directory (`.git`, or `<common>/worktrees/<name>`
    /// for a linked worktree).
    pub git_dir: PathBuf,
    /// The repository's shared git directory (refs, objects, config).
    pub common_dir: PathBuf,
}

/// Finds the work tree containing `path` (a directory, or a file whose
/// parent is used). `Ok(None)` when `path` is not inside a git work tree,
/// including bare repositories and paths inside a `.git` directory.
pub fn discover(path: &Path) -> Result<Option<RepoInfo>> {
    let canonical = canonicalize(path)?;
    let dir = if canonical.is_dir() {
        canonical
    } else {
        match canonical.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return Ok(None),
        }
    };
    let out = cmd::output(&dir, &["rev-parse", "--is-inside-work-tree"])?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("not a git repository") {
            return Ok(None);
        }
        return Err(command_error(&["rev-parse", "--is-inside-work-tree"], &out));
    }
    if line(&out.stdout) != "true" {
        return Ok(None);
    }
    let out = cmd::run(
        &dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    )?;
    let mut lines = out
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| PathBuf::from(OsStr::from_bytes(l)));
    let (Some(toplevel), Some(git_dir), Some(common_dir)) =
        (lines.next(), lines.next(), lines.next())
    else {
        return Err(GitError::Parse(format!(
            "unexpected `git rev-parse` output in {}",
            dir.display()
        )));
    };
    Ok(Some(RepoInfo {
        toplevel: canonicalize(&toplevel)?,
        git_dir: canonicalize(&git_dir)?,
        common_dir: canonicalize(&common_dir)?,
    }))
}

/// The branch new worktrees start from: `origin/HEAD`'s target (without the
/// `origin/` prefix), else a local `main`, else `master`, else the current
/// branch (also when unborn). `None` on a detached HEAD with none of those.
pub fn default_branch(repo: &Path) -> Result<Option<String>> {
    let out = cmd::output(
        repo,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )?;
    if out.status.success() {
        let target = line(&out.stdout);
        if let Some(name) = target.strip_prefix("refs/remotes/origin/")
            && !name.is_empty()
        {
            return Ok(Some(name.to_owned()));
        }
    }
    for name in ["main", "master"] {
        if branch_exists(repo, name)? {
            return Ok(Some(name.to_owned()));
        }
    }
    current_branch(repo)
}

/// The branch checked out in `worktree` (also when unborn); `None` when
/// HEAD is detached.
pub fn current_branch(worktree: &Path) -> Result<Option<String>> {
    let args = ["symbolic-ref", "--quiet", "HEAD"];
    let out = cmd::output(worktree, &args)?;
    if out.status.success() {
        let target = line(&out.stdout);
        Ok(Some(
            target
                .strip_prefix("refs/heads/")
                .map_or_else(|| target.clone(), str::to_owned),
        ))
    } else if out.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(command_error(&args, &out))
    }
}

/// Whether `refs/heads/<name>` exists.
pub(crate) fn branch_exists(repo: &Path, name: &str) -> Result<bool> {
    let full = format!("refs/heads/{name}");
    let args = ["show-ref", "--verify", "--quiet", full.as_str()];
    let out = cmd::output(repo, &args)?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(command_error(&args, &out)),
    }
}

/// One entry of `git worktree list`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    /// Commit checked out; `None` for a bare entry or an unborn branch.
    pub head: Option<String>,
    /// Short branch name (no `refs/heads/`); `None` when detached or bare.
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
}

/// Parses `git worktree list --porcelain -z`: NUL-terminated attribute
/// lines, records separated by an empty field.
pub fn parse_worktree_list(output: &[u8]) -> Result<Vec<WorktreeEntry>> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    let mut fields = output.split(|&b| b == 0).peekable();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }
        let (key, value) = match field.iter().position(|&b| b == b' ') {
            Some(i) => (&field[..i], Some(&field[i + 1..])),
            None => (field, None),
        };
        if key == b"worktree" {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            let path = value.ok_or_else(|| {
                GitError::Parse("`git worktree list` entry without a path".into())
            })?;
            current = Some(WorktreeEntry {
                path: PathBuf::from(OsStr::from_bytes(path)),
                ..WorktreeEntry::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            return Err(GitError::Parse(format!(
                "`git worktree list` attribute `{}` before any worktree",
                String::from_utf8_lossy(key)
            )));
        };
        match key {
            b"HEAD" => {
                let oid = String::from_utf8_lossy(value.unwrap_or_default()).into_owned();
                // An unborn branch reports the all-zero oid.
                if !oid.is_empty() && !oid.bytes().all(|b| b == b'0') {
                    entry.head = Some(oid);
                }
            }
            b"branch" => {
                let full = String::from_utf8_lossy(value.unwrap_or_default()).into_owned();
                entry.branch = Some(
                    full.strip_prefix("refs/heads/")
                        .map_or_else(|| full.clone(), str::to_owned),
                );
            }
            b"detached" => entry.detached = true,
            b"bare" => entry.bare = true,
            b"locked" => entry.locked = true,
            b"prunable" => entry.prunable = true,
            // Attributes added by future git versions are ignored.
            _ => {}
        }
        if fields.peek().is_none() {
            break;
        }
    }
    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    Ok(entries)
}

/// Every worktree of the repository containing `repo`; the first entry is
/// the main worktree.
pub fn list_worktrees(repo: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = cmd::run(repo, &["worktree", "list", "--porcelain", "-z"])?;
    parse_worktree_list(&out)
}

/// Clones `url` into `dest`, which must not exist; its parent must.
pub fn clone(url: &str, dest: &Path) -> Result<()> {
    if dest.symlink_metadata().is_ok() {
        return Err(GitError::Invalid(format!(
            "{} already exists",
            dest.display()
        )));
    }
    let parent = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if !parent.is_dir() {
        return Err(GitError::NotFound(format!(
            "parent directory {} does not exist",
            parent.display()
        )));
    }
    let args: [&OsStr; 4] = [
        "clone".as_ref(),
        "--".as_ref(),
        url.as_ref(),
        dest.as_os_str(),
    ];
    cmd::run(parent, &args)?;
    Ok(())
}

/// `fs::canonicalize` with a [`GitError::NotFound`] for missing paths.
pub(crate) fn canonicalize(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            GitError::NotFound(format!("{} does not exist", path.display()))
        } else {
            GitError::io(format!("cannot resolve {}", path.display()), e)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worktree_list() {
        let out = b"worktree /src/repo\0HEAD 1111111111111111111111111111111111111111\0branch refs/heads/main\0\0\
worktree /src/wt/feature x\0HEAD 2222222222222222222222222222222222222222\0branch refs/heads/flotilla/feature\0locked reason here\0\0\
worktree /src/wt/detached\0HEAD 3333333333333333333333333333333333333333\0detached\0prunable gitdir file points to non-existent location\0\0";
        let list = parse_worktree_list(out).unwrap();
        assert_eq!(
            list,
            vec![
                WorktreeEntry {
                    path: "/src/repo".into(),
                    head: Some("1".repeat(40)),
                    branch: Some("main".into()),
                    ..Default::default()
                },
                WorktreeEntry {
                    path: "/src/wt/feature x".into(),
                    head: Some("2".repeat(40)),
                    branch: Some("flotilla/feature".into()),
                    locked: true,
                    ..Default::default()
                },
                WorktreeEntry {
                    path: "/src/wt/detached".into(),
                    head: Some("3".repeat(40)),
                    detached: true,
                    prunable: true,
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn parses_bare_and_unborn_entries() {
        let out = b"worktree /src/bare.git\0bare\0\0worktree /src/new\0HEAD 0000000000000000000000000000000000000000\0branch refs/heads/main\0\0";
        let list = parse_worktree_list(out).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].bare);
        assert_eq!(list[0].head, None);
        assert_eq!(list[1].head, None);
        assert_eq!(list[1].branch.as_deref(), Some("main"));
    }

    #[test]
    fn rejects_attributes_before_a_worktree() {
        assert!(matches!(
            parse_worktree_list(b"HEAD abc\0\0"),
            Err(GitError::Parse(_))
        ));
        assert_eq!(parse_worktree_list(b"").unwrap(), vec![]);
    }
}
