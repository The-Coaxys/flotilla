//! The live control link of one supervisor generation.
//!
//! The supervisor publishes a [`ControlLink`] for every established link
//! (`Supervisor::links`). It multiplexes RPCs over the control stream with
//! request IDs and deadlines, and opens ticket-bound logical channels on the
//! same transport: a new bidirectional stream on the QUIC connection, or a new
//! `direct-streamlocal` channel (with the `flotillad attach` exec fallback) in
//! the pinned SSH session.
//!
//! A generation ends when the supervisor fails over, upgrades QUIC↔SSH,
//! reconnects or stops. The link is then [closed](ControlLink::closed): calls
//! fail with [`RpcCallError::LinkClosed`], its QUIC connection is closed, and
//! the daemon ends every channel bound to the old control session. Consumers
//! re-establish their channels on the next published link and discard traffic
//! tagged with an older [`generation`](ControlLink::generation).
use std::sync::Arc;
use std::time::Duration;

use fl_model::ClientSessionId;
use fl_proto::{
    ChannelRole, ClientMsg, ProtoVersion, RejectReason, Request, Response, RpcError, ServerMsg,
};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter, framed};
use fl_transport::ssh::{SshError, SshSession};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::supervisor::{ATTACH_COMMAND, TransportKind};

/// Concurrent RPCs per link. The daemon stops reading a control stream (and
/// therefore its heartbeats) at 32 in-flight requests; stay well below.
pub const MAX_INFLIGHT_RPCS: usize = 16;

pub(crate) type RpcReply = oneshot::Sender<Result<Response, RpcError>>;

/// One request queued for the supervisor's control-stream driver.
pub(crate) struct RpcCall {
    pub req: Request,
    pub reply: RpcReply,
}

#[derive(Debug, thiserror::Error)]
pub enum RpcCallError {
    #[error("the control link closed before the daemon replied")]
    LinkClosed,
    #[error("the daemon did not reply within {0:?}")]
    Timeout(Duration),
    #[error("daemon error {:?}: {}", .0.code, .0.message)]
    Remote(RpcError),
}

impl RpcCallError {
    /// The daemon answered with this error code.
    pub fn code(&self) -> Option<fl_proto::RpcErrorCode> {
        match self {
            Self::Remote(error) => Some(error.code),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error(transparent)]
    Rpc(#[from] RpcCallError),
    #[error("the daemon granted a channel for another session or role")]
    GrantMismatch,
    #[error("unexpected daemon reply while opening a channel")]
    Unexpected,
    #[error("opening a {0} stream failed: {1}")]
    Transport(TransportKind, String),
    #[error("the daemon rejected the channel: {0:?}")]
    Rejected(RejectReason),
    #[error("{0}")]
    Conn(#[from] ConnError),
}

/// How this generation opens additional logical streams.
#[derive(Clone)]
pub(crate) enum Opener {
    Quic {
        _endpoint: quinn::Endpoint,
        connection: quinn::Connection,
    },
    Ssh {
        session: Arc<SshSession>,
        socket_path: String,
        /// The control link itself needed the exec fallback.
        exec: bool,
    },
}

pub(crate) struct LinkInfo {
    pub generation: u64,
    pub kind: TransportKind,
    pub client_session: ClientSessionId,
    pub server_session: [u8; 16],
    pub daemon_version: String,
    pub daemon_proto: ProtoVersion,
    pub rpc_timeout: Duration,
}

struct Inner {
    info: LinkInfo,
    calls: mpsc::Sender<RpcCall>,
    permits: Semaphore,
    opener: Opener,
    closed: CancellationToken,
}

/// A cheap, cloneable handle to the current control link. See the module docs.
#[derive(Clone)]
pub struct ControlLink {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ControlLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlLink")
            .field("generation", &self.inner.info.generation)
            .field("kind", &self.inner.info.kind)
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// An authenticated logical stream bound to one control session and role.
pub struct LogicalChannel {
    pub role: ChannelRole,
    /// The generation of the control link that issued its ticket.
    pub generation: u64,
    pub reader: FrameReader,
    pub writer: FrameWriter,
}

impl ControlLink {
    pub(crate) fn new(info: LinkInfo, calls: mpsc::Sender<RpcCall>, opener: Opener) -> Self {
        Self {
            inner: Arc::new(Inner {
                info,
                calls,
                permits: Semaphore::new(MAX_INFLIGHT_RPCS),
                opener,
                closed: CancellationToken::new(),
            }),
        }
    }

    /// Increases with every link the supervisor establishes (reconnect,
    /// failover or upgrade); never reused within one supervisor.
    pub fn generation(&self) -> u64 {
        self.inner.info.generation
    }

    pub fn kind(&self) -> TransportKind {
        self.inner.info.kind
    }

    /// Stable for the supervisor's lifetime: input identities and owner leases
    /// survive reconnects and failover.
    pub fn client_session_id(&self) -> ClientSessionId {
        self.inner.info.client_session
    }

    /// The daemon's control session for this generation (binds channel tickets).
    pub fn server_session_id(&self) -> [u8; 16] {
        self.inner.info.server_session
    }

    pub fn daemon_version(&self) -> &str {
        &self.inner.info.daemon_version
    }

    pub fn daemon_proto(&self) -> ProtoVersion {
        self.inner.info.daemon_proto
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_cancelled()
    }

    /// Resolves when this generation has ended.
    pub async fn closed(&self) {
        self.inner.closed.cancelled().await
    }

    /// End this generation: pending calls fail and QUIC streams are reset.
    pub(crate) fn close(&self) {
        self.inner.closed.cancel();
        if let Opener::Quic { connection, .. } = &self.inner.opener {
            connection.close(0u32.into(), b"link generation ended");
        }
    }

    /// One RPC with the supervisor's default deadline (`Timings::rpc_timeout`).
    pub async fn rpc(&self, req: Request) -> Result<Response, RpcCallError> {
        self.rpc_within(req, self.inner.info.rpc_timeout).await
    }

    /// One RPC that fails with `Timeout` after `timeout`. A timed-out request
    /// may still be executed by the daemon; idempotent requests carry IDs.
    pub async fn rpc_within(
        &self,
        req: Request,
        timeout: Duration,
    ) -> Result<Response, RpcCallError> {
        let call = async {
            let _permit = self
                .inner
                .permits
                .acquire()
                .await
                .map_err(|_| RpcCallError::LinkClosed)?;
            let (reply, result) = oneshot::channel();
            self.inner
                .calls
                .send(RpcCall { req, reply })
                .await
                .map_err(|_| RpcCallError::LinkClosed)?;
            result
                .await
                .map_err(|_| RpcCallError::LinkClosed)?
                .map_err(RpcCallError::Remote)
        };
        tokio::select! {
            biased;
            _ = self.inner.closed.cancelled() => Err(RpcCallError::LinkClosed),
            result = tokio::time::timeout(timeout, call) => {
                result.unwrap_or(Err(RpcCallError::Timeout(timeout)))
            }
        }
    }

    /// Obtain a one-use ticket for `role`, open a new stream on this link's
    /// transport and bind it. The returned channel belongs to this generation.
    pub async fn open_channel(&self, role: ChannelRole) -> Result<LogicalChannel, ChannelError> {
        let grant = match self.rpc(Request::OpenChannel { role }).await? {
            Response::ChannelGrant(grant) => grant,
            _ => return Err(ChannelError::Unexpected),
        };
        let session = self.inner.info.server_session;
        if grant.role != role || grant.server_session_id != session {
            return Err(ChannelError::GrantMismatch);
        }
        let timeout = self.inner.info.rpc_timeout;
        let bind = async {
            let (mut reader, mut writer) = self.open_stream().await?;
            writer.send(&ClientMsg::BindChannel(grant.hello())).await?;
            match reader.recv::<ServerMsg>().await? {
                ServerMsg::ChannelReady(ready)
                    if ready.server_session_id == session && ready.role == role =>
                {
                    Ok(LogicalChannel {
                        role,
                        generation: self.generation(),
                        reader,
                        writer,
                    })
                }
                ServerMsg::Rejected(reason) => Err(ChannelError::Rejected(reason)),
                _ => Err(ChannelError::Unexpected),
            }
        };
        tokio::select! {
            biased;
            _ = self.inner.closed.cancelled() => Err(RpcCallError::LinkClosed.into()),
            result = tokio::time::timeout(timeout, bind) => {
                result.unwrap_or(Err(RpcCallError::Timeout(timeout).into()))
            }
        }
    }

    async fn open_stream(&self) -> Result<(FrameReader, FrameWriter), ChannelError> {
        let kind = self.kind();
        match &self.inner.opener {
            Opener::Quic { connection, .. } => {
                let (send, recv) = connection
                    .open_bi()
                    .await
                    .map_err(|e| ChannelError::Transport(kind, e.to_string()))?;
                Ok(framed(recv, send))
            }
            Opener::Ssh {
                session,
                socket_path,
                exec,
            } => {
                let transport = |e: SshError| ChannelError::Transport(kind, e.to_string());
                let stream = if *exec {
                    session
                        .open_exec_stream(ATTACH_COMMAND)
                        .await
                        .map_err(transport)?
                } else {
                    match session.open_unix_socket(socket_path).await {
                        Ok(stream) => stream,
                        Err(SshError::SocketUnavailable { .. }) if !session.is_closed() => session
                            .open_exec_stream(ATTACH_COMMAND)
                            .await
                            .map_err(transport)?,
                        Err(e) => return Err(transport(e)),
                    }
                };
                let (r, w) = tokio::io::split(stream);
                Ok(framed(r, w))
            }
        }
    }
}
