//! One logical stream. Its first message selects the stream kind:
//! `Hello` → authenticated control session (Welcome, Ping/Pong, RPCs), or
//! `BindChannel` → a terminal/bulk channel proven by a one-use session ticket.
use std::sync::Arc;
use std::time::{Duration, Instant};

use fl_model::ClientSessionId;
use fl_proto::{
    ChannelHello, ChannelReady, ClientMsg, DaemonInfo, Hello, PRODUCT_VERSION, PROTO, RejectReason,
    Request, Response, RpcError, RpcErrorCode, ServerMsg, Welcome, build_id,
};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::channels::Channels;
use crate::devices::DeviceRegistry;
use crate::terminal::TerminalService;

/// Concurrent RPCs per control session; further requests wait to be read.
const MAX_INFLIGHT_RPCS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerKind {
    /// Unix socket. The peer came through SSH (already authenticated) or is local.
    Local,
    /// QUIC. The peer must present an enrolled device token.
    Quic,
}

pub struct InfoSource {
    pub started: Instant,
    pub hostname: String,
    pub quic_port: Option<u16>,
    pub socket_path: String,
}

pub struct SessionCtx {
    pub registry: Arc<DeviceRegistry>,
    /// Authentication budget; QUIC starts it after TLS, before the control stream.
    pub hello_timeout: Duration,
    pub info: InfoSource,
    /// Cancelled when the daemon shuts down; every session then ends.
    pub shutdown: CancellationToken,
    pub(crate) channels: Arc<Channels>,
    pub(crate) terminals: Arc<TerminalService>,
}

/// Allow a best-effort timeout rejection without letting an unresponsive peer
/// keep the unauthenticated connection open indefinitely.
pub(crate) const HELLO_TIMEOUT_GRACE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// The session ended without an authentication timeout.
    Finished,
    /// Authentication expired; transport closure must not use the normal grace.
    HelloTimeout,
}

enum Opening {
    Control(Hello),
    Channel(ChannelHello),
}

pub async fn serve_connection(
    ctx: Arc<SessionCtx>,
    mut reader: FrameReader,
    mut writer: FrameWriter,
    peer: PeerKind,
    hello_deadline: tokio::time::Instant,
) -> Result<SessionEnd, ConnError> {
    let opening = tokio::select! {
        biased;
        _ = ctx.shutdown.cancelled() => return Ok(SessionEnd::Finished),
        _ = tokio::time::sleep_until(hello_deadline) => {
            if let Ok(Err(error)) = tokio::time::timeout_at(
                hello_deadline + HELLO_TIMEOUT_GRACE,
                reject(&mut writer, RejectReason::HelloTimeout),
            ).await {
                tracing::debug!("hello timeout rejection failed: {error}");
            }
            return Ok(SessionEnd::HelloTimeout);
        }
        result = authenticate(&ctx, &mut reader, peer) => result?,
    };
    match opening {
        Err(reason) => {
            reject(&mut writer, reason).await?;
            Ok(SessionEnd::Finished)
        }
        Ok(Opening::Control(hello)) => control(ctx, hello, reader, writer).await,
        Ok(Opening::Channel(hello)) => {
            let lease = match ctx.channels.bind(&hello) {
                Ok(lease) => lease,
                Err(reason) => {
                    reject(&mut writer, reason).await?;
                    return Ok(SessionEnd::Finished);
                }
            };
            writer
                .send(&ServerMsg::ChannelReady(ChannelReady {
                    server_session_id: lease.session,
                    role: lease.role,
                }))
                .await?;
            ctx.terminals
                .serve_channel(lease, reader, writer)
                .await
                .map(|()| SessionEnd::Finished)
        }
    }
}

async fn control(
    ctx: Arc<SessionCtx>,
    hello: Hello,
    mut reader: FrameReader,
    mut writer: FrameWriter,
) -> Result<SessionEnd, ConnError> {
    let lease = match ctx.channels.register(
        ClientSessionId::from_bytes(hello.client_session_id),
        &ctx.shutdown,
    ) {
        Ok(lease) => lease,
        Err(error) => {
            tracing::warn!("refusing control session: {error}");
            let _ = writer.shutdown().await;
            return Ok(SessionEnd::Finished);
        }
    };
    writer
        .send(&ServerMsg::Welcome(Welcome {
            daemon_version: PRODUCT_VERSION.to_string(),
            build: build_id().to_string(),
            proto: PROTO,
            hostname: ctx.info.hostname.clone(),
            server_session_id: lease.id,
        }))
        .await?;
    let mut rpcs = JoinSet::new();
    loop {
        tokio::select! {
            _ = lease.cancel.cancelled() => return Ok(SessionEnd::Finished),
            done = rpcs.join_next(), if !rpcs.is_empty() => {
                if let Some(Ok((id, result))) = done {
                    writer.send(&ServerMsg::RpcResult { id, result }).await?;
                }
            }
            message = reader.recv::<ClientMsg>(), if rpcs.len() < MAX_INFLIGHT_RPCS => match message {
                Ok(ClientMsg::Ping { nonce }) => writer.send(&ServerMsg::Pong { nonce }).await?,
                Ok(ClientMsg::Rpc { id, req }) => {
                    let ctx = ctx.clone();
                    let session = lease.id;
                    rpcs.spawn(async move { (id, handle_request(&ctx, session, req).await) });
                }
                // A repeated Hello/BindChannel on a control stream is ignored.
                Ok(ClientMsg::Hello(_) | ClientMsg::BindChannel(_)) => {}
                Err(ConnError::Closed) => return Ok(SessionEnd::Finished),
                Err(e) => return Err(e),
            },
        }
    }
}

async fn authenticate(
    ctx: &SessionCtx,
    reader: &mut FrameReader,
    peer: PeerKind,
) -> Result<Result<Opening, RejectReason>, ConnError> {
    let hello = match reader.recv::<ClientMsg>().await? {
        ClientMsg::Hello(hello) => hello,
        // The one-use ticket, issued to an authenticated control session,
        // is the credential; the channel needs no separate Hello.
        ClientMsg::BindChannel(hello) => return Ok(Ok(Opening::Channel(hello))),
        _ => return Ok(Err(RejectReason::ExpectedHello)),
    };
    if !PROTO.is_compatible_with(hello.proto) {
        return Ok(Err(RejectReason::ProtoMismatch { server: PROTO }));
    }
    if peer == PeerKind::Quic && !token_ok(ctx, &hello).await {
        return Ok(Err(RejectReason::BadToken));
    }
    Ok(Ok(Opening::Control(hello)))
}

async fn reject(writer: &mut FrameWriter, reason: RejectReason) -> Result<(), ConnError> {
    writer.send(&ServerMsg::Rejected(reason)).await?;
    let _ = writer.shutdown().await;
    Ok(())
}

async fn token_ok(ctx: &SessionCtx, hello: &Hello) -> bool {
    let (Some(device), Some(token)) = (hello.device_id.clone(), hello.token.clone()) else {
        return false;
    };
    let registry = ctx.registry.clone();
    tokio::task::spawn_blocking(move || registry.verify(&device, &token))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(false)
}

async fn handle_request(
    ctx: &SessionCtx,
    session: [u8; 16],
    req: Request,
) -> Result<Response, RpcError> {
    let terminals = &ctx.terminals;
    match req {
        Request::DaemonInfo => Ok(Response::DaemonInfo(DaemonInfo {
            version: PRODUCT_VERSION.to_string(),
            build: build_id().to_string(),
            hostname: ctx.info.hostname.clone(),
            pid: std::process::id(),
            uptime_secs: ctx.info.started.elapsed().as_secs(),
            quic_port: ctx.info.quic_port,
            socket_path: ctx.info.socket_path.clone(),
        })),
        Request::Capabilities => terminals.capabilities().map(Response::Capabilities),
        Request::GetLayout => terminals.get_layout().await.map(Response::Layout),
        Request::ApplyLayout {
            operation,
            base_rev,
            op,
        } => terminals
            .apply_layout(operation, base_rev, op)
            .await
            .map(Response::Layout),
        Request::ProcessInfo {
            terminal,
            incarnation,
        } => terminals
            .process_info(terminal, incarnation)
            .map(Response::ProcessInfo),
        Request::ListProcesses => Ok(Response::Processes(terminals.list_processes())),
        Request::OpenChannel { role } => ctx
            .channels
            .grant(session, role)
            .map(Response::ChannelGrant)
            .map_err(|error| RpcError {
                code: RpcErrorCode::InvalidCapability,
                message: error.to_string(),
            }),
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
