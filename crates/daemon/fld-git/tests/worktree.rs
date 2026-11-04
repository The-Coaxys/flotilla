mod common;

use common::{chmod, commit_all, git, git_output, mode, repo_with, tempdir, write};
use fld_git::GitError;
use fld_git::repo::list_worktrees;
use fld_git::worktree::{
    DeleteCheck, add_worktree, copy_includes, delete_branch, delete_check, init_submodules,
    remove_worktree, validate_branch_name,
};

#[test]
fn validates_branch_names() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[]);
    validate_branch_name(&repo, "feature/x").unwrap();
    validate_branch_name(&repo, "flotilla/brisk-otter-0a1b").unwrap();
    for bad in [
        "",
        "bad..name",
        "-x",
        "HEAD",
        "@{-1}",
        "a b",
        "ends.lock",
        "x/",
        "a~1",
    ] {
        assert!(
            matches!(validate_branch_name(&repo, bad), Err(GitError::Invalid(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn adds_worktrees() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("a.txt", "a\n")]);
    let path = dir.join("worktrees/nested/feature");
    add_worktree(&repo, &path, "feature", "main").unwrap();
    assert!(path.join("a.txt").is_file());
    assert_eq!(git(&path, &["symbolic-ref", "--short", "HEAD"]), "feature");
    // The new branch does not track its base.
    assert!(
        !git_output(&path, &["rev-parse", "--abbrev-ref", "@{upstream}"])
            .status
            .success()
    );

    let err = |result| match result {
        Err(e) => e,
        Ok(()) => panic!("expected an error"),
    };
    assert!(matches!(
        err(add_worktree(&repo, &path, "other", "main")),
        GitError::Invalid(_)
    ));
    assert!(matches!(
        err(add_worktree(&repo, &dir.join("w2"), "feature", "main")),
        GitError::Invalid(_)
    ));
    assert!(matches!(
        err(add_worktree(&repo, &dir.join("w3"), "bad..name", "main")),
        GitError::Invalid(_)
    ));
    assert!(matches!(
        err(add_worktree(
            &repo,
            &dir.join("w4"),
            "other",
            "no-such-base"
        )),
        GitError::NotFound(_)
    ));
    assert!(!dir.join("w4").exists());
}

#[test]
fn copies_worktree_includes() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(
        &dir,
        "repo",
        &[
            (".gitignore", ".env\nsecrets/\nnode_modules/\n*.log\n"),
            (".worktreeinclude", "# local config\n.env\nsecrets/\n"),
        ],
    );
    write(&repo, ".env", "TOKEN=1\n");
    chmod(&repo.join(".env"), 0o600);
    write(&repo, "secrets/key.pem", "key\n");
    write(&repo, "secrets/nested/cert.pem", "cert\n");
    write(&repo, "app/.env", "APP=1\n");
    write(&repo, "debug.log", "noise\n");
    write(&repo, "node_modules/pkg/.env", "skipped heavy dir\n");

    let wt = dir.join("wt");
    add_worktree(&repo, &wt, "feature", "main").unwrap();
    let copied = copy_includes(&repo, &wt).unwrap();
    assert_eq!(
        copied,
        vec![
            ".env",
            "app/.env",
            "secrets/key.pem",
            "secrets/nested/cert.pem"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(wt.join(".env")).unwrap(),
        "TOKEN=1\n"
    );
    assert_eq!(mode(&wt.join(".env")), 0o600);
    assert!(!wt.join("debug.log").exists());
    assert!(!wt.join("node_modules").exists());

    // Existing files are never overwritten.
    write(&wt, ".env", "CHANGED=1\n");
    std::fs::remove_file(wt.join("secrets/key.pem")).unwrap();
    assert_eq!(copy_includes(&repo, &wt).unwrap(), vec!["secrets/key.pem"]);
    assert_eq!(
        std::fs::read_to_string(wt.join(".env")).unwrap(),
        "CHANGED=1\n"
    );

    // A pattern naming a heavy directory walks into it.
    write(&repo, ".worktreeinclude", "node_modules/pkg/.env\n");
    assert_eq!(
        copy_includes(&repo, &wt).unwrap(),
        vec!["node_modules/pkg/.env"]
    );

    // No `.worktreeinclude`: nothing to copy.
    std::fs::remove_file(repo.join(".worktreeinclude")).unwrap();
    assert!(copy_includes(&repo, &wt).unwrap().is_empty());
}

#[test]
fn initialises_submodules() {
    let (_tmp, dir) = tempdir();
    let sub = repo_with(&dir, "sub", &[("lib.txt", "library\n")]);
    let repo = repo_with(&dir, "repo", &[("a.txt", "a\n")]);
    assert!(!init_submodules(&repo).unwrap());

    git(
        &repo,
        &[
            "submodule",
            "add",
            "-q",
            sub.to_str().unwrap(),
            "vendor/sub",
        ],
    );
    commit_all(&repo, "add submodule");

    let wt = dir.join("wt");
    add_worktree(&repo, &wt, "feature", "main").unwrap();
    assert!(!wt.join("vendor/sub/lib.txt").exists());
    assert!(init_submodules(&wt).unwrap());
    assert_eq!(
        std::fs::read_to_string(wt.join("vendor/sub/lib.txt")).unwrap(),
        "library\n"
    );
}

#[test]
fn checks_before_delete() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("a.txt", "a\n")]);
    let wt = dir.join("wt");
    add_worktree(&repo, &wt, "feature", "main").unwrap();
    assert_eq!(
        delete_check(&wt, Some("main")).unwrap(),
        DeleteCheck {
            dirty: false,
            unpushed: 0
        }
    );

    write(&wt, "b.txt", "b\n");
    commit_all(&wt, "one");
    write(&wt, "c.txt", "c\n");
    commit_all(&wt, "two");
    assert_eq!(
        delete_check(&wt, Some("main")).unwrap(),
        DeleteCheck {
            dirty: false,
            unpushed: 2
        }
    );
    // Without a (resolvable) target every commit off a remote counts.
    assert_eq!(delete_check(&wt, None).unwrap().unpushed, 3);
    assert_eq!(delete_check(&wt, Some("no-such")).unwrap().unpushed, 3);

    write(&wt, "untracked.txt", "u\n");
    assert!(delete_check(&wt, Some("main")).unwrap().dirty);

    // Commits on a remote branch are pushed.
    let clone = dir.join("clone");
    fld_git::repo::clone(repo.to_str().unwrap(), &clone).unwrap();
    assert_eq!(
        delete_check(&clone, None).unwrap(),
        DeleteCheck {
            dirty: false,
            unpushed: 0
        }
    );
}

#[test]
fn removes_worktrees() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("a.txt", "a\n")]);
    let wt = dir.join("wt");
    add_worktree(&repo, &wt, "feature", "main").unwrap();
    write(&wt, "dirty.txt", "x\n");
    assert!(matches!(
        remove_worktree(&repo, &wt, false),
        Err(GitError::Command { .. })
    ));
    assert!(wt.exists());
    remove_worktree(&repo, &wt, true).unwrap();
    assert!(!wt.exists());

    let gone = dir.join("gone");
    add_worktree(&repo, &gone, "gone", "main").unwrap();
    git(&repo, &["worktree", "lock", gone.to_str().unwrap()]);
    std::fs::remove_dir_all(&gone).unwrap();
    git(&repo, &["worktree", "unlock", gone.to_str().unwrap()]);
    remove_worktree(&repo, &gone, false).unwrap();
    let list = list_worktrees(&repo).unwrap();
    assert_eq!(list.len(), 1, "{list:?}");

    let locked = dir.join("locked");
    add_worktree(&repo, &locked, "locked", "main").unwrap();
    git(&repo, &["worktree", "lock", locked.to_str().unwrap()]);
    remove_worktree(&repo, &locked, true).unwrap();
    assert!(!locked.exists());
}

#[test]
fn deletes_branches() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("a.txt", "a\n")]);
    let branch_exists = |name: &str| {
        git_output(
            &repo,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
        )
        .status
        .success()
    };

    git(&repo, &["branch", "merged"]);
    assert!(delete_branch(&repo, "merged", Some("main"), false).unwrap());
    assert!(!branch_exists("merged"));

    let wt = dir.join("wt");
    add_worktree(&repo, &wt, "unmerged", "main").unwrap();
    write(&wt, "b.txt", "b\n");
    commit_all(&wt, "unmerged work");
    remove_worktree(&repo, &wt, false).unwrap();
    assert!(!delete_branch(&repo, "unmerged", Some("main"), false).unwrap());
    assert!(!delete_branch(&repo, "unmerged", None, false).unwrap());
    assert!(branch_exists("unmerged"));
    assert!(delete_branch(&repo, "unmerged", Some("main"), true).unwrap());
    assert!(!branch_exists("unmerged"));

    assert!(!delete_branch(&repo, "missing", Some("main"), false).unwrap());
    assert!(!delete_branch(&repo, "missing", None, true).unwrap());

    // Merged into the primary checkout's HEAD when no target is given.
    git(&repo, &["branch", "merged-into-head"]);
    assert!(delete_branch(&repo, "merged-into-head", None, false).unwrap());
}
