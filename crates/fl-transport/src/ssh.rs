//! SSH client used for bootstrap and as the fallback transport (Task 7).
//!
//! The data types below are fixed by the scaffold because `flc-conn` and the
//! client persist them; Task 7 adds `SshSession` and friends below them.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where to connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
}

/// How to authenticate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshAuth {
    /// Use an SSH agent. `socket: None` means the platform default:
    /// `$SSH_AUTH_SOCK` on Unix; the OpenSSH agent pipe
    /// `\\.\pipe\openssh-ssh-agent`, then Pageant, on Windows.
    /// `Some(path)` is a Unix socket path or a Windows named-pipe path.
    Agent { socket: Option<String> },
    /// An unencrypted OpenSSH/PEM private key file.
    KeyFile { path: PathBuf },
}

/// What to do with the server's host key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostKeyPolicy {
    /// Accept any key (the caller shows the fingerprint to the user and pins it).
    TrustOnFirstUse,
    /// Accept only this `SHA256:...` fingerprint.
    Expect(String),
}
