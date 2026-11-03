//! Bounded, independently read/written IPC. A dropped client never owns a PTY.
use fl_model::{IncarnationId, LogCursor, TerminalId};
use fl_prochost_proto::{self as wire, Request, Response};
use fl_proto::{Frame, FrameCodec, WireMessage};
use futures::{SinkExt, StreamExt};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::{codec::Framed, sync::CancellationToken};

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_INFLIGHT: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("process-host IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("process-host frame: {0}")]
    Frame(#[from] fl_proto::FrameError),
    #[error("process-host message: {0}")]
    Message(#[from] fl_proto::MsgError),
    #[error(transparent)]
    Remote(#[from] wire::Error),
    #[error("process-host connection closed: {0}")]
    Closed(Arc<str>),
    #[error("process-host request timed out")]
    Timeout,
    #[error("process-host request capacity exhausted")]
    Capacity,
    #[error("process-host protocol: {0}")]
    Protocol(&'static str),
}

type Reply = oneshot::Sender<Result<Response, ClientError>>;
#[derive(Default)]
struct State {
    pending: HashMap<u64, Reply>,
    closed: Option<Arc<str>>,
    next_id: u64,
}
struct Shared {
    state: Mutex<State>,
    cancel: CancellationToken,
}
impl Shared {
    fn fail(&self, reason: impl Into<Arc<str>>) {
        let (reason, pending) = {
            let mut state = self
                .state
                .lock()
                .expect("process-host pending lock poisoned");
            let reason = state.closed.get_or_insert_with(|| reason.into()).clone();
            (reason, std::mem::take(&mut state.pending))
        };
        self.cancel.cancel();
        for (_, reply) in pending {
            let _ = reply.send(Err(ClientError::Closed(reason.clone())));
        }
    }
}
struct Session {
    shared: Arc<Shared>,
    outgoing: mpsc::Sender<Frame>,
    tasks: Mutex<Option<[JoinHandle<()>; 2]>>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.shared.fail("last client dropped");
        if let Some(tasks) = self
            .tasks
            .get_mut()
            .expect("process-host task lock poisoned")
            .take()
        {
            for task in tasks {
                task.abort();
            }
        }
    }
}
struct Pending {
    shared: Arc<Shared>,
    id: u64,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.shared
            .state
            .lock()
            .expect("process-host pending lock poisoned")
            .pending
            .remove(&self.id);
    }
}

#[derive(Clone)]
pub struct Client {
    session: Arc<Session>,
}
impl Client {
    pub async fn connect(root: &Path) -> Result<Self, ClientError> {
        let framed = tokio::time::timeout(TIMEOUT, async {
            let socket = UnixStream::connect(root.join("run/prochost.sock")).await?;
            let mut framed = Framed::new(socket, FrameCodec);
            framed
                .send(
                    wire::ClientMsg::Hello {
                        version: wire::VERSION,
                    }
                    .to_frame()?,
                )
                .await?;
            let frame = framed
                .next()
                .await
                .ok_or(ClientError::Protocol("EOF before welcome"))??;
            match wire::ServerMsg::from_frame(&frame)? {
                wire::ServerMsg::Welcome { version, .. } if version == wire::VERSION => Ok(framed),
                wire::ServerMsg::Welcome { .. } => {
                    Err(ClientError::Protocol("unsupported process-host version"))
                }
                wire::ServerMsg::Rejected(error) => Err(ClientError::Remote(error)),
                _ => Err(ClientError::Protocol("expected process-host welcome")),
            }
        })
        .await
        .map_err(|_| ClientError::Timeout)??;
        let (mut writer, mut reader) = framed.split();
        let (outgoing, mut queue) = mpsc::channel::<Frame>(64);
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            cancel: CancellationToken::new(),
        });
        let write_state = shared.clone();
        let write_task = tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    _ = write_state.cancel.cancelled() => break,
                    frame = queue.recv() => match frame { Some(frame) => frame, None => break },
                };
                let sent = tokio::select! {
                    _ = write_state.cancel.cancelled() => break,
                    sent = writer.send(frame) => sent,
                };
                if let Err(error) = sent {
                    write_state.fail(error.to_string());
                    break;
                }
            }
        });
        let read_state = shared.clone();
        let read_task = tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    _ = read_state.cancel.cancelled() => break,
                    received = reader.next() => received,
                };
                let message = match received {
                    Some(Ok(frame)) => {
                        wire::ServerMsg::from_frame(&frame).map_err(|e| e.to_string())
                    }
                    Some(Err(error)) => Err(error.to_string()),
                    None => Err("EOF from process host".into()),
                };
                match message {
                    Ok(wire::ServerMsg::Response { id, result }) => {
                        let reply = read_state
                            .state
                            .lock()
                            .expect("process-host pending lock poisoned")
                            .pending
                            .remove(&id);
                        if let Some(reply) = reply {
                            let _ = reply.send(result.map_err(ClientError::Remote));
                        }
                    }
                    Ok(_) => {
                        read_state.fail("unexpected message after welcome");
                        break;
                    }
                    Err(reason) => {
                        read_state.fail(reason);
                        break;
                    }
                }
            }
        });
        Ok(Self {
            session: Arc::new(Session {
                shared,
                outgoing,
                tasks: Mutex::new(Some([write_task, read_task])),
            }),
        })
    }

    pub fn is_closed(&self) -> bool {
        self.session.shared.cancel.is_cancelled()
    }

    pub async fn request(&self, request: &Request) -> Result<Response, ClientError> {
        let expected = Expected::new(request);
        let (reply, result) = oneshot::channel();
        let id = {
            let mut state = self
                .session
                .shared
                .state
                .lock()
                .expect("process-host pending lock poisoned");
            if let Some(reason) = &state.closed {
                return Err(ClientError::Closed(reason.clone()));
            }
            if state.pending.len() >= MAX_INFLIGHT {
                return Err(ClientError::Capacity);
            }
            let id = state
                .next_id
                .checked_add(1)
                .ok_or(ClientError::Protocol("request sequence exhausted"))?;
            state.next_id = id;
            state.pending.insert(id, reply);
            id
        };
        let _pending = Pending {
            shared: self.session.shared.clone(),
            id,
        };
        let frame = wire::ClientMsg::Request {
            id,
            request: Cow::Borrowed(request),
        }
        .to_frame()?;
        let response = tokio::time::timeout(TIMEOUT, async {
            tokio::select! {
                _ = self.session.shared.cancel.cancelled() => {},
                sent = self.session.outgoing.send(frame) => {
                    if sent.is_err() { self.session.shared.fail("process-host writer stopped"); }
                },
            }
            result
                .await
                .map_err(|_| ClientError::Protocol("reply channel closed"))?
        })
        .await
        .map_err(|_| ClientError::Timeout)??;
        if let Err(error) = expected.check(&response) {
            self.session
                .shared
                .fail("response did not match its request");
            return Err(error);
        }
        Ok(response)
    }

    pub async fn shutdown(&self) {
        self.session.shared.fail("client shutdown");
        let tasks = self
            .session
            .tasks
            .lock()
            .expect("process-host task lock poisoned")
            .take();
        if let Some(tasks) = tasks {
            for task in tasks {
                let _ = task.await;
            }
        }
    }
}

enum Expected {
    Processes,
    Process(TerminalId, IncarnationId),
    Records(LogCursor),
    Written(usize),
    Cursor,
    Done,
}
impl Expected {
    fn new(request: &Request) -> Self {
        match request {
            Request::List => Self::Processes,
            Request::Spawn(spec) => Self::Process(spec.terminal, spec.incarnation),
            Request::Info {
                terminal,
                incarnation,
            } => Self::Process(*terminal, *incarnation),
            Request::Read { from, .. } => Self::Records(*from),
            Request::Write { bytes, .. } => Self::Written(bytes.len()),
            Request::Resize { .. } | Request::FlushSync { .. } => Self::Cursor,
            _ => Self::Done,
        }
    }
    fn check(&self, response: &Response) -> Result<(), ClientError> {
        let valid = match (self, response) {
            (Self::Processes, Response::Processes(_)) => true,
            (Self::Process(terminal, incarnation), Response::Process(info)) => {
                info.terminal == *terminal && info.incarnation == *incarnation
            }
            (Self::Records(from), Response::Records { records, next, .. }) => {
                records.first().map_or(*next, |record| record.at) == *from
            }
            (Self::Written(size), Response::Written { accepted, .. }) => {
                *accepted as usize <= *size
            }
            (Self::Cursor, Response::Cursor(_)) | (Self::Done, Response::Done) => true,
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(ClientError::Protocol(
                "incorrect response kind, identity or cursor",
            ))
        }
    }
}

/// Start the installed multi-call executable only when no process-host socket
/// answers. `setsid`, independent stdio and KillMode=process keep PTYs outside
/// the daemon's lifetime. Concurrent starters are serialized by the host lock.
pub async fn ensure_running(root: &Path, executable: &Path) -> anyhow::Result<Client> {
    use anyhow::Context;
    use std::fs::{OpenOptions, Permissions};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::process::Stdio;
    match Client::connect(root).await {
        Ok(client) => return Ok(client),
        Err(ClientError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(error.into()),
    }
    let logs = root.join("logs");
    std::fs::create_dir_all(&logs)?;
    std::fs::set_permissions(&logs, Permissions::from_mode(0o700))?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(logs.join("prochost.log"))
        .context("opening process-host log")?;
    log.set_permissions(Permissions::from_mode(0o600))?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("prochost")
        .env("FLOTILLA_HOME", root)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .kill_on_drop(false);
    // SAFETY: pre_exec performs only the async-signal-safe setsid syscall.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("launching independent process host")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut exited = None;
    loop {
        match tokio::time::timeout_at(deadline, Client::connect(root)).await {
            Ok(Ok(client)) => {
                if exited.is_none() {
                    tokio::spawn(async move {
                        let _ = child.wait().await;
                    });
                }
                return Ok(client);
            }
            Ok(Err(error)) => {
                if exited.is_none() {
                    exited = child.try_wait()?;
                }
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "process host did not become ready (child: {exited:?}): {error}; see {}",
                        logs.join("prochost.log").display()
                    );
                }
            }
            Err(_) => anyhow::bail!(
                "process-host startup timed out; see {}",
                logs.join("prochost.log").display()
            ),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
