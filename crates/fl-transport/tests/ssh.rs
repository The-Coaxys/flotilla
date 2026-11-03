#![cfg(unix)]
//! SSH client tests against the in-process test server (fl-testkit).
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fl_testkit::ssh_server::{TestSshOptions, TestSshServer};
use fl_transport::ssh::{HostKeyPolicy, SshAuth, SshError, SshSession, SshTarget, load_key};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const T: Duration = Duration::from_secs(10);

fn keygen(dir: &Path, name: &str, kind: &str, passphrase: &str) -> PathBuf {
    let path = dir.join(name);
    let status = Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            kind,
            "-N",
            passphrase,
            "-C",
            "flotilla-test",
            "-f",
        ])
        .arg(&path)
        .status()
        .expect("ssh-keygen must be installed (openssh-client)");
    assert!(status.success());
    path
}

struct Fx {
    dir: tempfile::TempDir,
    server: TestSshServer,
    key: PathBuf,
}

impl Fx {
    async fn new() -> Self {
        Self::with(TestSshOptions::default()).await
    }

    async fn with(opts: TestSshOptions) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let server = TestSshServer::start_with(dir.path(), opts).await.unwrap();
        let key = keygen(dir.path(), "id_ed25519", "ed25519", "");
        Self { dir, server, key }
    }

    fn target(&self) -> SshTarget {
        SshTarget {
            host: "127.0.0.1".into(),
            port: self.server.addr.port(),
            user: "tester".into(),
        }
    }

    async fn connect(&self) -> SshSession {
        SshSession::connect(
            &self.target(),
            &SshAuth::KeyFile {
                path: self.key.clone(),
            },
            &HostKeyPolicy::TrustOnFirstUse,
            T,
        )
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn tofu_reports_the_server_fingerprint() {
    let fx = Fx::new().await;
    let s = fx.connect().await;
    assert!(s.host_key_fingerprint().starts_with("SHA256:"));
    assert_eq!(s.host_key_fingerprint(), fx.server.host_key_fingerprint);
}

#[tokio::test]
async fn expected_fingerprint_connects() {
    let fx = Fx::new().await;
    let policy = HostKeyPolicy::Expect(fx.server.host_key_fingerprint.clone());
    SshSession::connect(
        &fx.target(),
        &SshAuth::KeyFile {
            path: fx.key.clone(),
        },
        &policy,
        T,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn host_key_mismatch_is_reported_and_fatal() {
    let fx = Fx::new().await;
    let policy = HostKeyPolicy::Expect("SHA256:not-the-key".into());
    let err = SshSession::connect(
        &fx.target(),
        &SshAuth::KeyFile {
            path: fx.key.clone(),
        },
        &policy,
        T,
    )
    .await
    .err()
    .expect("must fail");
    match &err {
        SshError::HostKeyMismatch { expected, actual } => {
            assert_eq!(expected, "SHA256:not-the-key");
            assert_eq!(actual, &fx.server.host_key_fingerprint);
        }
        other => panic!("expected HostKeyMismatch, got {other:?}"),
    }
    assert!(err.is_fatal());
}

#[tokio::test]
async fn exec_captures_stdout_stderr_and_exit_code() {
    let fx = Fx::new().await;
    let out = fx
        .connect()
        .await
        .exec("echo out; echo err >&2; exit 3", None)
        .await
        .unwrap();
    assert_eq!(out.stdout_str(), "out\n");
    assert_eq!(out.stderr_str(), "err\n");
    assert_eq!(out.code, Some(3));
}

#[tokio::test]
async fn exec_runs_with_home_set() {
    let fx = Fx::new().await;
    let out = fx
        .connect()
        .await
        .exec_checked("printf %s \"$HOME\"", None)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        fx.dir.path().display().to_string()
    );
}

#[tokio::test]
async fn exec_pipes_a_megabyte_through_stdin_and_back() {
    let fx = Fx::new().await;
    let input: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    let out = fx.connect().await.exec("cat", Some(&input)).await.unwrap();
    assert_eq!(out.code, Some(0));
    assert!(
        out.stdout == input,
        "echoed {} bytes, expected {}",
        out.stdout.len(),
        input.len()
    );
}

#[tokio::test]
async fn exec_checked_turns_failure_into_command_error() {
    let fx = Fx::new().await;
    let err = fx
        .connect()
        .await
        .exec_checked("echo nope >&2; exit 7", None)
        .await
        .unwrap_err();
    match err {
        SshError::Command { code, stderr, .. } => {
            assert_eq!(code, Some(7));
            assert_eq!(stderr, "nope");
        }
        other => panic!("expected Command, got {other:?}"),
    }
}

#[tokio::test]
async fn unix_socket_channel_bridges_bytes() {
    let fx = Fx::new().await;
    let sock = fx.dir.path().join("echo.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let (mut r, mut w) = s.split();
        let _ = tokio::io::copy(&mut r, &mut w).await;
    });
    let session = fx.connect().await;
    let mut stream = session
        .open_unix_socket(sock.to_str().unwrap())
        .await
        .unwrap();
    stream.write_all(b"over-streamlocal").await.unwrap();
    let mut buf = [0u8; 16];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"over-streamlocal");
}

#[tokio::test]
async fn missing_unix_socket_is_socket_unavailable() {
    let fx = Fx::new().await;
    let err = fx
        .connect()
        .await
        .open_unix_socket("/nonexistent/flotilla.sock")
        .await
        .err()
        .expect("must fail");
    assert!(
        matches!(err, SshError::SocketUnavailable { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn exec_stream_is_bidirectional() {
    let fx = Fx::new().await;
    let session = fx.connect().await;
    let mut stream = session.open_exec_stream("cat").await.unwrap();
    stream.write_all(b"hello\n").await.unwrap();
    let mut buf = [0u8; 6];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello\n");
}

#[tokio::test]
async fn rsa_key_authenticates() {
    let fx = Fx::new().await;
    let rsa = keygen(fx.dir.path(), "id_rsa", "rsa", "");
    SshSession::connect(
        &fx.target(),
        &SshAuth::KeyFile { path: rsa },
        &HostKeyPolicy::TrustOnFirstUse,
        T,
    )
    .await
    .unwrap();
}

/// Review Focus #1: an encrypted key must produce an actionable error quickly.
#[tokio::test]
async fn encrypted_key_needs_passphrase() {
    let fx = Fx::new().await;
    let locked = keygen(fx.dir.path(), "id_locked", "ed25519", "correct horse");
    let started = Instant::now();
    let err = SshSession::connect(
        &fx.target(),
        &SshAuth::KeyFile {
            path: locked.clone(),
        },
        &HostKeyPolicy::TrustOnFirstUse,
        T,
    )
    .await
    .err()
    .expect("must fail");
    assert!(started.elapsed() < Duration::from_secs(5));
    match &err {
        SshError::NeedsPassphrase(p) => assert_eq!(p, &locked),
        other => panic!("expected NeedsPassphrase, got {other:?}"),
    }
    assert!(err.to_string().contains("ssh-agent"));
    assert!(err.is_fatal());
}

#[tokio::test]
async fn missing_key_file_is_a_key_error() {
    let fx = Fx::new().await;
    let err = SshSession::connect(
        &fx.target(),
        &SshAuth::KeyFile {
            path: fx.dir.path().join("nope"),
        },
        &HostKeyPolicy::TrustOnFirstUse,
        T,
    )
    .await
    .err()
    .expect("must fail");
    assert!(matches!(err, SshError::Key { .. }), "got {err:?}");
}

#[tokio::test]
async fn unauthorized_key_is_auth_failed() {
    let dir = tempfile::tempdir().unwrap();
    let other = keygen(dir.path(), "other", "ed25519", "");
    let allowed = load_key(&other).unwrap().public_key().clone();
    let fx = Fx::with(TestSshOptions {
        allowed_key: Some(allowed),
        ..Default::default()
    })
    .await;
    let err = SshSession::connect(
        &fx.target(),
        &SshAuth::KeyFile {
            path: fx.key.clone(),
        },
        &HostKeyPolicy::TrustOnFirstUse,
        T,
    )
    .await
    .err()
    .expect("must fail");
    assert!(matches!(err, SshError::AuthFailed { .. }), "got {err:?}");
    assert!(err.is_fatal());
}

struct Agent {
    child: Child,
    sock: PathBuf,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_agent(dir: &Path) -> Agent {
    let sock = dir.join("agent.sock");
    let child = Command::new("ssh-agent")
        .arg("-D")
        .arg("-a")
        .arg(&sock)
        .stdout(Stdio::null())
        .spawn()
        .expect("ssh-agent");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sock.exists() {
        assert!(
            Instant::now() < deadline,
            "ssh-agent did not create its socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Agent { child, sock }
}

#[tokio::test]
async fn agent_auth_with_explicit_socket() {
    let fx = Fx::new().await;
    let agent = start_agent(fx.dir.path());
    let added = Command::new("ssh-add")
        .arg(&fx.key)
        .env("SSH_AUTH_SOCK", &agent.sock)
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let auth = SshAuth::Agent {
        socket: Some(agent.sock.display().to_string()),
    };
    SshSession::connect(&fx.target(), &auth, &HostKeyPolicy::TrustOnFirstUse, T)
        .await
        .unwrap();
}

#[tokio::test]
async fn empty_agent_is_no_agent() {
    let fx = Fx::new().await;
    let agent = start_agent(fx.dir.path());
    let auth = SshAuth::Agent {
        socket: Some(agent.sock.display().to_string()),
    };
    let err = SshSession::connect(&fx.target(), &auth, &HostKeyPolicy::TrustOnFirstUse, T)
        .await
        .err()
        .expect("must fail");
    match err {
        SshError::NoAgent(msg) => assert!(msg.contains("no keys"), "{msg}"),
        other => panic!("expected NoAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn silent_server_times_out() {
    // Accepts TCP (kernel backlog) but never speaks SSH.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = SshTarget {
        host: "127.0.0.1".into(),
        port: listener.local_addr().unwrap().port(),
        user: "x".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let key = keygen(dir.path(), "k", "ed25519", "");
    let started = Instant::now();
    let err = SshSession::connect(
        &target,
        &SshAuth::KeyFile { path: key },
        &HostKeyPolicy::TrustOnFirstUse,
        Duration::from_millis(300),
    )
    .await
    .err()
    .expect("must fail");
    assert!(matches!(err, SshError::Timeout), "got {err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!err.is_fatal());
    drop(listener);
}

#[tokio::test]
async fn killed_session_reports_closed() {
    let fx = Fx::new().await;
    let s = fx.connect().await;
    // Open a channel so the server registers this session's handle.
    s.exec("true", None).await.unwrap();
    assert!(!s.is_closed());
    fx.server.kill_sessions().await;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !s.is_closed() {
        assert!(
            Instant::now() < deadline,
            "session should notice the disconnect"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
