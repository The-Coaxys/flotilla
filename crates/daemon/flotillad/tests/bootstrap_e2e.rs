#![cfg(unix)]
//! Client bootstrap against the real flotillad binary over the in-process SSH server.
use std::borrow::Cow;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use fl_proto::{ClientMsg, Hello, PRODUCT_VERSION, PROTO, ServerMsg, build_id};
use fl_testkit::ssh_server::{TestSshOptions, TestSshServer};
use fl_transport::conn::framed;
use fl_transport::quic;
use fl_transport::ssh::{HostKeyPolicy, SshAuth, SshSession, SshTarget};
use flc_conn::bootstrap::{
    BootstrapError, BootstrapOptions, BootstrapStep, DaemonArtifacts, bootstrap,
};
use sha2::{Digest, Sha256};

const BIN: &str = env!("CARGO_BIN_EXE_flotillad");

/// Stops whatever daemon the test started in `home`, even if the test panics.
struct StopOnDrop(PathBuf);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = std::process::Command::new(BIN)
            .arg("stop")
            .env("HOME", &self.0)
            .env("FLOTILLA_HOME", self.0.join(".flotilla"))
            .output();
    }
}

struct Fx {
    // Fields drop in declaration order: the daemon must be stopped before the
    // tempdir (and its runtime info file) is removed.
    _stop: StopOnDrop,
    _dir: tempfile::TempDir,
    home: PathBuf,
    _server: TestSshServer,
    ssh: SshSession,
}

async fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let stop = StopOnDrop(home.clone());
    let server = TestSshServer::start_with(
        &home,
        TestSshOptions {
            env: vec![
                ("FLOTILLA_QUIC_BIND".into(), "127.0.0.1:0".into()),
                (
                    "FLOTILLA_HOME".into(),
                    home.join(".flotilla").display().to_string(),
                ),
            ],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let key = home.join("id_ed25519");
    let ok = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let target = SshTarget {
        host: "127.0.0.1".into(),
        port: server.addr.port(),
        user: "tester".into(),
    };
    let ssh = SshSession::connect(
        &target,
        &SshAuth::KeyFile { path: key },
        &HostKeyPolicy::TrustOnFirstUse,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    Fx {
        _stop: stop,
        _dir: dir,
        home,
        _server: server,
        ssh,
    }
}

fn real_artifacts() -> DaemonArtifacts {
    let bytes = std::fs::read(BIN).unwrap();
    DaemonArtifacts {
        x86_64: Cow::Owned(bytes.clone()),
        aarch64: Cow::Owned(bytes),
        version: PRODUCT_VERSION.into(),
        build: build_id().into(),
    }
}

fn test_options() -> BootstrapOptions {
    let mut opts = BootstrapOptions::new("e2e pc");
    opts.service_args = vec!["--no-systemd".into()];
    opts
}

fn hello(device_id: Option<String>, token: Option<String>) -> ClientMsg {
    ClientMsg::Hello(Hello {
        client_version: "e2e".into(),
        proto: PROTO,
        device_id,
        token,
        client_session_id: [5; 16],
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_installs_starts_enrolls_and_both_transports_work() {
    let fx = fixture().await;
    let artifacts = real_artifacts();
    let steps = Mutex::new(Vec::new());
    let outcome = bootstrap(&fx.ssh, &artifacts, &test_options(), &|s| {
        steps.lock().unwrap().push(s)
    })
    .await
    .unwrap();

    assert!(outcome.installed);
    let steps = steps.into_inner().unwrap();
    assert_eq!(steps.first(), Some(&BootstrapStep::Probing));
    assert!(steps.contains(&BootstrapStep::Uploading {
        bytes: artifacts.x86_64.len()
    }));
    assert!(steps.contains(&BootstrapStep::Starting));
    assert!(steps.contains(&BootstrapStep::Enrolling));
    assert_eq!(steps.last(), Some(&BootstrapStep::Done));
    let installed = std::fs::read(fx.home.join(".flotilla/bin/flotillad")).unwrap();
    assert_eq!(
        Sha256::digest(&installed),
        Sha256::digest(&artifacts.x86_64[..])
    );
    assert_eq!(outcome.daemon.build, build_id());
    let enrollment = outcome.enrollment.clone().expect("enrolled");

    // Already current: no upload, same daemon process, no new enrollment.
    let mut again_opts = test_options();
    again_opts.enroll = false;
    let again = bootstrap(&fx.ssh, &artifacts, &again_opts, &|_| {})
        .await
        .unwrap();
    assert!(!again.installed);
    assert_eq!(again.daemon.pid, outcome.daemon.pid);
    assert!(again.enrollment.is_none());

    // SSH transport: direct-streamlocal to the daemon socket.
    let stream = fx
        .ssh
        .open_unix_socket(&enrollment.socket_path)
        .await
        .unwrap();
    let (r, w) = tokio::io::split(stream);
    let (mut reader, mut writer) = framed(r, w);
    writer.send(&hello(None, None)).await.unwrap();
    assert!(matches!(
        reader.recv::<ServerMsg>().await.unwrap(),
        ServerMsg::Welcome(_)
    ));

    // QUIC transport with the enrolled credentials.
    let port = enrollment.quic_port.expect("daemon has QUIC");
    let fp = quic::parse_fingerprint(&enrollment.cert_sha256).unwrap();
    let (_endpoint, conn) =
        quic::connect(([127, 0, 0, 1], port).into(), fp, Duration::from_secs(5))
            .await
            .unwrap();
    let (send, recv) = quic::open_ctl(&conn).await.unwrap();
    let (mut qr, mut qw) = framed(recv, send);
    qw.send(&hello(
        Some(enrollment.device_id.clone()),
        Some(enrollment.token.clone()),
    ))
    .await
    .unwrap();
    assert!(matches!(
        qr.recv::<ServerMsg>().await.unwrap(),
        ServerMsg::Welcome(_)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outdated_binary_is_replaced() {
    let fx = fixture().await;
    let bin_dir = fx.home.join(".flotilla/bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let old = bin_dir.join("flotillad");
    std::fs::write(
        &old,
        "#!/bin/sh\ncase \"$1\" in version) echo '{\"version\":\"0.0.1\",\"build\":\"old\",\"proto\":{\"major\":0,\"minor\":1}}';; *) exit 1;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();

    let outcome = bootstrap(&fx.ssh, &real_artifacts(), &test_options(), &|_| {})
        .await
        .unwrap();
    assert!(outcome.installed);
    assert_eq!(outcome.daemon.build, build_id());
    assert_eq!(outcome.daemon.version, PRODUCT_VERSION);
}

#[tokio::test]
async fn missing_embedded_daemon_fails_before_touching_the_host() {
    let fx = fixture().await;
    let empty = DaemonArtifacts {
        x86_64: Cow::Borrowed(b""),
        aarch64: Cow::Borrowed(b""),
        version: PRODUCT_VERSION.into(),
        build: build_id().into(),
    };
    let err = bootstrap(&fx.ssh, &empty, &test_options(), &|_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(err, BootstrapError::NoEmbeddedDaemon(_)),
        "got {err:?}"
    );
    assert!(!Path::new(&fx.home.join(".flotilla/bin/flotillad")).exists());
}
