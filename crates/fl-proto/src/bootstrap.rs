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
