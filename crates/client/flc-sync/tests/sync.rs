#![cfg(unix)]
//! End-to-end client sync against a real daemon, process host and shells:
//! mirrors, exactly-once input, link failover, daemon restart, outages,
//! snapshot recovery and layout reconciliation.
use std::time::{Duration, Instant};

use fl_model::layout::LayoutOp;
use fl_model::workspace::LayoutScope;
use fl_model::{PaneId, Size, TabId, TerminalId, ViewId};
use fl_proto::{ClipboardTarget, TerminalNotice};
use flc_conn::supervisor::{Supervisor, TransportKind};
use flc_sync::{
    Config, DiscardReason, MirrorEvent, MirrorHandle, MirrorStatus, Sync, WorkspaceState,
    WorkspacesState,
};
use tokio::sync::watch;

mod support;
use support::{Harness, wait_link};

const WAIT: Duration = Duration::from_secs(20);

/// Text of the published frame, row by row, trailing blanks trimmed.
fn visible_text(handle: &MirrorHandle) -> String {
    let (_, frame) = handle.frame();
    let columns = frame.size.columns as usize;
    let mut text = String::new();
    for row in frame.cells.chunks(columns.max(1)) {
        let line: String = row
            .iter()
            .filter(|cell| !cell.flags.contains(fl_term::Flags::WIDE_CHAR_SPACER))
            .map(|cell| cell.c)
            .collect();
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

async fn wait_text(handle: &MirrorHandle, needle: &str, within: Duration) -> String {
    let deadline = Instant::now() + within;
    loop {
        let text = visible_text(handle);
        if text.contains(needle) {
            return text;
        }
        assert!(Instant::now() < deadline, "missing {needle:?} in:\n{text}");
        let generation = handle.generation();
        let _ = tokio::time::timeout(Duration::from_millis(200), handle.changed(generation)).await;
    }
}

async fn wait_live(handle: &MirrorHandle, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        if handle.state().status == MirrorStatus::Live {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "mirror never became live: {:?}",
            handle.state().status
        );
        let generation = handle.generation();
        let _ = tokio::time::timeout(Duration::from_millis(200), handle.changed(generation)).await;
    }
}

async fn wait_workspace(
    rx: &mut watch::Receiver<std::sync::Arc<WorkspaceState>>,
    within: Duration,
    what: &str,
    pred: impl Fn(&WorkspaceState) -> bool,
) -> std::sync::Arc<WorkspaceState> {
    let deadline = Instant::now() + within;
    loop {
        if pred(&rx.borrow()) {
            return rx.borrow().clone();
        }
        assert!(
            Instant::now() < deadline,
            "workspace never satisfied: {what}"
        );
        let _ = tokio::time::timeout(Duration::from_millis(200), rx.changed()).await;
    }
}

/// Create a terminal through the real layout path and return its handle.
async fn spawn_terminal(
    sync: &Sync,
    mut workspace: watch::Receiver<std::sync::Arc<WorkspaceState>>,
    view: ViewId,
) -> (MirrorHandle, TerminalId) {
    let state = wait_workspace(&mut workspace, WAIT, "initial layout", |state| {
        state.layout.is_some()
    })
    .await;
    let layout = state.layout.clone().unwrap();
    let terminal = TerminalId::new();
    sync.apply(LayoutOp::NewTab {
        view: layout.active,
        tab: TabId::new(),
        pane: PaneId::new(),
        terminal,
        name: None,
        size: Size::new(80, 24),
    })
    .wait()
    .await
    .expect("layout operation acknowledged");
    let state = wait_workspace(&mut workspace, WAIT, "spawned process", |state| {
        state
            .processes
            .iter()
            .any(|process| process.terminal == terminal)
    })
    .await;
    let incarnation = state
        .processes
        .iter()
        .find(|process| process.terminal == terminal)
        .unwrap()
        .incarnation;
    let handle = sync.attach(terminal, incarnation, view);
    wait_live(&handle, WAIT).await;
    (handle, terminal)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mirrors_input_and_survive_quic_ssh_failover_and_back() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut links = sup.links();
    let _ = wait_link(&mut links, WAIT, "QUIC link", |link| {
        link.kind() == TransportKind::Quic
    })
    .await;
    let sync = Sync::new(links.clone(), None, Config::default());
    let workspace = sync.workspace_changes();
    let (handle, terminal) = spawn_terminal(&sync, workspace.clone(), ViewId::new()).await;

    handle.send_input(b"printf 'MARKER%s\\n' one\n".to_vec());
    wait_text(&handle, "MARKERone", WAIT).await;
    let before = handle.state().clone();
    assert!(before.cursor.is_some());

    // Black-hole UDP: the session continues over SSH without losing input.
    h.relay.set_dropping(true);
    let ssh = wait_link(&mut links, WAIT, "SSH link", |link| {
        link.kind() == TransportKind::Ssh
    })
    .await;
    assert!(ssh.generation() > 0);
    wait_live(&handle, WAIT).await;
    handle.send_input(b"printf 'MARKER%s\\n' two\n".to_vec());
    let text = wait_text(&handle, "MARKERtwo", WAIT).await;
    // The typed line echoes the printf source, so the marker appears exactly
    // once: as the command's output. A replayed input would print it twice.
    assert_eq!(text.matches("MARKERtwo").count(), 1, "{text}");

    // A second, independent mirror installs a snapshot. Both paths — the live
    // record stream and the snapshot transfer — must agree byte for byte.
    let snapshot_mirror = sync.attach(terminal, handle.incarnation(), ViewId::new());
    wait_live(&snapshot_mirror, WAIT).await;
    let deadline = Instant::now() + WAIT;
    loop {
        let live = handle.state().cursor;
        let snapshot = snapshot_mirror.state().cursor;
        if live.is_some() && live == snapshot {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "mirrors never converged: {live:?} vs {snapshot:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let streamed = handle
        .with_engine(|engine| engine.checkpoint().unwrap())
        .unwrap();
    let transferred = snapshot_mirror
        .with_engine(|engine| engine.checkpoint().unwrap())
        .unwrap();
    assert_eq!(streamed, transferred, "snapshot and stream diverge");

    // UDP returns: a fresh QUIC generation keeps the same mirror working.
    h.relay.set_dropping(false);
    let _ = wait_link(&mut links, Duration::from_secs(25), "QUIC again", |link| {
        link.kind() == TransportKind::Quic
    })
    .await;
    wait_live(&handle, WAIT).await;
    handle.send_input(b"printf 'MARKER%s\\n' three\n".to_vec());
    let text = wait_text(&handle, "MARKERthree", WAIT).await;
    assert_eq!(text.matches("MARKERthree").count(), 1, "{text}");
    sync.shutdown().await;
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_survives_a_short_restart_and_exact_state_resumes() {
    let mut h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut links = sup.links();
    let _ = wait_link(&mut links, WAIT, "QUIC link", |link| {
        link.kind() == TransportKind::Quic
    })
    .await;
    let sync = Sync::new(links.clone(), None, Config::default());
    let workspace = sync.workspace_changes();
    let (handle, terminal) = spawn_terminal(&sync, workspace.clone(), ViewId::new()).await;
    handle.send_input(b"printf 'MARKER%s\\n' start\n".to_vec());
    wait_text(&handle, "MARKERstart", WAIT).await;
    let pid = handle.state().process.as_ref().unwrap().pid;

    // Stop the daemon, type while offline, restart within the five-second
    // budget: the input is written exactly once to the same child.
    h.stop_daemon().await;
    let deadline = Instant::now() + WAIT;
    while handle.state().status == MirrorStatus::Live {
        assert!(
            Instant::now() < deadline,
            "mirror stayed live without a daemon"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.send_input(b"printf 'MARKER%s\\n' resumed\n".to_vec());
    let started = Instant::now();
    h.daemon = Some(support::start_daemon(h.dir.path(), h.quic_port).await);
    assert!(started.elapsed() < Duration::from_secs(5));
    wait_live(&handle, WAIT).await;
    let text = wait_text(&handle, "MARKERresumed", WAIT).await;
    // Exactly one execution: the offline input was written once, not replayed.
    assert_eq!(text.matches("MARKERresumed").count(), 1, "{text}");

    let state = wait_workspace(&mut workspace.clone(), WAIT, "same child", |state| {
        state
            .processes
            .iter()
            .any(|process| process.terminal == terminal && process.pid == pid)
    })
    .await;
    assert!(state.processes.iter().any(|process| process.pid == pid));
    assert!(handle.state().resumes >= 1);
    sync.shutdown().await;
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_older_than_five_seconds_is_discarded_not_replayed() {
    let mut h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut links = sup.links();
    let _ = wait_link(&mut links, WAIT, "QUIC link", |link| {
        link.kind() == TransportKind::Quic
    })
    .await;
    let sync = Sync::new(links.clone(), None, Config::default());
    let (handle, _terminal) = spawn_terminal(&sync, sync.workspace_changes(), ViewId::new()).await;
    handle.send_input(b"printf 'MARKER%s\\n' first\n".to_vec());
    wait_text(&handle, "MARKERfirst", WAIT).await;
    let mut events = handle.events();

    h.stop_daemon().await;
    let deadline = Instant::now() + WAIT;
    while handle.state().status == MirrorStatus::Live {
        assert!(
            Instant::now() < deadline,
            "mirror stayed live without a daemon"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.send_input(b"printf 'MARKER%s\\n' stale\n".to_vec());
    tokio::time::sleep(Duration::from_secs(6)).await;
    h.daemon = Some(support::start_daemon(h.dir.path(), h.quic_port).await);
    wait_live(&handle, WAIT).await;

    let discard = tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await {
                Ok(MirrorEvent::InputDiscarded(discard)) => return discard,
                Ok(_) => {}
                Err(error) => panic!("mirror events ended: {error}"),
            }
        }
    })
    .await
    .expect("discard event");
    match discard.reason {
        DiscardReason::Outage { duration } => {
            assert!(duration >= Duration::from_secs(5), "{duration:?}")
        }
        other => panic!("discarded for {other:?}, not the five-second outage rule"),
    }
    assert_eq!(discard.committed, 0);
    // Give a wrong implementation time to surface the stale command.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!visible_text(&handle).contains("MARKERstale"));
    sync.shutdown().await;
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_layout_clients_reconcile_a_revision_conflict() {
    let h = Harness::new().await;
    let first = Supervisor::spawn(h.spec(true));
    let second = Supervisor::spawn(h.spec(true));
    let mut first_links = first.links();
    let mut second_links = second.links();
    let _ = wait_link(&mut first_links, WAIT, "first QUIC link", |link| {
        link.kind() == TransportKind::Quic
    })
    .await;
    let _ = wait_link(&mut second_links, WAIT, "second QUIC link", |link| {
        link.kind() == TransportKind::Quic
    })
    .await;
    let a = Sync::new(first_links.clone(), None, Config::default());
    let b = Sync::new(second_links.clone(), None, Config::default());
    let mut a_workspace = a.workspace_changes();
    let mut b_workspace = b.workspace_changes();
    let layout = wait_workspace(&mut a_workspace, WAIT, "layout a", |state| {
        state.layout.is_some()
    })
    .await
    .layout
    .clone()
    .unwrap();
    wait_workspace(&mut b_workspace, WAIT, "layout b", |state| {
        state.layout.is_some()
    })
    .await;

    let tab_a = TabId::new();
    let tab_b = TabId::new();
    a.apply(LayoutOp::NewTab {
        view: layout.active,
        tab: tab_a,
        pane: PaneId::new(),
        terminal: TerminalId::new(),
        name: Some("from-a".into()),
        size: Size::new(80, 24),
    })
    .wait()
    .await
    .expect("first tab");
    // `b` still holds the pre-conflict revision, so the daemon must reject the
    // stale base; the client refreshes, rebases and applies it.
    b.apply(LayoutOp::NewTab {
        view: layout.active,
        tab: tab_b,
        pane: PaneId::new(),
        terminal: TerminalId::new(),
        name: Some("from-b".into()),
        size: Size::new(80, 24),
    })
    .wait()
    .await
    .expect("reconciled tab");

    let deadline = Instant::now() + WAIT;
    loop {
        let both = |state: &WorkspaceState| {
            state.layout.as_ref().is_some_and(|layout| {
                let names: Vec<_> = layout
                    .views
                    .iter()
                    .flat_map(|view| &view.tabs)
                    .filter_map(|tab| tab.name.clone())
                    .collect();
                names.iter().any(|name| name == "from-a")
                    && names.iter().any(|name| name == "from-b")
            })
        };
        if both(&a.workspace()) && both(&b.workspace()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "layout never reconciled: {:?} / {:?}",
            a.workspace().layout,
            b.workspace().layout
        );
        a.refresh();
        b.refresh();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    a.shutdown().await;
    b.shutdown().await;
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cached_layout_and_mirror_render_before_the_server_answers() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut links = sup.links();
    let _ = wait_link(&mut links, WAIT, "QUIC link", |link| {
        link.kind() == TransportKind::Quic
    })
    .await;
    let cache_dir = h.dir.path().join("client-cache");
    let sync = Sync::new(
        links.clone(),
        Some(cache_dir.clone()),
        Config {
            cache_debounce: Duration::from_millis(50),
            cache_max_delay: Duration::from_millis(200),
            layout_refresh: Duration::from_secs(30),
        },
    );
    let workspace = sync.workspace_changes();
    let (handle, terminal) = spawn_terminal(&sync, workspace, ViewId::new()).await;
    handle.send_input(b"printf 'MARKER%s\\n' cached\n".to_vec());
    wait_text(&handle, "MARKERcached", WAIT).await;
    let incarnation = handle.incarnation();
    let live_checkpoint = handle
        .with_engine(|engine| engine.checkpoint().unwrap())
        .unwrap();
    // Let the debounced cache write land.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(cache_dir.join("workspace.bin").exists());
    sync.shutdown().await;
    sup.shutdown().await;

    // Offline: the same cache directory alone must render the last layout and
    // the last mirror state, marked stale until a server confirms it.
    let (_links_tx, links_rx) = watch::channel(None);
    let offline = Sync::new(links_rx, Some(cache_dir), Config::default());
    let state = offline.workspace();
    assert!(state.stale);
    assert!(state.layout.is_some());
    let mirror = offline.attach(terminal, incarnation, ViewId::new());
    assert!(mirror.state().status == MirrorStatus::Stale);
    // The mirror task loads its cache asynchronously; nothing touches a server.
    let deadline = Instant::now() + WAIT;
    while mirror.with_engine(|_| ()).is_none() {
        assert!(Instant::now() < deadline, "cached mirror never loaded");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        mirror
            .with_engine(|engine| engine.checkpoint().unwrap())
            .unwrap(),
        live_checkpoint
    );
    assert!(visible_text(&mirror).contains("MARKERcached"));
    offline.shutdown().await;
}

/// Title and working directory follow the canonical engine state (and so
/// survive a daemon restart), while bell and OSC 52 notices are delivered
/// once from live output and never replayed by the resumed subscription.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn titles_follow_state_and_live_notices_arrive_once() {
    let mut h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut links = sup.links();
    let _ = wait_link(&mut links, WAIT, "QUIC link", |link| {
        link.kind() == TransportKind::Quic
    })
    .await;
    let sync = Sync::new(links.clone(), None, Config::default());
    let (handle, _terminal) = spawn_terminal(&sync, sync.workspace_changes(), ViewId::new()).await;
    let mut events = handle.events();

    // OSC 2 title, OSC 7 cwd, BEL, OSC 52 store of "hi". The sleep keeps
    // the shell from returning to a prompt that may retitle the window.
    handle.send_input(
        b"printf '\\033]2;proj-%s\\a\\033]7;file://box/srv/x\\a\\a\\033]52;c;aGk=\\a' t; sleep 30\n"
            .to_vec(),
    );
    let mut title = None;
    let mut notices = Vec::new();
    tokio::time::timeout(WAIT, async {
        while title.is_none() || notices.len() < 2 {
            match events.recv().await {
                Ok(MirrorEvent::TitleChanged { title: t, cwd })
                    if t.as_deref() == Some("proj-t") =>
                {
                    title = Some((t, cwd))
                }
                Ok(MirrorEvent::Notice { sequence, notice }) => notices.push((sequence, notice)),
                Ok(_) => {}
                Err(error) => panic!("mirror events ended: {error}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("title {title:?}, notices {notices:?}"));
    assert_eq!(
        title,
        Some((Some("proj-t".into()), Some("file://box/srv/x".into())))
    );
    assert_eq!(notices[0].1, TerminalNotice::Bell);
    assert_eq!(
        notices[1].1,
        TerminalNotice::ClipboardStore {
            target: ClipboardTarget::Clipboard,
            text: "hi".into(),
        }
    );

    // Restart the daemon: the title comes back with the restored state and
    // the resumed subscription replays no notice.
    h.stop_daemon().await;
    let deadline = Instant::now() + WAIT;
    while handle.state().status == MirrorStatus::Live {
        assert!(Instant::now() < deadline, "mirror stayed live");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.daemon = Some(support::start_daemon(h.dir.path(), h.quic_port).await);
    wait_live(&handle, WAIT).await;
    // The child is still sleeping: the restored state carries its title.
    assert_eq!(handle.state().title.as_deref(), Some("proj-t"));
    assert_eq!(handle.state().cwd.as_deref(), Some("file://box/srv/x"));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, MirrorEvent::Notice { .. }),
            "replayed {event:?}"
        );
    }
    sync.shutdown().await;
    sup.shutdown().await;
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

async fn wait_workspaces(
    sync: &Sync,
    what: &str,
    pred: impl Fn(&WorkspacesState) -> bool,
) -> std::sync::Arc<WorkspacesState> {
    let mut changes = sync.workspaces_changes();
    let deadline = Instant::now() + WAIT;
    loop {
        if pred(&changes.borrow()) {
            return changes.borrow().clone();
        }
        assert!(
            Instant::now() < deadline,
            "workspaces never satisfied: {what}"
        );
        let _ = tokio::time::timeout(Duration::from_millis(200), changes.changed()).await;
    }
}

/// The client follows the daemon's workspaces: the tree and live +/- counts
/// arrive as pushed events, and a worktree's own layout spawns shells in
/// that worktree. A reconnect refetches everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspaces_status_and_worktree_layouts_follow_the_daemon() {
    let mut h = Harness::new().await;
    let repo = h.dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    let sup = Supervisor::spawn(h.spec(true));
    let mut links = sup.links();
    let _ = wait_link(&mut links, WAIT, "QUIC link", |link| {
        link.kind() == TransportKind::Quic
    })
    .await;
    let sync = Sync::new(links.clone(), None, Config::default());
    let state = wait_workspaces(&sync, "initial tree", |s| s.live && s.tree.is_some()).await;
    let workspace = state.tree.as_ref().unwrap().workspaces[0].id;

    // Requests can be awaited from executors other than Tokio (the UI's).
    let listing = std::thread::scope(|scope| {
        scope
            .spawn(|| futures::executor::block_on(sync.list_dir(repo.display().to_string())))
            .join()
            .unwrap()
    })
    .unwrap();
    assert!(listing.entries.iter().any(|entry| entry.name == "a.txt"));
    let project = sync
        .add_project(workspace, repo.display().to_string())
        .await
        .unwrap();
    let worktree = sync
        .create_worktree(project.id, Some("task".into()), None)
        .await
        .unwrap();
    wait_workspaces(&sync, "worktree in tree", |s| {
        s.worktree(worktree.id).is_some()
    })
    .await;

    // Live counts: a pushed status event reflects an edit.
    std::fs::write(std::path::Path::new(&worktree.path).join("b.txt"), "x\ny\n").unwrap();
    let state = wait_workspaces(&sync, "status with b.txt", |s| {
        s.statuses
            .get(&worktree.id)
            .is_some_and(|status| status.summary.added == 2)
    })
    .await;
    assert!(state.statuses[&worktree.id].summary.dirty);

    // The worktree's own layout: a new tab starts a shell in the worktree.
    let layout = sync.layout(LayoutScope::Worktree(worktree.id));
    let mut changes = layout.workspace_changes();
    let current = wait_workspace(&mut changes, WAIT, "worktree layout", |s| {
        s.layout.is_some()
    })
    .await
    .layout
    .clone()
    .unwrap();
    let terminal = TerminalId::new();
    layout
        .apply(LayoutOp::NewTab {
            view: current.active,
            tab: TabId::new(),
            pane: PaneId::new(),
            terminal,
            name: None,
            size: Size::new(80, 24),
        })
        .wait()
        .await
        .expect("worktree layout op acknowledged");
    let state = wait_workspace(&mut changes, WAIT, "worktree shell", |s| {
        s.processes.iter().any(|p| p.terminal == terminal)
    })
    .await;
    let incarnation = state
        .processes
        .iter()
        .find(|p| p.terminal == terminal)
        .unwrap()
        .incarnation;
    // The home layout does not contain the worktree's terminal.
    assert!(sync.workspace().layout.as_ref().is_none_or(|home| {
        home.views
            .iter()
            .flat_map(|v| &v.tabs)
            .flat_map(|t| &t.panes)
            .all(|p| p.terminal != terminal)
    }));
    let handle = sync.attach(terminal, incarnation, ViewId::new());
    wait_live(&handle, WAIT).await;
    handle.send_input(b"printf 'WT=%s\\n' \"$FLOTILLA_WORKTREE_NAME\"\n".to_vec());
    wait_text(&handle, &format!("WT={}", worktree.label), WAIT).await;

    // After a daemon restart the client refetches tree and status.
    h.stop_daemon().await;
    wait_workspaces(&sync, "offline", |s| !s.live).await;
    std::fs::write(std::path::Path::new(&worktree.path).join("c.txt"), "z\n").unwrap();
    h.daemon = Some(support::start_daemon(h.dir.path(), h.quic_port).await);
    let state = wait_workspaces(&sync, "refetched after restart", |s| {
        s.live
            && s.statuses
                .get(&worktree.id)
                .is_some_and(|status| status.summary.added == 3)
    })
    .await;
    assert!(state.worktree(worktree.id).is_some());
    sync.shutdown().await;
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sections_publish_replies_follow_events_and_refetch_after_reconnect() {
    use fl_model::SectionId;
    use fl_model::sections::{Section, SectionKind, SectionMutation};
    use fl_proto::{Request, Response, RpcErrorCode};

    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(false));
    let mut links = sup.links();
    let link = wait_link(&mut links, WAIT, "SSH link", |_| true).await;
    // Control which links the mirror follows, while keeping the real daemon
    // reachable for a change made while this mirror is disconnected.
    let (followed, following) = watch::channel(Some(link.clone()));
    let sync = Sync::new(following, None, Config::default());
    let initial = wait_workspaces(&sync, "initial sections", |s| {
        s.live && s.tree.is_some() && s.sections.is_some()
    }).await;
    let workspace = initial.tree.as_ref().unwrap().workspaces[0].id;
    let baseline = initial.sections.as_ref().unwrap().rev;
    let section = Section {
        id: SectionId::new(),
        workspace,
        name: "My tasks".into(),
        order: 0,
        priority: 0,
        kind: SectionKind::Manual,
    };
    let created = sync.mutate_sections(
        baseline,
        SectionMutation::Upsert(section.clone()),
    ).await.unwrap();
    assert_eq!(created.sections, vec![section.clone()]);
    assert_eq!(sync.workspaces().sections.as_ref(), Some(&created));
    assert_eq!(sync.list_sections().await.unwrap(), created);

    // A request outside Sync changes the daemon; only the event can update
    // the mirror with its new revision and name.
    let mut renamed = section.clone();
    renamed.name = "Renamed tasks".into();
    let pushed = match link.rpc(Request::MutateSections {
        expected_rev: created.rev,
        mutation: SectionMutation::Upsert(renamed.clone()),
    }).await.unwrap() {
        Response::SectionsState(state) => state,
        other => panic!("unexpected sections response: {other:?}"),
    };
    wait_workspaces(&sync, "pushed sections snapshot", |s| {
        s.sections.as_ref() == Some(&pushed)
    }).await;

    let error = sync.mutate_sections(
        created.rev,
        SectionMutation::Delete(section.id),
    ).await.unwrap_err();
    assert!(matches!(
        error,
        flc_sync::CallError::Rpc(error)
            if error.code() == Some(RpcErrorCode::RevisionConflict)
    ));
    assert_eq!(sync.workspaces().sections.as_ref(), Some(&pushed));

    followed.send(None).unwrap();
    wait_workspaces(&sync, "sections mirror offline", |s| !s.live).await;
    let deleted = match link.rpc(Request::MutateSections {
        expected_rev: pushed.rev,
        mutation: SectionMutation::Delete(section.id),
    }).await.unwrap() {
        Response::SectionsState(state) => state,
        other => panic!("unexpected sections response: {other:?}"),
    };
    assert_eq!(sync.workspaces().sections.as_ref(), Some(&pushed));
    followed.send(Some(link)).unwrap();
    let refreshed = wait_workspaces(&sync, "sections refetched on reconnect", |s| {
        s.live && s.sections.as_ref() == Some(&deleted)
    }).await;
    assert!(refreshed.sections.as_ref().unwrap().sections.is_empty());
    sync.shutdown().await;
    sup.shutdown().await;
}
