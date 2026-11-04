mod common;

use common::{LONG_TEXT, commit_all, git, git_output, repo_with, tempdir, write};
use fl_model::workspace::{ChangeKind, FileChange};
use fld_git::status::{diff_base, status};

fn change(
    path: &str,
    old_path: Option<&str>,
    kind: ChangeKind,
    added: Option<u32>,
    removed: Option<u32>,
) -> FileChange {
    FileChange {
        path: path.into(),
        old_path: old_path.map(Into::into),
        kind,
        added,
        removed,
    }
}

#[test]
fn status_against_merge_base() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(
        &dir,
        "repo",
        &[
            ("keep.txt", "a\nb\nc\n"),
            ("modify.txt", "1\n2\n3\n"),
            ("delete.txt", "x\ny\n"),
            ("rename_src.txt", LONG_TEXT),
        ],
    );
    let base = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "-b", "feature"]);
    write(&repo, "committed.txt", "c1\nc2\n");
    commit_all(&repo, "on branch");
    // main moves on; the merge-base stays at `base`.
    git(&repo, &["checkout", "-q", "main"]);
    write(&repo, "main-only.txt", "m\n");
    commit_all(&repo, "on main");
    git(&repo, &["checkout", "-q", "feature"]);

    write(&repo, "modify.txt", "1\nTWO\n3\n4\n");
    write(&repo, "added.txt", "n1\nn2\nn3\n");
    git(&repo, &["add", "added.txt"]);
    std::fs::remove_file(repo.join("delete.txt")).unwrap();
    git(&repo, &["mv", "rename_src.txt", "renamed.txt"]);
    write(&repo, "untracked.txt", "u1\nu2");
    write(&repo, "bin.dat", b"\0\x01\x02");

    assert_eq!(diff_base(&repo, Some("main")).unwrap(), Some(base));
    let head = git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        diff_base(&repo, Some("no-such-branch")).unwrap(),
        Some(head.clone())
    );
    assert_eq!(diff_base(&repo, None).unwrap(), Some(head.clone()));

    let (summary, files) = status(&repo, Some("main"), 100).unwrap();
    assert_eq!(
        files,
        vec![
            change("added.txt", None, ChangeKind::Added, Some(3), Some(0)),
            change("bin.dat", None, ChangeKind::Untracked, None, None),
            change("committed.txt", None, ChangeKind::Added, Some(2), Some(0)),
            change("delete.txt", None, ChangeKind::Deleted, Some(0), Some(2)),
            change("modify.txt", None, ChangeKind::Modified, Some(2), Some(1)),
            change(
                "renamed.txt",
                Some("rename_src.txt"),
                ChangeKind::Renamed,
                Some(0),
                Some(0)
            ),
            change(
                "untracked.txt",
                None,
                ChangeKind::Untracked,
                Some(2),
                Some(0)
            ),
        ]
    );
    assert_eq!(summary.branch.as_deref(), Some("feature"));
    assert_eq!(summary.head.as_deref(), Some(head.as_str()));
    assert_eq!(summary.upstream, None);
    assert_eq!((summary.ahead, summary.behind), (0, 0));
    assert_eq!((summary.added, summary.removed), (9, 3));
    assert!(summary.dirty);
    assert!(!summary.conflicted);

    // Totals cover every file; the list is truncated.
    let (truncated_summary, truncated) = status(&repo, Some("main"), 2).unwrap();
    assert_eq!(truncated.len(), 2);
    assert_eq!(truncated[0].path, "added.txt");
    assert_eq!((truncated_summary.added, truncated_summary.removed), (9, 3));

    // Without a target only uncommitted changes count.
    let (_, files) = status(&repo, None, 100).unwrap();
    assert!(!files.iter().any(|f| f.path == "committed.txt"));
}

#[test]
fn clean_worktree_is_not_dirty() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("a.txt", "a\n")]);
    let (summary, files) = status(&repo, Some("main"), 100).unwrap();
    assert!(files.is_empty());
    assert!(!summary.dirty);
    assert_eq!(summary.branch.as_deref(), Some("main"));
    assert_eq!((summary.added, summary.removed), (0, 0));
}

#[test]
fn conflicted_files() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("conflict.txt", "base\n")]);
    git(&repo, &["checkout", "-q", "-b", "feature"]);
    write(&repo, "conflict.txt", "feature\n");
    commit_all(&repo, "feature side");
    git(&repo, &["checkout", "-q", "main"]);
    write(&repo, "conflict.txt", "main\n");
    commit_all(&repo, "main side");
    git(&repo, &["checkout", "-q", "feature"]);
    let merge = git_output(&repo, &["merge", "-q", "main"]);
    assert!(!merge.status.success(), "the merge conflicts");

    let (summary, files) = status(&repo, Some("main"), 100).unwrap();
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0].path, "conflict.txt");
    assert_eq!(files[0].kind, ChangeKind::Conflicted);
    assert!(summary.conflicted);
    assert!(summary.dirty);
}

#[test]
fn unborn_head_diffs_against_the_empty_tree() {
    let (_tmp, dir) = tempdir();
    let repo = dir.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo, "staged.txt", "s1\ns2\n");
    git(&repo, &["add", "staged.txt"]);
    write(&repo, "dir/untracked.txt", "u\n");

    assert_eq!(diff_base(&repo, Some("main")).unwrap(), None);
    let (summary, files) = status(&repo, Some("main"), 100).unwrap();
    assert_eq!(
        files,
        vec![
            change(
                "dir/untracked.txt",
                None,
                ChangeKind::Untracked,
                Some(1),
                Some(0)
            ),
            change("staged.txt", None, ChangeKind::Added, Some(2), Some(0)),
        ]
    );
    assert_eq!(summary.head, None);
    assert_eq!(summary.branch.as_deref(), Some("main"));
    assert_eq!((summary.added, summary.removed), (3, 0));
}

#[test]
fn upstream_ahead_behind_and_origin_target() {
    let (_tmp, dir) = tempdir();
    let upstream = repo_with(&dir, "upstream", &[("a.txt", "a\n")]);
    let clone = dir.join("clone");
    fld_git::repo::clone(upstream.to_str().unwrap(), &clone).unwrap();
    write(&upstream, "b.txt", "b\n");
    commit_all(&upstream, "upstream moves");
    git(&clone, &["fetch", "-q"]);
    write(&clone, "c.txt", "c\n");
    let local = commit_all(&clone, "local work");

    let (summary, _) = status(&clone, Some("main"), 100).unwrap();
    assert_eq!(summary.upstream.as_deref(), Some("origin/main"));
    assert_eq!((summary.ahead, summary.behind), (1, 1));

    // A target only present as a remote branch resolves via `origin/`.
    git(&clone, &["checkout", "-q", "-b", "topic"]);
    git(&clone, &["branch", "-q", "-D", "main"]);
    let base = git(&clone, &["merge-base", "HEAD", "origin/main"]);
    assert_ne!(base, local);
    assert_eq!(diff_base(&clone, Some("main")).unwrap(), Some(base));
}
