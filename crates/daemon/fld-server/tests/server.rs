//! Daemon integration tests: real Unix socket and real QUIC, in-process.
use std::path::Path;
use std::time::Duration;

use fl_proto::{ClientMsg, Hello, PROTO, ProtoVersion, RejectReason, Request, Response, ServerMsg};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter, framed};
use fl_transport::quic;
use fld_server::devices::DeviceRegistry;
use fld_server::{DaemonConfig, DaemonHandle, FlotillaHome, ServerError};
use tokio::net::UnixStream;

fn home_in(dir: &Path) -> FlotillaHome {
    FlotillaHome::new(dir.join(".flotilla"))
}

async fn start_daemon(dir: &Path) -> Result<DaemonHandle, ServerError> {
    let mut config = DaemonConfig::new(home_in(dir));
    config.quic_bind = Some("127.0.0.1:0".parse().unwrap());
    config.hello_timeout = Duration::from_millis(300);
    fld_server::start(config).await
}

async fn local(d: &DaemonHandle) -> (FrameReader, FrameWriter) {
    let s = UnixStream::connect(&d.socket_path).await.unwrap();
    let (r, w) = s.into_split();
    framed(r, w)
}

struct QuicLink {
    _endpoint: quinn::Endpoint,
    _conn: quinn::Connection,
    reader: FrameReader,
    writer: FrameWriter,
}

async fn over_quic(d: &DaemonHandle) -> QuicLink {
    let (endpoint, conn) =
        quic::connect(d.quic_addr.unwrap(), d.cert_sha256, Duration::from_secs(5))
            .await
            .unwrap();
    let (send, recv) = quic::open_ctl(&conn).await.unwrap();
    let (reader, writer) = framed(recv, send);
    QuicLink {
        _endpoint: endpoint,
        _conn: conn,
        reader,
        writer,
    }
}

fn hello_with(proto: ProtoVersion, device_id: Option<&str>, token: Option<&str>) -> ClientMsg {
    ClientMsg::Hello(Hello {
        client_version: "test".into(),
        proto,
        device_id: device_id.map(Into::into),
        token: token.map(Into::into),
        client_session_id: [7; 16],
    })
}

fn hello() -> ClientMsg {
    hello_with(PROTO, None, None)
}

async fn recv(reader: &mut FrameReader) -> ServerMsg {
    tokio::time::timeout(Duration::from_secs(5), reader.recv::<ServerMsg>())
        .await
        .expect("reply within 5 s")
        .expect("valid reply")
}

#[tokio::test]
async fn local_hello_gets_welcome_and_ping_pong() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    match recv(&mut r).await {
        ServerMsg::Welcome(welcome) => {
            assert_eq!(welcome.proto, PROTO);
            assert!(!welcome.hostname.is_empty());
        }
        other => panic!("expected Welcome, got {other:?}"),
    }
    w.send(&ClientMsg::Ping { nonce: 42 }).await.unwrap();
    assert_eq!(recv(&mut r).await, ServerMsg::Pong { nonce: 42 });
    d.shutdown().await;
}

#[tokio::test]
async fn daemon_info_rpc_reports_pid_and_quic_port() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    recv(&mut r).await;
    w.send(&ClientMsg::Rpc {
        id: 5,
        req: Request::DaemonInfo,
    })
    .await
    .unwrap();
    match recv(&mut r).await {
        ServerMsg::RpcResult {
            id: 5,
            result: Ok(Response::DaemonInfo(info)),
        } => {
            assert_eq!(info.pid, std::process::id());
            assert_eq!(info.quic_port, d.quic_addr.map(|a| a.port()));
            assert_eq!(info.socket_path, d.socket_path.display().to_string());
            assert_eq!(info.version, fl_proto::PRODUCT_VERSION);
        }
        other => panic!("expected DaemonInfo, got {other:?}"),
    }
    d.shutdown().await;
}

#[tokio::test]
async fn quic_with_enrolled_token_is_welcomed() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (id, token) = DeviceRegistry::new(home_in(dir.path()).devices_path())
        .enroll("pc")
        .unwrap();
    let mut q = over_quic(&d).await;
    q.writer
        .send(&hello_with(PROTO, Some(&id), Some(&token)))
        .await
        .unwrap();
    assert!(matches!(recv(&mut q.reader).await, ServerMsg::Welcome(_)));
    q.writer.send(&ClientMsg::Ping { nonce: 1 }).await.unwrap();
    assert_eq!(recv(&mut q.reader).await, ServerMsg::Pong { nonce: 1 });
    d.shutdown().await;
}

#[tokio::test]
async fn quic_with_bad_token_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (id, _token) = DeviceRegistry::new(home_in(dir.path()).devices_path())
        .enroll("pc")
        .unwrap();
    let mut q = over_quic(&d).await;
    q.writer
        .send(&hello_with(PROTO, Some(&id), Some(&"00".repeat(32))))
        .await
        .unwrap();
    assert_eq!(
        recv(&mut q.reader).await,
        ServerMsg::Rejected(RejectReason::BadToken)
    );
    d.shutdown().await;
}

#[tokio::test]
async fn quic_without_token_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let mut q = over_quic(&d).await;
    q.writer.send(&hello()).await.unwrap();
    assert_eq!(
        recv(&mut q.reader).await,
        ServerMsg::Rejected(RejectReason::BadToken)
    );
    d.shutdown().await;
}

#[tokio::test]
async fn silent_client_gets_hello_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (mut r, _w) = local(&d).await;
    assert_eq!(
        recv(&mut r).await,
        ServerMsg::Rejected(RejectReason::HelloTimeout)
    );
    assert!(matches!(
        r.recv::<ServerMsg>().await,
        Err(ConnError::Closed)
    ));
    d.shutdown().await;
}

#[tokio::test]
async fn first_message_must_be_hello() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (mut r, mut w) = local(&d).await;
    w.send(&ClientMsg::Ping { nonce: 1 }).await.unwrap();
    assert_eq!(
        recv(&mut r).await,
        ServerMsg::Rejected(RejectReason::ExpectedHello)
    );
    d.shutdown().await;
}

#[tokio::test]
async fn incompatible_protocol_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let (mut r, mut w) = local(&d).await;
    w.send(&hello_with(
        ProtoVersion {
            major: PROTO.major + 1,
            minor: 0,
        },
        None,
        None,
    ))
    .await
    .unwrap();
    assert_eq!(
        recv(&mut r).await,
        ServerMsg::Rejected(RejectReason::ProtoMismatch { server: PROTO })
    );
    d.shutdown().await;
}

/// Review Focus #3.
#[tokio::test]
async fn stale_socket_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let home = home_in(dir.path());
    home.ensure_dirs().unwrap();
    {
        // Bind and drop: the socket file stays behind with nobody listening.
        let _l = std::os::unix::net::UnixListener::bind(home.socket_path()).unwrap();
    }
    assert!(home.socket_path().exists());
    let d = start_daemon(dir.path())
        .await
        .expect("a stale socket must not block startup");
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    assert!(matches!(recv(&mut r).await, ServerMsg::Welcome(_)));
    d.shutdown().await;
}

/// Review Focus #3.
#[tokio::test]
async fn second_daemon_on_same_home_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let err = start_daemon(dir.path())
        .await
        .err()
        .expect("second daemon must be refused");
    assert!(matches!(err, ServerError::AlreadyRunning(_)), "got {err}");
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    assert!(
        matches!(recv(&mut r).await, ServerMsg::Welcome(_)),
        "first daemon must keep working"
    );
    d.shutdown().await;
}

#[tokio::test]
async fn shutdown_ends_sessions_and_removes_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = home_in(dir.path());
    let d = start_daemon(dir.path()).await.unwrap();
    assert!(fld_server::read_runtime_info(&home).is_some());
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    recv(&mut r).await;
    let socket = d.socket_path.clone();
    d.shutdown().await;
    let next = tokio::time::timeout(Duration::from_secs(2), r.recv::<ServerMsg>())
        .await
        .expect("open sessions must end on shutdown");
    assert!(matches!(next, Err(ConnError::Closed)), "got {next:?}");
    assert!(!socket.exists());
    assert!(fld_server::read_runtime_info(&home).is_none());
}

#[tokio::test]
async fn identity_is_stable_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let first = start_daemon(dir.path()).await.unwrap();
    let fp = first.cert_sha256;
    first.shutdown().await;
    let second = start_daemon(dir.path()).await.unwrap();
    assert_eq!(second.cert_sha256, fp);
    second.shutdown().await;
}

#[tokio::test]
async fn runtime_info_describes_the_running_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let d = start_daemon(dir.path()).await.unwrap();
    let info = fld_server::read_runtime_info(&home_in(dir.path())).unwrap();
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.quic_port, d.quic_addr.map(|a| a.port()));
    assert_eq!(info.socket_path, d.socket_path.display().to_string());
    d.shutdown().await;
}

#[tokio::test]
async fn quic_port_in_use_disables_quic_but_daemon_runs() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut config = DaemonConfig::new(home_in(dir.path()));
    config.quic_bind = Some(blocker.local_addr().unwrap());
    let d = fld_server::start(config).await.unwrap();
    assert!(d.quic_addr.is_none());
    let (mut r, mut w) = local(&d).await;
    w.send(&hello()).await.unwrap();
    assert!(matches!(recv(&mut r).await, ServerMsg::Welcome(_)));
    d.shutdown().await;
}
