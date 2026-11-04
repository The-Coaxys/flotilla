//! Signed client updates: check the release feed, verify the manifest, stage a
//! verified download, replace the installed binary with a rollback path, and
//! describe the result for a non-modal banner.
//!
//! Rules this module enforces (spec §9, M7):
//! - a manifest is only parsed after its Ed25519 signature verifies against a
//!   key compiled into this build;
//! - downloads are bounded and hashed, and only become installable when size
//!   and SHA-256 match the signed manifest;
//! - a replacement keeps the previous build until the new one passes
//!   `--self-test`, at which point the backup is dropped;
//! - nothing restarts on its own: the worker only stages, and the UI asks.

mod download;
mod install;
mod manifest;
mod prompt;
pub mod signature;

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use semver::Version;
use tokio::sync::watch;

pub use download::Source;
pub use prompt::{BannerAction, Tone, UpdateState, banner};

/// Where releases are published; `FLOTILLA_UPDATE_FEED` overrides it.
pub const DEFAULT_RELEASE_BASE: &str = "https://github.com/The-Coaxys/flotilla/releases";
/// Re-check interval after the startup check.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone)]
pub struct Config {
    /// Release base, e.g. `https://github.com/The-Coaxys/flotilla/releases`.
    pub release_base: String,
    /// Where verified assets wait for a restart.
    pub staging_dir: PathBuf,
    /// The executable a staged update replaces.
    pub installed_exe: PathBuf,
    /// The running version; newer manifests are required.
    pub current_version: String,
    /// Manifest platform key for this build, if releases publish one.
    pub platform: Option<&'static str>,
    pub enabled: bool,
}

impl Config {
    /// Read the release feed and paths from the environment and the running
    /// executable. `FLOTILLA_NO_UPDATE_CHECK=1` disables the worker;
    /// `FLOTILLA_UPDATE_FEED` points it at a mirror or a local test server.
    pub fn detect() -> Self {
        let release_base = std::env::var("FLOTILLA_UPDATE_FEED")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_RELEASE_BASE.to_string());
        let staging_dir = dirs::data_local_dir()
            .map(|dir| dir.join("Flotilla").join("updates"))
            .unwrap_or_else(|| std::env::temp_dir().join("Flotilla").join("updates"));
        let installed_exe =
            std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flotilla.exe"));
        let enabled = match std::env::var("FLOTILLA_NO_UPDATE_CHECK") {
            Ok(value) => !matches!(value.trim(), "1" | "true" | "yes"),
            Err(_) => true,
        };
        Self {
            release_base,
            staging_dir,
            installed_exe,
            current_version: fl_proto::PRODUCT_VERSION.to_string(),
            platform: manifest::client_platform(),
            enabled,
        }
    }

    fn base(&self) -> &str {
        self.release_base.trim_end_matches('/')
    }

    pub fn manifest_url(&self) -> String {
        format!("{}/latest/download/latest.json", self.base())
    }

    pub fn signature_url(&self) -> String {
        format!("{}/latest/download/latest.json.sig", self.base())
    }

    pub fn asset_url(&self, tag: &str, name: &str) -> String {
        format!("{}/download/{}/{}", self.base(), tag, name)
    }

    fn install_dir(&self) -> Result<PathBuf, UpdateError> {
        self.installed_exe
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                UpdateError::Install(install::InstallError::Staged(format!(
                    "{} has no parent directory",
                    self.installed_exe.display()
                )))
            })
    }
}

/// A verified, hash-checked asset waiting for a restart.
#[derive(Debug, Clone)]
pub struct Staged {
    pub version: Version,
    pub tag: String,
    pub asset: manifest::Asset,
    pub path: PathBuf,
}

/// What the check phase reports while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    Available {
        version: String,
    },
    Downloading {
        version: String,
        received: u64,
        total: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("signature check failed: {0}")]
    Signature(#[from] signature::VerifyError),
    #[error("{0}")]
    Refusal(#[from] manifest::Refusal),
    #[error("{0}")]
    Download(#[from] download::DownloadError),
    #[error("{0}")]
    Install(#[from] install::InstallError),
    #[error("no signed release is published yet")]
    NoRelease,
    #[error("the release publishes no signed manifest signature")]
    Unsigned,
    #[error("releases have no client build for this platform")]
    NoPlatform,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Check the feed and stage a verified asset. `Ok(None)` means there is
/// nothing to install (already current or no release yet).
pub fn check_and_stage(
    config: &Config,
    source: &dyn Source,
    keys: &[(&str, &str)],
    events: &mut dyn FnMut(UpdateEvent),
) -> Result<Option<Staged>, UpdateError> {
    let platform = config.platform.ok_or(UpdateError::NoPlatform)?;

    let manifest_bytes =
        download::fetch_small(source, &config.manifest_url(), manifest::MAX_MANIFEST_BYTES)
            .map_err(no_release)?;
    let signature_bytes = download::fetch_small(
        source,
        &config.signature_url(),
        manifest::MAX_SIGNATURE_BYTES,
    )
    .map_err(|e| match e {
        download::DownloadError::Status { status: 404, .. } => UpdateError::Unsigned,
        other => UpdateError::Download(other),
    })?;

    // The declared key id is untrusted input: it only selects which compiled-in
    // key must verify the *raw* bytes, which happens before any parsing.
    let declared = declared_key_id(&manifest_bytes);
    let signature_text = signature::signature_text(&signature_bytes)?;
    signature::verify(keys, &declared, &manifest_bytes, signature_text)?;

    let manifest = manifest::Manifest::parse(&manifest_bytes)?;
    let version = manifest.newer_than(&config.current_version)?;
    let asset = manifest.client_asset(platform)?.clone();
    events(UpdateEvent::Available {
        version: version.to_string(),
    });

    let staging = config.staging_dir.join(&manifest.tag);
    let url = config.asset_url(&manifest.tag, &asset.name);
    let announced = version.to_string();
    let mut progress = |received: u64, total: u64| {
        events(UpdateEvent::Downloading {
            version: announced.clone(),
            received,
            total,
        });
    };
    let path = download::fetch_asset(source, &url, &staging, &asset, &mut progress)?;
    download::verify_staged(&path, &asset)?;

    Ok(Some(Staged {
        version,
        tag: manifest.tag,
        asset,
        path,
    }))
}

/// Replace the installed binary with the staged one, then self-check it. A
/// failed self-check restores the previous build before returning.
pub fn apply(
    config: &Config,
    staged: &Staged,
    self_check: impl Fn(&Path) -> bool,
) -> Result<install::Replacement, UpdateError> {
    download::verify_staged(&staged.path, &staged.asset)?;
    let dir = config.install_dir()?;
    let installed_name = config
        .installed_exe
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            UpdateError::Install(install::InstallError::Staged(format!(
                "{} has no file name",
                config.installed_exe.display()
            )))
        })?;
    let pending = install::Pending::new(
        &config.current_version,
        &staged.version.to_string(),
        &staged.tag,
        installed_name,
    );
    install::write_pending(&dir, &pending)?;
    match install::replace(&config.installed_exe, &staged.path, self_check) {
        Ok(replacement) => Ok(replacement),
        Err(e) => {
            // The swap was rolled back; the marker must not survive it.
            let _ = install::clear_pending(&dir);
            Err(e.into())
        }
    }
}

fn declared_key_id(manifest_bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(manifest_bytes)
        .ok()
        .and_then(|value| {
            value
                .get("signing")
                .and_then(|signing| signing.get("key_id"))
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn no_release(error: download::DownloadError) -> UpdateError {
    match error {
        download::DownloadError::Status { status: 404, .. } => UpdateError::NoRelease,
        other => UpdateError::Download(other),
    }
}

/// Run the build's own `--self-test` and require it to report `ok` for
/// `expected_version`.
pub fn run_self_check(exe: &Path, expected_version: &str) -> bool {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let report = std::env::temp_dir().join(format!(
        "flotilla-selftest-{}-{stamp}.json",
        std::process::id()
    ));
    let status = std::process::Command::new(exe)
        .arg(format!("--self-test={}", report.display()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let passed = status.map(|s| s.success()).unwrap_or(false) && {
        let report = std::fs::read(&report)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        match report {
            Some(value) => {
                value.get("ok").and_then(|v| v.as_bool()) == Some(true)
                    && value.get("version").and_then(|v| v.as_str()) == Some(expected_version)
            }
            None => false,
        }
    };
    let _ = std::fs::remove_file(&report);
    passed
}

fn spawn_detached(exe: &Path) -> bool {
    std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// Startup gate for a replacement that was in flight.
///
/// Confirms a healthy new build (dropping the backup) or restores and starts
/// the previous build. Returns `true` when this process has been superseded by
/// the restored build and must exit without opening a window.
pub fn startup_recovery() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(dir) = exe.parent() else {
        return false;
    };
    let version = fl_proto::PRODUCT_VERSION;
    match install::recover(
        dir,
        version,
        |path| run_self_check(path, version),
        spawn_detached,
    ) {
        Ok(install::Recovery::Nothing) => false,
        Ok(install::Recovery::Confirmed { version }) => {
            tracing::info!("update to {version} confirmed; the previous build was removed");
            false
        }
        Ok(install::Recovery::Cleaned { version }) => {
            tracing::info!("cleaned up an interrupted update at {version}");
            false
        }
        Ok(install::Recovery::RolledBack { version }) => {
            tracing::warn!(
                "the new build failed its self-check; {version} was restored and started"
            );
            true
        }
        Ok(install::Recovery::Unrelated { running, pending }) => {
            tracing::warn!(
                "update marker for {pending} does not describe this build ({running}); left in place"
            );
            false
        }
        Err(e) => {
            tracing::error!("update recovery: {e:#}");
            false
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Check,
    Restart,
}

/// Handle to the updater worker: read its state, ask it to check or restart.
pub struct Updater {
    state: watch::Receiver<UpdateState>,
    commands: Sender<Command>,
}

impl Updater {
    /// Start the worker thread. It checks once at startup and then every
    /// [`CHECK_INTERVAL`], or when asked.
    pub fn spawn(config: Config) -> Self {
        let (state_tx, state_rx) = watch::channel(UpdateState::Idle);
        let (commands, inbox) = std::sync::mpsc::channel();
        let worker = Worker {
            config,
            states: state_tx,
            source: download::HttpSource::new(),
            staged: None,
        };
        let spawned = std::thread::Builder::new()
            .name("updater".to_string())
            .spawn(move || worker.run(inbox));
        if let Err(e) = spawned {
            tracing::warn!("cannot start the update worker: {e}");
        }
        Self {
            state: state_rx,
            commands,
        }
    }

    /// A receiver that changes whenever the state does (read with
    /// `borrow_and_update` + `changed`).
    pub fn watch(&self) -> watch::Receiver<UpdateState> {
        self.state.clone()
    }

    pub fn state(&self) -> UpdateState {
        self.state.borrow().clone()
    }

    /// Ask the feed again (after a failure).
    pub fn check_now(&self) {
        let _ = self.commands.send(Command::Check);
    }

    /// Apply the staged update and restart. Refused while a turn is running, so
    /// no update can interrupt one.
    pub fn restart_if_idle(&self, busy: bool) -> bool {
        if !prompt::restart_allowed(busy) {
            return false;
        }
        self.commands.send(Command::Restart).is_ok()
    }
}

struct Worker {
    config: Config,
    states: watch::Sender<UpdateState>,
    source: download::HttpSource,
    staged: Option<Staged>,
}

impl Worker {
    fn run(mut self, commands: Receiver<Command>) {
        if !self.config.enabled || self.config.platform.is_none() {
            self.set(UpdateState::Idle);
            return;
        }
        let mut next = Command::Check;
        loop {
            match next {
                Command::Check => self.check(),
                Command::Restart => self.restart(),
            }
            next = match commands.recv_timeout(CHECK_INTERVAL) {
                Ok(command) => command,
                Err(RecvTimeoutError::Timeout) => Command::Check,
                Err(RecvTimeoutError::Disconnected) => return,
            };
        }
    }

    fn set(&self, state: UpdateState) {
        let _ = self.states.send(state);
    }

    fn check(&mut self) {
        self.set(UpdateState::Checking);
        let states = self.states.clone();
        let mut events = |event: UpdateEvent| match event {
            UpdateEvent::Available { version } => {
                let _ = states.send(UpdateState::Available { version });
            }
            UpdateEvent::Downloading {
                version,
                received,
                total,
            } => {
                let _ = states.send(UpdateState::Downloading {
                    version,
                    received,
                    total,
                });
            }
        };
        let result = check_and_stage(
            &self.config,
            &self.source,
            signature::TRUSTED_KEYS,
            &mut events,
        );
        match result {
            Ok(Some(staged)) => {
                let version = staged.version.to_string();
                self.staged = Some(staged);
                self.set(UpdateState::Staged { version });
            }
            Ok(None) => self.set(UpdateState::Idle),
            Err(UpdateError::Refusal(manifest::Refusal::NotNewer { .. }))
            | Err(UpdateError::NoRelease)
            | Err(UpdateError::NoPlatform) => self.set(UpdateState::Idle),
            Err(e) => {
                tracing::warn!("update check failed: {e:#}");
                self.set(UpdateState::Failed {
                    version: None,
                    message: format!("{e:#}"),
                });
            }
        }
    }

    fn restart(&mut self) {
        let Some(staged) = self.staged.clone() else {
            self.set(UpdateState::Failed {
                version: None,
                message: "No update is staged.".to_string(),
            });
            return;
        };
        let version = staged.version.to_string();
        let expected = version.clone();
        let result = apply(&self.config, &staged, move |exe| {
            run_self_check(exe, &expected)
        });
        match result {
            Ok(replacement) => {
                if spawn_detached(&replacement.installed) {
                    // The new build confirms itself at startup and drops the
                    // backup; the UI quits on this state.
                    self.set(UpdateState::Restarting { version });
                } else {
                    let _ = install::rollback(&replacement);
                    if let Ok(dir) = self.config.install_dir() {
                        let _ = install::clear_pending(&dir);
                    }
                    self.set(UpdateState::Failed {
                        version: Some(version),
                        message:
                            "The new build could not be started; the previous build is in place."
                                .to_string(),
                    });
                }
            }
            Err(e) => {
                tracing::warn!("update restart failed: {e:#}");
                self.set(UpdateState::Failed {
                    version: Some(version),
                    message: format!("{e:#}"),
                });
            }
        }
    }
}
