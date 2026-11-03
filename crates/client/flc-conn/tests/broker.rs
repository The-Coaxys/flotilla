#![cfg(unix)]
//! Connection broker: multiplexed RPCs and ticket-bound channels on QUIC
//! streams and SSH channels, re-established on every link generation.
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use fl_model::{IncarnationId, SubscriptionId, TerminalId};
use fl_proto::{
    ChannelRole, Request, Response, RpcErrorCode, TerminalClientMsg, TerminalError,
    TerminalServerMsg,
};
use flc_conn::broker::{ControlLink, LogicalChannel, RpcCallError};
use flc_conn::supervisor::{Supervisor, TransportKind};

mod support;
use support::{Harness, wait_link};

const WAIT: Duration = Duration::from_secs(10);

/// A bound terminal channel answers a subscription for an unknown terminal.
async fn assert_channel_serves(channel: &mut LogicalChannel) {
    let subscribe = TerminalClientMsg::Subscribe {
        id: 7,
        subscription: SubscriptionId::new(),
        terminal: TerminalId::new(),
        incarnation: IncarnationId::new(),
        from: None,
    };
    channel.writer.send(&subscribe).await.unwrap();
    let reply = tokio::time::timeout(WAIT, channel.reader.recv::<TerminalServerMsg>())
        .await
        .expect("terminal channel reply")
        .unwrap();
    assert!(
        matches!(
            reply,
            TerminalServerMsg::Error {
                id: Some(7),
                error: TerminalError::NotFound,
                ..
            }
        ),
        "{reply:?}"
    );
}

/// Many concurrent calls exceed the in-flight window and still all resolve
/// with their own replies.
async fn assert_rpcs_multiplex(link: &ControlLink) {
    let mut calls = tokio::task::JoinSet::new();
    for index in 0..40 {
        let link = link.clone();
        calls.spawn(async move {
            let request = match index % 3 {
                0 => Request::Capabilities,
                1 => Request::GetLayout,
                _ => Request::DaemonInfo,
            };
            (index, link.rpc(request).await)
        });
    }
    while let Some(done) = calls.join_next().await {
        let (index, result) = done.unwrap();
        match (index % 3, result.unwrap()) {
            (0, Response::Capabilities(caps)) => assert_eq!(caps.engine_id, fl_term::ENGINE_ID),
            (1, Response::Layout(_)) | (2, Response::DaemonInfo(_)) => {}
            (_, other) => panic!("request {index} received {other:?}"),
        }
    }
    // Remote errors are typed, not link failures.
    let error = link
        .rpc(Request::ProcessInfo {
            terminal: TerminalId::new(),
            incarnation: IncarnationId::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(RpcErrorCode::NotFound), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpcs_and_channels_follow_quic_ssh_failover_and_back() {
    let h = Harness::new().await;
    let sup = Supervisor::spawn(h.spec(true));
    let mut links = sup.links();

    let quic = wait_link(&mut links, WAIT, "QUIC link", |l| {
        l.kind() == TransportKind::Quic
    })
    .await;
    assert_eq!(quic.client_session_id(), sup.client_session_id());
    assert_eq!(quic.daemon_proto(), fl_proto::PROTO);
    assert_rpcs_multiplex(&quic).await;
    let mut hot = quic.open_channel(ChannelRole::TerminalHot).await.unwrap();
    let bulk = quic.open_channel(ChannelRole::Bulk).await.unwrap();
    assert_eq!(
        (hot.generation, bulk.generation),
        (quic.generation(), quic.generation())
    );
    assert_channel_serves(&mut hot).await;

    // Black-hole UDP: the next generation runs over SSH, and the old link,
    // its calls and its channels are all closed.
    h.relay.set_dropping(true);
    let ssh = wait_link(&mut links, WAIT, "SSH link", |l| {
        l.kind() == TransportKind::Ssh
    })
    .await;
    assert!(ssh.generation() > quic.generation());
    assert!(quic.is_closed());
    assert_eq!(ssh.client_session_id(), quic.client_session_id());
    assert_ne!(ssh.server_session_id(), quic.server_session_id());
    assert!(matches!(
        quic.rpc(Request::Capabilities).await,
        Err(RpcCallError::LinkClosed)
    ));
    let old = tokio::time::timeout(WAIT, hot.reader.recv::<TerminalServerMsg>())
        .await
        .expect("an old-generation channel ends with its link");
    assert!(old.is_err(), "old channel delivered {old:?}");
    assert_rpcs_multiplex(&ssh).await;
    let mut hot = ssh.open_channel(ChannelRole::TerminalHot).await.unwrap();
    let _bulk = ssh.open_channel(ChannelRole::Bulk).await.unwrap();
    assert_eq!(hot.generation, ssh.generation());
    assert_channel_serves(&mut hot).await;

    // UDP returns: the supervisor upgrades to a new QUIC generation.
    h.relay.set_dropping(false);
    let back = wait_link(&mut links, Duration::from_secs(15), "QUIC again", |l| {
        l.kind() == TransportKind::Quic
    })
    .await;
    assert!(back.generation() > ssh.generation());
    assert!(ssh.is_closed());
    let mut hot = back.open_channel(ChannelRole::TerminalHot).await.unwrap();
    assert_channel_serves(&mut hot).await;
    sup.shutdown().await;
    assert!(back.is_closed());
    assert!(links.borrow().is_none());
}

/// Without `direct-streamlocal` (here: an unreachable socket path), both the
/// control link and every channel use `flotillad attach` over SSH exec.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_use_the_exec_attach_fallback() {
    let h = Harness::new().await;
    let socket = h.daemon.as_ref().unwrap().socket_path.clone();
    let bin = h.dir.path().join(".flotilla/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let attach = bin.join("flotillad");
    // A byte bridge standing in for `flotillad attach`.
    std::fs::write(
        &attach,
        format!(
            r#"#!/usr/bin/env python3
import os, socket, sys, threading
s = socket.socket(socket.AF_UNIX)
s.connect({socket:?})
def up():
    while True:
        b = os.read(0, 65536)
        if not b:
            s.shutdown(socket.SHUT_WR)
            return
        s.sendall(b)
threading.Thread(target=up, daemon=True).start()
while True:
    b = s.recv(65536)
    if not b:
        break
    view = memoryview(b)
    while view:
        view = view[os.write(1, view):]
"#,
            socket = socket.display().to_string()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&attach, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut spec = h.spec(false);
    spec.socket_path = h.dir.path().join("missing.sock").display().to_string();
    let sup = Supervisor::spawn(spec);
    let mut links = sup.links();
    let link = wait_link(&mut links, WAIT, "SSH exec link", |_| true).await;
    assert_eq!(link.kind(), TransportKind::Ssh);
    assert_rpcs_multiplex(&link).await;
    let mut hot = link.open_channel(ChannelRole::TerminalHot).await.unwrap();
    let _bulk = link.open_channel(ChannelRole::Bulk).await.unwrap();
    assert_channel_serves(&mut hot).await;
    sup.shutdown().await;
}
