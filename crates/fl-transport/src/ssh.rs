//! SSH client used for bootstrap and as the fallback transport (Task 7).
//!
//! The data types below are fixed by the scaffold because `flc-conn` and the
//! client persist them; Task 7 adds `SshSession` and friends below them.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::Handle;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
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

/// A bidirectional byte stream over an SSH channel.
pub type SshStream = russh::ChannelStream<russh::client::Msg>;

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("cannot connect to {addr}: {reason}")]
    Connect { addr: String, reason: String },
    #[error("SSH connection timed out")]
    Timeout,
    #[error("host key mismatch: expected {expected}, server presented {actual}")]
    HostKeyMismatch { expected: String, actual: String },
    #[error("authentication rejected for user {user}")]
    AuthFailed { user: String },
    #[error("SSH agent unavailable: {0}")]
    NoAgent(String),
    #[error("cannot load key {}: {reason}", .path.display())]
    Key { path: PathBuf, reason: String },
    #[error("key {} is passphrase-protected; load it into ssh-agent (ssh-add) and choose agent authentication", .0.display())]
    NeedsPassphrase(PathBuf),
    #[error("cannot open remote socket {path}: {reason}")]
    SocketUnavailable { path: String, reason: String },
    #[error("remote command `{cmd}` exited with {code:?}: {stderr}")]
    Command {
        cmd: String,
        code: Option<u32>,
        stderr: String,
    },
    #[error("SSH protocol error: {0}")]
    Protocol(String),
}

impl SshError {
    /// Errors that retrying cannot fix without the user changing something.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            SshError::HostKeyMismatch { .. }
                | SshError::AuthFailed { .. }
                | SshError::NoAgent(_)
                | SshError::Key { .. }
                | SshError::NeedsPassphrase(_)
        )
    }
}

fn proto(e: russh::Error) -> SshError {
    SshError::Protocol(e.to_string())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<u32>,
}

impl ExecOutput {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

struct ClientHandler {
    policy: HostKeyPolicy,
    seen: Arc<Mutex<Option<String>>>,
}

impl russh::client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            // Host certificates are not supported in M0.
            return Ok(false);
        };
        let fp = key.fingerprint(HashAlg::Sha256).to_string();
        *self.seen.lock().unwrap() = Some(fp.clone());
        Ok(match &self.policy {
            HostKeyPolicy::TrustOnFirstUse => true,
            HostKeyPolicy::Expect(expected) => expected == &fp,
        })
    }
}

/// Load an unencrypted private key; encrypted keys map to `NeedsPassphrase`.
pub fn load_key(path: &Path) -> Result<russh::keys::PrivateKey, SshError> {
    match russh::keys::load_secret_key(path, None) {
        Ok(k) => Ok(k),
        Err(russh::keys::Error::KeyIsEncrypted) => {
            Err(SshError::NeedsPassphrase(path.to_path_buf()))
        }
        Err(e) => Err(SshError::Key {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }),
    }
}

pub struct SshSession {
    handle: Handle<ClientHandler>,
    host_key: String,
    target: SshTarget,
}

impl SshSession {
    pub async fn connect(
        target: &SshTarget,
        auth: &SshAuth,
        policy: &HostKeyPolicy,
        timeout: Duration,
    ) -> Result<Self, SshError> {
        // Load the key before touching the network: a bad key fails fast and offline.
        let key = match auth {
            SshAuth::KeyFile { path } => Some(load_key(path)?),
            SshAuth::Agent { .. } => None,
        };
        tokio::time::timeout(timeout, Self::connect_inner(target, auth, key, policy))
            .await
            .map_err(|_| SshError::Timeout)?
    }

    async fn connect_inner(
        target: &SshTarget,
        auth: &SshAuth,
        key: Option<russh::keys::PrivateKey>,
        policy: &HostKeyPolicy,
    ) -> Result<Self, SshError> {
        let seen = Arc::new(Mutex::new(None));
        let handler = ClientHandler {
            policy: policy.clone(),
            seen: seen.clone(),
        };
        let config = Arc::new(russh::client::Config {
            keepalive_interval: Some(Duration::from_secs(5)),
            keepalive_max: 3,
            nodelay: true,
            inactivity_timeout: None,
            ..Default::default()
        });
        let mut handle = match russh::client::connect(
            config,
            (target.host.as_str(), target.port),
            handler,
        )
        .await
        {
            Ok(h) => h,
            Err(e) => {
                let presented = seen.lock().unwrap().clone();
                return Err(match (policy, presented) {
                    (HostKeyPolicy::Expect(expected), Some(actual)) if &actual != expected => {
                        SshError::HostKeyMismatch {
                            expected: expected.clone(),
                            actual,
                        }
                    }
                    _ => SshError::Connect {
                        addr: format!("{}:{}", target.host, target.port),
                        reason: e.to_string(),
                    },
                });
            }
        };
        let host_key = seen.lock().unwrap().clone().unwrap_or_default();
        match (auth, key) {
            (SshAuth::KeyFile { .. }, Some(key)) => {
                let hash = if matches!(key.algorithm(), russh::keys::Algorithm::Rsa { .. }) {
                    handle
                        .best_supported_rsa_hash()
                        .await
                        .map_err(proto)?
                        .flatten()
                } else {
                    None
                };
                let res = handle
                    .authenticate_publickey(
                        &target.user,
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await
                    .map_err(proto)?;
                if !res.success() {
                    return Err(SshError::AuthFailed {
                        user: target.user.clone(),
                    });
                }
            }
            (SshAuth::Agent { socket }, _) => {
                agent_auth(&mut handle, &target.user, socket.as_deref()).await?
            }
            (SshAuth::KeyFile { path }, None) => {
                return Err(SshError::Key {
                    path: path.clone(),
                    reason: "key not loaded".into(),
                });
            }
        }
        Ok(Self {
            handle,
            host_key,
            target: target.clone(),
        })
    }

    /// `SHA256:...` fingerprint of the server host key seen during this connection.
    pub fn host_key_fingerprint(&self) -> &str {
        &self.host_key
    }

    pub fn target(&self) -> &SshTarget {
        &self.target
    }

    /// Run `command` through the remote login shell, feeding `stdin` (then EOF).
    /// Output is collected until the channel closes.
    pub async fn exec(&self, command: &str, stdin: Option<&[u8]>) -> Result<ExecOutput, SshError> {
        let channel = self.handle.channel_open_session().await.map_err(proto)?;
        channel.exec(true, command).await.map_err(proto)?;
        let (mut read, write) = channel.split();
        let writer = async {
            if let Some(input) = stdin {
                write.data(input).await.map_err(proto)?;
            }
            write.eof().await.map_err(proto)?;
            Ok::<(), SshError>(())
        };
        let reader = async {
            let mut out = ExecOutput::default();
            while let Some(msg) = read.wait().await {
                match msg {
                    ChannelMsg::Data { data } => out.stdout.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, ext: 1 } => {
                        out.stderr.extend_from_slice(&data)
                    }
                    ChannelMsg::ExitStatus { exit_status } => out.code = Some(exit_status),
                    _ => {}
                }
            }
            out
        };
        // Write and read concurrently so large stdin and large stdout cannot deadlock.
        let (written, out) = tokio::join!(writer, reader);
        written?;
        Ok(out)
    }

    /// Like `exec`, but a non-zero exit status is an error carrying stderr.
    pub async fn exec_checked(
        &self,
        command: &str,
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>, SshError> {
        let out = self.exec(command, stdin).await?;
        if out.code == Some(0) {
            Ok(out.stdout)
        } else {
            Err(SshError::Command {
                cmd: command.to_string(),
                code: out.code,
                stderr: out.stderr_str().trim().to_string(),
            })
        }
    }

    /// Open a `direct-streamlocal@openssh.com` channel to a Unix socket on the server.
    pub async fn open_unix_socket(&self, path: &str) -> Result<SshStream, SshError> {
        let channel = self
            .handle
            .channel_open_direct_streamlocal(path)
            .await
            .map_err(|e| SshError::SocketUnavailable {
                path: path.to_string(),
                reason: e.to_string(),
            })?;
        Ok(channel.into_stream())
    }

    /// Start `command` and return its stdin/stdout as a byte stream (stderr is discarded).
    pub async fn open_exec_stream(&self, command: &str) -> Result<SshStream, SshError> {
        let channel = self.handle.channel_open_session().await.map_err(proto)?;
        channel.exec(true, command).await.map_err(proto)?;
        Ok(channel.into_stream())
    }

    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    pub async fn disconnect(&self) {
        let _ = self
            .handle
            .disconnect(Disconnect::ByApplication, "bye", "en")
            .await;
    }
}

#[cfg(unix)]
async fn agent_auth(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    socket: Option<&str>,
) -> Result<(), SshError> {
    use russh::keys::agent::client::AgentClient;
    let agent = match socket {
        Some(path) => AgentClient::connect_uds(path).await,
        None => AgentClient::connect_env().await,
    }
    .map_err(|e| SshError::NoAgent(e.to_string()))?;
    auth_with_agent(handle, user, agent).await
}

#[cfg(windows)]
async fn agent_auth(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    socket: Option<&str>,
) -> Result<(), SshError> {
    use russh::keys::agent::client::AgentClient;
    if let Some(pipe) = socket {
        let agent = AgentClient::connect_named_pipe(pipe)
            .await
            .map_err(|e| SshError::NoAgent(format!("{pipe}: {e}")))?;
        return auth_with_agent(handle, user, agent).await;
    }
    match AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        Ok(agent) => auth_with_agent(handle, user, agent).await,
        Err(pipe_err) => match AgentClient::connect_pageant().await {
            Ok(agent) => auth_with_agent(handle, user, agent).await,
            Err(pageant_err) => Err(SshError::NoAgent(format!(
                "OpenSSH agent: {pipe_err}; Pageant: {pageant_err}"
            ))),
        },
    }
}

async fn auth_with_agent<S>(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    mut agent: russh::keys::agent::client::AgentClient<S>,
) -> Result<(), SshError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| SshError::NoAgent(e.to_string()))?;
    let mut tried = 0usize;
    for identity in identities {
        let russh::keys::agent::AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        tried += 1;
        let hash = if matches!(key.algorithm(), russh::keys::Algorithm::Rsa { .. }) {
            handle
                .best_supported_rsa_hash()
                .await
                .map_err(proto)?
                .flatten()
        } else {
            None
        };
        let res = handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .map_err(|e| SshError::Protocol(e.to_string()))?;
        if res.success() {
            return Ok(());
        }
    }
    if tried == 0 {
        return Err(SshError::NoAgent(
            "the agent has no keys loaded (run ssh-add)".into(),
        ));
    }
    Err(SshError::AuthFailed {
        user: user.to_string(),
    })
}
