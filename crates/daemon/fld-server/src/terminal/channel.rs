//! One authenticated terminal channel (hot or background role): subscriptions,
//! parking, input with deadlines, input-owner leases and owner-gated resize.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fl_model::{IncarnationId, InputId, TerminalId, ViewId};
use fl_prochost_proto::{Request, Response, WriteId};
use fl_proto::{InputAck, TerminalClientMsg, TerminalError, TerminalServerMsg};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter};
use fld_store::TerminalKey;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::TerminalService;
use super::prochost::ProchostError;
use super::sink::Sink;
use super::worker::{Command, Terminal};
use crate::channels::ChannelLease;

/// Clients derive deadlines from the advertised boot clock plus at most this TTL.
pub(crate) const MAX_INPUT_TTL_NS: u64 = 5_000_000_000;
const INPUT_QUEUE: usize = 32;

static NEXT_CHANNEL: AtomicU64 = AtomicU64::new(1);

struct InputJob {
    id: u64,
    input: InputId,
    view: ViewId,
    bytes: Vec<u8>,
    deadline_ns: u64,
}

pub(crate) async fn serve(
    service: Arc<TerminalService>,
    lease: ChannelLease,
    mut reader: FrameReader,
    writer: FrameWriter,
) -> Result<(), ConnError> {
    let channel = NEXT_CHANNEL.fetch_add(1, Ordering::Relaxed);
    let cancel = lease.cancel.clone();
    let (sink, writer_task) =
        Sink::new(channel, lease.session, lease.client, writer, cancel.clone());
    let mut session = Session {
        service,
        sink,
        channel,
        cancel: cancel.clone(),
        inputs: HashMap::new(),
        attached: HashSet::new(),
    };
    let result = loop {
        let message = tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            message = reader.recv::<TerminalClientMsg>() => message,
        };
        match message {
            Ok(message) => session.handle(message).await,
            Err(ConnError::Closed) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    for terminal in session.attached.drain() {
        let _ = terminal.command(Command::Detach { channel });
    }
    drop(session);
    drop(lease);
    match writer_task.await {
        Ok(Err(error)) if result.is_ok() => Err(error),
        _ => result,
    }
}

struct AttachedTerminal(Arc<Terminal>);

impl std::hash::Hash for AttachedTerminal {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.key.terminal.hash(state);
        self.0.key.incarnation.hash(state);
    }
}
impl PartialEq for AttachedTerminal {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for AttachedTerminal {}
impl std::ops::Deref for AttachedTerminal {
    type Target = Terminal;
    fn deref(&self) -> &Terminal {
        &self.0
    }
}

struct Session {
    service: Arc<TerminalService>,
    sink: Sink,
    channel: u64,
    cancel: CancellationToken,
    inputs: HashMap<(TerminalId, IncarnationId), mpsc::Sender<InputJob>>,
    attached: HashSet<AttachedTerminal>,
}

impl Session {
    async fn reply_error(
        &self,
        id: u64,
        terminal: TerminalId,
        incarnation: IncarnationId,
        error: TerminalError,
    ) {
        self.sink
            .send(TerminalServerMsg::Error {
                id: Some(id),
                terminal,
                incarnation,
                error,
            })
            .await;
    }

    async fn handle(&mut self, message: TerminalClientMsg) {
        match message {
            TerminalClientMsg::Subscribe {
                id,
                subscription,
                terminal,
                incarnation,
                from,
            } => {
                let result = self
                    .service
                    .lookup(terminal, incarnation)
                    .and_then(|found| {
                        found.command(Command::Subscribe {
                            id,
                            subscription,
                            from,
                            sink: self.sink.clone(),
                        })?;
                        Ok(found)
                    });
                match result {
                    Ok(found) => {
                        self.attached.insert(AttachedTerminal(found));
                    }
                    Err(error) => self.reply_error(id, terminal, incarnation, error).await,
                }
            }
            TerminalClientMsg::Unsubscribe {
                id,
                subscription,
                terminal,
                incarnation,
            } => {
                let result = self
                    .service
                    .lookup(terminal, incarnation)
                    .and_then(|found| {
                        found.command(Command::Unsubscribe {
                            id,
                            subscription,
                            sink: self.sink.clone(),
                        })
                    });
                if let Err(error) = result {
                    self.reply_error(id, terminal, incarnation, error).await;
                }
            }
            TerminalClientMsg::SetParked {
                id,
                subscription,
                terminal,
                incarnation,
                parked,
            } => {
                let result = self
                    .service
                    .lookup(terminal, incarnation)
                    .and_then(|found| {
                        found.command(Command::SetParked {
                            id,
                            subscription,
                            parked,
                            sink: self.sink.clone(),
                        })
                    });
                if let Err(error) = result {
                    self.reply_error(id, terminal, incarnation, error).await;
                }
            }
            TerminalClientMsg::Input {
                id,
                terminal,
                incarnation,
                input,
                view,
                bytes,
                deadline_ns,
            } => {
                if let Err(error) =
                    self.admit_input(id, terminal, incarnation, input, view, bytes, deadline_ns)
                {
                    self.reply_error(id, terminal, incarnation, error).await;
                }
            }
            TerminalClientMsg::ClaimOwner {
                id,
                terminal,
                incarnation,
                view,
            } => {
                let found = match self.service.lookup(terminal, incarnation) {
                    Ok(found) => found,
                    Err(error) => return self.reply_error(id, terminal, incarnation, error).await,
                };
                let (owner, changed) = found.claim(self.sink.client, view);
                self.sink
                    .send(TerminalServerMsg::OwnerChanged {
                        id: Some(id),
                        terminal,
                        incarnation,
                        owner: Some(owner),
                    })
                    .await;
                if changed {
                    found.broadcast_owner(Some(self.channel), Some(owner));
                }
            }
            TerminalClientMsg::Resize {
                id,
                terminal,
                incarnation,
                owner,
                size,
            } => {
                let found = match self.service.lookup(terminal, incarnation) {
                    Ok(found) => found,
                    Err(error) => return self.reply_error(id, terminal, incarnation, error).await,
                };
                let authorized = if owner.client == self.sink.client {
                    found.check_owner(owner)
                } else {
                    Err(found.owner())
                };
                if let Err(current) = authorized {
                    let error = TerminalError::NotOwner { owner: current };
                    return self.reply_error(id, terminal, incarnation, error).await;
                }
                let request = Request::Resize {
                    terminal,
                    incarnation,
                    size,
                };
                match self.service.shared.prochost.request(&request).await {
                    Ok(Response::Cursor(cursor)) => {
                        self.sink
                            .send(TerminalServerMsg::Resized {
                                id,
                                terminal,
                                incarnation,
                                size,
                                cursor,
                            })
                            .await;
                    }
                    Ok(_) => {
                        let error = TerminalError::Internal {
                            message: "unexpected process-host resize response".into(),
                        };
                        self.reply_error(id, terminal, incarnation, error).await;
                    }
                    Err(error) => {
                        self.reply_error(id, terminal, incarnation, terminal_error(error))
                            .await
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn admit_input(
        &mut self,
        id: u64,
        terminal: TerminalId,
        incarnation: IncarnationId,
        input: InputId,
        view: ViewId,
        bytes: Vec<u8>,
        deadline_ns: u64,
    ) -> Result<(), TerminalError> {
        let found = self.service.lookup(terminal, incarnation)?;
        if input.client != self.sink.client {
            return Err(TerminalError::InvalidArgument {
                message: "input identity belongs to another client session".into(),
            });
        }
        let now = fld_prochost::clock_ns().map_err(|error| TerminalError::Internal {
            message: format!("reading admission clock: {error}"),
        })?;
        if deadline_ns > now.saturating_add(MAX_INPUT_TTL_NS) {
            return Err(TerminalError::InvalidArgument {
                message: "input deadline exceeds the five-second boot-clock limit".into(),
            });
        }
        let key = (terminal, incarnation);
        let queue = self.inputs.entry(key).or_insert_with(|| {
            let (queue, jobs) = mpsc::channel(INPUT_QUEUE);
            tokio::spawn(input_loop(
                self.service.clone(),
                found.clone(),
                self.sink.clone(),
                jobs,
                self.cancel.clone(),
            ));
            queue
        });
        queue
            .try_send(InputJob {
                id,
                input,
                view,
                bytes,
                deadline_ns,
            })
            .map_err(|_| TerminalError::Backpressured)
    }
}

/// Writes one client's input to one terminal in order. Partial PTY writes are
/// retried until fully committed or the original deadline expires.
async fn input_loop(
    service: Arc<TerminalService>,
    terminal: Arc<Terminal>,
    sink: Sink,
    mut jobs: mpsc::Receiver<InputJob>,
    cancel: CancellationToken,
) {
    let TerminalKey {
        terminal: terminal_id,
        incarnation,
    } = terminal.key;
    loop {
        let job = tokio::select! {
            _ = cancel.cancelled() => return,
            job = jobs.recv() => job,
        };
        let Some(job) = job else { return };
        let (owner, changed) = terminal.claim(sink.client, job.view);
        if changed {
            terminal.broadcast_owner(None, Some(owner));
        }
        let length = job.bytes.len();
        let request = Request::Write {
            terminal: terminal_id,
            incarnation,
            id: WriteId::Client(job.input),
            bytes: job.bytes,
            deadline_ns: Some(job.deadline_ns),
        };
        let mut delay = Duration::from_millis(1);
        let reply = loop {
            let response = tokio::select! {
                _ = cancel.cancelled() => return,
                response = service.shared.prochost.request(&request) => response,
            };
            match response {
                Ok(Response::Written {
                    accepted,
                    echo,
                    at,
                    expired,
                }) => {
                    if accepted as usize == length || expired {
                        break TerminalServerMsg::InputAck(InputAck {
                            id: job.id,
                            terminal: terminal_id,
                            incarnation,
                            input: job.input,
                            accepted_bytes: accepted,
                            echo,
                            cursor: at,
                            expired,
                        });
                    }
                }
                Ok(_) => {
                    break error_message(
                        job.id,
                        terminal.key,
                        TerminalError::Internal {
                            message: "unexpected process-host write response".into(),
                        },
                    );
                }
                Err(ProchostError::Unavailable(message))
                    if fld_prochost::clock_ns().is_ok_and(|now| now < job.deadline_ns) =>
                {
                    tracing::debug!("input delivery waiting for process host: {message}");
                }
                Err(error) => break error_message(job.id, terminal.key, terminal_error(error)),
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {},
            }
            delay = (delay * 2).min(Duration::from_millis(20));
        };
        if !sink.send(reply).await {
            return;
        }
    }
}

fn error_message(id: u64, key: TerminalKey, error: TerminalError) -> TerminalServerMsg {
    TerminalServerMsg::Error {
        id: Some(id),
        terminal: key.terminal,
        incarnation: key.incarnation,
        error,
    }
}

pub(crate) fn terminal_error(error: ProchostError) -> TerminalError {
    use fl_prochost_proto::ErrorCode;
    match error {
        ProchostError::Remote(error) => match error.code {
            ErrorCode::NotFound | ErrorCode::WrongIncarnation => TerminalError::NotFound,
            ErrorCode::Backpressured => TerminalError::Backpressured,
            ErrorCode::NotStopped => TerminalError::InvalidArgument {
                message: "the terminal process has exited and accepts no input or resize".into(),
            },
            ErrorCode::InputConflict | ErrorCode::InvalidArgument => {
                TerminalError::InvalidArgument {
                    message: super::bounded(error.message),
                }
            }
            _ => TerminalError::Internal {
                message: super::bounded(error.message),
            },
        },
        ProchostError::Unavailable(message) => TerminalError::Internal {
            message: super::bounded(message),
        },
    }
}
