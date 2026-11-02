//! Connection supervisor: keeps one live control link to the daemon.
//!
//! * The first connect races QUIC against SSH and takes whichever completes
//!   first, so silently dropped UDP never delays the first link.
//! * While connected it sends `Ping` every `heartbeat` and declares the link
//!   dead after `dead_after` without any frame from the daemon.
//! * The authenticated SSH session is kept warm, so failover only costs a
//!   channel open and a Hello round trip.
//! * While on SSH it re-probes QUIC every `quic_reprobe` and upgrades when it works.
//! * Retries use exponential backoff with ±20 % jitter. Host-key, auth, key and
//!   protocol-version problems are fatal (`ConnPhase::Failed`).
use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fl_proto::{ClientMsg, Hello, PROTO, RejectReason, ServerMsg};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter, framed};
use fl_transport::quic::{self, QuicError};
use fl_transport::ssh::{HostKeyPolicy, SshAuth, SshError, SshSession, SshTarget};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

/// Fallback when the SSH server refuses `direct-streamlocal` forwarding.
pub const ATTACH_COMMAND: &str =
    r#"sh -c 'exec "${FLOTILLA_HOME:-$HOME/.flotilla}/bin/flotillad" attach'"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Quic,
    Ssh,
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TransportKind::Quic => "QUIC",
            TransportKind::Ssh => "SSH",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnPhase {
    Idle,
    Connecting,
    Connected {
        kind: TransportKind,
        rtt_ms: u32,
        daemon_version: String,
    },
    Reconnecting {
        attempt: u32,
        error: String,
    },
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct Timings {
    pub heartbeat: Duration,
    pub dead_after: Duration,
    pub quic_reprobe: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub connect_timeout: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(2),
            dead_after: Duration::from_secs(6),
            quic_reprobe: Duration::from_secs(30),
            backoff_min: Duration::from_millis(250),
            backoff_max: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuicSpec {
    pub host: String,
    pub port: u16,
    pub cert_sha256: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct ConnectSpec {
    pub ssh: SshTarget,
    pub auth: SshAuth,
    pub host_key: HostKeyPolicy,
    pub socket_path: String,
    pub quic: Option<QuicSpec>,
    pub device_id: Option<String>,
    pub token: Option<String>,
    pub client_version: String,
    pub timings: Timings,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("SSH: {0}")]
    Ssh(#[from] SshError),
    #[error("QUIC: {0}")]
    Quic(#[from] QuicError),
    #[error("{0}")]
    Conn(#[from] ConnError),
    #[error("the daemon rejected the connection: {0:?}")]
    Rejected(RejectReason),
    #[error("handshake timed out")]
    HandshakeTimeout,
    #[error("unexpected message during handshake")]
    Unexpected,
    #[error("cannot resolve {0}")]
    Resolve(String),
    #[error("QUIC is not configured for this host")]
    QuicNotConfigured,
    #[error("connection task failed: {0}")]
    Task(String),
}

impl ConnectError {
    /// Errors that retrying cannot fix without user action.
    pub fn is_fatal(&self) -> bool {
        match self {
            ConnectError::Ssh(e) => e.is_fatal(),
            ConnectError::Rejected(RejectReason::ProtoMismatch { .. }) => true,
            _ => false,
        }
    }
}

pub struct Supervisor {
    phase: watch::Receiver<ConnPhase>,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Supervisor {
    /// Must be called from inside a Tokio runtime.
    pub fn spawn(spec: ConnectSpec) -> Self {
        Self::spawn_on(&tokio::runtime::Handle::current(), spec)
    }

    /// Spawn onto `runtime` (callable from any thread, e.g. the UI thread).
    pub fn spawn_on(runtime: &tokio::runtime::Handle, spec: ConnectSpec) -> Self {
        let (tx, rx) = watch::channel(ConnPhase::Idle);
        let cancel = CancellationToken::new();
        let task = runtime.spawn(run(spec, tx, cancel.clone()));
        Self {
            phase: rx,
            cancel,
            task: Some(task),
        }
    }

    pub fn phase(&self) -> watch::Receiver<ConnPhase> {
        self.phase.clone()
    }

    pub fn current(&self) -> ConnPhase {
        self.phase.borrow().clone()
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Stop and wait until the supervisor has closed its links (phase becomes `Idle`).
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Shared {
    spec: ConnectSpec,
    /// Authenticated SSH session kept warm for fast failover.
    ssh: Mutex<Option<Arc<SshSession>>>,
}

struct Link {
    kind: TransportKind,
    reader: FrameReader,
    writer: FrameWriter,
    daemon_version: String,
    handshake_rtt: Duration,
    /// Keeps the QUIC endpoint and connection alive as long as the link.
    _quic: Option<(quinn::Endpoint, quinn::Connection)>,
}

// The enum is short-lived and moved once per upgrade, so the size gap is harmless.
#[allow(clippy::large_enum_variant)]
enum LinkEnd {
    Cancelled,
    Dead(String),
    Upgrade(Link),
}

type LinkTask = JoinHandle<Result<Link, ConnectError>>;

fn set(tx: &watch::Sender<ConnPhase>, next: ConnPhase) {
    tx.send_if_modified(|current| {
        if *current == next {
            false
        } else {
            *current = next;
            true
        }
    });
}

fn ms(d: Duration) -> u32 {
    d.as_millis().min(u32::MAX as u128) as u32
}

fn backoff(t: &Timings, attempt: u32) -> Duration {
    let exp = t
        .backoff_min
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
    let capped = exp.min(t.backoff_max);
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    let jitter = 0.8 + 0.4 * (u16::from_le_bytes(b) as f64 / u16::MAX as f64);
    capped.mul_f64(jitter)
}

async fn run(spec: ConnectSpec, phase: watch::Sender<ConnPhase>, cancel: CancellationToken) {
    let shared = Arc::new(Shared {
        spec,
        ssh: Mutex::new(None),
    });
    let mut attempt: u32 = 0;
    let mut warmup = None;
    let mut failed = false;
    set(&phase, ConnPhase::Connecting);
    'outer: loop {
        let established = establish(&shared, &cancel, &mut warmup).await;
        if cancel.is_cancelled() {
            break;
        }
        match established {
            Ok(mut link) => {
                let connected_at = Instant::now();
                let reason = loop {
                    set(
                        &phase,
                        ConnPhase::Connected {
                            kind: link.kind,
                            rtt_ms: ms(link.handshake_rtt),
                            daemon_version: link.daemon_version.clone(),
                        },
                    );
                    match drive(&shared, &mut link, &phase, &cancel, &mut warmup).await {
                        LinkEnd::Cancelled => break 'outer,
                        LinkEnd::Upgrade(better) => link = better,
                        LinkEnd::Dead(reason) => break reason,
                    }
                };
                drop(link);
                // A link that lived a while resets the backoff; a flapping one does not.
                if connected_at.elapsed() >= shared.spec.timings.dead_after {
                    attempt = 0;
                }
                attempt += 1;
                set(
                    &phase,
                    ConnPhase::Reconnecting {
                        attempt,
                        error: reason,
                    },
                );
                if attempt > 1 {
                    let delay = backoff(&shared.spec.timings, attempt);
                    tokio::select! {
                        _ = cancel.cancelled() => break 'outer,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
            Err(e) if e.is_fatal() => {
                set(
                    &phase,
                    ConnPhase::Failed {
                        error: e.to_string(),
                    },
                );
                failed = true;
                break;
            }
            Err(e) => {
                attempt += 1;
                set(
                    &phase,
                    ConnPhase::Reconnecting {
                        attempt,
                        error: e.to_string(),
                    },
                );
                let delay = backoff(&shared.spec.timings, attempt);
                tokio::select! {
                    _ = cancel.cancelled() => break 'outer,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
    stop_link_task(&mut warmup).await;
    if let Some(session) = shared.ssh.lock().await.take() {
        session.disconnect().await;
    }
    if !failed {
        set(&phase, ConnPhase::Idle);
    }
}

async fn join_link(task: &mut Option<LinkTask>) -> Result<Link, ConnectError> {
    match task.as_mut() {
        Some(t) => match t.await {
            Ok(result) => result,
            Err(e) => Err(ConnectError::Task(e.to_string())),
        },
        None => std::future::pending().await,
    }
}

async fn stop_link_task(task: &mut Option<LinkTask>) {
    if let Some(task) = task.take() {
        task.abort();
        let _ = task.await;
    }
}

/// Race QUIC (if configured) against SSH; first success wins.
async fn establish(
    shared: &Arc<Shared>,
    cancel: &CancellationToken,
    warmup: &mut Option<LinkTask>,
) -> Result<Link, ConnectError> {
    stop_link_task(warmup).await;
    let mut quic_task: Option<LinkTask> = shared.spec.quic.clone().map(|q| {
        let s = shared.clone();
        tokio::spawn(async move { quic_link(&s.spec, &q).await })
    });
    let mut ssh_task: Option<LinkTask> = Some({
        let s = shared.clone();
        tokio::spawn(async move { ssh_link(&s).await })
    });
    let mut quic_err = if shared.spec.quic.is_none() {
        Some(ConnectError::QuicNotConfigured)
    } else {
        None
    };
    let mut ssh_err: Option<ConnectError> = None;
    let result = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break Err(ConnectError::Task("cancelled".into())),
            r = join_link(&mut quic_task), if quic_task.is_some() => {
                quic_task = None;
                match r {
                    Ok(link) => break Ok(link),
                    Err(e) => quic_err = Some(e),
                }
            }
            r = join_link(&mut ssh_task), if ssh_task.is_some() => {
                ssh_task = None;
                match r {
                    Ok(link) => break Ok(link),
                    Err(e) => ssh_err = Some(e),
                }
            }
        }
        if quic_task.is_none() && ssh_task.is_none() {
            let ssh_err = ssh_err
                .take()
                .unwrap_or_else(|| ConnectError::Task("SSH attempt vanished".into()));
            let quic_fatal = quic_err.as_ref().is_some_and(ConnectError::is_fatal);
            break Err(if !ssh_err.is_fatal() && quic_fatal {
                quic_err.take().expect("checked above")
            } else {
                ssh_err
            });
        }
    };
    // The supervisor owns standby SSH until drive consumes it or shutdown joins it.
    if result.is_ok() && !cancel.is_cancelled() {
        *warmup = ssh_task.take();
    }
    stop_link_task(&mut ssh_task).await;
    stop_link_task(&mut quic_task).await;
    result
}

async fn ssh_session(shared: &Shared) -> Result<(Arc<SshSession>, bool), SshError> {
    let mut slot = shared.ssh.lock().await;
    if let Some(s) = slot.as_ref()
        && !s.is_closed()
    {
        return Ok((s.clone(), true));
    }
    *slot = None;
    let spec = &shared.spec;
    let session = Arc::new(
        SshSession::connect(
            &spec.ssh,
            &spec.auth,
            &spec.host_key,
            spec.timings.connect_timeout,
        )
        .await?,
    );
    *slot = Some(session.clone());
    Ok((session, false))
}

async fn forget_ssh_session(shared: &Shared, session: &Arc<SshSession>) {
    let mut slot = shared.ssh.lock().await;
    if slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, session)) {
        *slot = None;
    }
}

async fn ssh_link(shared: &Shared) -> Result<Link, ConnectError> {
    let t = &shared.spec.timings;
    let (session, reused) = ssh_session(shared).await?;
    if reused {
        // A cached session may be silently dead after a network change:
        // give it a short leash, then retry on a fresh session.
        match tokio::time::timeout(t.heartbeat * 2, open_ssh_link(&shared.spec, &session)).await {
            Ok(Ok(link)) => return Ok(link),
            // The session answered, so the problem is elsewhere (e.g. daemon down).
            Ok(Err(e)) if !session.is_closed() => return Err(e),
            _ => forget_ssh_session(shared, &session).await,
        }
        let (fresh, _) = ssh_session(shared).await?;
        return tokio::time::timeout(t.connect_timeout, open_ssh_link(&shared.spec, &fresh))
            .await
            .map_err(|_| ConnectError::HandshakeTimeout)?;
    }
    tokio::time::timeout(t.connect_timeout, open_ssh_link(&shared.spec, &session))
        .await
        .map_err(|_| ConnectError::HandshakeTimeout)?
}

async fn open_ssh_link(spec: &ConnectSpec, session: &SshSession) -> Result<Link, ConnectError> {
    let stream = match session.open_unix_socket(&spec.socket_path).await {
        Ok(stream) => stream,
        Err(SshError::SocketUnavailable { .. }) if !session.is_closed() => {
            session.open_exec_stream(ATTACH_COMMAND).await?
        }
        Err(e) => return Err(e.into()),
    };
    let (r, w) = tokio::io::split(stream);
    let (reader, writer) = framed(r, w);
    handshake(spec, TransportKind::Ssh, reader, writer, None).await
}

async fn quic_link(spec: &ConnectSpec, q: &QuicSpec) -> Result<Link, ConnectError> {
    tokio::time::timeout(spec.timings.connect_timeout, async {
        let addresses = tokio::net::lookup_host((q.host.as_str(), q.port))
            .await
            .map_err(|e| ConnectError::Resolve(format!("{}: {e}", q.host)))?;
        quic_link_addresses(spec, q, addresses).await
    })
    .await
    .map_err(|_| ConnectError::HandshakeTimeout)?
}

async fn quic_link_addresses(
    spec: &ConnectSpec,
    q: &QuicSpec,
    addresses: impl IntoIterator<Item = SocketAddr>,
) -> Result<Link, ConnectError> {
    let mut attempts = FuturesUnordered::new();
    for address in addresses {
        attempts.push(quic_link_address(spec, q, address));
    }
    let mut last_error = None;
    while let Some(result) = attempts.next().await {
        match result {
            Ok(link) => return Ok(link),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.unwrap_or_else(|| ConnectError::Resolve(q.host.clone())))
}

async fn quic_link_address(
    spec: &ConnectSpec,
    q: &QuicSpec,
    address: SocketAddr,
) -> Result<Link, ConnectError> {
    let (endpoint, conn) =
        quic::connect(address, q.cert_sha256, spec.timings.connect_timeout).await?;
    let (send, recv) = quic::open_ctl(&conn).await?;
    let (reader, writer) = framed(recv, send);
    handshake(
        spec,
        TransportKind::Quic,
        reader,
        writer,
        Some((endpoint, conn)),
    )
    .await
}

async fn handshake(
    spec: &ConnectSpec,
    kind: TransportKind,
    mut reader: FrameReader,
    mut writer: FrameWriter,
    quic: Option<(quinn::Endpoint, quinn::Connection)>,
) -> Result<Link, ConnectError> {
    let mut client_session_id = [0u8; 16];
    let _ = getrandom::fill(&mut client_session_id);
    let started = Instant::now();
    writer
        .send(&ClientMsg::Hello(Hello {
            client_version: spec.client_version.clone(),
            proto: PROTO,
            device_id: spec.device_id.clone(),
            token: spec.token.clone(),
            client_session_id,
        }))
        .await?;
    let reply = tokio::time::timeout(spec.timings.connect_timeout, reader.recv::<ServerMsg>())
        .await
        .map_err(|_| ConnectError::HandshakeTimeout)??;
    match reply {
        ServerMsg::Welcome(w) => Ok(Link {
            kind,
            reader,
            writer,
            daemon_version: w.daemon_version,
            handshake_rtt: started.elapsed(),
            _quic: quic,
        }),
        ServerMsg::Rejected(reason) => Err(ConnectError::Rejected(reason)),
        _ => Err(ConnectError::Unexpected),
    }
}

/// Heartbeat the link, report RTT, and (on SSH) re-probe QUIC.
async fn drive(
    shared: &Arc<Shared>,
    link: &mut Link,
    phase: &watch::Sender<ConnPhase>,
    cancel: &CancellationToken,
    warmup: &mut Option<LinkTask>,
) -> LinkEnd {
    let t = shared.spec.timings.clone();
    let mut heartbeat = tokio::time::interval(t.heartbeat);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let can_upgrade = link.kind == TransportKind::Ssh && shared.spec.quic.is_some();
    let mut reprobe =
        tokio::time::interval_at(tokio::time::Instant::now() + t.quic_reprobe, t.quic_reprobe);
    reprobe.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut probe: Option<LinkTask> = None;
    let mut last_rx = Instant::now();
    let mut next_nonce: u64 = 0;
    let mut in_flight: HashMap<u64, Instant> = HashMap::new();
    let mut rtt: Option<Duration> = None;

    let end = loop {
        tokio::select! {
            _ = cancel.cancelled() => break LinkEnd::Cancelled,
            _ = join_link(warmup), if warmup.is_some() => {
                *warmup = None;
            }
            _ = heartbeat.tick() => {
                let silent = last_rx.elapsed();
                if silent >= t.dead_after {
                    break LinkEnd::Dead(format!("no response from the daemon for {} ms", silent.as_millis()));
                }
                next_nonce += 1;
                in_flight.insert(next_nonce, Instant::now());
                in_flight.retain(|_, sent| sent.elapsed() < t.dead_after);
                match tokio::time::timeout(t.heartbeat, link.writer.send(&ClientMsg::Ping { nonce: next_nonce })).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => break LinkEnd::Dead(format!("send failed: {e}")),
                    Err(_) => break LinkEnd::Dead("send timed out".into()),
                }
            }
            msg = link.reader.recv::<ServerMsg>() => match msg {
                Ok(ServerMsg::Pong { nonce: echoed }) => {
                    last_rx = Instant::now();
                    if let Some(sent) = in_flight.remove(&echoed) {
                        let sample = sent.elapsed();
                        let smoothed = rtt.map_or(sample, |prev| (prev * 7 + sample) / 8);
                        rtt = Some(smoothed);
                        set(phase, ConnPhase::Connected {
                            kind: link.kind,
                            rtt_ms: ms(smoothed),
                            daemon_version: link.daemon_version.clone(),
                        });
                    }
                }
                Ok(_) => last_rx = Instant::now(),
                Err(e) => break LinkEnd::Dead(format!("link lost: {e}")),
            },
            _ = reprobe.tick(), if can_upgrade && probe.is_none() => {
                let s = shared.clone();
                let q = shared.spec.quic.clone().expect("can_upgrade implies QUIC is configured");
                probe = Some(tokio::spawn(async move { quic_link(&s.spec, &q).await }));
            }
            r = join_link(&mut probe), if probe.is_some() => {
                probe = None;
                if let Ok(better) = r {
                    break LinkEnd::Upgrade(better);
                }
            }
        }
    };
    stop_link_task(&mut probe).await;
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_is_capped_with_jitter() {
        let t = Timings::default();
        for _ in 0..50 {
            let first = backoff(&t, 1);
            assert!(
                first >= Duration::from_millis(200) && first <= Duration::from_millis(300),
                "{first:?}"
            );
            let third = backoff(&t, 3);
            assert!(
                third >= Duration::from_millis(800) && third <= Duration::from_millis(1200),
                "{third:?}"
            );
            let late = backoff(&t, 40);
            assert!(
                late >= Duration::from_secs(4) && late <= Duration::from_secs(6),
                "{late:?}"
            );
        }
    }

    #[test]
    fn transport_kind_display() {
        assert_eq!(TransportKind::Quic.to_string(), "QUIC");
        assert_eq!(TransportKind::Ssh.to_string(), "SSH");
    }

    #[test]
    fn fatality() {
        let mismatch = SshError::HostKeyMismatch {
            expected: "a".into(),
            actual: "b".into(),
        };
        assert!(ConnectError::Ssh(mismatch).is_fatal());
        assert!(ConnectError::Rejected(RejectReason::ProtoMismatch { server: PROTO }).is_fatal());
        assert!(!ConnectError::Rejected(RejectReason::BadToken).is_fatal());
        assert!(!ConnectError::HandshakeTimeout.is_fatal());
        assert!(!ConnectError::Ssh(SshError::Timeout).is_fatal());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn quic_tries_ipv4_when_first_address_is_unreachable_ipv6() {
        let dir = tempfile::tempdir().unwrap();
        let home = fld_server::FlotillaHome::new(dir.path());
        let mut config = fld_server::DaemonConfig::new(home.clone());
        config.quic_bind = Some("127.0.0.1:0".parse().unwrap());
        let daemon = fld_server::start(config).await.unwrap();
        let (device_id, token) = fld_server::devices::DeviceRegistry::new(home.devices_path())
            .enroll("address regression")
            .unwrap();
        // Retain an IPv6 endpoint which never accepts TLS, rather than depending
        // on DNS order, disabled IPv6, or an unbound UDP port's ICMP behavior.
        let blackhole = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
        let q = QuicSpec {
            host: "localhost".into(),
            port: daemon.quic_addr.unwrap().port(),
            cert_sha256: daemon.cert_sha256,
        };
        let spec = ConnectSpec {
            ssh: SshTarget {
                host: "127.0.0.1".into(),
                port: 22,
                user: "unused".into(),
            },
            auth: SshAuth::Agent { socket: None },
            host_key: HostKeyPolicy::TrustOnFirstUse,
            socket_path: daemon.socket_path.display().to_string(),
            quic: Some(q.clone()),
            device_id: Some(device_id),
            token: Some(token),
            client_version: "address regression".into(),
            timings: Timings::default(),
        };
        let link = tokio::time::timeout(
            Duration::from_secs(2),
            quic_link_addresses(
                &spec,
                &q,
                [blackhole.local_addr().unwrap(), daemon.quic_addr.unwrap()],
            ),
        )
        .await
        .expect("IPv6 blackhole prevented the reachable IPv4 connection")
        .unwrap();
        assert_eq!(link.kind, TransportKind::Quic);
        assert_eq!(link.daemon_version, fl_proto::PRODUCT_VERSION);
        drop(link);
        daemon.shutdown().await;
    }
}
