//! PTYs belong to a dedicated OS-I/O actor, never to an RPC future.
//!
//! Limits: 128 retained terminals, 64 live PTYs, 128 queued RPCs, 256 client
//! writer identities per incarnation, and one 64-KiB output chunk plus at most
//! two ordered controls awaiting journal durability. No actor operation runs on
//! a Tokio worker. Dropping the manager's last sender leaves live PTYs running.
use crate::journal::{Journal, JournalError};
use anyhow::{Context, bail};
use bytes::BytesMut;
use fl_model::{
    ClientSessionId, ExitStatus, IncarnationId, LogCursor, ProcessInfo, ProcessState, Size,
    TerminalId, TerminalRecordKind,
};
use fl_prochost_proto::{
    Error, ErrorCode, MAX_OUTPUT_BYTES, Request, Response, SpawnSpec, WriteId,
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

const MAX_PROCESSES: usize = 128;
const MAX_ACTIVE: usize = 64;
const COMMAND_CAPACITY: usize = 128;
const MAX_WRITERS: usize = 256;
const MAX_METADATA_BYTES: u64 = 512 * 1024;
const BOUNDARY_READS: usize = 16;
const RETRY: Duration = Duration::from_millis(250);

pub struct Manager {
    commands: mpsc::SyncSender<Command>,
    changed: Arc<Notify>,
    shutdown: CancellationToken,
    wake: UnixStream,
}

struct Command {
    request: Request,
    reply: oneshot::Sender<Result<Response, Error>>,
}

struct Host {
    root: PathBuf,
    entries: BTreeMap<TerminalId, Entry>,
    changed: Arc<Notify>,
    shutdown: CancellationToken,
    closing: bool,
    // An actor outliving its last RPC sender must retain the singleton lock.
    wake: UnixStream,
    _lock: File,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u32,
    info: ProcessInfo,
    quota_bytes: u64,
}

#[derive(Serialize)]
struct MetadataRef<'a> {
    version: u32,
    info: &'a ProcessInfo,
    quota_bytes: u64,
}

struct Entry {
    dir: PathBuf,
    info: ProcessInfo,
    journal: Journal,
    native: Option<Native>,
    pending: VecDeque<TerminalRecordKind>,
    exit: Option<ExitStatus>,
    eof: bool,
    blocked: Option<Error>,
    retry_at: Instant,
    metadata_dirty: bool,
    clients: BTreeMap<ClientSessionId, WriteProgress>,
    effect: Option<WriteProgress>,
}

struct Native {
    master: Box<dyn MasterPty + Send>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    read_buffer: BytesMut,
    child: Box<dyn Child + Send + Sync>,
    fd: RawFd,
    leader: ProcessIdentity,
    pidfd: OwnedFd,
    session_members: BTreeMap<i32, ProcessIdentity>,
    termination: Option<Termination>,
}

#[derive(Clone, Copy)]
struct ProcessIdentity {
    pid: i32,
    session: i32,
    started: u64,
    state: u8,
}

struct Termination {
    deadline: Instant,
    last_signal: Instant,
    forced: bool,
    stopping_since: Option<Instant>,
    stopped_snapshot: bool,
    members: BTreeMap<i32, ProcessIdentity>,
}

struct WriteProgress {
    sequence: u64,
    digest: [u8; 32],
    length: usize,
    accepted: usize,
    deadline_ns: Option<u64>,
    expired: bool,
}

impl Manager {
    /// Recover journal metadata synchronously at startup. A previous prochost's
    /// PTYs cannot be resurrected; their retained logs remain readable as Lost.
    pub fn open(root: &Path) -> anyhow::Result<Arc<Self>> {
        if !cfg!(target_os = "linux") {
            bail!("persistent PTY session ownership requires Linux /proc and pidfds");
        }
        let root = root.join("state").join("prochost");
        private_dir(&root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join("manager.lock"))?;
        // SAFETY: lock is an owned live descriptor. flock is nonblocking.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error()).context("another PTY manager owns this root");
        }
        let changed = Arc::new(Notify::new());
        let shutdown = CancellationToken::new();
        let (wake, actor_wake) = UnixStream::pair().context("create PTY actor wakeup")?;
        wake.set_nonblocking(true)?;
        actor_wake.set_nonblocking(true)?;
        let mut host = Host {
            root,
            entries: BTreeMap::new(),
            changed: changed.clone(),
            shutdown: shutdown.clone(),
            closing: false,
            _lock: lock,
            wake: actor_wake,
        };
        host.recover()?;
        let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        std::thread::Builder::new()
            .name("flotilla-pty-io".into())
            .spawn(move || host.run(receiver))
            .context("start persistent PTY I/O actor")?;
        Ok(Arc::new(Self {
            commands,
            changed,
            shutdown,
            wake,
        }))
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub async fn dispatch(self: &Arc<Self>, request: Request) -> Result<Response, Error> {
        request.validate()?;
        if let Request::Spawn(spec) = &request {
            validate_env(spec)?;
        }
        if let Request::Read {
            terminal,
            incarnation,
            from,
            max_bytes,
            wait_ms,
        } = request
        {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms.into());
            loop {
                // Register before the read, so an append between read/reply and
                // sleeping cannot strand the long poll until its deadline.
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let response = self
                    .call(Request::Read {
                        terminal,
                        incarnation,
                        from,
                        max_bytes,
                        wait_ms: 0,
                    })
                    .await?;
                let wait = matches!(
                    &response,
                    Response::Records { records, state, .. }
                        if records.is_empty()
                            && !matches!(state, ProcessState::Exited(_) | ProcessState::Lost)
                );
                if !wait || wait_ms == 0 || tokio::time::Instant::now() >= deadline {
                    return Ok(response);
                }
                tokio::select! {
                    _ = &mut changed => {},
                    _ = tokio::time::sleep_until(deadline) => return Ok(response),
                }
            }
        } else {
            self.call(request).await
        }
    }

    async fn call(&self, request: Request) -> Result<Response, Error> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(Command { request, reply })
            .map_err(|err| match err {
                mpsc::TrySendError::Full(_) => capacity("PTY command queue is full (128 RPCs)"),
                mpsc::TrySendError::Disconnected(_) => internal("PTY manager has stopped"),
            })?;
        wake_actor(&self.wake);
        receiver
            .await
            .map_err(|_| internal("PTY manager stopped before returning the operation result"))?
    }
}

impl Host {
    fn recover(&mut self) -> anyhow::Result<()> {
        for terminal in fs::read_dir(&self.root)? {
            let terminal = terminal?;
            let Some(name) = terminal.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(terminal_id) = name.parse::<TerminalId>() else {
                continue;
            };
            require_directory(&terminal.path())?;
            for incarnation in fs::read_dir(terminal.path())? {
                let incarnation = incarnation?;
                let Some(name) = incarnation.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Ok(incarnation_id) = name.parse::<IncarnationId>() else {
                    continue;
                };
                require_directory(&incarnation.path())?;
                let path = incarnation.path().join("process.json");
                if !path.try_exists()? {
                    // A spawn failed before metadata commit; no PTY was exposed.
                    continue;
                }
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&path)?;
                if file.metadata()?.len() > MAX_METADATA_BYTES {
                    bail!("oversized process metadata: {}", path.display());
                }
                let mut metadata: Metadata = serde_json::from_reader(file)
                    .with_context(|| format!("decode {}", path.display()))?;
                if metadata.version != 1
                    || metadata.info.terminal != terminal_id
                    || metadata.info.incarnation != incarnation_id
                    || metadata.info.pid == 0
                {
                    bail!("invalid process metadata identity: {}", path.display());
                }
                metadata.info.size.validate()?;
                fl_proto::validation::launch(&metadata.info.argv, &metadata.info.cwd)?;
                if self.entries.len() == MAX_PROCESSES {
                    bail!("more than 128 retained terminals; explicit cleanup is required");
                }
                if self.entries.contains_key(&terminal_id) {
                    bail!("multiple retained incarnations for terminal {terminal_id}");
                }
                let journal =
                    Journal::open(&incarnation.path().join("journal"), metadata.quota_bytes)?;
                metadata.info.state = ProcessState::Lost;
                metadata.info.tail = journal.tail();
                let mut entry = Entry {
                    dir: incarnation.path(),
                    info: metadata.info,
                    journal,
                    native: None,
                    pending: VecDeque::with_capacity(3),
                    exit: None,
                    eof: true,
                    blocked: None,
                    retry_at: Instant::now(),
                    metadata_dirty: true,
                    clients: BTreeMap::new(),
                    effect: None,
                };
                entry.persist()?;
                self.entries.insert(terminal_id, entry);
            }
        }
        Ok(())
    }

    fn run(mut self, receiver: mpsc::Receiver<Command>) {
        let mut connected = true;
        let mut descriptors = Vec::with_capacity(MAX_ACTIVE * 2 + 1);
        loop {
            if connected {
                let mut bytes = [0u8; COMMAND_CAPACITY];
                while unsafe {
                    libc::read(
                        self.wake.as_raw_fd(),
                        bytes.as_mut_ptr().cast(),
                        bytes.len(),
                    )
                } > 0
                {}
                for _ in 0..COMMAND_CAPACITY {
                    match receiver.try_recv() {
                        Ok(command) => self.command(command),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            connected = false;
                            break;
                        }
                    }
                }
            }
            if self.closing {
                self.shutdown.cancel();
                break;
            }
            let mut changed = false;
            for entry in self.entries.values_mut() {
                changed |= entry.tick();
            }
            if changed {
                self.changed.notify_waiters();
            }
            if !connected && !self.entries.values().any(|entry| entry.native.is_some()) {
                break;
            }
            descriptors.clear();
            if connected {
                descriptors.push(libc::pollfd {
                    fd: self.wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            let now = Instant::now();
            let mut wake_at: Option<Instant> = None;
            for entry in self.entries.values() {
                if entry.blocked.is_some() {
                    wake_at = Some(wake_at.map_or(entry.retry_at, |old| old.min(entry.retry_at)));
                }
                if let Some(native) = &entry.native {
                    if entry.blocked.is_none() && !entry.eof {
                        descriptors.push(libc::pollfd {
                            fd: native.fd,
                            events: libc::POLLIN,
                            revents: 0,
                        });
                    }
                    if entry.exit.is_none() && entry.blocked.is_none() {
                        descriptors.push(libc::pollfd {
                            fd: native.pidfd.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        });
                    }
                    if let Some(termination) = &native.termination
                        && !termination.forced
                    {
                        let due = termination
                            .deadline
                            .max(termination.last_signal + Duration::from_millis(50));
                        wake_at = Some(wake_at.map_or(due, |old| old.min(due)));
                    }
                }
            }
            let timeout = wake_at.map_or(-1, |due| {
                due.saturating_duration_since(now)
                    .as_millis()
                    .saturating_add(1)
                    .min(i32::MAX as u128) as i32
            });
            // Block on command wakeup, PTY readability, pidfd exit, or an actual
            // retry/escalation deadline. A silent shell incurs no periodic scan.
            if unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    timeout,
                )
            } < 0
            {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    tracing::error!(error = %err, "PTY actor poll failed; retaining owned PTYs");
                    for entry in self
                        .entries
                        .values_mut()
                        .filter(|entry| entry.native.is_some())
                    {
                        entry.mark_blocked(internal(format!("poll PTY actor descriptors: {err}")));
                    }
                    self.changed.notify_waiters();
                    std::thread::sleep(RETRY);
                }
            }
        }
    }

    fn command(&mut self, command: Command) {
        let notify = !matches!(
            &command.request,
            Request::List | Request::Info { .. } | Request::Read { .. }
        );
        let result = self.handle(command.request);
        let _ = command.reply.send(result);
        if notify {
            self.changed.notify_waiters();
        }
    }

    fn handle(&mut self, request: Request) -> Result<Response, Error> {
        match request {
            Request::List => Ok(Response::Processes(
                self.entries
                    .values()
                    .map(|entry| entry.info.clone())
                    .collect(),
            )),
            Request::Spawn(spec) => self.spawn(spec),
            Request::ShutdownIfIdle => {
                if self.entries.values().any(|entry| entry.native.is_some()) {
                    return Err(Error::new(
                        ErrorCode::NotStopped,
                        "PTYs are still running or draining",
                    ));
                }
                self.closing = true;
                Ok(Response::Done)
            }
            Request::Forget {
                terminal,
                incarnation,
            } => {
                let entry = self.entry(terminal, incarnation)?;
                if !matches!(
                    entry.info.state,
                    ProcessState::Exited(_) | ProcessState::Lost
                ) {
                    return Err(Error::new(
                        ErrorCode::NotStopped,
                        "wait for durable final output and Exit before forgetting",
                    ));
                }
                // Rename outside the UUID namespace first. A crash during recursive
                // deletion cannot resurrect a partly deleted terminal on startup.
                let dir = entry.dir.clone();
                let tombstone = self.root.join(format!(".forget-{terminal}-{incarnation}"));
                fs::rename(&dir, &tombstone).map_err(|e| os("retire terminal directory", e))?;
                sync_dir(dir.parent().expect("incarnation directory has parent"))
                    .map_err(|e| os("sync retired terminal parent", e))?;
                sync_dir(&self.root).map_err(|e| os("sync terminal retirement", e))?;
                self.entries.remove(&terminal);
                fs::remove_dir_all(&tombstone).map_err(|e| os("remove retired terminal", e))?;
                sync_dir(&self.root).map_err(|e| os("sync terminal removal", e))?;
                Ok(Response::Done)
            }
            Request::Info {
                terminal,
                incarnation,
            } => Ok(Response::Process(
                self.entry(terminal, incarnation)?.info.clone(),
            )),
            Request::Read {
                terminal,
                incarnation,
                from,
                max_bytes,
                ..
            } => {
                let entry = self.entry(terminal, incarnation)?;
                let records = entry
                    .journal
                    .read(from, max_bytes as usize)
                    .map_err(journal_error)?;
                let next = records
                    .last()
                    .and_then(|record| record.end())
                    .unwrap_or(from);
                Ok(Response::Records {
                    records,
                    next,
                    tail: entry.journal.tail(),
                    state: entry.info.state.clone(),
                })
            }
            Request::Write {
                terminal,
                incarnation,
                id,
                bytes,
                deadline_ns,
            } => self
                .entry(terminal, incarnation)?
                .write(id, &bytes, deadline_ns),
            Request::Resize {
                terminal,
                incarnation,
                size,
            } => {
                let entry = self.entry(terminal, incarnation)?;
                entry.boundary()?;
                entry
                    .native
                    .as_ref()
                    .ok_or_else(stopped)?
                    .master
                    .resize(pty_size(size))
                    .map_err(|e| internal(format!("resize native PTY: {e:#}")))?;
                entry.info.size = size;
                entry.metadata_dirty = true;
                entry.pending.push_back(TerminalRecordKind::Resize(size));
                entry.commit_pending()?;
                Ok(Response::Cursor(entry.journal.tail()))
            }
            Request::FlushSync {
                terminal,
                incarnation,
            } => {
                let entry = self.entry(terminal, incarnation)?;
                entry.boundary()?;
                entry.pending.push_back(TerminalRecordKind::SyncFlush);
                entry.commit_pending()?;
                Ok(Response::Cursor(entry.journal.tail()))
            }
            Request::AckCheckpoint {
                terminal,
                incarnation,
                through,
            } => {
                let entry = self.entry(terminal, incarnation)?;
                entry.journal.acknowledge(through).map_err(journal_error)?;
                entry.retry_at = Instant::now();
                // A committed checkpoint remains successful even if another pending
                // output record still cannot fit. Info exposes that backpressure.
                let _ = entry.commit_pending();
                Ok(Response::Done)
            }
            Request::SetQuota {
                terminal,
                incarnation,
                bytes,
            } => {
                let entry = self.entry(terminal, incarnation)?;
                entry.journal.set_quota(bytes).map_err(journal_error)?;
                entry.metadata_dirty = true;
                entry.retry_at = Instant::now();
                if let Err(err) = entry.persist() {
                    let error = internal(format!("persist journal quota: {err:#}"));
                    entry.mark_blocked(error.clone());
                    return Err(error);
                }
                let _ = entry.commit_pending();
                Ok(Response::Done)
            }
            Request::Terminate {
                terminal,
                incarnation,
                grace_ms,
            } => {
                let entry = self.entry(terminal, incarnation)?;
                if let Some(native) = entry.native.as_mut() {
                    native.terminate(Duration::from_millis(grace_ms.into()))?;
                }
                Ok(Response::Done)
            }
        }
    }

    fn entry(
        &mut self,
        terminal: TerminalId,
        incarnation: IncarnationId,
    ) -> Result<&mut Entry, Error> {
        let entry = self.entries.get_mut(&terminal).ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!("terminal {terminal} is not retained"),
            )
        })?;
        if entry.info.incarnation != incarnation {
            return Err(Error::new(
                ErrorCode::WrongIncarnation,
                "terminal belongs to a different incarnation",
            ));
        }
        Ok(entry)
    }

    fn spawn(&mut self, mut spec: SpawnSpec) -> Result<Response, Error> {
        if self.closing {
            return Err(Error::new(
                ErrorCode::NotStopped,
                "PTY host is shutting down",
            ));
        }
        if let Some(entry) = self.entries.get(&spec.terminal) {
            return if entry.info.incarnation == spec.incarnation {
                Ok(Response::Process(entry.info.clone()))
            } else {
                Err(Error::new(
                    ErrorCode::WrongIncarnation,
                    "terminal already has another retained incarnation",
                ))
            };
        }
        if self.entries.len() >= MAX_PROCESSES {
            return Err(capacity(
                "retained terminal capacity is 128; forget stopped terminals",
            ));
        }
        if self
            .entries
            .values()
            .filter(|entry| entry.native.is_some())
            .count()
            >= MAX_ACTIVE
        {
            return Err(capacity("live PTY capacity is 64"));
        }
        validate_env(&spec)?;
        if spec.argv.is_empty() {
            spec.argv = vec![login_shell(&spec)?, "-l".into()];
        }
        if spec.cwd.is_empty() {
            spec.cwd = spec
                .env
                .get("HOME")
                .cloned()
                .or_else(|| std::env::var("HOME").ok())
                .unwrap_or_else(|| "/".into());
        }
        let cwd = fs::canonicalize(&spec.cwd).map_err(|e| {
            Error::new(
                ErrorCode::InvalidArgument,
                format!("resolve child working directory {:?}: {e}", spec.cwd),
            )
        })?;
        spec.cwd = cwd
            .to_str()
            .ok_or_else(|| {
                Error::new(ErrorCode::InvalidArgument, "working directory is not UTF-8")
            })?
            .to_owned();
        fl_proto::validation::launch(&spec.argv, &spec.cwd)
            .map_err(|e| Error::new(ErrorCode::InvalidArgument, e.to_string()))?;
        let parent = self.root.join(spec.terminal.to_string());
        private_dir(&parent).map_err(|e| internal(format!("create terminal directory: {e:#}")))?;
        let dir = parent.join(spec.incarnation.to_string());
        private_dir(&dir).map_err(|e| internal(format!("create incarnation directory: {e:#}")))?;
        let journal =
            Journal::open(&dir.join("journal"), spec.quota_bytes).map_err(journal_error)?;
        if journal.tail() != LogCursor::default() {
            return Err(Error::new(
                ErrorCode::AlreadyExists,
                "unindexed nonempty journal exists for requested incarnation",
            ));
        }
        let mut native = Native::spawn(&spec)?;
        let info = ProcessInfo {
            terminal: spec.terminal,
            incarnation: spec.incarnation,
            pid: native.leader.pid as u32,
            size: spec.size,
            argv: spec.argv,
            cwd: spec.cwd,
            state: ProcessState::Running,
            tail: journal.tail(),
            echo: native.echo(),
        };
        let mut entry = Entry {
            dir,
            info,
            journal,
            native: None,
            pending: VecDeque::with_capacity(3),
            exit: None,
            eof: false,
            blocked: None,
            retry_at: Instant::now(),
            metadata_dirty: true,
            clients: BTreeMap::new(),
            effect: None,
        };
        if let Err(err) = entry.persist() {
            // A failed spawn must not orphan a live child without durable metadata.
            native.abort_spawn();
            return Err(internal(format!("persist newly spawned PTY: {err:#}")));
        }
        entry.native = Some(native);
        entry
            .pending
            .push_back(TerminalRecordKind::Resize(entry.info.size));
        entry
            .pending
            .push_back(TerminalRecordKind::Echo(entry.info.echo));
        let result = entry
            .commit_pending()
            .map(|()| Response::Process(entry.info.clone()));
        self.entries.insert(spec.terminal, entry);
        result
    }
}

impl Entry {
    fn persist(&mut self) -> anyhow::Result<()> {
        let metadata = MetadataRef {
            version: 1,
            info: &self.info,
            quota_bytes: self.journal.quota_bytes(),
        };
        let bytes = serde_json::to_vec(&metadata)?;
        if bytes.len() as u64 > MAX_METADATA_BYTES {
            bail!("process metadata exceeds 512 KiB");
        }
        let temporary = self.dir.join(".process.json.tmp");
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, self.dir.join("process.json"))?;
        sync_dir(&self.dir)?;
        self.metadata_dirty = false;
        Ok(())
    }

    fn mark_blocked(&mut self, error: Error) {
        self.info.state = ProcessState::Backpressured {
            retained_bytes: self.journal.retained_bytes(),
            quota_bytes: self.journal.quota_bytes(),
            reason: bounded_message(&error.message),
        };
        self.blocked = Some(error);
        self.retry_at = Instant::now() + RETRY;
    }

    fn state_after_unblock(&mut self) {
        self.info.state = match (self.native.is_some(), self.exit) {
            (false, Some(status)) => ProcessState::Exited(status),
            (true, Some(status)) => ProcessState::Draining(status),
            (true, None) => ProcessState::Running,
            (false, None) => ProcessState::Lost,
        };
    }

    fn commit_pending(&mut self) -> Result<(), Error> {
        while let Some(kind) = self.pending.front() {
            if let Err(err) = self.journal.append(kind) {
                let err = journal_error(err);
                self.mark_blocked(err.clone());
                return Err(err);
            }
            let kind = self
                .pending
                .pop_front()
                .expect("appended pending record exists");
            self.info.tail = self.journal.tail();
            match kind {
                TerminalRecordKind::Echo(echo) => {
                    self.info.echo = echo;
                    self.metadata_dirty = true;
                }
                TerminalRecordKind::Exit(status) => {
                    self.metadata_dirty = true;
                    self.info.state = ProcessState::Exited(status);
                    // Only a durable Exit after EOF releases the persistent writer.
                    self.native.take();
                }
                _ => {}
            }
        }
        self.blocked = None;
        self.state_after_unblock();
        if self.metadata_dirty
            && let Err(err) = self.persist()
        {
            let err = internal(format!("persist PTY metadata: {err:#}"));
            self.mark_blocked(err.clone());
            return Err(err);
        }
        Ok(())
    }

    /// Read exactly one bounded chunk. EIO is Linux's final slave-close EOF.
    fn read_chunk(&mut self) -> Result<bool, Error> {
        if self.eof || self.native.is_none() {
            return Ok(false);
        }
        let native = self.native.as_mut().expect("checked native PTY");
        let mut readiness = libc::pollfd {
            fd: native.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut readiness, 1, 0) };
        if ready == 0 {
            return Ok(false);
        }
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            let err = os("poll native PTY output", err);
            self.mark_blocked(err.clone());
            return Err(err);
        }
        let echo = native.echo();
        if native.read_buffer.is_empty() {
            native.read_buffer.resize(MAX_OUTPUT_BYTES, 0);
        }
        match native.reader.read(&mut native.read_buffer) {
            Ok(0) => {
                self.eof = true;
                Ok(false)
            }
            Ok(count) => {
                let bytes = native.read_buffer.split_to(count).freeze();
                if echo != self.info.echo {
                    self.pending.push_back(TerminalRecordKind::Echo(echo));
                }
                self.pending
                    .push_back(TerminalRecordKind::Output { bytes, echo });
                self.commit_pending()?;
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(false),
            Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                self.eof = true;
                Ok(false)
            }
            Err(err) => {
                let err = os("read native PTY", err);
                self.mark_blocked(err.clone());
                Err(err)
            }
        }
    }

    fn boundary(&mut self) -> Result<(), Error> {
        if self.native.is_none() || self.exit.is_some() {
            return Err(stopped());
        }
        self.commit_pending()?;
        // Drain all already-readable old-size/old-echo bytes before a control.
        // A continuously producing child cannot monopolize the command actor.
        for _ in 0..BOUNDARY_READS {
            if !self.read_chunk()? {
                return Ok(());
            }
        }
        Err(capacity(
            "PTY control boundary is busy draining output; retry after reading/checkpointing",
        ))
    }

    fn write(
        &mut self,
        id: WriteId,
        bytes: &[u8],
        deadline_ns: Option<u64>,
    ) -> Result<Response, Error> {
        if matches!(id, WriteId::Client(_)) && deadline_ns.is_none() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "client input requires a CLOCK_BOOTTIME deadline",
            ));
        }
        if matches!(id, WriteId::Effect(_)) && deadline_ns.is_some() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "engine effect input must not carry a client deadline",
            ));
        }
        let now = crate::clock_ns().map_err(|e| os("read input admission clock", e))?;
        let deadline_ns =
            deadline_ns.map(|deadline| deadline.min(now.saturating_add(5_000_000_000)));
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let (sequence, existing) = match id {
            WriteId::Client(input) => (input.sequence, self.clients.get_mut(&input.client)),
            WriteId::Effect(sequence) => (sequence, self.effect.as_mut()),
        };
        if let Some(previous) = existing {
            if sequence < previous.sequence {
                return match id {
                    // Deterministic effects from older engine checkpoints must be
                    // acknowledged without re-injecting their committed bytes.
                    WriteId::Effect(_) => Ok(Response::Written {
                        accepted: bytes.len() as u32,
                        echo: self.info.echo,
                        at: self.journal.tail(),
                        expired: false,
                    }),
                    WriteId::Client(_) => Err(Error::new(
                        ErrorCode::InputConflict,
                        "input sequence is older than the retained writer watermark",
                    )),
                };
            }
            if sequence == previous.sequence {
                if previous.digest != digest || previous.length != bytes.len() {
                    return Err(Error::new(
                        ErrorCode::InputConflict,
                        "write ID was reused with different input bytes",
                    ));
                }
                previous.deadline_ns = match (previous.deadline_ns, deadline_ns) {
                    (Some(old), Some(new)) => Some(old.min(new)),
                    (old, _) => old,
                };
                previous.expired |= previous.accepted < previous.length
                    && previous.deadline_ns.is_some_and(|deadline| now >= deadline);
                if previous.accepted == bytes.len() || previous.expired {
                    return Ok(Response::Written {
                        accepted: previous.accepted as u32,
                        echo: self.info.echo,
                        at: self.journal.tail(),
                        expired: previous.expired,
                    });
                }
            } else {
                previous.expired |= previous.accepted < previous.length
                    && previous.deadline_ns.is_some_and(|deadline| now >= deadline);
                if previous.accepted != previous.length && !previous.expired {
                    return Err(Error::new(
                        ErrorCode::InputConflict,
                        "finish or expire previous partial input before advancing this writer's sequence",
                    ));
                }
            }
        } else if matches!(id, WriteId::Client(_)) && self.clients.len() >= MAX_WRITERS {
            return Err(capacity(
                "writer identity capacity is 256 per incarnation; identities are never evicted",
            ));
        }
        if self.native.is_none() || self.exit.is_some() {
            return Err(stopped());
        }
        // Pin the first admitted deadline even when an old-output boundary is
        // quota-paused. A retry can shorten, never extend, the original TTL.
        let progress = match id {
            WriteId::Client(input) => {
                self.clients
                    .entry(input.client)
                    .or_insert_with(|| WriteProgress {
                        sequence,
                        digest,
                        length: bytes.len(),
                        accepted: 0,
                        deadline_ns,
                        expired: false,
                    })
            }
            WriteId::Effect(_) => self.effect.get_or_insert(WriteProgress {
                sequence,
                digest,
                length: bytes.len(),
                accepted: 0,
                deadline_ns,
                expired: false,
            }),
        };
        if sequence > progress.sequence {
            *progress = WriteProgress {
                sequence,
                digest,
                length: bytes.len(),
                accepted: 0,
                deadline_ns,
                expired: false,
            };
        }
        if progress.deadline_ns.is_some_and(|deadline| now >= deadline) {
            progress.expired = true;
            return Ok(Response::Written {
                accepted: progress.accepted as u32,
                echo: self.info.echo,
                at: self.journal.tail(),
                expired: true,
            });
        }
        self.boundary()?;
        let echo = self.native.as_ref().ok_or_else(stopped)?.echo();
        if echo != self.info.echo {
            self.pending.push_back(TerminalRecordKind::Echo(echo));
            self.commit_pending()?;
        }
        let progress = match id {
            WriteId::Client(input) => self
                .clients
                .get_mut(&input.client)
                .expect("admitted client writer"),
            WriteId::Effect(_) => self.effect.as_mut().expect("admitted engine writer"),
        };
        // Check after slow filesystem work, immediately before the actual syscall.
        let now = crate::clock_ns().map_err(|e| os("read input write clock", e))?;
        progress.expired |= progress.accepted < progress.length
            && progress.deadline_ns.is_some_and(|deadline| now >= deadline);
        if !progress.expired && progress.accepted < bytes.len() {
            let result = self
                .native
                .as_mut()
                .ok_or_else(stopped)?
                .writer
                .write(&bytes[progress.accepted..]);
            match result {
                Ok(0) => {
                    return Err(internal(
                        "native PTY accepted zero bytes from nonempty input",
                    ));
                }
                Ok(count) => progress.accepted += count,
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(err) => {
                    let error = os("write native PTY", err);
                    self.mark_blocked(error.clone());
                    return Err(error);
                }
            }
        }
        Ok(Response::Written {
            accepted: progress.accepted as u32,
            echo: self.info.echo,
            at: self.journal.tail(),
            expired: progress.expired,
        })
    }

    fn tick(&mut self) -> bool {
        let before = (self.info.tail, self.info.state.clone());
        if let Some(native) = self.native.as_mut()
            && let Err(err) = native.escalate()
        {
            self.mark_blocked(err);
        }
        if self.exit.is_none()
            && (self.blocked.is_none() || Instant::now() >= self.retry_at)
            && let Some(native) = self.native.as_mut()
        {
            match native.try_wait() {
                Ok(Some(status)) => {
                    self.exit = Some(status);
                    self.metadata_dirty = true;
                    if self.blocked.is_none() {
                        self.info.state = ProcessState::Draining(status);
                    }
                }
                Ok(None) => {}
                Err(err) => self.mark_blocked(os("poll PTY child exit", err)),
            }
        }
        if self.blocked.is_some() && Instant::now() < self.retry_at {
            return before != (self.info.tail, self.info.state.clone());
        }
        if self.commit_pending().is_ok() {
            let _ = self.read_chunk();
            if self.pending.is_empty()
                && self.blocked.is_none()
                && self.eof
                && let Some(status) = self.exit
                && self.native.is_some()
            {
                self.pending.push_back(TerminalRecordKind::Exit(status));
                let _ = self.commit_pending();
            }
        }
        before != (self.info.tail, self.info.state.clone())
    }
}

impl Native {
    fn spawn(spec: &SpawnSpec) -> Result<Self, Error> {
        let pair = portable_pty::native_pty_system()
            .openpty(pty_size(spec.size))
            .map_err(|e| internal(format!("open native PTY: {e:#}")))?;
        let fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| internal("native PTY exposes no Unix descriptor"))?;
        // Clones share this open-file description, so both portable reader and
        // writer become nonblocking. No write_all/tcdrain can stall the actor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(os("set PTY nonblocking", io::Error::last_os_error()));
        }
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| internal(format!("clone PTY reader: {e:#}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| internal(format!("take persistent PTY writer: {e:#}")))?;
        let mut command = CommandBuilder::new(&spec.argv[0]);
        command.args(&spec.argv[1..]);
        command.cwd(&spec.cwd);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        let mut child = pair.slave.spawn_command(command).map_err(|e| {
            let invalid_launch = e
                .chain()
                .filter_map(|cause| cause.downcast_ref::<io::Error>())
                .filter_map(io::Error::raw_os_error)
                .any(|code| {
                    matches!(
                        code,
                        libc::ENOENT
                            | libc::ENOTDIR
                            | libc::EACCES
                            | libc::ENOEXEC
                            | libc::E2BIG
                            | libc::EINVAL
                    )
                });
            Error::new(
                if invalid_launch {
                    ErrorCode::InvalidArgument
                } else {
                    ErrorCode::Internal
                },
                format!("spawn PTY child {:?}: {e:#}", spec.argv),
            )
        })?;
        drop(pair.slave);
        let pid = child
            .process_id()
            .ok_or_else(|| internal("native child did not return its PID"))?;
        let leader = match process_identity(pid as i32) {
            Ok(identity) if identity.session == pid as i32 => identity,
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(internal(format!(
                    "verify spawned PTY session {pid}: {}",
                    match result {
                        Ok(_) => "child is not its session leader".into(),
                        Err(err) => err.to_string(),
                    }
                )));
            }
        };
        let pidfd = match open_pidfd(leader.pid) {
            Ok(pidfd) => pidfd,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(os("open native child pidfd (Linux 5.3+ required)", err));
            }
        };
        Ok(Self {
            master: pair.master,
            reader,
            writer,
            child,
            fd,
            leader,
            pidfd,
            read_buffer: BytesMut::with_capacity(MAX_OUTPUT_BYTES),
            session_members: BTreeMap::from([(leader.pid, leader)]),
            termination: None,
        })
    }

    fn echo(&self) -> bool {
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        // Unknown is deliberately false; never manufacture echo=true.
        unsafe {
            libc::tcgetattr(self.fd, termios.as_mut_ptr()) == 0
                && termios.assume_init().c_lflag & libc::ECHO != 0
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut readiness = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut readiness, 1, 0) } > 0 {
            // Snapshot the session while its zombie leader still pins the
            // original /proc identity. Terminate must also work during Draining,
            // after reaping a leader whose background jobs keep the slave open.
            refresh_session(self.fd, self.leader, &mut self.session_members)?;
        }
        // portable-pty's Unix backend is std::process::Child. Its own ExitStatus
        // converts numeric signals to localized strings, losing the wire value.
        let portable_child: &mut dyn Child = self.child.as_mut();
        let child = portable_child
            .downcast_mut::<std::process::Child>()
            .ok_or_else(|| io::Error::other("native Unix PTY child is not std::process::Child"))?;
        child.try_wait().map(|status| {
            status.map(|status| ExitStatus {
                code: status.code(),
                signal: status.signal(),
            })
        })
    }

    fn terminate(&mut self, grace: Duration) -> Result<(), Error> {
        if self.termination.is_some() {
            return Ok(());
        }
        refresh_session(self.fd, self.leader, &mut self.session_members)
            .map_err(|e| os("enumerate owned PTY session", e))?;
        for identity in self.session_members.values() {
            signal_identity(*identity, libc::SIGTERM)
                .map_err(|e| os("terminate PTY session member", e))?;
        }
        signal_identity(self.leader, libc::SIGHUP)
            .map_err(|e| os("hang up PTY session leader", e))?;
        let members = std::mem::take(&mut self.session_members);
        let now = Instant::now();
        self.termination = Some(Termination {
            deadline: now + grace,
            last_signal: now.checked_sub(Duration::from_secs(1)).unwrap_or(now),
            forced: false,
            stopping_since: None,
            stopped_snapshot: false,
            members,
        });
        Ok(())
    }

    fn escalate(&mut self) -> Result<(), Error> {
        let Some(termination) = self.termination.as_mut() else {
            return Ok(());
        };
        let now = Instant::now();
        if termination.forced
            || now < termination.deadline
            || now.duration_since(termination.last_signal) < Duration::from_millis(50)
        {
            return Ok(());
        }
        termination.last_signal = now;
        refresh_session(self.fd, self.leader, &mut termination.members)
            .map_err(|e| os("refresh terminating PTY session", e))?;
        let stopping_since = *termination.stopping_since.get_or_insert(now);
        let all_stopped = termination
            .members
            .values()
            .all(|identity| matches!(identity.state, b'T' | b't' | b'Z' | b'X' | b'x'));
        if now.duration_since(stopping_since) < Duration::from_millis(250) {
            if !all_stopped {
                // Stop the complete session before its final membership snapshot:
                // killing a forking shell first can otherwise orphan a new slave
                // holder born between /proc enumeration and the signal sweep.
                for identity in termination.members.values() {
                    signal_identity(*identity, libc::SIGSTOP)
                        .map_err(|e| os("stop owned PTY session before final kill", e))?;
                }
                termination.stopped_snapshot = false;
                return Ok(());
            }
            if !termination.stopped_snapshot {
                // One additional scan after all parents stopped admits children
                // born during the previous scan. No blocking wait on stopped jobs.
                termination.stopped_snapshot = true;
                return Ok(());
            }
        }
        // After a bounded 250-ms stop handshake, send SIGKILL on the next actor
        // turn even to uninterruptible tasks that cannot yet acknowledge SIGSTOP.
        for identity in termination.members.values() {
            signal_identity(*identity, libc::SIGKILL)
                .map_err(|e| os("kill owned PTY session member", e))?;
        }
        termination.forced = true;
        Ok(())
    }

    fn abort_spawn(&mut self) {
        let mut members = BTreeMap::new();
        members.insert(self.leader.pid, self.leader);
        let _ = refresh_session(self.fd, self.leader, &mut members);
        for identity in members.values() {
            let _ = signal_identity(*identity, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

#[cfg(target_os = "linux")]
fn process_identity(pid: i32) -> io::Result<ProcessIdentity> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // comm may contain spaces and ')'; fields after its final ')' start at 3.
    let mut fields = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("invalid /proc stat comm"))?
        .1
        .split_ascii_whitespace();
    let state = fields
        .next()
        .and_then(|value| value.as_bytes().first())
        .copied()
        .ok_or_else(|| io::Error::other("invalid /proc task state"))?;
    let session = fields
        .nth(2)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::other("invalid /proc session identity"))?;
    let started = fields
        .nth(15)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::other("invalid /proc starttime identity"))?;
    Ok(ProcessIdentity {
        pid,
        session,
        started,
        state,
    })
}

#[cfg(not(target_os = "linux"))]
fn process_identity(_: i32) -> io::Result<ProcessIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "PTY session identities require Linux /proc",
    ))
}

#[cfg(target_os = "linux")]
fn refresh_session(
    fd: RawFd,
    leader: ProcessIdentity,
    members: &mut BTreeMap<i32, ProcessIdentity>,
) -> io::Result<()> {
    // Never discover processes in a recycled numeric session. The controlling
    // terminal or an already pinned live member must still prove ownership.
    let tty_owned = unsafe { libc::tcgetsid(fd) } == leader.session;
    let member_owned = members.values().any(|identity| {
        process_identity(identity.pid).is_ok_and(|current| {
            current.started == identity.started && current.session == leader.session
        })
    });
    if !tty_owned && !member_owned {
        return Ok(());
    }
    members.retain(|_, identity| {
        process_identity(identity.pid).is_ok_and(|current| {
            current.started == identity.started && current.session == identity.session
        })
    });
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        match process_identity(pid) {
            Ok(identity) if identity.session == leader.session => {
                members.insert(pid, identity);
            }
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn refresh_session(
    _: RawFd,
    _: ProcessIdentity,
    _: &mut BTreeMap<i32, ProcessIdentity>,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "complete PTY session termination requires Linux",
    ))
}

#[cfg(target_os = "linux")]
fn signal_identity(identity: ProcessIdentity, signal: i32) -> io::Result<()> {
    // pidfd pins the kernel task before verification; a disappearing/reused PID
    // cannot cause kill(2) to strike an unrelated caller-owned process.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.pid, 0) };
    if fd < 0 {
        let err = io::Error::last_os_error();
        // A reaped leader's numeric PID can remain allocated as a live
        // session/group ID. pidfd_open then returns EINVAL, not ESRCH.
        if err.raw_os_error() == Some(libc::EINVAL) {
            match process_identity(identity.pid) {
                Err(gone) if gone.kind() == io::ErrorKind::NotFound => return Ok(()),
                Ok(current)
                    if current.started != identity.started
                        || current.session != identity.session =>
                {
                    return Ok(());
                }
                _ => {}
            }
        }
        return if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(err)
        };
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    match process_identity(identity.pid) {
        Ok(current)
            if current.started == identity.started && current.session == identity.session => {}
        Ok(_) => return Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    }
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } < 0
    {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn signal_identity(_: ProcessIdentity, _: i32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "safe PTY session signals require Linux pidfds",
    ))
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }
}

#[cfg(not(target_os = "linux"))]
fn open_pidfd(_: i32) -> io::Result<OwnedFd> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "child exit notification requires Linux pidfds",
    ))
}

fn wake_actor(wake: &UnixStream) {
    // One-byte nonblocking writes are atomic. EAGAIN means a wake is already
    // pending; sender disconnection also wakes the actor through POLLHUP.
    let byte = 1u8;
    unsafe {
        libc::write(wake.as_raw_fd(), (&byte as *const u8).cast(), 1);
    }
}

fn pty_size(size: Size) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.columns,
        pixel_width: size.columns * size.cell_width,
        pixel_height: size.rows * size.cell_height,
    }
}

fn login_shell(spec: &SpawnSpec) -> Result<String, Error> {
    if let Some(shell) = spec.env.get("SHELL").filter(|shell| !shell.is_empty()) {
        return Ok(shell.clone());
    }
    if let Ok(shell) = std::env::var("SHELL")
        && !shell.is_empty()
    {
        return Ok(shell);
    }
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let code = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if code != 0 {
            return Err(os(
                "resolve remote user's login shell",
                io::Error::from_raw_os_error(code),
            ));
        }
        if !result.is_null() {
            let passwd = unsafe { passwd.assume_init() };
            if !passwd.pw_shell.is_null() {
                let shell = unsafe { std::ffi::CStr::from_ptr(passwd.pw_shell) }
                    .to_str()
                    .map_err(|e| internal(format!("login shell is not UTF-8: {e}")))?;
                if !shell.is_empty() {
                    return Ok(shell.to_owned());
                }
            }
        }
        return Ok("/bin/sh".into());
    }
}

fn validate_env(spec: &SpawnSpec) -> Result<(), Error> {
    if spec.env.len() > 128 {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            "at most 128 child environment overrides are supported",
        ));
    }
    let mut total = 0usize;
    for (key, value) in &spec.env {
        total = total.saturating_add(key.len()).saturating_add(value.len());
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') || total > 128 * 1024
        {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "invalid environment override or environment exceeds 128 KiB",
            ));
        }
    }
    Ok(())
}

fn private_dir(path: &Path) -> anyhow::Result<()> {
    let mut missing = Vec::new();
    for ancestor in path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
    {
        if ancestor.try_exists()? {
            break;
        }
        missing.push(ancestor.to_path_buf());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)?;
    require_directory(path)?;
    // mkdir -p alone is not durable: every newly created ancestor also needs
    // its binding synced in its parent, including a new FLOTILLA_HOME/state.
    for created in missing.iter().rev() {
        let parent = created
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        sync_dir(parent)?;
        sync_dir(created)?;
    }
    if missing.is_empty() {
        sync_dir(path)?;
    }
    Ok(())
}

fn require_directory(path: &Path) -> anyhow::Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        bail!(
            "managed PTY path is not a real directory: {}",
            path.display()
        );
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn bounded_message(message: &str) -> String {
    let end = message.len().min(4000);
    let mut end = end;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].replace('\0', "?")
}

fn journal_error(error: JournalError) -> Error {
    let code = match &error {
        JournalError::Backpressured { .. } => ErrorCode::Backpressured,
        JournalError::CursorExpired { .. } => ErrorCode::CursorExpired,
        JournalError::CursorMismatch => ErrorCode::CursorMismatch,
        JournalError::InvalidQuota => ErrorCode::InvalidArgument,
        _ => ErrorCode::Internal,
    };
    Error::new(code, format!("PTY journal: {error}"))
}

fn capacity(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::Backpressured, message)
}

fn internal(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::Internal, message)
}

fn stopped() -> Error {
    Error::new(
        ErrorCode::NotStopped,
        "PTY no longer accepts input or controls; it is exited, draining, or lost",
    )
}

fn os(context: &str, error: io::Error) -> Error {
    internal(format!("{context}: {error}"))
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
