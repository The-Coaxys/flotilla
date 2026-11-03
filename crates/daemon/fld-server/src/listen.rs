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
            serve_quic_connection(ctx, conn).await;
        });
    }
}

/// Every bidirectional stream is a logical stream: a control session or a
/// ticket-bound channel. The connection lives while any stream is active.
async fn serve_quic_connection(ctx: Arc<SessionCtx>, conn: quinn::Connection) {
    // TLS is complete. Opening the first stream and authenticating its first
    // message must consume the same budget.
    let hello_deadline = tokio::time::Instant::now() + ctx.hello_timeout;
    let first = tokio::select! {
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
    let mut streams = tokio::task::JoinSet::new();
    let spawn_stream = |streams: &mut tokio::task::JoinSet<_>,
                        (send, recv): (quinn::SendStream, quinn::RecvStream),
                        deadline: tokio::time::Instant| {
        let ctx = ctx.clone();
        streams.spawn(async move {
            let (reader, writer) = framed(recv, send);
            (
                serve_connection(ctx, reader, writer, PeerKind::Quic, deadline).await,
                deadline,
            )
        });
    };
    spawn_stream(&mut streams, first, hello_deadline);
    let mut authenticated = false;
    let mut idle_deadline = None;
    loop {
        let idle = async {
            match idle_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = ctx.shutdown.cancelled() => {
                conn.close(0u32.into(), b"daemon shutting down");
                return;
            }
            settled = streams.join_next(), if !streams.is_empty() => {
                match settled {
                    Some(Ok((Ok(SessionEnd::HelloTimeout), deadline))) if !authenticated => {
                        if streams.is_empty() {
                            // Do not extend the authentication budget beyond
                            // a brief rejection grace.
                            tokio::select! {
                                _ = tokio::time::sleep_until(deadline + HELLO_TIMEOUT_GRACE) => {}
                                _ = conn.closed() => {}
                            }
                            conn.close(0u32.into(), b"hello timeout");
                            return;
                        }
                    }
                    Some(Ok((Ok(SessionEnd::Finished), _))) => authenticated = true,
                    Some(Ok((Err(e), _))) => tracing::debug!("quic stream ended: {e}"),
                    _ => {}
                }
                if streams.is_empty() {
                    // Let the client read our last frame and close first.
                    idle_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                }
            }
            _ = idle => break,
            _ = conn.closed() => break,
            accepted = accept_ctl(&conn) => match accepted {
                Ok(stream) => {
                    idle_deadline = None;
                    spawn_stream(&mut streams, stream, tokio::time::Instant::now() + ctx.hello_timeout);
                }
                Err(_) => break,
            },
        }
    }
    streams.abort_all();
    conn.close(0u32.into(), b"bye");
}
