//! Flotilla daemon core: state home, device registry, control sessions,
//! listeners (Unix socket for SSH-forwarded peers, QUIC for direct peers),
//! the persistent terminal service and workspaces (projects, worktrees, git
//! status, files and scripts).
pub mod channels;
mod chat;
pub mod devices;
mod events;
pub mod identity;
mod instructions;
pub mod listen;
pub mod paths;
pub mod session;
mod terminal;
mod workspace;

use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use fld_store::LaunchDefaults;
pub use paths::FlotillaHome;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("another flotillad is already serving {}", .0.display())]
    AlreadyRunning(PathBuf),
    #[error("corrupt state file {}: {reason}", .path.display())]
    Corrupt { path: PathBuf, reason: String },
    #[error("neither FLOTILLA_HOME nor HOME is set")]
    NoHome,
    #[error(transparent)]
    Quic(#[from] fl_transport::quic::QuicError),
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub home: FlotillaHome,
    /// `None` disables QUIC.
    pub quic_bind: Option<SocketAddr>,
    /// Complete authentication budget, including QUIC control-stream acceptance.
    pub hello_timeout: Duration,
    /// Installed multi-call executable started as `<exe> prochost` when no
    /// process host answers; the daemon then also stops it when idle at
    /// shutdown. `None` (the default) only connects to an externally managed host.
    pub prochost_executable: Option<PathBuf>,
    /// Launch policy for new terminals (empty argv: the user's login shell).
    pub launch: LaunchDefaults,
    /// Provider overrides for tests and embedded hosts. Production leaves this
    /// empty and uses the built-in registry.
    pub providers: Option<fld_agent::ProviderRegistry>,
}

/// Production launch policy: login shell in `$HOME`, 256 MiB journal quota.
pub fn default_launch() -> LaunchDefaults {
    LaunchDefaults {
        cwd: String::new(),
        argv: Vec::new(),
        env: [
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ]
        .into(),
        quota_bytes: fl_prochost_proto::DEFAULT_QUOTA_BYTES,
    }
}

impl DaemonConfig {
    pub fn new(home: FlotillaHome) -> Self {
        Self {
            home,
            quic_bind: Some(SocketAddr::from(([0, 0, 0, 0], 7443))),
            hello_timeout: Duration::from_secs(3),
            prochost_executable: None,
            launch: default_launch(),
            providers: None,
        }
    }
}

/// Contents of `run/daemon.json` while the daemon runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub pid: u32,
    pub quic_port: Option<u16>,
    pub version: String,
    pub build: String,
    pub socket_path: String,
}

pub struct DaemonHandle {
    pub socket_path: PathBuf,
    pub quic_addr: Option<SocketAddr>,
    pub cert_sha256: [u8; 32],
    home: FlotillaHome,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
    quic: Option<quinn::Endpoint>,
    terminals: Arc<terminal::TerminalService>,
}

/// Start listening. Fails with `AlreadyRunning` if another daemon owns the socket.
/// A QUIC bind failure (e.g. port in use) only disables QUIC, with a warning.
pub async fn start(config: DaemonConfig) -> Result<DaemonHandle, ServerError> {
    let home = config.home.clone();
    home.ensure_dirs()?;
    let identity = identity::load_or_create(&home)?;
    let socket_path = home.socket_path();
    let unix = listen::bind_unix(&socket_path)?;
    let quic = match config.quic_bind {
        Some(bind) => match fl_transport::quic::server_endpoint(&identity, bind) {
            Ok(ep) => Some(ep),
            Err(e) => {
                tracing::warn!("QUIC disabled: cannot listen on {bind}: {e}");
                None
            }
        },
        None => None,
    };
    let quic_addr = quic.as_ref().and_then(|ep| ep.local_addr().ok());
    let terminals = terminal::TerminalService::start(
        &home,
        config.prochost_executable.clone(),
        config.launch.clone(),
    )
    .await?;
    let cancel = CancellationToken::new();
    let events = Arc::new(events::EventBus::default());
    let chat = match config.providers.clone() {
        Some(registry) => {
            chat::ChatService::start_with_registry(
                terminals.clone(),
                events.clone(),
                cancel.child_token(),
                registry,
            )
            .await
        }
        None => {
            chat::ChatService::start(terminals.clone(), events.clone(), cancel.child_token()).await
        }
    }
    .map_err(|error| std::io::Error::other(format!("starting chats: {}", error.message)))?;
    let workspace = workspace::WorkspaceService::start(
        home.clone(),
        terminals.clone(),
        events.clone(),
        chat.clone(),
        cancel.child_token(),
    )
    .await
    .map_err(|error| std::io::Error::other(format!("starting workspaces: {}", error.message)))?;
    let ctx = Arc::new(session::SessionCtx {
        registry: Arc::new(devices::DeviceRegistry::new(home.devices_path())),
        hello_timeout: config.hello_timeout,
        info: session::InfoSource {
            started: Instant::now(),
            hostname: session::hostname(),
            quic_port: quic_addr.map(|a| a.port()),
            socket_path: socket_path.display().to_string(),
        },
        shutdown: cancel.clone(),
        channels: Arc::default(),
        terminals: terminals.clone(),
        events,
        workspace,
        chat,
    });
    let mut tasks = vec![tokio::spawn(listen::unix_accept_loop(unix, ctx.clone()))];
    if let Some(ep) = &quic {
        tasks.push(tokio::spawn(listen::quic_accept_loop(
            ep.clone(),
            ctx.clone(),
        )));
    }
    let info = RuntimeInfo {
        pid: std::process::id(),
        quic_port: quic_addr.map(|a| a.port()),
        version: fl_proto::PRODUCT_VERSION.to_string(),
        build: fl_proto::build_id().to_string(),
        socket_path: socket_path.display().to_string(),
    };
    write_json_atomic(&home.runtime_info_path(), &info)?;
    Ok(DaemonHandle {
        socket_path,
        quic_addr,
        cert_sha256: identity.fingerprint(),
        home,
        cancel,
        tasks,
        quic,
        terminals,
    })
}

impl DaemonHandle {
    pub fn home(&self) -> &FlotillaHome {
        &self.home
    }

    /// Stop accepting, end every session, remove the socket and runtime info.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(ep) = self.quic.take() {
            ep.close(0u32.into(), b"daemon shutting down");
        }
        for t in std::mem::take(&mut self.tasks) {
            let _ = t.await;
        }
        self.terminals.shutdown().await;
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(self.home.runtime_info_path());
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.terminals.abort();
    }
}

pub fn read_runtime_info(home: &FlotillaHome) -> Option<RuntimeInfo> {
    let bytes = std::fs::read(home.runtime_info_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write JSON through a unique temp file + rename (mode 0600), so readers
/// never observe a partially written file.
pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{n}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
