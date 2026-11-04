mod common;

use common::{git, repo_with, tempdir};
use fld_git::GitError;
use fld_git::repo::{clone, current_branch, default_branch, discover, list_worktrees};
use fld_git::worktree::add_worktree;

#[test]
fn discovers_work_trees() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("src/lib.rs", "")]);

    let info = discover(&repo.join("src")).unwrap().unwrap();
    assert_eq!(info.toplevel, repo);
    assert_eq!(info.git_dir, repo.join(".git"));
    assert_eq!(info.common_dir, repo.join(".git"));
    // A file resolves through its directory.
    assert_eq!(discover(&repo.join("src/lib.rs")).unwrap().unwrap(), info);

    let plain = dir.join("plain");
    std::fs::create_dir(&plain).unwrap();
    assert_eq!(discover(&plain).unwrap(), None);
    assert_eq!(discover(&repo.join(".git")).unwrap(), None);

    let bare = dir.join("bare.git");
    git(&dir, &["init", "-q", "--bare", bare.to_str().unwrap()]);
    assert_eq!(discover(&bare).unwrap(), None);

    assert!(matches!(
        discover(&dir.join("missing")),
        Err(GitError::NotFound(_))
    ));

    let linked = dir.join("wt/linked");
    add_worktree(&repo, &linked, "linked", "main").unwrap();
    let info = discover(&linked).unwrap().unwrap();
    assert_eq!(info.toplevel, linked);
    assert_eq!(info.git_dir, repo.join(".git/worktrees/linked"));
    assert_eq!(info.common_dir, repo.join(".git"));
}

#[test]
fn default_branch_prefers_origin_head() {
    let (_tmp, dir) = tempdir();
    let upstream = dir.join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "trunk"]);
    git(
        &upstream,
        &["commit", "-q", "--allow-empty", "-m", "initial"],
    );
    git(&upstream, &["branch", "main"]);

    let cloned = dir.join("cloned");
    clone(upstream.to_str().unwrap(), &cloned).unwrap();
    assert_eq!(default_branch(&cloned).unwrap().as_deref(), Some("trunk"));
    git(&cloned, &["checkout", "-q", "-b", "elsewhere"]);
    assert_eq!(default_branch(&cloned).unwrap().as_deref(), Some("trunk"));
}

#[test]
fn default_branch_falls_back_to_main_master_then_current() {
    let (_tmp, dir) = tempdir();

    let with_main = repo_with(&dir, "with-main", &[]);
    git(&with_main, &["checkout", "-q", "-b", "dev"]);
    assert_eq!(default_branch(&with_main).unwrap().as_deref(), Some("main"));

    let with_master = dir.join("with-master");
    std::fs::create_dir(&with_master).unwrap();
    git(&with_master, &["init", "-q", "-b", "master"]);
    git(
        &with_master,
        &["commit", "-q", "--allow-empty", "-m", "initial"],
    );
    git(&with_master, &["checkout", "-q", "-b", "dev"]);
    assert_eq!(
        default_branch(&with_master).unwrap().as_deref(),
        Some("master")
    );

    let current = dir.join("current");
    std::fs::create_dir(&current).unwrap();
    git(&current, &["init", "-q", "-b", "trunk"]);
    // Unborn: the current branch still counts.
    assert_eq!(default_branch(&current).unwrap().as_deref(), Some("trunk"));
    assert_eq!(current_branch(&current).unwrap().as_deref(), Some("trunk"));
    git(
        &current,
        &["commit", "-q", "--allow-empty", "-m", "initial"],
    );
    assert_eq!(default_branch(&current).unwrap().as_deref(), Some("trunk"));

    git(&current, &["checkout", "-q", "--detach"]);
    assert_eq!(current_branch(&current).unwrap(), None);
    assert_eq!(default_branch(&current).unwrap(), None);
}

#[test]
fn current_branch_keeps_slashes() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[]);
    git(
        &repo,
        &["checkout", "-q", "-b", "flotilla/brisk-otter-0a1b"],
    );
    assert_eq!(
        current_branch(&repo).unwrap().as_deref(),
        Some("flotilla/brisk-otter-0a1b")
    );
}

#[test]
fn lists_worktrees() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("a", "a\n")]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let linked = dir.join("worktrees/feature x");
    add_worktree(&repo, &linked, "feature/x", "main").unwrap();
    let detached = dir.join("worktrees/detached");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().unwrap(),
        ],
    );

    let list = list_worktrees(&linked).unwrap();
    assert_eq!(list.len(), 3, "{list:?}");
    assert_eq!(list[0].path, repo);
    assert_eq!(list[0].branch.as_deref(), Some("main"));
    assert_eq!(list[0].head.as_deref(), Some(head.as_str()));
    assert!(!list[0].detached && !list[0].bare);
    let feature = list.iter().find(|w| w.path == linked).unwrap();
    assert_eq!(feature.branch.as_deref(), Some("feature/x"));
    assert_eq!(feature.head.as_deref(), Some(head.as_str()));
    let det = list.iter().find(|w| w.path == detached).unwrap();
    assert!(det.detached);
    assert_eq!(det.branch, None);

    git(&repo, &["worktree", "lock", detached.to_str().unwrap()]);
    let list = list_worktrees(&repo).unwrap();
    assert!(list.iter().find(|w| w.path == detached).unwrap().locked);
}

#[test]
fn clone_validates_destination() {
    let (_tmp, dir) = tempdir();
    let upstream = repo_with(&dir, "upstream", &[("a", "a\n")]);
    let url = upstream.to_str().unwrap();
    assert!(matches!(clone(url, &upstream), Err(GitError::Invalid(_))));
    assert!(matches!(
        clone(url, &dir.join("missing/dest")),
        Err(GitError::NotFound(_))
    ));
    assert!(matches!(
        clone(
            &dir.join("no-such-repo").to_string_lossy(),
            &dir.join("dest")
        ),
        Err(GitError::Command { .. })
    ));
    clone(url, &dir.join("dest")).unwrap();
    assert!(dir.join("dest/a").is_file());
}
