#![cfg(unix)]
//! End-to-end client sync against a real daemon, process host and shells:
//! mirrors, exactly-once input, link failover, daemon restart, outages,
//! snapshot recovery and layout reconciliation.
use std::time::{Duration, Instant};

use fl_model::layout::LayoutOp;
use fl_model::{PaneId, Size, TabId, TerminalId, ViewId};
use fl_proto::{ClipboardTarget, TerminalNotice};
use flc_conn::supervisor::{Supervisor, TransportKind};
use flc_sync::{
    Config, DiscardReason, MirrorEvent, MirrorHandle, MirrorStatus, Sync, WorkspaceState,
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
