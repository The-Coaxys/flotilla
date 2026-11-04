//! In-process process host and daemon, the test SSH server and a UDP relay
//! that can black-hole QUIC.
#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::time::Duration;

use fl_testkit::ssh_server::TestSshServer;
use fl_testkit::udp_relay::UdpRelay;
use fl_transport::ssh::{HostKeyPolicy, SshAuth, SshTarget};
use flc_conn::broker::ControlLink;
use flc_conn::supervisor::{ConnPhase, ConnectSpec, QuicSpec, Timings, TransportKind};
use fld_prochost::Client;
use fld_prochost::client::ClientError;
use fld_server::devices::DeviceRegistry;
use fld_server::{DaemonConfig, DaemonHandle, FlotillaHome, ServerError};
use tokio::sync::watch;

pub fn fast() -> Timings {
    Timings {
        heartbeat: Duration::from_millis(100),
        dead_after: Duration::from_millis(500),
        quic_reprobe: Duration::from_millis(300),
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(300),
        connect_timeout: Duration::from_secs(2),
        rpc_timeout: Duration::from_secs(5),
    }
}

pub fn home(dir: &Path) -> FlotillaHome {
    FlotillaHome::new(dir.join(".flotilla"))
}

/// Start `fld_prochost::serve` for `root` unless one already answers there.
pub async fn ensure_prochost(root: &Path) -> Client {
    if let Ok(client) = Client::connect(root).await {
        return client;
    }
    std::fs::create_dir_all(root.join("run")).unwrap();
    tokio::spawn(fld_prochost::serve(root.to_path_buf()));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match Client::connect(root).await {
                Ok(client) => break client,
                Err(ClientError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("process-host startup failed: {error}"),
            }
        }
    })
    .await
    .expect("process host starts")
}

/// Retries while a stopped predecessor still releases the socket.
pub async fn start_daemon(dir: &Path) -> DaemonHandle {
    let home = home(dir);
    home.ensure_dirs().unwrap();
    ensure_prochost(home.root()).await;
    let mut config = DaemonConfig::new(home);
    config.quic_bind = Some("127.0.0.1:0".parse().unwrap());
    config.hello_timeout = Duration::from_secs(2);
    config.prochost_executable = None;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match fld_server::start(config.clone()).await {
                Ok(daemon) => break daemon,
                Err(ServerError::AlreadyRunning(_)) => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => panic!("daemon start failed: {error}"),
            }
        }
    })
    .await
    .expect("daemon starts")
}

pub struct Harness {
    pub dir: tempfile::TempDir,
    pub daemon: Option<DaemonHandle>,
    pub ssh: TestSshServer,
    pub key: PathBuf,
    pub device: (String, String),
    pub relay: UdpRelay,
}

impl Harness {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let daemon = start_daemon(dir.path()).await;
        let device = DeviceRegistry::new(home(dir.path()).devices_path())
            .enroll("test")
            .unwrap();
        let ssh = TestSshServer::start(dir.path()).await.unwrap();
        let key = dir.path().join("id_ed25519");
        let ok = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let relay = UdpRelay::start(daemon.quic_addr.unwrap()).await.unwrap();
        Self {
            dir,
            daemon: Some(daemon),
            ssh,
            key,
            device,
            relay,
        }
    }

    pub fn spec(&self, with_quic: bool) -> ConnectSpec {
        let d = self.daemon.as_ref().expect("daemon running");
        ConnectSpec {
            ssh: SshTarget {
                host: "127.0.0.1".into(),
                port: self.ssh.addr.port(),
                user: "tester".into(),
            },
            auth: SshAuth::KeyFile {
                path: self.key.clone(),
            },
            host_key: HostKeyPolicy::Expect(self.ssh.host_key_fingerprint.clone()),
            socket_path: d.socket_path.display().to_string(),
            quic: with_quic.then(|| QuicSpec {
                host: "127.0.0.1".into(),
                port: self.relay.addr.port(),
                cert_sha256: d.cert_sha256,
            }),
            device_id: Some(self.device.0.clone()),
            token: Some(self.device.1.clone()),
            client_version: "supervisor-test".into(),
            timings: fast(),
            upgrade: None,
        }
    }

    pub async fn stop_daemon(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown().await;
        }
    }
}

pub async fn wait_phase(
    rx: &mut watch::Receiver<ConnPhase>,
    within: Duration,
    what: &str,
    pred: impl Fn(&ConnPhase) -> bool,
) -> ConnPhase {
    let fut = async {
        loop {
            let current = rx.borrow_and_update().clone();
            if pred(&current) {
                return current;
            }
            if rx.changed().await.is_err() {
                panic!("supervisor stopped while waiting for {what}");
            }
        }
    };
    match tokio::time::timeout(within, fut).await {
        Ok(p) => p,
        Err(_) => panic!(
            "timed out after {within:?} waiting for {what}; last phase: {:?}",
            *rx.borrow()
        ),
    }
}

pub fn connected(kind: TransportKind) -> impl Fn(&ConnPhase) -> bool {
    move |p| matches!(p, ConnPhase::Connected { kind: k, .. } if *k == kind)
}

/// The next published link satisfying `pred`.
pub async fn wait_link(
    rx: &mut watch::Receiver<Option<ControlLink>>,
    within: Duration,
    what: &str,
    pred: impl Fn(&ControlLink) -> bool,
) -> ControlLink {
    let fut = async {
        loop {
            if let Some(link) = rx.borrow_and_update().clone()
                && !link.is_closed()
                && pred(&link)
            {
                return link;
            }
            if rx.changed().await.is_err() {
                panic!("supervisor stopped while waiting for {what}");
            }
        }
    };
    tokio::time::timeout(within, fut)
        .await
        .unwrap_or_else(|_| panic!("timed out after {within:?} waiting for {what}"))
}
