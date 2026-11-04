//! Commit, push and update against a real bare "origin".
mod common;

use common::*;
use fld_git::GitError;
use fld_git::ops;

/// A clone of a bare origin, both on `main` with one commit.
fn cloned() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let (dir, root) = tempdir();
    let seed = repo_with(&root, "seed", &[("a.txt", "one\n")]);
    let origin = root.join("origin.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    let work = root.join("work");
    git(
        &root,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    (dir, work, origin)
}

#[test]
fn commit_all_includes_untracked_and_refuses_empty() {
    let (_dir, work, _) = cloned();
    write(&work, "a.txt", "two\n");
    write(&work, "new.txt", "x\n");
    let head = ops::commit(&work, "Change a and add new\n\nWith a body.", true).unwrap();
    assert_eq!(head, git(&work, &["rev-parse", "HEAD"]));
    assert_eq!(
        git(&work, &["log", "-1", "--format=%s"]),
        "Change a and add new"
    );
    assert_eq!(git(&work, &["status", "--porcelain"]), "");
    assert!(matches!(
        ops::commit(&work, "again", true),
        Err(GitError::Refused(_))
    ));
    assert!(matches!(
        ops::commit(&work, "  ", true),
        Err(GitError::Invalid(_))
    ));
    // Without `all`, only the index is committed.
    write(&work, "a.txt", "three\n");
    write(&work, "b.txt", "b\n");
    git(&work, &["add", "b.txt"]);
    ops::commit(&work, "only b", false).unwrap();
    assert_eq!(git(&work, &["status", "--porcelain"]), "M a.txt");
}

#[test]
fn first_push_sets_the_upstream_and_later_pushes_follow_it() {
    let (_dir, work, origin) = cloned();
    git(&work, &["switch", "-q", "-c", "feature"]);
    write(&work, "f.txt", "f\n");
    ops::commit(&work, "feature work", true).unwrap();
    assert_eq!(ops::upstream(&work).unwrap(), None);
    ops::push(&work).unwrap();
    assert_eq!(
        ops::upstream(&work).unwrap().as_deref(),
        Some("origin/feature")
    );
    write(&work, "f.txt", "f2\n");
    let head = ops::commit(&work, "more", true).unwrap();
    ops::push(&work).unwrap();
    assert_eq!(git(&origin, &["rev-parse", "feature"]), head);
}

#[test]
fn update_merges_the_target_and_aborts_on_conflict() {
    let (_dir, work, origin) = cloned();
    git(&work, &["switch", "-q", "-c", "feature"]);
    write(&work, "f.txt", "f\n");
    ops::commit(&work, "feature", true).unwrap();

    // Someone else advances main.
    let (_other_dir, other_root) = tempdir();
    let other = other_root.join("other");
    git(
        &other_root,
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    write(&other, "main.txt", "m\n");
    commit_all(&other, "main moves");
    git(&other, &["push", "-q", "origin", "main"]);

    ops::update(&work, "main").unwrap();
    assert!(work.join("main.txt").exists());
    assert!(work.join("f.txt").exists());

    // A dirty tree is refused before anything happens.
    write(&work, "f.txt", "dirty\n");
    assert!(matches!(
        ops::update(&work, "main"),
        Err(GitError::Refused(_))
    ));
    git(&work, &["checkout", "--", "f.txt"]);

    // A conflicting update is rolled back.
    write(&work, "a.txt", "feature side\n");
    ops::commit(&work, "feature edits a", true).unwrap();
    write(&other, "a.txt", "main side\n");
    commit_all(&other, "main edits a");
    git(&other, &["push", "-q", "origin", "main"]);
    let before = git(&work, &["rev-parse", "HEAD"]);
    assert!(matches!(
        ops::update(&work, "main"),
        Err(GitError::Refused(_))
    ));
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), before);
    assert_eq!(git(&work, &["status", "--porcelain"]), "");
    assert!(matches!(
        ops::update(&work, "--upload-pack=x"),
        Err(GitError::Invalid(_))
    ));
}

#[test]
fn the_github_repository_comes_from_origin() {
    let (_dir, work, _) = cloned();
    assert_eq!(ops::github_repository(&work).unwrap(), None);
    git(
        &work,
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:acme/widgets.git",
        ],
    );
    assert_eq!(
        ops::github_repository(&work).unwrap(),
        Some(("acme".into(), "widgets".into()))
    );
}

#[test]
fn update_preserves_an_existing_clean_pending_merge() {
    let (_dir, work, _) = cloned();
    git(&work, &["switch", "-q", "-c", "other"]);
    write(&work, "other.txt", "other\n");
    commit_all(&work, "other");
    git(&work, &["switch", "-q", "main"]);
    git(
        &work,
        &["merge", "--no-commit", "--no-ff", "-s", "ours", "other"],
    );
    assert_eq!(git(&work, &["status", "--porcelain"]), "");
    let pending = git(&work, &["rev-parse", "MERGE_HEAD"]);
    assert!(matches!(
        ops::update(&work, "main"),
        Err(GitError::Refused(_))
    ));
    assert_eq!(git(&work, &["rev-parse", "MERGE_HEAD"]), pending);
}

#[test]
fn commit_all_preserves_unresolved_index_entries() {
    let (_dir, work, _) = cloned();
    git(&work, &["switch", "-q", "-c", "other"]);
    write(&work, "a.txt", "other side\n");
    commit_all(&work, "other");
    git(&work, &["switch", "-q", "main"]);
    write(&work, "a.txt", "main side\n");
    commit_all(&work, "main");
    let merge = git_output(&work, &["merge", "--no-edit", "other"]);
    assert!(!merge.status.success());
    let head = git(&work, &["rev-parse", "HEAD"]);
    let unmerged = git(&work, &["ls-files", "--unmerged"]);
    assert!(matches!(
        ops::commit(&work, "not resolved", true),
        Err(GitError::Refused(_))
    ));
    assert_eq!(git(&work, &["ls-files", "--unmerged"]), unmerged);
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), head);
}
