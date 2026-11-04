//! Install or upgrade `flotillad` on the remote host over SSH, make sure it is
//! running, and enroll this device.
//!
//! Every remote command runs under `sh -c` so the user's login shell (bash,
//! zsh, fish, ...) does not matter.
use std::borrow::Cow;
use std::time::{Duration, Instant};

use fl_proto::{DaemonInfo, EnrollResponse, VersionInfo};
use fl_transport::ssh::{SshError, SshSession};
use sha2::{Digest, Sha256};

/// Remote daemon path; the remote `sh` expands the home override or default.
pub const REMOTE_BIN: &str = "${FLOTILLA_HOME:-$HOME/.flotilla}/bin/flotillad";

/// One line each: OS, arch, sha256sum availability, installed version JSON (or `none`).
pub const PROBE_SCRIPT: &str = r#"uname -s; uname -m; if command -v sha256sum >/dev/null 2>&1; then echo sha256sum=yes; else echo sha256sum=no; fi; "${FLOTILLA_HOME:-$HOME/.flotilla}/bin/flotillad" version --json 2>/dev/null || echo none"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteArch {
    X86_64,
    Aarch64,
}

impl RemoteArch {
    pub fn parse(uname_m: &str) -> Option<Self> {
        match uname_m.trim() {
            "x86_64" | "amd64" => Some(Self::X86_64),
            "aarch64" | "arm64" => Some(Self::Aarch64),
            _ => None,
        }
    }
}

/// Daemon binaries embedded in the client, plus the version/build they report.
#[derive(Clone)]
pub struct DaemonArtifacts {
    pub x86_64: Cow<'static, [u8]>,
    pub aarch64: Cow<'static, [u8]>,
    pub version: String,
    pub build: String,
}

impl DaemonArtifacts {
    pub fn for_arch(&self, arch: RemoteArch) -> &[u8] {
        match arch {
            RemoteArch::X86_64 => &self.x86_64,
            RemoteArch::Aarch64 => &self.aarch64,
        }
    }
}

impl std::fmt::Debug for DaemonArtifacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonArtifacts")
            .field("x86_64_len", &self.x86_64.len())
            .field("aarch64_len", &self.aarch64.len())
            .field("version", &self.version)
            .field("build", &self.build)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    /// Shown in `flotillad` device listings; sanitized before use.
    pub device_name: String,
    /// Run `flotillad enroll` and return credentials.
    pub enroll: bool,
    /// Extra arguments for `flotillad install-service` (tests pass `--no-systemd`).
    pub service_args: Vec<String>,
    /// How long to wait for the daemon to answer after starting it.
    pub start_timeout: Duration,
}

impl BootstrapOptions {
    pub fn new(device_name: impl Into<String>) -> Self {
        Self {
            device_name: device_name.into(),
            enroll: true,
            service_args: Vec::new(),
            start_timeout: Duration::from_secs(20),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapStep {
    Probing,
    Uploading { bytes: usize },
    Starting,
    Enrolling,
    Done,
}

#[derive(Debug, Clone)]
pub struct BootstrapOutcome {
    pub arch: RemoteArch,
    /// True when a new binary was uploaded.
    pub installed: bool,
    pub daemon: DaemonInfo,
    pub enrollment: Option<EnrollResponse>,
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error(transparent)]
    Ssh(#[from] SshError),
    #[error("the remote OS is `{0}`; Flotilla's daemon runs on Linux only")]
    UnsupportedOs(String),
    #[error("unsupported remote architecture `{0}` (Flotilla supports x86_64 and aarch64 Linux)")]
    UnsupportedArch(String),
    #[error(
        "this Flotilla build has no embedded daemon for {0:?}; install a release build of Flotilla"
    )]
    NoEmbeddedDaemon(RemoteArch),
    #[error("the remote host is missing `{0}`; install coreutils and try again")]
    MissingTool(&'static str),
    #[error("upload corrupted: expected sha256 {expected}, remote computed {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("the daemon did not become ready within {timeout:?}: {last_error}")]
    DaemonNotReady {
        timeout: Duration,
        last_error: String,
    },
    #[error("unexpected output from remote `{what}`: {detail}")]
    BadOutput { what: &'static str, detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub os: String,
    pub arch: RemoteArch,
    pub has_sha256sum: bool,
    /// `None` when absent or when its version output can't be parsed (treated as outdated).
    pub installed: Option<VersionInfo>,
}

pub fn parse_probe(stdout: &str) -> Result<Probe, BootstrapError> {
    let mut lines = stdout.lines().map(str::trim);
    let os = lines.next().unwrap_or("").to_string();
    if os != "Linux" {
        return Err(BootstrapError::UnsupportedOs(os));
    }
    let arch_raw = lines.next().unwrap_or("");
    let arch = RemoteArch::parse(arch_raw)
        .ok_or_else(|| BootstrapError::UnsupportedArch(arch_raw.to_string()))?;
    let has_sha256sum = match lines.next() {
        Some("sha256sum=yes") => true,
        Some("sha256sum=no") => false,
        other => {
            return Err(BootstrapError::BadOutput {
                what: "probe",
                detail: format!("expected sha256sum=yes|no, got {other:?}"),
            });
        }
    };
    let installed = match lines.next() {
        None | Some("") | Some("none") => None,
        Some(json) => serde_json::from_str::<VersionInfo>(json).ok(),
    };
    Ok(Probe {
        os,
        arch,
        has_sha256sum,
        installed,
    })
}

/// The embedded daemon speaks this client's protocol. Two builds can share
/// a version and build label (local builds are all `dev`), so a daemon on
/// another protocol is replaced even when its labels match.
pub fn should_install(installed: Option<&VersionInfo>, artifacts: &DaemonArtifacts) -> bool {
    match installed {
        None => true,
        Some(v) => {
            v.version != artifacts.version
                || v.build != artifacts.build
                || v.proto != fl_proto::PROTO
        }
    }
}

/// What to upload: `Ok(None)` when the installed daemon is current.
pub fn plan_install<'a>(
    probe: &Probe,
    artifacts: &'a DaemonArtifacts,
) -> Result<Option<&'a [u8]>, BootstrapError> {
    if !should_install(probe.installed.as_ref(), artifacts) {
        return Ok(None);
    }
    let bytes = artifacts.for_arch(probe.arch);
    if bytes.is_empty() {
        return Err(BootstrapError::NoEmbeddedDaemon(probe.arch));
    }
    if !probe.has_sha256sum {
        return Err(BootstrapError::MissingTool("sha256sum"));
    }
    Ok(Some(bytes))
}

/// Quote `s` as one POSIX shell word.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Run `script` with `/bin/sh` whatever the user's login shell is.
fn sh(script: &str) -> String {
    format!("sh -c {}", sh_quote(script))
}

/// Keep `[A-Za-z0-9 ._-]`, at most 64 chars; fall back to `windows-pc`.
pub fn sanitize_device_name(name: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .take(64)
        .collect();
    let kept = kept.trim();
    if kept.is_empty() {
        "windows-pc".to_string()
    } else {
        kept.to_string()
    }
}

pub async fn bootstrap(
    ssh: &SshSession,
    artifacts: &DaemonArtifacts,
    opts: &BootstrapOptions,
    progress: &(dyn Fn(BootstrapStep) + Send + Sync),
) -> Result<BootstrapOutcome, BootstrapError> {
    progress(BootstrapStep::Probing);
    let out = ssh.exec(&sh(PROBE_SCRIPT), None).await?;
    let probe = parse_probe(&out.stdout_str())?;
    let upload_bytes = plan_install(&probe, artifacts)?;
    if let Some(bytes) = upload_bytes {
        progress(BootstrapStep::Uploading { bytes: bytes.len() });
        upload(ssh, bytes).await?;
    }

    progress(BootstrapStep::Starting);
    let current = matches!(
        query_status(ssh).await,
        Ok(info) if info.version == artifacts.version && info.build == artifacts.build
    );
    if upload_bytes.is_some() || !current {
        let mut cmd = format!("\"{REMOTE_BIN}\" install-service");
        for arg in &opts.service_args {
            cmd.push(' ');
            cmd.push_str(&sh_quote(arg));
        }
        let out = ssh.exec(&sh(&cmd), None).await?;
        if out.code != Some(0) {
            return Err(BootstrapError::DaemonNotReady {
                timeout: opts.start_timeout,
                last_error: format!(
                    "install-service exited with {:?}: {} {}",
                    out.code,
                    out.stdout_str().trim(),
                    out.stderr_str().trim()
                ),
            });
        }
    }
    let daemon = wait_ready(ssh, artifacts, opts.start_timeout).await?;

    let enrollment = if opts.enroll {
        progress(BootstrapStep::Enrolling);
        let cmd = format!(
            "\"{REMOTE_BIN}\" enroll --device-name {}",
            sh_quote(&sanitize_device_name(&opts.device_name))
        );
        let raw = ssh.exec_checked(&sh(&cmd), None).await?;
        Some(serde_json::from_slice::<EnrollResponse>(&raw).map_err(|e| {
            BootstrapError::BadOutput {
                what: "enroll",
                detail: e.to_string(),
            }
        })?)
    } else {
        None
    };
    progress(BootstrapStep::Done);
    Ok(BootstrapOutcome {
        arch: probe.arch,
        installed: upload_bytes.is_some(),
        daemon,
        enrollment,
    })
}

async fn upload(ssh: &SshSession, bytes: &[u8]) -> Result<(), BootstrapError> {
    let expected = hex::encode(Sha256::digest(bytes));
    let script = r#"set -e; h="${FLOTILLA_HOME:-$HOME/.flotilla}"; d="$h/bin"; mkdir -p "$d"; chmod 700 "$h"; cat > "$d/flotillad.new"; chmod 755 "$d/flotillad.new"; sha256sum < "$d/flotillad.new""#;
    let out = ssh.exec_checked(&sh(script), Some(bytes)).await?;
    let text = String::from_utf8_lossy(&out);
    let actual = text.split_whitespace().next().unwrap_or("").to_string();
    if actual != expected {
        let _ = ssh
            .exec(
                &sh(r#"rm -f "${FLOTILLA_HOME:-$HOME/.flotilla}/bin/flotillad.new""#),
                None,
            )
            .await;
        return Err(BootstrapError::HashMismatch { expected, actual });
    }
    ssh.exec_checked(
        &sh(r#"h="${FLOTILLA_HOME:-$HOME/.flotilla}"; mv -f "$h/bin/flotillad.new" "$h/bin/flotillad""#),
        None,
    )
    .await?;
    Ok(())
}

async fn query_status(ssh: &SshSession) -> Result<DaemonInfo, BootstrapError> {
    let out = ssh
        .exec(&sh(&format!("\"{REMOTE_BIN}\" status --json")), None)
        .await?;
    if out.code != Some(0) {
        return Err(BootstrapError::BadOutput {
            what: "status",
            detail: format!(
                "exit {:?}: {} {}",
                out.code,
                out.stdout_str().trim(),
                out.stderr_str().trim()
            ),
        });
    }
    serde_json::from_slice(&out.stdout).map_err(|e| BootstrapError::BadOutput {
        what: "status",
        detail: e.to_string(),
    })
}

async fn wait_ready(
    ssh: &SshSession,
    artifacts: &DaemonArtifacts,
    timeout: Duration,
) -> Result<DaemonInfo, BootstrapError> {
    let deadline = Instant::now() + timeout;
    loop {
        let last_error = match query_status(ssh).await {
            Ok(info) if info.version == artifacts.version && info.build == artifacts.build => {
                return Ok(info);
            }
            Ok(info) => format!(
                "an older daemon {} ({}) is still running",
                info.version, info.build
            ),
            Err(e) => e.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(BootstrapError::DaemonNotReady {
                timeout,
                last_error,
            });
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifacts(version: &str, build: &str) -> DaemonArtifacts {
        DaemonArtifacts {
            x86_64: Cow::Borrowed(b"x86-bytes"),
            aarch64: Cow::Borrowed(b"arm-bytes"),
            version: version.into(),
            build: build.into(),
        }
    }

    fn version(version: &str, build: &str) -> VersionInfo {
        VersionInfo {
            version: version.into(),
            build: build.into(),
            proto: fl_proto::PROTO,
        }
    }

    fn probe(installed: Option<VersionInfo>, has_sha256sum: bool) -> Probe {
        Probe {
            os: "Linux".into(),
            arch: RemoteArch::X86_64,
            has_sha256sum,
            installed,
        }
    }

    #[test]
    fn parse_probe_fresh_linux_x86() {
        let p = parse_probe("Linux\nx86_64\nsha256sum=yes\nnone\n").unwrap();
        assert_eq!(p.arch, RemoteArch::X86_64);
        assert!(p.has_sha256sum);
        assert!(p.installed.is_none());
    }

    #[test]
    fn parse_probe_reads_installed_version() {
        let json = serde_json::to_string(&version("0.1.0", "abc")).unwrap();
        let p = parse_probe(&format!("Linux\naarch64\nsha256sum=yes\n{json}\n")).unwrap();
        assert_eq!(p.arch, RemoteArch::Aarch64);
        assert_eq!(p.installed, Some(version("0.1.0", "abc")));
    }

    #[test]
    fn parse_probe_treats_unparseable_version_as_not_installed() {
        let p = parse_probe("Linux\nx86_64\nsha256sum=yes\nflotillad 0.0.1\n").unwrap();
        assert!(p.installed.is_none());
    }

    /// Review Focus #5.
    #[test]
    fn parse_probe_rejects_unsupported_arch() {
        match parse_probe("Linux\nriscv64\nsha256sum=yes\nnone\n") {
            Err(BootstrapError::UnsupportedArch(a)) => assert_eq!(a, "riscv64"),
            other => panic!("expected UnsupportedArch, got {other:?}"),
        }
    }

    /// Review Focus #5.
    #[test]
    fn parse_probe_rejects_non_linux() {
        match parse_probe("Darwin\narm64\nsha256sum=no\nnone\n") {
            Err(e @ BootstrapError::UnsupportedOs(_)) => assert!(e.to_string().contains("Linux")),
            other => panic!("expected UnsupportedOs, got {other:?}"),
        }
    }

    #[test]
    fn parse_probe_rejects_truncated_output() {
        assert!(matches!(
            parse_probe(""),
            Err(BootstrapError::UnsupportedOs(_))
        ));
        assert!(matches!(
            parse_probe("Linux\nx86_64\n"),
            Err(BootstrapError::BadOutput { .. })
        ));
    }

    #[test]
    fn should_install_compares_version_and_build() {
        let a = artifacts("0.1.0", "abc");
        assert!(should_install(None, &a));
        assert!(!should_install(Some(&version("0.1.0", "abc")), &a));
        assert!(should_install(Some(&version("0.1.0", "old")), &a));
        assert!(should_install(Some(&version("0.0.9", "abc")), &a));
        // Same labels, older protocol: still replaced.
        let older = VersionInfo {
            proto: fl_proto::ProtoVersion {
                major: fl_proto::PROTO.major,
                minor: fl_proto::PROTO.minor - 1,
            },
            ..version("0.1.0", "abc")
        };
        assert!(should_install(Some(&older), &a));
    }

    /// Review Focus #5.
    #[test]
    fn plan_install_requires_sha256sum_only_when_installing() {
        let a = artifacts("0.1.0", "abc");
        assert!(matches!(
            plan_install(&probe(None, false), &a),
            Err(BootstrapError::MissingTool("sha256sum"))
        ));
        assert_eq!(
            plan_install(&probe(Some(version("0.1.0", "abc")), false), &a).unwrap(),
            None
        );
        assert_eq!(
            plan_install(&probe(None, true), &a).unwrap(),
            Some(&b"x86-bytes"[..])
        );
    }

    #[test]
    fn plan_install_without_embedded_daemon_is_a_clear_error() {
        let mut a = artifacts("0.1.0", "abc");
        a.x86_64 = Cow::Borrowed(b"");
        assert!(matches!(
            plan_install(&probe(None, true), &a),
            Err(BootstrapError::NoEmbeddedDaemon(RemoteArch::X86_64))
        ));
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh("echo hi"), "sh -c 'echo hi'");
    }

    #[cfg(unix)]
    #[test]
    fn sh_quote_roundtrips_through_a_real_shell() {
        for s in [
            "plain",
            "it's",
            "a b",
            "$HOME",
            "`id`",
            "\"q\"",
            "semi;colon",
            "new\nline",
        ] {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", sh_quote(s)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), s);
        }
    }

    #[test]
    fn sanitize_device_name_keeps_safe_characters() {
        assert_eq!(sanitize_device_name("DESKTOP-AB12"), "DESKTOP-AB12");
        assert_eq!(sanitize_device_name("my pc; rm -rf /"), "my pc rm -rf");
        assert_eq!(sanitize_device_name("$(boom)"), "boom");
        assert_eq!(sanitize_device_name("   "), "windows-pc");
        assert_eq!(sanitize_device_name(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn artifacts_debug_hides_the_bytes() {
        let dbg = format!("{:?}", artifacts("0.1.0", "abc"));
        assert!(dbg.contains("x86_64_len: 9"), "{dbg}");
        assert!(!dbg.contains("x86-bytes"));
    }

    #[test]
    fn remote_arch_parsing() {
        assert_eq!(RemoteArch::parse("x86_64\n"), Some(RemoteArch::X86_64));
        assert_eq!(RemoteArch::parse("arm64"), Some(RemoteArch::Aarch64));
        assert_eq!(RemoteArch::parse("i686"), None);
    }
}
