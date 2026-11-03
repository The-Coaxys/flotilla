//! End-to-end tests of the flotillad binary.
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use fl_proto::{ClientMsg, DaemonInfo, EnrollResponse, Hello, PROTO, ServerMsg, VersionInfo};
use fl_transport::conn::framed;
use fl_transport::quic;

const BIN: &str = env!("CARGO_BIN_EXE_flotillad");

fn cmd_with(bin: &Path, home: &Path) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(bin);
    c.env("HOME", home)
        .env("FLOTILLA_HOME", home.join(".flotilla"))
        .env_remove("FLOTILLA_QUIC_BIND")
        .env_remove("XDG_CONFIG_HOME")
        .kill_on_drop(true);
    c
}

fn cmd(home: &Path) -> tokio::process::Command {
    cmd_with(Path::new(BIN), home)
}

async fn wait_for(cond: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn hello(device_id: Option<String>, token: Option<String>) -> ClientMsg {
    ClientMsg::Hello(Hello {
        client_version: "cli-test".into(),
        proto: PROTO,
        device_id,
        token,
        client_session_id: [3; 16],
    })
}

/// Runs `flotillad stop` for a home when dropped (cleans up detached daemons).
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

#[tokio::test]
async fn version_json_matches_versioninfo() {
    let dir = tempfile::tempdir().unwrap();
    let out = cmd(dir.path())
        .args(["version", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let v: VersionInfo = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, VersionInfo::current());
}

#[tokio::test]
async fn status_reports_not_running() {
    let dir = tempfile::tempdir().unwrap();
    let out = cmd(dir.path())
        .args(["status", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["running"], false);
}

#[tokio::test]
async fn serve_status_enroll_quic_then_stop() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let mut daemon = cmd(home)
        .args(["serve", "--quic", "127.0.0.1:0"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let sock = home.join(".flotilla/run/daemon.sock");
    wait_for(|| sock.exists(), "daemon socket").await;

    let out = cmd(home).args(["status", "--json"]).output().await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let info: DaemonInfo = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(Some(info.pid), daemon.id());
    let port = info.quic_port.expect("QUIC enabled");

    let out = cmd(home)
        .args(["enroll", "--device-name", "test pc"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let creds: EnrollResponse = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(creds.quic_port, Some(port));
    assert_eq!(creds.token.len(), 64);
    assert_eq!(creds.socket_path, sock.display().to_string());

    let fp = quic::parse_fingerprint(&creds.cert_sha256).unwrap();
    let (_endpoint, conn) =
        quic::connect(([127, 0, 0, 1], port).into(), fp, Duration::from_secs(5))
            .await
            .unwrap();
    let (send, recv) = quic::open_ctl(&conn).await.unwrap();
    let (mut reader, mut writer) = framed(recv, send);
    writer
        .send(&hello(
            Some(creds.device_id.clone()),
            Some(creds.token.clone()),
        ))
        .await
        .unwrap();
    assert!(matches!(
        reader.recv::<ServerMsg>().await.unwrap(),
        ServerMsg::Welcome(_)
    ));

    let out = cmd(home).arg("stop").output().await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status = tokio::time::timeout(Duration::from_secs(10), daemon.wait())
        .await
        .expect("daemon exits")
        .unwrap();
    assert!(status.success());
    assert!(!sock.exists());
}

#[tokio::test]
async fn attach_bridges_stdio_to_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let mut daemon = cmd(home)
        .args(["serve", "--no-quic"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let sock = home.join(".flotilla/run/daemon.sock");
    wait_for(|| sock.exists(), "daemon socket").await;

    let mut attach = cmd(home)
        .arg("attach")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut reader, mut writer) =
        framed(attach.stdout.take().unwrap(), attach.stdin.take().unwrap());
    writer.send(&hello(None, None)).await.unwrap();
    assert!(matches!(
        reader.recv::<ServerMsg>().await.unwrap(),
        ServerMsg::Welcome(_)
    ));
    writer.send(&ClientMsg::Ping { nonce: 9 }).await.unwrap();
    assert_eq!(
        reader.recv::<ServerMsg>().await.unwrap(),
        ServerMsg::Pong { nonce: 9 }
    );
    drop(writer); // closes attach's stdin
    let status = tokio::time::timeout(Duration::from_secs(10), attach.wait())
        .await
        .expect("attach exits on EOF")
        .unwrap();
    assert!(status.success());

    daemon.start_kill().unwrap();
    let _ = daemon.wait().await;
}

#[tokio::test]
async fn install_service_detached_starts_daemon_and_stop_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let bin_dir = home.join(".flotilla/bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let installed = bin_dir.join("flotillad");
    std::fs::copy(BIN, &installed).unwrap();
    let _guard = StopOnDrop(home.to_path_buf());

    // Exec right after copy can hit ETXTBSY if a concurrently forked test child
    // briefly inherited the write fd; retry a few times.
    let mut attempt = 0;
    let out = loop {
        let mut c = cmd_with(&installed, home);
        c.args(["install-service", "--no-systemd"])
            .env("FLOTILLA_QUIC_BIND", "127.0.0.1:0");
        match c.output().await {
            Ok(out) => break out,
            Err(e) if e.raw_os_error() == Some(26) && attempt < 20 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("cannot run installed flotillad: {e}"),
        }
    };
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["mode"], "detached");
    assert_eq!(report["running"], true);
    assert!(home.join(".config/systemd/user/flotillad.service").exists());

    let out = cmd(home).args(["status", "--json"]).output().await.unwrap();
    assert!(out.status.success());

    let out = cmd(home).arg("stop").output().await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["stopped"], true);
}
