//! Direct git actions on a worktree: commit, push and update from the
//! target branch, plus reading which forge repository `origin` points at.
use std::path::Path;

use crate::cmd::{self, line};
use crate::{GitError, Result};

/// Commit the index, or with `all` every change including untracked files.
/// Returns the new HEAD commit.
pub fn commit(worktree: &Path, message: &str, all: bool) -> Result<String> {
    if message.trim().is_empty() {
        return Err(GitError::Invalid("the commit message is empty".into()));
    }
    if !cmd::run(worktree, &["ls-files", "--unmerged", "-z"])?.is_empty() {
        return Err(GitError::Refused(
            "resolve and stage the conflicted files before committing".into(),
        ));
    }
    if all {
        cmd::run(worktree, &["add", "--all"])?;
    }
    let staged = cmd::output(worktree, &["diff", "--cached", "--quiet"])?;
    if staged.status.success() {
        return Err(GitError::Refused("there is nothing to commit".into()));
    }
    let out = cmd::output_with_input(
        worktree,
        &["commit", "--quiet", "--file", "-"],
        message.as_bytes(),
    )?;
    if !out.status.success() {
        return Err(cmd::command_error(&["commit"], &out));
    }
    Ok(line(&cmd::run(worktree, &["rev-parse", "HEAD"])?))
}

/// Rename the worktree's current branch. The caller validates the name
/// against the repository first; this rechecks the basic shape.
pub fn rename_branch(worktree: &Path, name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 200 || name.starts_with('-') || name.contains('\0') {
        return Err(GitError::Invalid("invalid branch name".into()));
    }
    let out = cmd::output(worktree, &["branch", "-m", name])?;
    if !out.status.success() {
        return Err(cmd::command_error(&["branch", "-m"], &out));
    }
    Ok(())
}

/// The branch's upstream (`origin/feature`), if any.
pub fn upstream(worktree: &Path) -> Result<Option<String>> {
    let out = cmd::output(
        worktree,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )?;
    Ok(out.status.success().then(|| line(&out.stdout)))
}

/// Push the current branch; the first push sets `origin/<branch>` as its
/// upstream.
pub fn push(worktree: &Path) -> Result<()> {
    let Some(branch) = crate::repo::current_branch(worktree)? else {
        return Err(GitError::Refused("cannot push a detached HEAD".into()));
    };
    if upstream(worktree)?.is_some() {
        cmd::run(worktree, &["push", "--quiet"])?;
    } else {
        cmd::run(
            worktree,
            &["push", "--quiet", "--set-upstream", "origin", &branch],
        )?;
    }
    Ok(())
}

/// Bring the target branch's latest remote commits into the worktree with a
/// merge. A conflicting merge is aborted and reported, leaving the worktree
/// as it was.
pub fn update(worktree: &Path, target: &str) -> Result<()> {
    if cmd::is_option_like(target) {
        return Err(GitError::Invalid(format!(
            "invalid target branch {target:?}"
        )));
    }
    let git_dir = cmd::run(worktree, &["rev-parse", "--absolute-git-dir"])?;
    let git_dir = std::path::PathBuf::from(line(&git_dir));
    if [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ]
    .iter()
    .any(|name| git_dir.join(name).exists())
    {
        return Err(GitError::Refused(
            "finish or abort the current merge, rebase or cherry-pick before updating".into(),
        ));
    }
    let dirty = !cmd::run(worktree, &["status", "--porcelain", "--untracked-files=no"])?.is_empty();
    if dirty {
        return Err(GitError::Refused(
            "commit or stash your changes before updating the branch".into(),
        ));
    }
    cmd::run(worktree, &["fetch", "--quiet", "origin", target])?;
    let merged = cmd::output(
        worktree,
        &["merge", "--no-edit", "--quiet", &format!("origin/{target}")],
    )?;
    if merged.status.success() {
        return Ok(());
    }
    let _ = cmd::output(worktree, &["merge", "--abort"]);
    Err(GitError::Refused(format!(
        "merging origin/{target} conflicts; resolve it in a terminal"
    )))
}

/// `owner/repo` of a GitHub `origin`, if it is one. Reads the configured
/// URL itself: `remote get-url` applies `insteadOf` rewrites (mirrors,
/// proxies), which do not change which repository the forge knows.
pub fn github_repository(worktree: &Path) -> Result<Option<(String, String)>> {
    let out = cmd::output(worktree, &["config", "--get", "remote.origin.url"])?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(parse_github_url(&line(&out.stdout)))
}

/// Parses `https://github.com/o/r(.git)`, `git@github.com:o/r(.git)` and
/// `ssh://git@github.com/o/r(.git)`.
pub fn parse_github_url(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/'))
        .then(|| (owner.to_string(), repo.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls_in_every_common_form() {
        let want = Some(("The-Coaxys".to_string(), "flotilla".to_string()));
        for url in [
            "https://github.com/The-Coaxys/flotilla.git",
            "https://github.com/The-Coaxys/flotilla",
            "https://github.com/The-Coaxys/flotilla/",
            "git@github.com:The-Coaxys/flotilla.git",
            "ssh://git@github.com/The-Coaxys/flotilla.git",
        ] {
            assert_eq!(parse_github_url(url), want, "{url}");
        }
        assert_eq!(parse_github_url("https://gitlab.com/a/b.git"), None);
        assert_eq!(parse_github_url("https://github.com/only-owner"), None);
        assert_eq!(parse_github_url("https://github.com/a/b/c"), None);
    }
}
