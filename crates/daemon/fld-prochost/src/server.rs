use crate::process::Manager;
use anyhow::{Context, Result, bail};
use fl_model::IncarnationId;
use fl_prochost_proto::{self as wire, ErrorCode};
use fl_proto::{Frame, FrameCodec, WireMessage};
use futures::{SinkExt, StreamExt};
use std::fs::{File, OpenOptions, Permissions};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::{codec::Framed, sync::CancellationToken};

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 64;
const MAX_REQUESTS: usize = 256;

struct SocketFiles {
    socket: PathBuf,
    info: PathBuf,
}
impl Drop for SocketFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.info);
    }
}

/// Runs only in the detached `flotillad prochost` process. Its lifetime is not
/// tied to a daemon RPC connection. Idle shutdown is explicit and race-checked.
pub async fn serve(root: PathBuf) -> Result<()> {
    let run = root.join("run");
    private_dir(&run)?;
    private_dir(&root.join("state/prochost"))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(run.join("prochost.lock"))
        .context("opening process-host ownership lock")?;
    lock.set_permissions(Permissions::from_mode(0o600))?;
    lock.try_lock()
        .context("another process host holds the ownership lock")?;
    let socket = run.join("prochost.sock");
    match std::fs::symlink_metadata(&socket) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                bail!("refusing to replace non-socket {}", socket.display());
            }
            if UnixStream::connect(&socket).await.is_ok() {
                bail!("process-host socket is already active");
            }
            std::fs::remove_file(&socket).context("removing stale process-host socket")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let manager_root = root.clone();
    let manager = tokio::task::spawn_blocking(move || Manager::open(&manager_root))
        .await
        .context("joining process-host recovery")??;
    let listener = UnixListener::bind(&socket).context("binding process-host socket")?;
    let files = SocketFiles {
        socket,
        info: run.join("prochost.json"),
    };
    std::fs::set_permissions(&files.socket, Permissions::from_mode(0o600))?;
    let instance = IncarnationId::new();
    let info = serde_json::to_vec(
        &serde_json::json!({"pid":std::process::id(),"protocol":wire::VERSION,"instance":instance}),
    )?;
    let temporary = run.join("prochost.json.new");
    {
        use std::io::Write;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&info)?;
        file.sync_all()?;
    }
    std::fs::rename(temporary, &files.info)?;
    File::open(&run)?.sync_all()?;
    let shutdown = manager.shutdown_token();
    let mut connections = JoinSet::new();
    tracing::info!(socket = %files.socket.display(), "process host ready");
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            settled = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = settled { tracing::warn!(%error, "process-host connection task failed"); }
            },
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    if connections.len() >= MAX_CONNECTIONS { drop(stream); continue; }
                    // SAFETY: geteuid has no pointer arguments and cannot mutate Rust state.
                    let uid = unsafe { libc::geteuid() };
                    match stream.peer_cred() {
                        Ok(cred) if cred.uid() == uid => {},
                        Ok(_) => { drop(stream); continue; },
                        Err(error) => {
                            tracing::debug!(%error, "rejecting process-host peer without credentials");
                            drop(stream);
                            continue;
                        },
                    }
                    let manager = manager.clone();
                    let shutdown = shutdown.clone();
                    connections.spawn(async move {
                        if let Err(error) = connection(stream, manager, instance, shutdown).await {
                            tracing::debug!(%error, "process-host connection ended");
                        }
                    });
                },
                Err(error) => {
                    tracing::warn!(%error, "process-host accept failed; preserving owned PTYs");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                },
            },
        }
    }
    // An accepted idle shutdown cancels the manager first. Give its response
    // and other already-admitted replies time to flush before closing sockets.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !connections.is_empty() {
        if tokio::time::timeout_at(deadline, connections.join_next())
            .await
            .is_err()
        {
            break;
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    drop(files);
    drop(lock);
    Ok(())
}

fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, Permissions::from_mode(0o700))?;
    Ok(())
}

async fn connection(
    stream: UnixStream,
    manager: Arc<Manager>,
    instance: IncarnationId,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut framed = Framed::new(stream, FrameCodec);
    let first = tokio::time::timeout(HELLO_TIMEOUT, framed.next())
        .await
        .context("process-host hello timeout")?
        .context("EOF before process-host hello")??;
    let rejection = match wire::ClientMsg::from_frame(&first)? {
        wire::ClientMsg::Hello { version } if version == wire::VERSION => None,
        wire::ClientMsg::Hello { version } => Some(wire::Error::new(
            ErrorCode::UnsupportedVersion,
            format!(
                "process-host protocol {version} is unsupported; expected {}",
                wire::VERSION
            ),
        )),
        _ => Some(wire::Error::new(
            ErrorCode::ExpectedHello,
            "process-host hello must be first",
        )),
    };
    if let Some(error) = rejection {
        tokio::time::timeout(
            Duration::from_millis(100),
            framed.send(wire::ServerMsg::Rejected(error).to_frame()?),
        )
        .await??;
        return Ok(());
    }
    framed
        .send(
            wire::ServerMsg::Welcome {
                version: wire::VERSION,
                pid: std::process::id(),
                instance,
            }
            .to_frame()?,
        )
        .await?;
    let (mut sink, mut source) = framed.split();
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let (send, mut outgoing) = mpsc::channel::<Frame>(32);
    let writer_cancel = cancel.clone();
    let writer = tokio::spawn(async move {
        loop {
            let frame = tokio::select! {
                _ = writer_cancel.cancelled() => break,
                frame = outgoing.recv() => match frame { Some(frame) => frame, None => break },
            };
            tokio::select! {
                _ = writer_cancel.cancelled() => break,
                result = sink.send(frame) => if result.is_err() { break; },
            }
        }
        writer_cancel.cancel();
    });
    let mut requests = JoinSet::new();
    let mut graceful = false;
    let result: Result<()> = async {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => { graceful = true; break; },
                _ = cancel.cancelled() => break,
                settled = requests.join_next(), if !requests.is_empty() => {
                    if let Some(Err(error)) = settled { tracing::warn!(%error, "process-host request task failed"); }
                },
                next = source.next() => {
                    let Some(frame) = next else { break };
                    let wire::ClientMsg::Request { id, request } = wire::ClientMsg::from_frame(&frame?)? else {
                        bail!("unexpected repeated process-host hello");
                    };
                    if requests.len() >= MAX_REQUESTS {
                        let message = wire::ServerMsg::Response { id, result: Err(wire::Error::new(ErrorCode::Backpressured, "process-host request capacity exhausted")) };
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            sent = send.send(message.to_frame()?) => if sent.is_err() { break; },
                        }
                        continue;
                    }
                    let request = request.into_owned();
                    let manager = manager.clone();
                    let send = send.clone();
                    requests.spawn(async move {
                        let result = manager.dispatch(request).await;
                        match (wire::ServerMsg::Response { id, result }).to_frame() {
                            Ok(frame) => { let _ = send.send(frame).await; },
                            Err(error) => {
                                let failed = wire::ServerMsg::Response { id, result: Err(wire::Error::new(ErrorCode::Internal, format!("encoding process-host response: {error}"))) };
                                if let Ok(frame) = failed.to_frame() { let _ = send.send(frame).await; }
                            },
                        }
                    });
                },
            }
        }
        Ok(())
    }.await;
    if graceful && result.is_ok() {
        while requests.join_next().await.is_some() {}
        drop(send);
    } else {
        cancel.cancel();
        requests.abort_all();
        while requests.join_next().await.is_some() {}
        drop(send);
    }
    let _ = writer.await;
    result
}
