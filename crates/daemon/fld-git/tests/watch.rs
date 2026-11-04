mod common;

use std::time::Duration;

use common::{commit_all, git, repo_with, tempdir, write};
use fld_git::repo::{RepoInfo, discover};
use fld_git::watch::{WatchConfig, Watcher};
use fld_git::worktree::add_worktree;
use tokio::sync::mpsc::Receiver;
use tokio::time::timeout;

const SIGNAL_WITHIN: Duration = Duration::from_secs(1);
const QUIET_FOR: Duration = Duration::from_millis(600);

fn config() -> WatchConfig {
    WatchConfig {
        // No fallback signal during a test.
        poll: Duration::from_secs(3600),
        ..WatchConfig::default()
    }
}

fn start(info: &RepoInfo) -> (Watcher, Receiver<()>) {
    Watcher::start(&info.toplevel, &info.git_dir, &info.common_dir, config()).unwrap()
}

async fn expect_signal(rx: &mut Receiver<()>, what: &str) {
    match timeout(SIGNAL_WITHIN, rx.recv()).await {
        Ok(Some(())) => {}
        Ok(None) => panic!("watcher stopped before signalling {what}"),
        Err(_) => panic!("no signal within {SIGNAL_WITHIN:?} after {what}"),
    }
}

async fn expect_quiet(rx: &mut Receiver<()>, what: &str) {
    if let Ok(signal) = timeout(QUIET_FOR, rx.recv()).await {
        panic!("unexpected {signal:?} after {what}");
    }
}

#[tokio::test]
async fn signals_worktree_writes_and_skips_ignored_dirs() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("src/main.rs", "fn main() {}\n")]);
    write(&repo, "node_modules/pkg/index.js", "");
    write(&repo, "a/target/debug/out", "");
    let info = discover(&repo).unwrap().unwrap();
    let (_watcher, mut rx) = start(&info);
    expect_quiet(&mut rx, "starting").await;

    write(&repo, "src/main.rs", "fn main() { println!(); }\n");
    expect_signal(&mut rx, "writing a tracked file").await;
    expect_quiet(&mut rx, "the debounced burst").await;

    write(&repo, "node_modules/pkg/index.js", "changed");
    write(&repo, "node_modules/pkg/new/deep.js", "new");
    write(&repo, "a/target/debug/out", "changed");
    expect_quiet(&mut rx, "writing in ignored directories").await;

    // Directories created later are watched too.
    std::fs::create_dir(repo.join("newdir")).unwrap();
    expect_signal(&mut rx, "creating a directory").await;
    write(&repo, "newdir/file.txt", "x");
    expect_signal(&mut rx, "writing in a new directory").await;
}

#[tokio::test]
async fn signals_git_commits() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("a.txt", "a\n")]);
    write(&repo, "b.txt", "b\n");
    git(&repo, &["add", "b.txt"]);
    let info = discover(&repo).unwrap().unwrap();
    let (_watcher, mut rx) = start(&info);
    expect_quiet(&mut rx, "starting").await;

    git(&repo, &["commit", "-q", "-m", "commit b"]);
    expect_signal(&mut rx, "git commit").await;
}

#[tokio::test]
async fn signals_branch_updates_in_linked_worktrees() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("a.txt", "a\n")]);
    let wt = dir.join("wt");
    add_worktree(&repo, &wt, "feature", "main").unwrap();
    let info = discover(&wt).unwrap().unwrap();
    assert_ne!(info.git_dir, info.common_dir);
    let (_watcher, mut rx) = start(&info);
    expect_quiet(&mut rx, "starting").await;

    // A commit in the primary checkout moves `main` in the shared refs.
    write(&repo, "c.txt", "c\n");
    commit_all(&repo, "main moves");
    expect_signal(&mut rx, "a commit on main").await;
    expect_quiet(&mut rx, "the debounced burst").await;

    write(&wt, "d.txt", "d\n");
    expect_signal(&mut rx, "a write in the linked worktree").await;
}

#[tokio::test]
async fn polls_and_stops_when_dropped() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("a.txt", "a\n")]);
    let info = discover(&repo).unwrap().unwrap();
    let config = WatchConfig {
        poll: Duration::from_millis(200),
        ..WatchConfig::default()
    };
    let (watcher, mut rx) =
        Watcher::start(&info.toplevel, &info.git_dir, &info.common_dir, config).unwrap();
    expect_signal(&mut rx, "the poll interval").await;
    expect_signal(&mut rx, "the next poll interval").await;

    drop(watcher);
    match timeout(SIGNAL_WITHIN, async { while rx.recv().await.is_some() {} }).await {
        Ok(()) => {}
        Err(_) => panic!("signals continue after the watcher was dropped"),
    }
}

#[tokio::test]
async fn bursts_are_capped_by_max_delay() {
    let (_tmp, dir) = tempdir();
    let repo = repo_with(&dir, "repo", &[("a.txt", "a\n")]);
    let info = discover(&repo).unwrap().unwrap();
    let config = WatchConfig {
        debounce: Duration::from_millis(300),
        max_delay: Duration::from_millis(400),
        poll: Duration::from_secs(3600),
    };
    let (_watcher, mut rx) =
        Watcher::start(&info.toplevel, &info.git_dir, &info.common_dir, config).unwrap();

    // Writes every 100 ms never leave a 300 ms quiet gap, so only
    // `max_delay` can release the signal while they continue.
    let writer = tokio::spawn({
        let repo = repo.clone();
        async move {
            for i in 0..15 {
                write(&repo, "a.txt", format!("{i}\n"));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    });
    expect_signal(&mut rx, "a continuous burst of writes").await;
    assert!(
        !writer.is_finished(),
        "signalled only after the burst ended"
    );
    writer.await.unwrap();
}
