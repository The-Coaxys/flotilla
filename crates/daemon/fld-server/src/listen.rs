//! Accept loops for the Unix socket and QUIC.
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fl_transport::conn::framed;
use fl_transport::quic::accept_ctl;
use tokio::net::UnixListener;

use crate::ServerError;
use crate::session::{PeerKind, SessionCtx, serve_connection};

/// Bind the control socket, replacing a stale socket file left by a crash.
/// Refuses with `AlreadyRunning` if a live daemon answers on it.
pub fn bind_unix(path: &Path) -> Result<UnixListener, ServerError> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(ServerError::AlreadyRunning(path.to_path_buf()));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn unix_accept_loop(listener: UnixListener, ctx: Arc<SessionCtx>) {
    loop {
        let accepted = tokio::select! {
            _ = ctx.shutdown.cancelled() => break,
            a = listener.accept() => a,
        };
        match accepted {
            Ok((stream, _addr)) => {
                let (r, w) = stream.into_split();
                let (reader, writer) = framed(r, w);
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_connection(ctx, reader, writer, PeerKind::Local).await {
                        tracing::debug!("local session ended: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("unix accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

pub async fn quic_accept_loop(endpoint: quinn::Endpoint, ctx: Arc<SessionCtx>) {
    loop {
        let incoming = tokio::select! {
            _ = ctx.shutdown.cancelled() => break,
            i = endpoint.accept() => match i {
                Some(i) => i,
                None => break,
            },
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            let Ok((send, recv)) = accept_ctl(&conn).await else {
                return;
            };
            let (reader, writer) = framed(recv, send);
            if let Err(e) = serve_connection(ctx, reader, writer, PeerKind::Quic).await {
                tracing::debug!("quic session ended: {e}");
            }
            // Let the client read our last frame and close first; then close.
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
            conn.close(0u32.into(), b"bye");
        });
    }
}
