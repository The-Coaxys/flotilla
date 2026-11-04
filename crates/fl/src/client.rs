//! A small direct control-stream client. `fl` needs no terminal channels, no
//! UI state and no supervisor; it opens one Unix socket and speaks the
//! existing framed protocol.
use std::path::{Path, PathBuf};

use fl_proto::{Request, Response};

use crate::error::CliError;

/// `$FLOTILLA_SOCKET`, else `$FLOTILLA_HOME/run/daemon.sock`, else
/// `$HOME/.flotilla/run/daemon.sock`.
pub fn socket_path() -> Result<PathBuf, CliError> {
    if let Some(socket) = std::env::var_os("FLOTILLA_SOCKET").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(socket));
    }
    if let Some(root) = std::env::var_os("FLOTILLA_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root).join("run").join("daemon.sock"));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CliError::unavailable("neither FLOTILLA_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home)
        .join(".flotilla")
        .join("run")
        .join("daemon.sock"))
}

#[cfg(unix)]
use fl_proto::{ClientMsg, Hello, PRODUCT_VERSION, PROTO, ServerMsg};
#[cfg(unix)]
use fl_transport::conn::{FrameReader, FrameWriter, framed};
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(unix)]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Forge actions and provider discovery can legitimately be slow.
#[cfg(unix)]
const RPC_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(unix)]
pub struct DaemonClient {
    reader: FrameReader,
    writer: FrameWriter,
    next_id: u64,
}

#[cfg(unix)]
impl DaemonClient {
    pub async fn connect(path: &Path) -> Result<Self, CliError> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(path))
            .await
            .map_err(|_| {
                CliError::unavailable(format!("timed out connecting to {}", path.display()))
            })?
            .map_err(|error| {
                CliError::unavailable(format!("cannot connect to {}: {error}", path.display()))
            })?;
        let (read, write) = stream.into_split();
        let (mut reader, mut writer) = framed(read, write);
        let hello = ClientMsg::Hello(Hello {
            client_version: format!("fl/{PRODUCT_VERSION}"),
            proto: PROTO,
            device_id: None,
            token: None,
            client_session_id: *uuid::Uuid::new_v4().as_bytes(),
        });
        writer
            .send(&hello)
            .await
            .map_err(|error| CliError::unavailable(format!("sending hello: {error}")))?;
        match tokio::time::timeout(HELLO_TIMEOUT, reader.recv::<ServerMsg>())
            .await
            .map_err(|_| CliError::unavailable("timed out waiting for the daemon welcome"))?
            .map_err(CliError::from)?
        {
            ServerMsg::Welcome(_) => {}
            ServerMsg::Rejected(reason) => {
                return Err(CliError::new(
                    "rejected",
                    format!("daemon rejected the connection: {reason:?}"),
                ));
            }
            other => {
                return Err(CliError::internal(format!(
                    "unexpected daemon reply to hello: {other:?}"
                )));
            }
        }
        Ok(Self {
            reader,
            writer,
            next_id: 0,
        })
    }

    pub async fn rpc(&mut self, request: Request) -> Result<Response, CliError> {
        self.next_id += 1;
        let id = self.next_id;
        self.writer
            .send(&ClientMsg::Rpc { id, req: request })
            .await
            .map_err(|error| CliError::unavailable(format!("sending request: {error}")))?;
        let deadline = tokio::time::Instant::now() + RPC_TIMEOUT;
        loop {
            let message = tokio::time::timeout_at(deadline, self.reader.recv::<ServerMsg>())
                .await
                .map_err(|_| CliError::internal("daemon did not answer the request in time"))?
                .map_err(CliError::from)?;
            match message {
                ServerMsg::RpcResult { id: got, result } if got == id => {
                    return result.map_err(CliError::rpc);
                }
                ServerMsg::RpcResult { .. } => continue,
                ServerMsg::Event(_) | ServerMsg::Pong { .. } => continue,
                ServerMsg::Rejected(reason) => {
                    return Err(CliError::new(
                        "rejected",
                        format!("daemon rejected the connection: {reason:?}"),
                    ));
                }
                other => {
                    return Err(CliError::internal(format!(
                        "unexpected daemon message: {other:?}"
                    )));
                }
            }
        }
    }
}

/// `fl` drives the daemon through its Unix socket; other hosts get a clear
/// refusal rather than a build-time failure.
#[cfg(not(unix))]
pub struct DaemonClient;

#[cfg(not(unix))]
impl DaemonClient {
    pub async fn connect(_path: &Path) -> Result<Self, CliError> {
        Err(CliError::unsupported(
            "fl requires a Unix host with a Flotilla daemon socket",
        ))
    }

    pub async fn rpc(&mut self, _request: Request) -> Result<Response, CliError> {
        Err(CliError::unsupported(
            "fl requires a Unix host with a Flotilla daemon socket",
        ))
    }
}
