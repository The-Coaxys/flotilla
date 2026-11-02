//! One control session: Hello → (auth) → Welcome, then Ping/Pong and RPCs.
use std::sync::Arc;
use std::time::{Duration, Instant};

use fl_proto::{
    ClientMsg, DaemonInfo, Hello, PRODUCT_VERSION, PROTO, RejectReason, Request, Response,
    RpcError, ServerMsg, Welcome, build_id,
};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter};
use tokio_util::sync::CancellationToken;

use crate::devices::DeviceRegistry;

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

pub async fn serve_connection(
    ctx: Arc<SessionCtx>,
    mut reader: FrameReader,
    mut writer: FrameWriter,
    peer: PeerKind,
    hello_deadline: tokio::time::Instant,
) -> Result<SessionEnd, ConnError> {
    let rejection = tokio::select! {
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
    if let Some(reason) = rejection {
        reject(&mut writer, reason).await?;
        return Ok(SessionEnd::Finished);
    }
    let mut server_session_id = [0u8; 16];
    let _ = getrandom::fill(&mut server_session_id);
    writer
        .send(&ServerMsg::Welcome(Welcome {
            daemon_version: PRODUCT_VERSION.to_string(),
            build: build_id().to_string(),
            proto: PROTO,
            hostname: ctx.info.hostname.clone(),
            server_session_id,
        }))
        .await?;
    loop {
        let msg = tokio::select! {
            _ = ctx.shutdown.cancelled() => return Ok(SessionEnd::Finished),
            m = reader.recv::<ClientMsg>() => m,
        };
        match msg {
            Ok(ClientMsg::Ping { nonce }) => writer.send(&ServerMsg::Pong { nonce }).await?,
            Ok(ClientMsg::Rpc { id, req }) => {
                let result = handle_request(&ctx, req);
                writer.send(&ServerMsg::RpcResult { id, result }).await?;
            }
            Ok(ClientMsg::Hello(_)) => {} // a repeated Hello is harmless; ignore it
            Err(ConnError::Closed) => return Ok(SessionEnd::Finished),
            Err(e) => return Err(e),
        }
    }
}

async fn authenticate(
    ctx: &SessionCtx,
    reader: &mut FrameReader,
    peer: PeerKind,
) -> Result<Option<RejectReason>, ConnError> {
    let hello = match reader.recv::<ClientMsg>().await? {
        ClientMsg::Hello(hello) => hello,
        _ => return Ok(Some(RejectReason::ExpectedHello)),
    };
    if !PROTO.is_compatible_with(hello.proto) {
        return Ok(Some(RejectReason::ProtoMismatch { server: PROTO }));
    }
    if peer == PeerKind::Quic && !token_ok(ctx, &hello).await {
        return Ok(Some(RejectReason::BadToken));
    }
    Ok(None)
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

fn handle_request(ctx: &SessionCtx, req: Request) -> Result<Response, RpcError> {
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
