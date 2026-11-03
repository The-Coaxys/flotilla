//! UI-independent logic: add-host form validation, profile building,
//! connection specs, and status/step copy.
use std::path::PathBuf;

use fl_proto::{EnrollResponse, PRODUCT_VERSION, build_id};
use fl_transport::quic::parse_fingerprint;
use fl_transport::ssh::{HostKeyPolicy, SshAuth, SshTarget};
use flc_conn::bootstrap::{BootstrapOptions, BootstrapStep};
use flc_conn::profile::{HostProfile, QuicEndpoint};
use flc_conn::supervisor::{ConnPhase, ConnectSpec, DaemonUpgrade, QuicSpec, Timings};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub name: String,
    pub target: SshTarget,
    pub auth: SshAuth,
}

/// Validate the add-host form. Empty port → 22, empty user → root, empty name → host.
/// `~/` or `~\` at the start of the key path expands to `home`.
pub fn parse_draft(
    name: &str,
    host: &str,
    port: &str,
    user: &str,
    key_path: &str,
    use_agent: bool,
    home: Option<PathBuf>,
) -> Result<Draft, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("Enter the server's address.".into());
    }
    if host.contains(char::is_whitespace) || host.contains('/') || host.contains('@') {
        return Err("Enter just the address, e.g. 203.0.113.10 or vps.example.com.".into());
    }
    let port = match port.trim() {
        "" => 22,
        p => match p.parse::<u16>() {
            Ok(n) if n > 0 => n,
            _ => return Err("Port must be a number from 1 to 65535.".into()),
        },
    };
    let user = match user.trim() {
        "" => "root".to_string(),
        u if u.contains(char::is_whitespace) => {
            return Err("The user name can't contain spaces.".into());
        }
        u => u.to_string(),
    };
    let auth = if use_agent {
        SshAuth::Agent { socket: None }
    } else {
        let raw = key_path.trim().trim_matches('"');
        if raw.is_empty() {
            return Err("Choose a private key file, or use the SSH agent.".into());
        }
        SshAuth::KeyFile {
            path: expand_home(raw, home),
        }
    };
    let name = match name.trim() {
        "" => host.to_string(),
        n => n.to_string(),
    };
    Ok(Draft {
        name,
        target: SshTarget {
            host: host.to_string(),
            port,
            user,
        },
        auth,
    })
}

fn expand_home(raw: &str, home: Option<PathBuf>) -> PathBuf {
    match (
        raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")),
        home,
    ) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(raw),
    }
}

pub fn default_key_path(home: Option<PathBuf>) -> String {
    home.map(|h| h.join(".ssh").join("id_ed25519").display().to_string())
        .unwrap_or_default()
}

pub fn profile_from_enrollment(
    id: String,
    draft: &Draft,
    host_key: &str,
    enrollment: &EnrollResponse,
) -> HostProfile {
    HostProfile {
        id,
        name: draft.name.clone(),
        ssh: draft.target.clone(),
        auth: draft.auth.clone(),
        host_key: Some(host_key.to_string()),
        quic: enrollment.quic_port.map(|port| QuicEndpoint {
            port,
            cert_sha256: enrollment.cert_sha256.clone(),
        }),
        device_id: Some(enrollment.device_id.clone()),
        socket_path: Some(enrollment.socket_path.clone()),
    }
}

/// `None` if the profile was never set up (no pinned host key or socket path).
/// Without a token, QUIC is skipped and the host is reached over SSH only.
pub fn connect_spec(profile: &HostProfile, token: Option<String>) -> Option<ConnectSpec> {
    let host_key = profile.host_key.clone()?;
    let socket_path = profile.socket_path.clone()?;
    let quic = match (&profile.quic, &token) {
        (Some(q), Some(_)) => parse_fingerprint(&q.cert_sha256)
            .ok()
            .map(|cert_sha256| QuicSpec {
                host: profile.ssh.host.clone(),
                port: q.port,
                cert_sha256,
            }),
        _ => None,
    };
    Some(ConnectSpec {
        ssh: profile.ssh.clone(),
        auth: profile.auth.clone(),
        host_key: HostKeyPolicy::Expect(host_key),
        socket_path,
        quic,
        device_id: profile.device_id.clone(),
        token,
        client_version: client_version(),
        timings: Timings::default(),
        // A remembered host upgrades an older daemon in place over the pinned
        // SSH session, keeping its enrollment and running shells.
        upgrade: Some(DaemonUpgrade {
            artifacts: crate::embedded::artifacts(),
            options: BootstrapOptions::new(device_name()),
        }),
    })
}

pub fn client_version() -> String {
    format!("flotilla/{PRODUCT_VERSION} ({})", build_id())
}

/// This PC's name for the daemon's device list.
pub fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "windows-pc".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Good,
    Busy,
    Bad,
}

/// Title-bar status, e.g. `Connected · QUIC · 42 ms`.
pub fn status_line(phase: &ConnPhase) -> (Tone, String) {
    match phase {
        ConnPhase::Idle => (Tone::Neutral, "Disconnected".into()),
        ConnPhase::Connecting => (Tone::Busy, "Connecting…".into()),
        ConnPhase::Connected { kind, rtt_ms, .. } => {
            (Tone::Good, format!("Connected · {kind} · {rtt_ms} ms"))
        }
        ConnPhase::Reconnecting { attempt, .. } => {
            (Tone::Busy, format!("Reconnecting… (attempt {attempt})"))
        }
        ConnPhase::Upgrading { daemon_version } => {
            (Tone::Busy, format!("Upgrading daemon {daemon_version}…"))
        }
        ConnPhase::Failed { .. } => (Tone::Bad, "Connection failed".into()),
    }
}

/// Install-progress copy; `Uploading` shows the size in MB.
pub fn step_label(step: &BootstrapStep) -> String {
    match step {
        BootstrapStep::Probing => "Checking the server".into(),
        BootstrapStep::Uploading { bytes } => format!(
            "Uploading the daemon ({:.1} MB)",
            *bytes as f64 / 1_048_576.0
        ),
        BootstrapStep::Starting => "Starting the daemon".into(),
        BootstrapStep::Enrolling => "Registering this PC".into(),
        BootstrapStep::Done => "Done".into(),
    }
}

/// Position of a step in the visible checklist (Probing=0 … Enrolling=3, Done=4).
pub fn step_index(step: &BootstrapStep) -> usize {
    match step {
        BootstrapStep::Probing => 0,
        BootstrapStep::Uploading { .. } => 1,
        BootstrapStep::Starting => 2,
        BootstrapStep::Enrolling => 3,
        BootstrapStep::Done => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flc_conn::supervisor::TransportKind;

    fn home() -> Option<PathBuf> {
        Some(PathBuf::from("/home/me"))
    }

    #[test]
    fn draft_defaults() {
        let d = parse_draft(
            "",
            " 203.0.113.7 ",
            "",
            "",
            "~/.ssh/id_ed25519",
            false,
            home(),
        )
        .unwrap();
        assert_eq!(d.name, "203.0.113.7");
        assert_eq!(
            d.target,
            SshTarget {
                host: "203.0.113.7".into(),
                port: 22,
                user: "root".into()
            }
        );
        assert_eq!(
            d.auth,
            SshAuth::KeyFile {
                path: PathBuf::from("/home/me/.ssh/id_ed25519")
            }
        );
    }

    #[test]
    fn draft_with_agent_ignores_key_path() {
        let d = parse_draft("vps", "vps.example.com", "2222", "dev", "", true, home()).unwrap();
        assert_eq!(d.auth, SshAuth::Agent { socket: None });
        assert_eq!(d.target.port, 2222);
        assert_eq!(d.target.user, "dev");
        assert_eq!(d.name, "vps");
    }

    #[test]
    fn draft_strips_quotes_from_pasted_windows_paths() {
        let d = parse_draft(
            "",
            "h",
            "",
            "",
            "\"C:\\Users\\me\\.ssh\\id_ed25519\"",
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            d.auth,
            SshAuth::KeyFile {
                path: PathBuf::from("C:\\Users\\me\\.ssh\\id_ed25519")
            }
        );
    }

    #[test]
    fn draft_errors_are_specific() {
        assert!(
            parse_draft("", "", "", "", "k", false, None)
                .unwrap_err()
                .contains("address")
        );
        assert!(parse_draft("", "root@host", "", "", "k", false, None).is_err());
        assert!(
            parse_draft("", "h", "70000", "", "k", false, None)
                .unwrap_err()
                .contains("Port")
        );
        assert!(parse_draft("", "h", "0", "", "k", false, None).is_err());
        assert!(parse_draft("", "h", "", "a b", "k", false, None).is_err());
        assert!(
            parse_draft("", "h", "", "", "  ", false, None)
                .unwrap_err()
                .contains("key")
        );
    }

    #[test]
    fn status_line_copy_matches_spec() {
        let p = ConnPhase::Connected {
            kind: TransportKind::Quic,
            rtt_ms: 42,
            daemon_version: "0.1.0".into(),
        };
        assert_eq!(
            status_line(&p),
            (Tone::Good, "Connected · QUIC · 42 ms".to_string())
        );
        let p = ConnPhase::Connected {
            kind: TransportKind::Ssh,
            rtt_ms: 7,
            daemon_version: "0.1.0".into(),
        };
        assert_eq!(status_line(&p).1, "Connected · SSH · 7 ms");
        assert_eq!(
            status_line(&ConnPhase::Reconnecting {
                attempt: 2,
                error: "x".into()
            })
            .0,
            Tone::Busy
        );
        assert_eq!(
            status_line(&ConnPhase::Failed { error: "x".into() }).0,
            Tone::Bad
        );
    }

    fn enrollment(port: Option<u16>) -> EnrollResponse {
        EnrollResponse {
            device_id: "dev-1".into(),
            token: "t".repeat(64),
            quic_port: port,
            cert_sha256: "ab".repeat(32),
            socket_path: "/root/.flotilla/run/daemon.sock".into(),
        }
    }

    #[test]
    fn profile_and_spec_from_enrollment() {
        let draft = parse_draft("vps", "203.0.113.7", "", "", "~/.ssh/k", false, home()).unwrap();
        let p =
            profile_from_enrollment("id-1".into(), &draft, "SHA256:fp", &enrollment(Some(7443)));
        assert_eq!(
            p.quic,
            Some(QuicEndpoint {
                port: 7443,
                cert_sha256: "ab".repeat(32)
            })
        );
        let spec = connect_spec(&p, Some("tok".into())).unwrap();
        assert_eq!(spec.host_key, HostKeyPolicy::Expect("SHA256:fp".into()));
        let q = spec.quic.expect("QUIC configured");
        assert_eq!(
            (q.host.as_str(), q.port, q.cert_sha256),
            ("203.0.113.7", 7443, [0xab; 32])
        );
        assert_eq!(spec.token.as_deref(), Some("tok"));
    }

    #[test]
    fn spec_without_token_or_port_is_ssh_only() {
        let draft = parse_draft("", "h", "", "", "k", false, None).unwrap();
        let p = profile_from_enrollment("id".into(), &draft, "SHA256:fp", &enrollment(Some(7443)));
        assert!(connect_spec(&p, None).unwrap().quic.is_none());
        let p = profile_from_enrollment("id".into(), &draft, "SHA256:fp", &enrollment(None));
        assert!(connect_spec(&p, Some("t".into())).unwrap().quic.is_none());
    }

    #[test]
    fn spec_requires_setup() {
        let draft = parse_draft("", "h", "", "", "k", false, None).unwrap();
        let mut p = profile_from_enrollment("id".into(), &draft, "SHA256:fp", &enrollment(None));
        p.host_key = None;
        assert!(connect_spec(&p, None).is_none());
    }

    #[test]
    fn step_copy() {
        assert_eq!(
            step_label(&BootstrapStep::Uploading {
                bytes: 3 * 1_048_576
            }),
            "Uploading the daemon (3.0 MB)"
        );
        assert_eq!(step_index(&BootstrapStep::Enrolling), 3);
    }
}
