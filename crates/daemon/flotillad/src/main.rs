//! `flotillad`: the Flotilla workspace daemon and its helper commands.
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fl_proto::{
    ClientMsg, DaemonInfo, EnrollResponse, Hello, PRODUCT_VERSION, PROTO, Request, Response,
    ServerMsg, VersionInfo, build_id,
};
use fl_transport::conn::framed;
use fld_server::devices::DeviceRegistry;
use fld_server::{DaemonConfig, FlotillaHome};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "flotillad", version, about = "Flotilla workspace daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground (systemd runs this).
    Serve {
        /// UDP address for QUIC.
        #[arg(long, env = "FLOTILLA_QUIC_BIND", default_value = "0.0.0.0:7443")]
        quic: SocketAddr,
        /// Disable QUIC; clients then connect over SSH only.
        #[arg(long)]
        no_quic: bool,
    },
    /// Print this binary's version.
    Version {
        #[arg(long)]
        json: bool,
    },
    /// Query the running daemon (exit status 1 when it is not reachable).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Register a client device and print its credentials as JSON.
    Enroll {
        #[arg(long)]
        device_name: String,
    },
    /// Bridge stdin/stdout to the daemon socket (SSH fallback transport).
    Attach,
    /// Install and (re)start the per-user service.
    InstallService {
        /// Only write the unit file.
        #[arg(long)]
        no_start: bool,
        /// Never use systemd; start a detached background process instead.
        #[arg(long)]
        no_systemd: bool,
    },
    /// Stop a daemon that was started without systemd.
    Stop,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = rt.block_on(run(cli));
    // `attach` reads stdin on a blocking thread; don't wait for it to finish.
    rt.shutdown_timeout(Duration::from_millis(200));
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("flotillad: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Serve { quic, no_quic } => serve(quic, no_quic).await,
        Command::Version { json } => {
            version(json);
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { json } => status(json).await,
        Command::Enroll { device_name } => enroll(&device_name),
        Command::Attach => attach().await,
        Command::InstallService {
            no_start,
            no_systemd,
        } => install_service(no_start, no_systemd).await,
        Command::Stop => {
            let home = FlotillaHome::from_env()?;
            let stopped = stop_daemon(&home, Duration::from_secs(10)).await?;
            println!("{}", json!({ "stopped": stopped }));
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn serve(quic: SocketAddr, no_quic: bool) -> Result<ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLOTILLA_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let home = FlotillaHome::from_env()?;
    let mut config = DaemonConfig::new(home);
    config.quic_bind = (!no_quic).then_some(quic);
    let daemon = fld_server::start(config).await?;
    tracing::info!(
        socket = %daemon.socket_path.display(),
        quic = ?daemon.quic_addr,
        "flotillad {} ({}) ready",
        PRODUCT_VERSION,
        build_id()
    );
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    daemon.shutdown().await;
    Ok(ExitCode::SUCCESS)
}

fn version(json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(&VersionInfo::current()).expect("VersionInfo serializes")
        );
    } else {
        println!(
            "flotillad {} ({}) protocol {}",
            PRODUCT_VERSION,
            build_id(),
            PROTO
        );
    }
}

async fn status(json: bool) -> Result<ExitCode> {
    let home = FlotillaHome::from_env()?;
    match query_daemon_info(&home.socket_path()).await {
        Ok(info) => {
            if json {
                println!("{}", serde_json::to_string(&info)?);
            } else {
                println!(
                    "flotillad {} ({}) pid {} up {}s, quic {}, socket {}",
                    info.version,
                    info.build,
                    info.pid,
                    info.uptime_secs,
                    info.quic_port
                        .map_or_else(|| "off".to_string(), |p| format!("udp/{p}")),
                    info.socket_path
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            if json {
                println!("{}", json!({ "running": false, "error": format!("{e:#}") }));
            } else {
                eprintln!("flotillad is not running: {e:#}");
            }
            Ok(ExitCode::from(1))
        }
    }
}

async fn query_daemon_info(socket: &Path) -> Result<DaemonInfo> {
    let io_timeout = Duration::from_secs(5);
    let stream = tokio::time::timeout(io_timeout, UnixStream::connect(socket))
        .await
        .context("timed out connecting to the daemon")?
        .with_context(|| format!("cannot connect to {}", socket.display()))?;
    let (r, w) = stream.into_split();
    let (mut reader, mut writer) = framed(r, w);
    writer
        .send(&ClientMsg::Hello(Hello {
            client_version: format!("flotillad-cli/{PRODUCT_VERSION}"),
            proto: PROTO,
            device_id: None,
            token: None,
            client_session_id: [0; 16],
        }))
        .await?;
    match tokio::time::timeout(io_timeout, reader.recv::<ServerMsg>())
        .await
        .context("timed out waiting for Welcome")??
    {
        ServerMsg::Welcome(_) => {}
        ServerMsg::Rejected(reason) => bail!("daemon rejected the connection: {reason:?}"),
        other => bail!("unexpected reply: {other:?}"),
    }
    writer
        .send(&ClientMsg::Rpc {
            id: 1,
            req: Request::DaemonInfo,
        })
        .await?;
    loop {
        match tokio::time::timeout(io_timeout, reader.recv::<ServerMsg>())
            .await
            .context("timed out waiting for DaemonInfo")??
        {
            ServerMsg::RpcResult {
                id: 1,
                result: Ok(Response::DaemonInfo(info)),
            } => return Ok(info),
            ServerMsg::RpcResult { result: Err(e), .. } => bail!("daemon error: {}", e.message),
            ServerMsg::Pong { .. } => continue,
            other => bail!("unexpected reply: {other:?}"),
        }
    }
}

fn enroll(device_name: &str) -> Result<ExitCode> {
    let home = FlotillaHome::from_env()?;
    home.ensure_dirs()?;
    let identity = fld_server::identity::load_or_create(&home)?;
    let (device_id, token) = DeviceRegistry::new(home.devices_path()).enroll(device_name)?;
    let quic_port = fld_server::read_runtime_info(&home).and_then(|i| i.quic_port);
    let response = EnrollResponse {
        device_id,
        token,
        quic_port,
        cert_sha256: fl_transport::quic::fingerprint_hex(&identity.fingerprint()),
        socket_path: home.socket_path().display().to_string(),
    };
    println!("{}", serde_json::to_string(&response)?);
    Ok(ExitCode::SUCCESS)
}

async fn attach() -> Result<ExitCode> {
    let home = FlotillaHome::from_env()?;
    let socket = home.socket_path();
    let stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("cannot connect to {}", socket.display()))?;
    let (mut from_daemon, mut to_daemon) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        r = tokio::io::copy(&mut stdin, &mut to_daemon) => {
            r.context("stdin to daemon")?;
        }
        r = tokio::io::copy(&mut from_daemon, &mut stdout) => {
            r.context("daemon to stdout")?;
            stdout.flush().await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn install_service(no_start: bool, no_systemd: bool) -> Result<ExitCode> {
    let home = FlotillaHome::from_env()?;
    home.ensure_dirs()?;
    let exe = home.bin_dir().join("flotillad");
    if !exe.exists() {
        bail!(
            "{} does not exist; install-service runs the installed binary",
            exe.display()
        );
    }
    let unit_path = systemd_unit_path()?;
    std::fs::create_dir_all(unit_path.parent().expect("unit path has a parent"))?;
    std::fs::write(&unit_path, unit_file(&exe, home.root()))?;
    if no_start {
        println!(
            "{}",
            json!({ "mode": "none", "unit_path": unit_path, "linger": false, "running": false })
        );
        return Ok(ExitCode::SUCCESS);
    }
    let (mode, linger) = if !no_systemd && systemd_user_available() {
        if !systemd_unit_active() {
            // A daemon started earlier without systemd would hold the socket.
            stop_daemon(&home, Duration::from_secs(10)).await?;
        }
        run_checked("systemctl", &["--user", "daemon-reload"])?;
        run_checked("systemctl", &["--user", "enable", "flotillad.service"])?;
        run_checked("systemctl", &["--user", "restart", "flotillad.service"])?;
        ("systemd", enable_linger())
    } else {
        stop_daemon(&home, Duration::from_secs(10)).await?;
        spawn_detached(&exe, &home)?;
        ("detached", false)
    };
    let running = wait_until_running(&home.socket_path(), Duration::from_secs(10)).await;
    println!(
        "{}",
        json!({ "mode": mode, "unit_path": unit_path, "linger": linger, "running": running })
    );
    Ok(if running {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn systemd_unit_path() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(config
        .join("systemd")
        .join("user")
        .join("flotillad.service"))
}

fn unit_file(exe: &Path, home: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Flotilla workspace daemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{exe}\" serve\n\
         Environment=\"FLOTILLA_HOME={home}\"\n\
         Restart=always\n\
         RestartSec=2\n\
         KillMode=process\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
        home = home.display()
    )
}

fn systemd_user_available() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn systemd_unit_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "flotillad.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_checked(program: &str, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("cannot run {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Keep user services running after logout. Best effort.
fn enable_linger() -> bool {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    if user.is_empty() {
        return false;
    }
    if Path::new("/var/lib/systemd/linger").join(&user).exists() {
        return true;
    }
    std::process::Command::new("loginctl")
        .args(["enable-linger", &user])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// The daemon must outlive this process, so the child is intentionally not waited for.
#[allow(clippy::zombie_processes)]
fn spawn_detached(exe: &Path, home: &FlotillaHome) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let log_path = home.logs_dir().join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve")
        .env("FLOTILLA_HOME", home.root())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid() is async-signal-safe; it detaches the daemon from the SSH session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
        .with_context(|| format!("cannot start {}", exe.display()))?;
    Ok(())
}

async fn wait_until_running(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if query_daemon_info(socket).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// SIGTERM the daemon recorded in `run/daemon.json` and wait for it to exit.
/// Returns false when no live `flotillad serve` owns that pid (stale file).
async fn stop_daemon(home: &FlotillaHome, wait: Duration) -> Result<bool> {
    let Some(info) = fld_server::read_runtime_info(home) else {
        return Ok(false);
    };
    if info.pid == std::process::id() || !is_flotillad_serve(info.pid) {
        let _ = std::fs::remove_file(home.runtime_info_path());
        return Ok(false);
    }
    // SAFETY: plain kill(2) on a pid we just verified is a flotillad serve process.
    unsafe {
        libc::kill(info.pid as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if !is_flotillad_serve(info.pid) {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!(
        "flotillad (pid {}) did not exit within {:?}",
        info.pid,
        wait
    )
}

/// True when `pid` is a live `flotillad serve` process (guards against pid reuse).
fn is_flotillad_serve(pid: u32) -> bool {
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(raw) => {
            let args: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
            args.first().is_some_and(|a| a.ends_with(b"flotillad"))
                && args.iter().any(|a| *a == b"serve")
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_file_runs_installed_binary_and_restarts() {
        let u = unit_file(
            Path::new("/home/u/.flotilla/bin/flotillad"),
            Path::new("/home/u/.flotilla"),
        );
        assert!(
            u.contains("ExecStart=\"/home/u/.flotilla/bin/flotillad\" serve\n"),
            "{u}"
        );
        assert!(u.contains("Environment=\"FLOTILLA_HOME=/home/u/.flotilla\"\n"));
        assert!(u.contains("Restart=always\n"));
        assert!(u.contains("KillMode=process\n"));
        assert!(u.contains("WantedBy=default.target\n"));
    }

    #[test]
    fn cli_accepts_every_command() {
        for args in [
            &["flotillad", "serve", "--quic", "127.0.0.1:0"][..],
            &["flotillad", "serve", "--no-quic"],
            &["flotillad", "version", "--json"],
            &["flotillad", "status", "--json"],
            &["flotillad", "enroll", "--device-name", "my pc"],
            &["flotillad", "attach"],
            &["flotillad", "install-service", "--no-systemd"],
            &["flotillad", "install-service", "--no-start"],
            &["flotillad", "stop"],
        ] {
            Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }

    #[test]
    fn this_test_process_is_not_a_serve_process() {
        assert!(!is_flotillad_serve(std::process::id()));
        assert!(!is_flotillad_serve(u32::MAX));
    }
}
