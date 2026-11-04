mod common;

use std::os::unix::fs::symlink;

use common::{LONG_TEXT, chmod, git, mode, repo_with, tempdir, write};
use fl_model::workspace::EntryKind;
use fld_git::GitError;
use fld_git::files::{diff_sides, list_dir, read_file, write_file};
use sha2::{Digest, Sha256};

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[test]
fn lists_directories_sorted_with_ignored_flags() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(
        &dir,
        "repo",
        &[
            (".gitignore", "*.log\nbuild/\ntracked.log\n"),
            ("Zeta.txt", "z"),
            ("alpha.txt", "alpha"),
        ],
    );
    std::fs::create_dir(repo.join("a_dir")).unwrap();
    std::fs::create_dir(repo.join("B_dir")).unwrap();
    std::fs::create_dir(repo.join("build")).unwrap();
    write(&repo, "x.log", "log");
    write(&repo, "beta", "b");
    write(&repo, "Beta", "B");
    symlink("alpha.txt", repo.join("link")).unwrap();
    write(&repo, "tracked.log", "t");
    git(&repo, &["add", "-f", "tracked.log"]);

    let listing = list_dir(&repo.join("a_dir/.."), 100).unwrap();
    assert_eq!(listing.path, repo.to_str().unwrap());
    assert!(!listing.truncated);
    let names: Vec<(&str, EntryKind, bool)> = listing
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind, e.ignored))
        .collect();
    assert_eq!(
        names,
        vec![
            (".git", EntryKind::Dir, false),
            ("a_dir", EntryKind::Dir, false),
            ("B_dir", EntryKind::Dir, false),
            ("build", EntryKind::Dir, true),
            (".gitignore", EntryKind::File, false),
            ("alpha.txt", EntryKind::File, false),
            ("Beta", EntryKind::File, false),
            ("beta", EntryKind::File, false),
            ("link", EntryKind::Symlink, false),
            ("tracked.log", EntryKind::File, false),
            ("x.log", EntryKind::File, true),
            ("Zeta.txt", EntryKind::File, false),
        ]
    );
    let alpha = listing
        .entries
        .iter()
        .find(|e| e.name == "alpha.txt")
        .unwrap();
    assert_eq!(alpha.size, 5);
    assert!(alpha.modified_ms > 0);

    let truncated = list_dir(&repo, 3).unwrap();
    assert!(truncated.truncated);
    assert_eq!(truncated.entries.len(), 3);
    assert_eq!(truncated.entries[2].name, "B_dir");

    // Subdirectories use the work tree's rules too.
    write(&repo, "a_dir/inner.log", "x");
    let inner = list_dir(&repo.join("a_dir"), 100).unwrap();
    assert!(inner.entries[0].ignored);

    // Outside a work tree nothing is ignored.
    write(&dir, "plain/x.log", "x");
    let plain = list_dir(&dir.join("plain"), 100).unwrap();
    assert_eq!(plain.entries.len(), 1);
    assert!(!plain.entries[0].ignored);

    assert!(matches!(
        list_dir(&repo.join("alpha.txt"), 10),
        Err(GitError::Invalid(_))
    ));
    assert!(matches!(
        list_dir(&repo.join("missing"), 10),
        Err(GitError::NotFound(_))
    ));
}

#[test]
fn reads_files() {
    let (_tmp, dir) = tempdir();
    let text = b"hello world\n";
    write(&dir, "text.txt", text);
    let content = read_file(&dir.join("text.txt"), 1024).unwrap();
    assert_eq!(content.bytes, text);
    assert_eq!(content.sha256, sha(text));
    assert_eq!(content.size, text.len() as u64);
    assert!(!content.truncated && !content.binary);
    assert_eq!(content.path, dir.join("text.txt").to_str().unwrap());

    let truncated = read_file(&dir.join("text.txt"), 5).unwrap();
    assert_eq!(truncated.bytes, b"hello");
    assert!(truncated.truncated);
    assert_eq!(truncated.size, text.len() as u64);
    assert_eq!(truncated.sha256, sha(text), "hash covers the whole file");

    let big: Vec<u8> = (0..200_000u32).flat_map(|i| i.to_le_bytes()).collect();
    write(&dir, "big.bin", &big);
    let content = read_file(&dir.join("big.bin"), 10).unwrap();
    assert!(content.binary);
    assert_eq!(content.sha256, sha(&big));
    assert_eq!(content.size, big.len() as u64);

    symlink("text.txt", dir.join("link")).unwrap();
    assert_eq!(read_file(&dir.join("link"), 100).unwrap().bytes, text);

    assert!(matches!(read_file(&dir, 10), Err(GitError::Invalid(_))));
    assert!(matches!(
        read_file(&dir.join("missing"), 10),
        Err(GitError::NotFound(_))
    ));
}

fn leftovers(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".flotilla-"))
        .collect()
}

#[test]
fn writes_files_atomically() {
    let (_tmp, dir) = tempdir();
    let path = dir.join("new.txt");
    assert_eq!(write_file(&path, b"first", None).unwrap(), sha(b"first"));
    assert_eq!(std::fs::read(&path).unwrap(), b"first");
    assert_eq!(mode(&path), 0o644);

    chmod(&path, 0o600);
    assert_eq!(
        write_file(&path, b"second", Some(sha(b"first"))).unwrap(),
        sha(b"second")
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"second");
    assert_eq!(mode(&path), 0o600);

    chmod(&path, 0o755);
    write_file(&path, b"third", None).unwrap();
    assert_eq!(mode(&path), 0o755);

    match write_file(&path, b"stale", Some(sha(b"second"))) {
        Err(GitError::Conflict { current }) => assert_eq!(current, Some(sha(b"third"))),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"third");
    match write_file(&dir.join("gone.txt"), b"x", Some(sha(b"x"))) {
        Err(GitError::Conflict { current }) => assert_eq!(current, None),
        other => panic!("unexpected {other:?}"),
    }
    assert!(!dir.join("gone.txt").exists());

    // Writing through a symlink replaces its target and keeps the link.
    symlink("new.txt", dir.join("link")).unwrap();
    write_file(&dir.join("link"), b"via link", Some(sha(b"third"))).unwrap();
    assert!(
        dir.join("link")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"via link");

    assert!(matches!(
        write_file(&dir.join("missing/x.txt"), b"x", None),
        Err(GitError::NotFound(_))
    ));
    std::fs::create_dir(dir.join("subdir")).unwrap();
    assert!(matches!(
        write_file(&dir.join("subdir"), b"x", None),
        Err(GitError::Invalid(_))
    ));
    assert_eq!(leftovers(&dir), Vec::<String>::new());
}

#[test]
fn diff_sides_for_each_change_kind() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(
        &dir,
        "repo",
        &[
            ("mod.txt", "old\n"),
            ("del.txt", "deleted\n"),
            ("ren_src.txt", LONG_TEXT),
            ("sub/nested.txt", "nested\n"),
        ],
    );
    let base = git(&repo, &["rev-parse", "HEAD"]);
    write(&repo, "mod.txt", "new\n");
    std::fs::remove_file(repo.join("del.txt")).unwrap();
    write(&repo, "added.txt", "added\n");
    git(&repo, &["mv", "ren_src.txt", "renamed.txt"]);

    let (old, new) = diff_sides(&repo, Some(&base), "mod.txt", None, 1024).unwrap();
    let old = old.unwrap();
    let new = new.unwrap();
    assert_eq!(old.bytes, b"old\n");
    assert_eq!(old.sha256, sha(b"old\n"));
    assert_eq!(old.path, "mod.txt");
    assert_eq!(new.bytes, b"new\n");
    assert_eq!(new.path, "mod.txt");

    let (old, new) = diff_sides(&repo, Some(&base), "added.txt", None, 1024).unwrap();
    assert!(old.is_none());
    assert_eq!(new.unwrap().bytes, b"added\n");

    let (old, new) = diff_sides(&repo, Some(&base), "del.txt", None, 1024).unwrap();
    assert_eq!(old.unwrap().bytes, b"deleted\n");
    assert!(new.is_none());

    let (old, new) = diff_sides(&repo, Some(&base), "renamed.txt", Some("ren_src.txt"), 4).unwrap();
    let old = old.unwrap();
    assert_eq!(old.path, "renamed.txt");
    assert_eq!(old.bytes, b"one\n");
    assert!(old.truncated);
    assert_eq!(old.size, LONG_TEXT.len() as u64);
    assert_eq!(old.sha256, sha(LONG_TEXT.as_bytes()));
    assert_eq!(new.unwrap().size, LONG_TEXT.len() as u64);

    let (old, new) = diff_sides(&repo, None, "mod.txt", None, 1024).unwrap();
    assert!(old.is_none());
    assert!(new.is_some());

    let (old, _) = diff_sides(&repo, Some(&base), "sub/nested.txt", None, 1024).unwrap();
    assert_eq!(old.unwrap().bytes, b"nested\n");
    // A directory is not a blob.
    let (old, new) = diff_sides(&repo, Some(&base), "sub", None, 1024).unwrap();
    assert!(old.is_none() && new.is_none());

    for bad in ["../outside", "/etc/passwd", "sub/../../x"] {
        assert!(
            matches!(
                diff_sides(&repo, Some(&base), bad, None, 10),
                Err(GitError::Invalid(_))
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        diff_sides(&repo, Some(&base), "mod.txt", Some("../x"), 10),
        Err(GitError::Invalid(_))
    ));
}
