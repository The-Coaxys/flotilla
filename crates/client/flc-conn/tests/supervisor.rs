#![cfg(unix)]
//! Supervisor tests against an in-process daemon, the test SSH server, and a
//! UDP relay that can black-hole QUIC.
use std::time::{Duration, Instant};

use fl_transport::ssh::HostKeyPolicy;
use flc_conn::supervisor::{ConnPhase, Supervisor, TransportKind};

mod support;
use support::{Harness, connected, start_daemon, wait_phase};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connects_over_quic_when_available() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut rx = sup.phase();
    match wait_phase(
        &mut rx,
        Duration::from_secs(5),
        "QUIC",
        connected(TransportKind::Quic),
    )
    .await
    {
        ConnPhase::Connected {
            rtt_ms,
            daemon_version,
            ..
        } => {
            assert!(rtt_ms < 1000);
            assert_eq!(daemon_version, fl_proto::PRODUCT_VERSION);
        }
        other => unreachable!("{other:?}"),
    }
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uses_ssh_when_quic_is_not_configured() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(false));
    let mut rx = sup.phase();
    wait_phase(
        &mut rx,
        Duration::from_secs(5),
        "SSH",
        connected(TransportKind::Ssh),
    )
    .await;
    sup.shutdown().await;
}

/// Review Focus #2: silently dropped UDP must not delay the first connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn falls_back_to_ssh_quickly_when_udp_is_blackholed() {
    let h = Harness::new().await;
    h.relay.set_dropping(true);
    let started = Instant::now();
    let sup = Supervisor::spawn(h.spec(true));
    let mut rx = sup.phase();
    wait_phase(
        &mut rx,
        Duration::from_secs(5),
        "SSH",
        connected(TransportKind::Ssh),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "SSH must win the race instead of waiting for the 2 s QUIC timeout (took {:?})",
        started.elapsed()
    );
    sup.shutdown().await;
}

/// Review Focus #2: a QUIC link that goes silent fails over to SSH, and the
/// supervisor upgrades back to QUIC once UDP flows again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fails_over_to_ssh_when_quic_dies_then_upgrades_back() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut rx = sup.phase();
    wait_phase(
        &mut rx,
        Duration::from_secs(5),
        "QUIC",
        connected(TransportKind::Quic),
    )
    .await;

    h.relay.set_dropping(true);
    let dropped_at = Instant::now();
    wait_phase(
        &mut rx,
        Duration::from_secs(5),
        "failover to SSH",
        connected(TransportKind::Ssh),
    )
    .await;
    assert!(
        dropped_at.elapsed() < Duration::from_secs(3),
        "failover took {:?}",
        dropped_at.elapsed()
    );

    h.relay.set_dropping(false);
    wait_phase(
        &mut rx,
        Duration::from_secs(10),
        "upgrade back to QUIC",
        connected(TransportKind::Quic),
    )
    .await;
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_after_the_ssh_session_is_killed() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(false));
    let mut rx = sup.phase();
    wait_phase(
        &mut rx,
        Duration::from_secs(5),
        "SSH",
        connected(TransportKind::Ssh),
    )
    .await;
    h.ssh.kill_sessions().await;
    // If the supervisor had not reconnected, it would be Reconnecting/Failed by now
    // (the killed link is noticed immediately, dead_after is 500 ms).
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let now = sup.current();
    assert!(
        connected(TransportKind::Ssh)(&now),
        "expected a fresh SSH link, got {now:?}"
    );
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_after_daemon_restart() {
    let mut h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(false));
    let mut rx = sup.phase();
    wait_phase(
        &mut rx,
        Duration::from_secs(5),
        "SSH",
        connected(TransportKind::Ssh),
    )
    .await;

    h.daemon.take().unwrap().shutdown().await;
    wait_phase(&mut rx, Duration::from_secs(5), "Reconnecting", |p| {
        matches!(p, ConnPhase::Reconnecting { .. })
    })
    .await;

    h.daemon = Some(start_daemon(h.dir.path()).await);
    wait_phase(
        &mut rx,
        Duration::from_secs(10),
        "SSH after restart",
        connected(TransportKind::Ssh),
    )
    .await;
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_key_mismatch_is_fatal() {
    let h = Harness::new().await;
    let mut spec = h.spec(false);
    spec.host_key = HostKeyPolicy::Expect("SHA256:somebody-else".into());
    let sup = Supervisor::spawn(spec);
    let mut rx = sup.phase();
    match wait_phase(&mut rx, Duration::from_secs(5), "Failed", |p| {
        matches!(p, ConnPhase::Failed { .. })
    })
    .await
    {
        ConnPhase::Failed { error } => assert!(error.contains("host key"), "{error}"),
        other => unreachable!("{other:?}"),
    }
    sup.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_returns_to_idle() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut rx = sup.phase();
    wait_phase(&mut rx, Duration::from_secs(5), "connected", |p| {
        matches!(p, ConnPhase::Connected { .. })
    })
    .await;
    sup.shutdown().await;
    assert_eq!(*rx.borrow(), ConnPhase::Idle);
}

async fn shutdown_closes_pending_ssh(with_quic: bool) {
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    let h = Harness::new().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut spec = h.spec(with_quic);
    spec.ssh.port = listener.local_addr().unwrap().port();
    spec.timings.connect_timeout = Duration::from_secs(30);
    let sup = Supervisor::spawn(spec);
    let mut rx = sup.phase();
    // Keep the TCP connection alive without completing the SSH handshake.
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    if with_quic {
        wait_phase(
            &mut rx,
            Duration::from_secs(5),
            "QUIC while SSH is warming",
            connected(TransportKind::Quic),
        )
        .await;
    }
    sup.shutdown().await;
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut received))
        .await
        .expect("shutdown left the SSH connection attempt running")
        .unwrap();
    assert_eq!(*rx.borrow(), ConnPhase::Idle);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_cancels_connecting_ssh() {
    shutdown_closes_pending_ssh(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_cancels_standby_ssh_warmup() {
    shutdown_closes_pending_ssh(true).await;
}

/// A resume or focus change retries immediately with a fresh backlog: the
/// supervisor must not sit out a long backoff after the machine wakes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_now_resets_the_backoff_and_attempts_at_once() {
    let mut h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut rx = sup.phase();
    wait_phase(
        &mut rx,
        Duration::from_secs(5),
        "connected",
        connected(TransportKind::Quic),
    )
    .await;

    // Take the transports away and let the attempt counter climb past the
    // point where the next wait would be seconds long.
    h.stop_daemon().await;
    h.relay.set_dropping(true);
    h.ssh.kill_sessions().await;
    let mut attempts = Vec::new();
    while attempts.len() < 3 {
        let phase = wait_phase(
            &mut rx,
            Duration::from_secs(15),
            "retry",
            |phase| matches!(phase, ConnPhase::Reconnecting { attempt, .. } if *attempt >= 3),
        )
        .await;
        let ConnPhase::Reconnecting { attempt, .. } = phase else {
            continue;
        };
        attempts.push(attempt);
    }
    // A resume must not wait out the seconds-long backoff that three failures
    // accumulated: a new attempt starts at once.
    let before = sup.attempts();
    let started = Instant::now();
    sup.retry_now();
    while sup.attempts() == before {
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "no attempt after a resume: {:?}",
            sup.current()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = &mut rx;
    sup.shutdown().await;
}
