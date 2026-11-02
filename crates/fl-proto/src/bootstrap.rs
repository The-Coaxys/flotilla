//! JSON documents printed by `flotillad` and parsed by the client during
//! install and enrollment. Field names are a cross-version contract.
use serde::{Deserialize, Serialize};

use crate::{PRODUCT_VERSION, PROTO, ProtoVersion, build_id};

/// Output of `flotillad version --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub build: String,
    pub proto: ProtoVersion,
}

impl VersionInfo {
    pub fn current() -> Self {
        Self {
            version: PRODUCT_VERSION.to_string(),
            build: build_id().to_string(),
            proto: PROTO,
        }
    }
}

/// Output of `flotillad enroll --device-name <name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub device_id: String,
    /// 64 lowercase hex chars. Shown once; the daemon keeps only its SHA-256.
    pub token: String,
    /// `None` when the daemon runs without QUIC.
    pub quic_port: Option<u16>,
    /// Lowercase hex SHA-256 of the daemon's DER certificate.
    pub cert_sha256: String,
    pub socket_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_info_json_shape() {
        let json = serde_json::to_string(&VersionInfo::current()).unwrap();
        assert!(json.contains(r#""version":"0.1.0""#), "{json}");
        assert!(json.contains(r#""proto":{"major":0,"minor":1}"#), "{json}");
        let back: VersionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, VersionInfo::current());
    }

    #[test]
    fn enroll_response_roundtrips_with_null_port() {
        let r = EnrollResponse {
            device_id: "d".into(),
            token: "ab".repeat(32),
            quic_port: None,
            cert_sha256: "cd".repeat(32),
            socket_path: "/home/u/.flotilla/run/daemon.sock".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""quic_port":null"#));
        assert_eq!(serde_json::from_str::<EnrollResponse>(&json).unwrap(), r);
    }
}
