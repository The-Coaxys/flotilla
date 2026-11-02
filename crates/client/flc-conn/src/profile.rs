//! Persisted description of a remote host. Written by the scaffold; stable.
use fl_transport::ssh::{SshAuth, SshTarget};
use serde::{Deserialize, Serialize};

/// QUIC endpoint learned at enrollment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuicEndpoint {
    pub port: u16,
    /// Lowercase hex SHA-256 of the daemon's DER certificate (64 chars).
    pub cert_sha256: String,
}

/// One remote workspace host. The device token is NOT stored here; the client
/// keeps it in the OS secret store keyed by `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProfile {
    /// Stable local id (UUID v4 string).
    pub id: String,
    /// Display name, e.g. "my-vps".
    pub name: String,
    pub ssh: SshTarget,
    pub auth: SshAuth,
    /// Pinned `SHA256:...` host-key fingerprint; `None` until the user trusts it.
    pub host_key: Option<String>,
    /// `None` when the daemon has QUIC disabled or the port is unknown.
    pub quic: Option<QuicEndpoint>,
    /// Device id issued by `flotillad enroll`.
    pub device_id: Option<String>,
    /// Remote daemon socket path, e.g. `/root/.flotilla/run/daemon.sock`.
    pub socket_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn profile_json_roundtrip() {
        let p = HostProfile {
            id: "8d7c2a0e-0000-4000-8000-000000000001".into(),
            name: "vps".into(),
            ssh: SshTarget { host: "203.0.113.7".into(), port: 22, user: "root".into() },
            auth: SshAuth::KeyFile { path: PathBuf::from(r"C:\Users\me\.ssh\id_ed25519") },
            host_key: Some("SHA256:abc".into()),
            quic: Some(QuicEndpoint { port: 7443, cert_sha256: "00".repeat(32) }),
            device_id: Some("dev-1".into()),
            socket_path: Some("/root/.flotilla/run/daemon.sock".into()),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: HostProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }
}
