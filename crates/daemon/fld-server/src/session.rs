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
    pub hello_timeout: Duration,
    pub info: InfoSource,
    /// Cancelled when the daemon shuts down; every session then ends.
    pub shutdown: CancellationToken,
}

pub async fn serve_connection(
    ctx: Arc<SessionCtx>,
    mut reader: FrameReader,
    mut writer: FrameWriter,
    peer: PeerKind,
) -> Result<(), ConnError> {
    let first = tokio::select! {
        _ = ctx.shutdown.cancelled() => return Ok(()),
        r = tokio::time::timeout(ctx.hello_timeout, reader.recv::<ClientMsg>()) => r,
    };
    let hello = match first {
        Err(_elapsed) => return reject(&mut writer, RejectReason::HelloTimeout).await,
        Ok(Err(e)) => return Err(e),
        Ok(Ok(ClientMsg::Hello(h))) => h,
        Ok(Ok(_)) => return reject(&mut writer, RejectReason::ExpectedHello).await,
    };
    if !PROTO.is_compatible_with(hello.proto) {
        return reject(&mut writer, RejectReason::ProtoMismatch { server: PROTO }).await;
    }
    if peer == PeerKind::Quic && !token_ok(&ctx, &hello).await {
        return reject(&mut writer, RejectReason::BadToken).await;
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
            _ = ctx.shutdown.cancelled() => return Ok(()),
            m = reader.recv::<ClientMsg>() => m,
        };
        match msg {
            Ok(ClientMsg::Ping { nonce }) => writer.send(&ServerMsg::Pong { nonce }).await?,
            Ok(ClientMsg::Rpc { id, req }) => {
                let result = handle_request(&ctx, req);
                writer.send(&ServerMsg::RpcResult { id, result }).await?;
            }
            Ok(ClientMsg::Hello(_)) => {} // a repeated Hello is harmless; ignore it
            Err(ConnError::Closed) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
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
