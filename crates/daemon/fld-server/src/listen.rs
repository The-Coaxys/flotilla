//! Accept loops for the Unix socket and QUIC.
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fl_transport::conn::framed;
use fl_transport::quic::accept_ctl;
use tokio::net::UnixListener;

use crate::ServerError;
use crate::session::{HELLO_TIMEOUT_GRACE, PeerKind, SessionCtx, SessionEnd, serve_connection};

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
                    let hello_deadline = tokio::time::Instant::now() + ctx.hello_timeout;
                    if let Err(e) =
                        serve_connection(ctx, reader, writer, PeerKind::Local, hello_deadline).await
                    {
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
            let conn = tokio::select! {
                biased;
                _ = ctx.shutdown.cancelled() => return,
                result = incoming => match result {
                    Ok(conn) => conn,
                    Err(_) => return,
                },
            };
            // TLS is complete. Opening the control stream and authenticating
            // its Hello must consume the same budget.
            let hello_deadline = tokio::time::Instant::now() + ctx.hello_timeout;
            let (send, recv) = tokio::select! {
                biased;
                _ = ctx.shutdown.cancelled() => {
                    conn.close(0u32.into(), b"daemon shutting down");
                    return;
                }
                _ = tokio::time::sleep_until(hello_deadline) => {
                    conn.close(0u32.into(), b"hello timeout");
                    return;
                }
                result = accept_ctl(&conn) => match result {
                    Ok(streams) => streams,
                    Err(_) => {
                        conn.close(0u32.into(), b"control stream unavailable");
                        return;
                    }
                },
            };
            let (reader, writer) = framed(recv, send);
            let result = tokio::select! {
                biased;
                _ = ctx.shutdown.cancelled() => {
                    conn.close(0u32.into(), b"daemon shutting down");
                    return;
                }
                result = serve_connection(
                    ctx.clone(), reader, writer, PeerKind::Quic, hello_deadline,
                ) => result,
            };
            let close_deadline = match result {
                Ok(SessionEnd::HelloTimeout) => hello_deadline + HELLO_TIMEOUT_GRACE,
                Ok(SessionEnd::Finished) => tokio::time::Instant::now() + Duration::from_secs(5),
                Err(e) => {
                    tracing::debug!("quic session ended: {e}");
                    tokio::time::Instant::now() + Duration::from_secs(5)
                }
            };
            // Let the client read our last frame and close first, but do not
            // extend the authentication budget beyond a brief rejection grace.
            tokio::select! {
                biased;
                _ = ctx.shutdown.cancelled() => {}
                _ = tokio::time::sleep_until(close_deadline) => {}
                _ = conn.closed() => {}
            }
            conn.close(0u32.into(), b"bye");
        });
    }
}
