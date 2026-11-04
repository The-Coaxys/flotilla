//! Terminal service against a real process host, real shells and real Unix
//! sockets (and QUIC for logical-channel binding).
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fl_model::layout::{Layout, LayoutOp};
use fl_model::{
    ClientSessionId, IncarnationId, InputId, InputOwner, LogCursor, OperationId, PaneId,
    ProcessInfo, ProcessState, Size, SubscriptionId, TabId, TerminalId, TerminalRecordKind, ViewId,
};
use fl_proto::{
    BulkClientMsg, BulkServerMsg, Capabilities, ChannelGrant, ChannelRole, ClientMsg, Hello, PROTO,
    RecordBatch, RejectReason, Request, Response, RpcError, RpcErrorCode, ServerMsg, SnapshotOffer,
    TerminalClientMsg, TerminalError, TerminalNotice, TerminalServerMsg,
};
use fl_term::{ENGINE_ID, EffectMode, Engine, TextPoint};
use fl_transport::conn::{ConnError, FrameReader, FrameWriter, framed};
use fl_transport::quic;
use fld_server::devices::DeviceRegistry;
use fld_server::{DaemonConfig, DaemonHandle, FlotillaHome, LaunchDefaults, ServerError};
use sha2::{Digest, Sha256};
use tokio::net::UnixStream;

mod support;

const WAIT: Duration = Duration::from_secs(15);

struct Fixture {
    _dir: tempfile::TempDir,
    home: FlotillaHome,
    cwd: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = FlotillaHome::new(dir.path().join(".flotilla"));
        home.ensure_dirs().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        support::ensure_prochost(home.root()).await;
        Self {
            _dir: dir,
            home,
            cwd,
        }
    }

    fn config(&self, quic: bool) -> DaemonConfig {
        let mut config = DaemonConfig::new(self.home.clone());
        config.quic_bind = quic.then(|| "127.0.0.1:0".parse().unwrap());
        config.prochost_executable = None;
        config.launch = LaunchDefaults {
            cwd: self.cwd.display().to_string(),
            argv: vec!["/bin/sh".into()],
            env: BTreeMap::from([
                ("PS1".into(), "$ ".into()),
                ("TERM".into(), "xterm-256color".into()),
                ("ENV".into(), String::new()),
            ]),
            quota_bytes: fl_prochost_proto::DEFAULT_QUOTA_BYTES,
        };
        config
    }

    async fn start(&self) -> DaemonHandle {
        self.start_with(false).await
    }

    /// Retries while a just-dropped predecessor still releases the socket.
    async fn start_with(&self, quic: bool) -> DaemonHandle {
        tokio::time::timeout(WAIT, async {
            loop {
                match fld_server::start(self.config(quic)).await {
                    Ok(handle) => break handle,
                    Err(ServerError::AlreadyRunning(_)) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(error) => panic!("daemon start failed: {error}"),
                }
            }
        })
        .await
        .expect("daemon restarts")
    }

    fn store_root(&self) -> PathBuf {
        self.home.state_dir().join("workspace")
    }
}

async fn recv_server(reader: &mut FrameReader) -> ServerMsg {
    tokio::time::timeout(WAIT, reader.recv::<ServerMsg>())
        .await
        .expect("server reply in time")
        .expect("valid server message")
}

struct Control {
    reader: FrameReader,
    writer: FrameWriter,
    next: u64,
    session: [u8; 16],
    client: ClientSessionId,
    socket: PathBuf,
}

impl Control {
    async fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (r, w) = stream.into_split();
        let (reader, writer) = framed(r, w);
        Self::hello(reader, writer, None, socket).await
    }

    async fn hello(
        mut reader: FrameReader,
        mut writer: FrameWriter,
        credentials: Option<(String, String)>,
        socket: &Path,
    ) -> Self {
        let client = ClientSessionId::new();
        let (device_id, token) = credentials.unzip();
        writer
            .send(&ClientMsg::Hello(Hello {
                client_version: "terminal-test".into(),
                proto: PROTO,
                device_id,
                token,
                client_session_id: *client.as_bytes(),
            }))
            .await
            .unwrap();
        let ServerMsg::Welcome(welcome) = recv_server(&mut reader).await else {
            panic!("expected welcome")
        };
        Self {
            reader,
            writer,
            next: 0,
            session: welcome.server_session_id,
            client,
            socket: socket.to_path_buf(),
        }
    }

    async fn rpc(&mut self, req: Request) -> Result<Response, RpcError> {
        self.next += 1;
        let id = self.next;
        self.writer.send(&ClientMsg::Rpc { id, req }).await.unwrap();
        match recv_server(&mut self.reader).await {
            ServerMsg::RpcResult { id: got, result } if got == id => result,
            other => panic!("unexpected control message {other:?}"),
        }
    }

    async fn capabilities(&mut self) -> Capabilities {
        match self.rpc(Request::Capabilities).await.unwrap() {
            Response::Capabilities(value) => value,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn layout(&mut self) -> Layout {
        match self.rpc(Request::GetLayout).await.unwrap() {
            Response::Layout(layout) => layout,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn apply(&mut self, op: LayoutOp) -> Result<Layout, RpcError> {
        let base_rev = self.layout().await.rev;
        self.apply_at(OperationId::new(), base_rev, op).await
    }

    async fn apply_at(
        &mut self,
        operation: OperationId,
        base_rev: u64,
        op: LayoutOp,
    ) -> Result<Layout, RpcError> {
        self.rpc(Request::ApplyLayout {
            operation,
            base_rev,
            op,
        })
        .await
        .map(|response| match response {
            Response::Layout(layout) => layout,
            other => panic!("unexpected {other:?}"),
        })
    }

    async fn processes(&mut self) -> Vec<ProcessInfo> {
        match self.rpc(Request::ListProcesses).await.unwrap() {
            Response::Processes(list) => list,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn grant(&mut self, role: ChannelRole) -> ChannelGrant {
        match self.rpc(Request::OpenChannel { role }).await.unwrap() {
            Response::ChannelGrant(grant) => grant,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Bind a new Unix connection with a fresh ticket.
    async fn channel(&mut self, role: ChannelRole) -> (FrameReader, FrameWriter) {
        let grant = self.grant(role).await;
        let (reader, writer, reply) = bind_unix(&self.socket, &grant.hello()).await;
        assert!(
            matches!(reply, ServerMsg::ChannelReady(ready) if ready.role == role && ready.server_session_id == self.session)
        );
        (reader, writer)
    }
}

async fn bind_unix(
    socket: &Path,
    hello: &fl_proto::ChannelHello,
) -> (FrameReader, FrameWriter, ServerMsg) {
    let stream = UnixStream::connect(socket).await.unwrap();
    let (r, w) = stream.into_split();
    let (mut reader, mut writer) = framed(r, w);
    writer
        .send(&ClientMsg::BindChannel(hello.clone()))
        .await
        .unwrap();
    let reply = recv_server(&mut reader).await;
    (reader, writer, reply)
}

struct TermChannel {
    reader: FrameReader,
    writer: FrameWriter,
    next: u64,
}

impl TermChannel {
    fn new((reader, writer): (FrameReader, FrameWriter)) -> Self {
        Self {
            reader,
            writer,
            next: 1000,
        }
    }

    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    async fn send(&mut self, message: TerminalClientMsg) {
        self.writer.send(&message).await.unwrap();
    }

    async fn recv(&mut self) -> TerminalServerMsg {
        tokio::time::timeout(WAIT, self.reader.recv::<TerminalServerMsg>())
            .await
            .expect("terminal message in time")
            .expect("valid terminal message")
    }

    /// Next message that is not a record batch/process notification.
    async fn reply(&mut self, mirror: &mut Mirror) -> TerminalServerMsg {
        loop {
            match self.recv().await {
                TerminalServerMsg::RecordBatch(batch) => mirror.apply(&batch),
                TerminalServerMsg::ProcessChanged(_)
                | TerminalServerMsg::OwnerChanged { id: None, .. } => {}
                other => return other,
            }
        }
    }
}

struct Bulk {
    reader: FrameReader,
    writer: FrameWriter,
}

impl Bulk {
    async fn fetch(&mut self, offer: &SnapshotOffer) -> Vec<u8> {
        offer.validate().unwrap();
        self.writer
            .send(&BulkClientMsg::Request {
                transfer: offer.transfer,
                offset: 0,
            })
            .await
            .unwrap();
        let mut bytes = Vec::new();
        loop {
            let message = tokio::time::timeout(WAIT, self.reader.recv::<BulkServerMsg>())
                .await
                .unwrap()
                .unwrap();
            match message {
                BulkServerMsg::Chunk(chunk) => {
                    chunk
                        .validate_for(offer.transfer, bytes.len() as u64, offer.total_bytes)
                        .unwrap();
                    bytes.extend_from_slice(&chunk.bytes);
                }
                BulkServerMsg::Complete { transfer, sha256 } => {
                    assert_eq!(transfer, offer.transfer);
                    assert_eq!(sha256, offer.sha256);
                    break;
                }
                BulkServerMsg::Failure { error, .. } => panic!("bulk failure {error:?}"),
            }
        }
        assert_eq!(bytes.len() as u64, offer.total_bytes);
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        assert_eq!(digest, offer.sha256);
        bytes
    }
}

/// A client-side mirror engine: exactly the records the daemon consumed.
#[derive(Default)]
struct Mirror {
    engine: Option<Engine>,
    cursor: LogCursor,
    pending: Vec<RecordBatch>,
}

impl Mirror {
    fn install(&mut self, offer: &SnapshotOffer, bytes: &[u8]) {
        assert_eq!(offer.engine_id, ENGINE_ID);
        let engine = Engine::restore(bytes, EffectMode::Mirror).unwrap();
        assert_eq!(engine.offset(), offer.cursor.next_byte);
        assert_eq!(engine.size(), offer.size);
        self.engine = Some(engine);
        self.cursor = offer.cursor;
        for batch in std::mem::take(&mut self.pending) {
            self.apply(&batch);
        }
    }

    fn apply(&mut self, batch: &RecordBatch) {
        batch.validate().unwrap();
        let Some(engine) = self.engine.as_mut() else {
            self.pending.push(batch.clone());
            return;
        };
        if batch.next.next_record <= self.cursor.next_record {
            return; // already covered by an installed snapshot
        }
        if batch
            .records
            .first()
            .is_some_and(|r| r.at.next_record < self.cursor.next_record)
        {
            // Straddles an installed snapshot: apply only the suffix.
            for record in &batch.records {
                if record.at.next_record >= self.cursor.next_record {
                    Self::feed(engine, &mut self.cursor, record);
                }
            }
            return;
        }
        batch.validate_from(self.cursor).unwrap();
        for record in &batch.records {
            Self::feed(engine, &mut self.cursor, record);
        }
    }

    fn feed(engine: &mut Engine, cursor: &mut LogCursor, record: &fl_model::TerminalRecord) {
        assert_eq!(record.at, *cursor);
        let effects = match &record.kind {
            TerminalRecordKind::Output { bytes, .. } => {
                engine.feed(record.at.next_byte, bytes).unwrap()
            }
            TerminalRecordKind::Resize(size) => engine.resize(*size).unwrap(),
            TerminalRecordKind::SyncFlush => engine.flush_sync().unwrap(),
            _ => Vec::new(),
        };
        assert!(
            effects
                .iter()
                .all(|e| !matches!(e.kind, fl_term::EffectKind::Reply(_)))
        );
        *cursor = record.end().unwrap();
    }

    fn engine(&self) -> &Engine {
        self.engine.as_ref().expect("snapshot installed")
    }

    fn text(&self) -> String {
        let engine = self.engine();
        let size = engine.size();
        engine.text_between(
            TextPoint {
                line: -1000,
                column: 0,
            },
            TextPoint {
                line: size.rows as i32 - 1,
                column: size.columns - 1,
            },
        )
    }
}

/// Receive and apply terminal traffic until `done` holds.
async fn pump_until(
    channel: &mut TermChannel,
    bulk: &mut Bulk,
    mirror: &mut Mirror,
    mut done: impl FnMut(&Mirror) -> bool,
) {
    tokio::time::timeout(WAIT, async {
        while mirror.engine.is_none() || !done(mirror) {
            match channel.recv().await {
                TerminalServerMsg::RecordBatch(batch) => mirror.apply(&batch),
                TerminalServerMsg::SnapshotOffer(offer) => {
                    let bytes = bulk.fetch(&offer).await;
                    mirror.install(&offer, &bytes);
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "condition not reached; screen:\n{}",
            mirror
                .engine
                .as_ref()
                .map(|_| mirror.text())
                .unwrap_or_default()
        )
    });
}

fn new_tab(layout: &Layout, terminal: TerminalId) -> (TabId, LayoutOp) {
    let tab = TabId::new();
    (
        tab,
        LayoutOp::NewTab {
            view: layout.active,
            tab,
            pane: PaneId::new(),
            terminal,
            name: Some("Shell".into()),
            size: Size::new(80, 24),
        },
    )
}

struct Attached {
    channel: TermChannel,
    bulk: Bulk,
    mirror: Mirror,
    subscription: SubscriptionId,
}

async fn attach(
    control: &mut Control,
    role: ChannelRole,
    terminal: TerminalId,
    incarnation: IncarnationId,
    from: Option<LogCursor>,
) -> (Attached, LogCursor) {
    let mut channel = TermChannel::new(control.channel(role).await);
    let (reader, writer) = control.channel(ChannelRole::Bulk).await;
    let mut bulk = Bulk { reader, writer };
    let subscription = SubscriptionId::new();
    let id = channel.id();
    channel
        .send(TerminalClientMsg::Subscribe {
            id,
            subscription,
            terminal,
            incarnation,
            from,
        })
        .await;
    let TerminalServerMsg::Subscribed {
        id: got, cursor, ..
    } = channel.recv().await
    else {
        panic!("expected Subscribed")
    };
    assert_eq!(got, id);
    let mut mirror = Mirror::default();
    if from.is_none() {
        pump_until(&mut channel, &mut bulk, &mut mirror, |_| true).await;
        assert_eq!(mirror.cursor, cursor);
    }
    (
        Attached {
            channel,
            bulk,
            mirror,
            subscription,
        },
        cursor,
    )
}

impl Attached {
    async fn input(
        &mut self,
        client: ClientSessionId,
        sequence: u64,
        view: ViewId,
        terminal: (TerminalId, IncarnationId),
        bytes: &[u8],
        deadline_ns: u64,
    ) -> TerminalServerMsg {
        let id = self.channel.id();
        self.channel
            .send(TerminalClientMsg::Input {
                id,
                terminal: terminal.0,
                incarnation: terminal.1,
                input: InputId { client, sequence },
                view,
                bytes: bytes.to_vec(),
                deadline_ns,
            })
            .await;
        self.channel.reply(&mut self.mirror).await
    }

    async fn until(&mut self, done: impl FnMut(&Mirror) -> bool) {
        pump_until(&mut self.channel, &mut self.bulk, &mut self.mirror, done).await
    }
}

fn now_ns() -> u64 {
    fld_prochost::clock_ns().unwrap()
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shell_input_resize_restart_with_exact_state_and_close() {
    let fixture = Fixture::new().await;
    let daemon = fixture.start().await;
    let mut control = Control::connect(&daemon.socket_path).await;

    let capabilities = control.capabilities().await;
    capabilities.validate().unwrap();
    assert_eq!(capabilities.engine_id, ENGINE_ID);
    assert_eq!(capabilities.roles.len(), 3);
    let skew = now_ns().abs_diff(capabilities.clock_ns);
    assert!(skew < 5_000_000_000, "boot clock sample is current");

    // Create a tab: the committed layout spawns a real shell.
    let layout = control.layout().await;
    let terminal = TerminalId::new();
    let (tab, op) = new_tab(&layout, terminal);
    let operation = OperationId::new();
    let applied = control
        .apply_at(operation, layout.rev, op.clone())
        .await
        .unwrap();
    assert_eq!(applied.rev, layout.rev + 1);
    // Same operation retried: deduplicated, no second spawn.
    assert_eq!(
        control.apply_at(operation, layout.rev, op).await.unwrap(),
        applied
    );
    // Stale base revision: typed conflict.
    let (_, other) = new_tab(&applied, TerminalId::new());
    assert_eq!(
        control
            .apply_at(OperationId::new(), layout.rev, other)
            .await
            .unwrap_err()
            .code,
        RpcErrorCode::RevisionConflict
    );
    let processes = control.processes().await;
    assert_eq!(processes.len(), 1);
    let info = processes[0].clone();
    assert_eq!(info.terminal, terminal);
    assert_eq!(info.state, ProcessState::Running);
    let pid = info.pid;
    let incarnation = info.incarnation;
    assert!(alive(pid));
    let target = (terminal, incarnation);

    let (mut attached, _) = attach(
        &mut control,
        ChannelRole::TerminalHot,
        terminal,
        incarnation,
        None,
    )
    .await;
    let view = ViewId::new();
    let claim = attached.channel.id();
    attached
        .channel
        .send(TerminalClientMsg::ClaimOwner {
            id: claim,
            terminal,
            incarnation,
            view,
        })
        .await;
    let TerminalServerMsg::OwnerChanged {
        id: Some(got),
        owner: Some(owner),
        ..
    } = attached.channel.reply(&mut attached.mirror).await
    else {
        panic!("expected owner lease")
    };
    assert_eq!(got, claim);
    assert_eq!((owner.client, owner.view), (control.client, view));

    // Input echoes through the PTY and the mirror engine.
    let ack = attached
        .input(
            control.client,
            1,
            view,
            target,
            b"echo hello-$((6*7)) pid=$$\n",
            now_ns() + 4_000_000_000,
        )
        .await;
    let TerminalServerMsg::InputAck(ack) = ack else {
        panic!("expected InputAck, got {ack:?}")
    };
    assert_eq!(
        ack.accepted_bytes as usize,
        b"echo hello-$((6*7)) pid=$$\n".len()
    );
    assert!(!ack.expired);
    let expected = format!("hello-42 pid={pid}");
    attached.until(|m| m.text().contains(&expected)).await;

    // Resize by the lease holder is applied in the PTY and recorded in order.
    let resize = attached.channel.id();
    attached
        .channel
        .send(TerminalClientMsg::Resize {
            id: resize,
            terminal,
            incarnation,
            owner,
            size: Size::new(100, 30),
        })
        .await;
    let TerminalServerMsg::Resized { id, size, .. } =
        attached.channel.reply(&mut attached.mirror).await
    else {
        panic!("expected Resized")
    };
    assert_eq!((id, size), (resize, Size::new(100, 30)));
    attached
        .input(
            control.client,
            2,
            view,
            target,
            b"stty size\n",
            now_ns() + 4_000_000_000,
        )
        .await;
    attached.until(|m| m.text().contains("30 100")).await;
    assert_eq!(attached.mirror.engine().size(), Size::new(100, 30));

    // A stale lease generation cannot resize.
    let stale = InputOwner {
        generation: owner.generation - 1,
        ..owner
    };
    let id = attached.channel.id();
    attached
        .channel
        .send(TerminalClientMsg::Resize {
            id,
            terminal,
            incarnation,
            owner: stale,
            size: Size::new(90, 20),
        })
        .await;
    let reply = attached.channel.reply(&mut attached.mirror).await;
    assert!(
        matches!(&reply, TerminalServerMsg::Error { error: TerminalError::NotOwner { owner: Some(current) }, .. } if *current == owner),
        "got {reply:?}"
    );

    // Admission: an already-expired input never reaches the PTY; a deadline
    // beyond the five-second limit is rejected.
    let reply = attached
        .input(
            control.client,
            3,
            view,
            target,
            b"echo never\n",
            now_ns().saturating_sub(1),
        )
        .await;
    assert!(
        matches!(&reply, TerminalServerMsg::InputAck(ack) if ack.expired && ack.accepted_bytes == 0),
        "got {reply:?}"
    );
    let reply = attached
        .input(
            control.client,
            4,
            view,
            target,
            b"x",
            now_ns() + 60_000_000_000,
        )
        .await;
    assert!(
        matches!(
            &reply,
            TerminalServerMsg::Error {
                error: TerminalError::InvalidArgument { .. },
                ..
            }
        ),
        "got {reply:?}"
    );

    // Wait for a durable periodic checkpoint, then produce a journal suffix.
    let store_root = fixture.store_root();
    let key = fld_store::TerminalKey {
        terminal,
        incarnation,
    };
    let checkpoint = tokio::time::timeout(WAIT, async {
        loop {
            let database = fld_store::database::Database::open(&store_root).unwrap();
            if let Some(reference) = database.terminal(key).unwrap().checkpoint {
                break reference;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("periodic checkpoint");
    attached
        .input(
            control.client,
            5,
            view,
            target,
            b"echo after-checkpoint\n",
            now_ns() + 4_000_000_000,
        )
        .await;
    attached
        .until(|m| m.text().matches("after-checkpoint").count() >= 2)
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    attached.until(|_| true).await;
    let before_restart = attached.mirror.cursor;
    assert!(before_restart.next_byte > checkpoint.cursor.next_byte);
    let mirror_before = attached.mirror;

    // Crash-equivalent restart: no final checkpoint.
    drop(attached.channel);
    drop(attached.bulk);
    drop(control);
    drop(daemon);
    let daemon = fixture.start().await;
    let stored = fld_store::database::Database::open(&store_root)
        .unwrap()
        .terminal(key)
        .unwrap();
    let restored_from = stored.checkpoint.as_ref().map(|c| c.cursor).unwrap();
    assert!(restored_from.next_byte <= before_restart.next_byte);
    let mut control = Control::connect(&daemon.socket_path).await;
    let processes = control.processes().await;
    assert_eq!(processes.len(), 1);
    assert_eq!(processes[0].incarnation, incarnation);
    assert_eq!(processes[0].pid, pid, "the same shell continues");
    assert!(alive(pid));

    // Resume the pre-restart mirror from its own cursor: the restored
    // engine replayed the suffix and serves exactly the following records.
    let (mut resumed, cursor) = attach(
        &mut control,
        ChannelRole::TerminalBackground,
        terminal,
        incarnation,
        Some(before_restart),
    )
    .await;
    assert_eq!(cursor, before_restart);
    resumed.mirror = mirror_before;
    // A fresh snapshot of the restored authoritative engine equals the
    // continuously fed mirror at the same cursor, byte for byte.
    let (mut fresh, snapshot_cursor) = attach(
        &mut control,
        ChannelRole::TerminalHot,
        terminal,
        incarnation,
        None,
    )
    .await;
    resumed.until(|m| m.cursor == snapshot_cursor).await;
    assert_eq!(
        resumed.mirror.engine().checkpoint().unwrap(),
        fresh.mirror.engine().checkpoint().unwrap()
    );
    assert_eq!(resumed.mirror.text(), fresh.mirror.text());

    // Input continues in the same shell after restart.
    let view = ViewId::new();
    let reply = fresh
        .input(
            control.client,
            1,
            view,
            target,
            b"echo again-$$\n",
            now_ns() + 4_000_000_000,
        )
        .await;
    assert!(
        matches!(&reply, TerminalServerMsg::InputAck(ack) if !ack.expired),
        "got {reply:?}"
    );
    let expected = format!("again-{pid}");
    fresh.until(|m| m.text().contains(&expected)).await;
    resumed.until(|m| m.text().contains(&expected)).await;

    // Closing the tab terminates the process and removes its records.
    let layout = control.layout().await;
    control
        .apply(LayoutOp::CloseTab {
            view: layout.active,
            tab,
        })
        .await
        .unwrap();
    let prochost = support::ensure_prochost(fixture.home.root()).await;
    tokio::time::timeout(WAIT, async {
        loop {
            let Ok(fl_prochost_proto::Response::Processes(list)) =
                prochost.request(&fl_prochost_proto::Request::List).await
            else {
                panic!("process-host list")
            };
            let removed = fld_store::database::Database::open(&store_root)
                .unwrap()
                .list_terminals()
                .unwrap()
                .is_empty();
            if list.is_empty() && removed && !alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("closed terminal is terminated, forgotten and removed");
    assert_eq!(
        control
            .rpc(Request::ProcessInfo {
                terminal,
                incarnation
            })
            .await
            .unwrap_err()
            .code,
        RpcErrorCode::NotFound
    );
    assert!(control.processes().await.is_empty());
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channel_tickets_are_session_bound_one_use_and_role_checked() {
    let fixture = Fixture::new().await;
    let daemon = fixture.start().await;
    let socket = daemon.socket_path.clone();
    let mut control = Control::connect(&socket).await;

    // A ticket is single-use.
    let grant = control.grant(ChannelRole::TerminalHot).await;
    let (_reader, _writer, reply) = bind_unix(&socket, &grant.hello()).await;
    assert!(matches!(reply, ServerMsg::ChannelReady(_)));
    let (_, _, reply) = bind_unix(&socket, &grant.hello()).await;
    assert_eq!(reply, ServerMsg::Rejected(RejectReason::InvalidCapability));

    // The role is part of the capability.
    let grant = control.grant(ChannelRole::TerminalBackground).await;
    let mut hello = grant.hello();
    hello.role = ChannelRole::Bulk;
    let (_, _, reply) = bind_unix(&socket, &hello).await;
    assert_eq!(reply, ServerMsg::Rejected(RejectReason::WrongChannelRole));

    // Forged tickets and another session's identity are rejected.
    let grant = control.grant(ChannelRole::Bulk).await;
    let mut forged = grant.hello();
    forged.ticket[0] ^= 1;
    let (_, _, reply) = bind_unix(&socket, &forged).await;
    assert_eq!(reply, ServerMsg::Rejected(RejectReason::InvalidCapability));
    let mut other_session = grant.hello();
    other_session.server_session_id[0] ^= 1;
    let (_, _, reply) = bind_unix(&socket, &other_session).await;
    assert_eq!(reply, ServerMsg::Rejected(RejectReason::InvalidCapability));

    // Unknown terminals and stale incarnations cannot be attached.
    let mut channel = TermChannel::new(control.channel(ChannelRole::TerminalHot).await);
    let mut mirror = Mirror::default();
    let id = channel.id();
    channel
        .send(TerminalClientMsg::Subscribe {
            id,
            subscription: SubscriptionId::new(),
            terminal: TerminalId::new(),
            incarnation: IncarnationId::new(),
            from: None,
        })
        .await;
    assert!(matches!(
        channel.reply(&mut mirror).await,
        TerminalServerMsg::Error { id: Some(got), error: TerminalError::NotFound, .. } if got == id
    ));
    let layout = control.layout().await;
    let terminal = TerminalId::new();
    control.apply(new_tab(&layout, terminal).1).await.unwrap();
    let incarnation = control.processes().await[0].incarnation;
    let id = channel.id();
    channel
        .send(TerminalClientMsg::Subscribe {
            id,
            subscription: SubscriptionId::new(),
            terminal,
            incarnation: IncarnationId::new(),
            from: None,
        })
        .await;
    assert!(matches!(
        channel.reply(&mut mirror).await,
        TerminalServerMsg::Error { error: TerminalError::WrongIncarnation { current }, .. } if current == incarnation
    ));
    // Input cannot impersonate another client session's write identity.
    let id = channel.id();
    channel
        .send(TerminalClientMsg::Input {
            id,
            terminal,
            incarnation,
            input: InputId {
                client: ClientSessionId::new(),
                sequence: 1,
            },
            view: ViewId::new(),
            bytes: b"echo spoof\n".to_vec(),
            deadline_ns: now_ns() + 1_000_000_000,
        })
        .await;
    assert!(matches!(
        channel.reply(&mut mirror).await,
        TerminalServerMsg::Error {
            error: TerminalError::InvalidArgument { .. },
            ..
        }
    ));

    // Ending the control session closes its channels and voids its tickets.
    let unused = control.grant(ChannelRole::TerminalBackground).await;
    drop(control);
    let closed = tokio::time::timeout(WAIT, async {
        loop {
            match channel.reader.recv::<TerminalServerMsg>().await {
                Err(ConnError::Closed) => break,
                Ok(_) => {}
                Err(error) => panic!("unexpected channel error {error}"),
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "channel closes with its control session");
    let (_, _, reply) = bind_unix(&socket, &unused.hello()).await;
    assert_eq!(reply, ServerMsg::Rejected(RejectReason::InvalidCapability));
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_subscriptions_pause_and_slow_clients_get_replacement_snapshots() {
    let fixture = Fixture::new().await;
    let daemon = fixture.start().await;
    let mut control = Control::connect(&daemon.socket_path).await;
    let layout = control.layout().await;
    let terminal = TerminalId::new();
    control.apply(new_tab(&layout, terminal).1).await.unwrap();
    let incarnation = control.processes().await[0].incarnation;
    let target = (terminal, incarnation);
    let (mut attached, _) = attach(
        &mut control,
        ChannelRole::TerminalHot,
        terminal,
        incarnation,
        None,
    )
    .await;
    let view = ViewId::new();

    // Parked: no records are streamed until unparked.
    let id = attached.channel.id();
    let subscription = attached.subscription;
    attached
        .channel
        .send(TerminalClientMsg::SetParked {
            id,
            subscription,
            terminal,
            incarnation,
            parked: true,
        })
        .await;
    assert!(matches!(
        attached.channel.reply(&mut attached.mirror).await,
        TerminalServerMsg::Parked { parked: true, .. }
    ));
    let reply = attached
        .input(
            control.client,
            1,
            view,
            target,
            b"echo parked-output\n",
            now_ns() + 4_000_000_000,
        )
        .await;
    assert!(
        matches!(reply, TerminalServerMsg::InputAck(_)),
        "got {reply:?}"
    );
    let quiet = tokio::time::timeout(Duration::from_millis(700), async {
        loop {
            if let TerminalServerMsg::RecordBatch(_) = attached.channel.recv().await {
                break;
            }
        }
    })
    .await;
    assert!(
        quiet.is_err(),
        "parked subscription must not stream records"
    );
    let id = attached.channel.id();
    attached
        .channel
        .send(TerminalClientMsg::SetParked {
            id,
            subscription,
            terminal,
            incarnation,
            parked: false,
        })
        .await;
    attached
        .until(|m| m.text().matches("parked-output").count() >= 2)
        .await;

    // Flood far beyond the lag limit while not reading the hot channel.
    // Random hex defeats frame compression, so transport buffers stay small.
    let snapshots_before = 1;
    let reply = attached
        .input(
            control.client,
            2,
            view,
            target,
            b"head -c 3000000 /dev/urandom | od -An -tx1 -v; echo flood-$((200000+1))-done\n",
            now_ns() + 4_000_000_000,
        )
        .await;
    assert!(
        matches!(reply, TerminalServerMsg::InputAck(_)),
        "got {reply:?}"
    );
    let prochost = support::ensure_prochost(fixture.home.root()).await;
    // Let the whole flood (several MiB) complete while this client is stalled.
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut last = 0;
        loop {
            let Ok(fl_prochost_proto::Response::Process(info)) = prochost
                .request(&fl_prochost_proto::Request::Info {
                    terminal,
                    incarnation,
                })
                .await
            else {
                panic!("process info")
            };
            let tail = info.tail.next_byte;
            if tail == last && tail > attached.mirror.cursor.next_byte + 3 * 1024 * 1024 {
                break;
            }
            last = tail;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await
    .expect("flood output");
    let mut snapshots = snapshots_before;
    let mut batches_bytes = 0u64;
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match attached.channel.recv().await {
                TerminalServerMsg::RecordBatch(batch) => {
                    batches_bytes += batch.next.next_byte
                        - batch.records.first().map_or(batch.next, |r| r.at).next_byte;
                    attached.mirror.apply(&batch);
                }
                TerminalServerMsg::SnapshotOffer(offer) => {
                    snapshots += 1;
                    let bytes = attached.bulk.fetch(&offer).await;
                    attached.mirror.install(&offer, &bytes);
                }
                _ => {}
            }
            if attached.mirror.text().contains("flood-200001-done") {
                break;
            }
        }
    })
    .await
    .expect("slow client catches up");
    assert!(
        snapshots > snapshots_before,
        "a stalled client receives a replacement snapshot (streamed {batches_bytes} bytes)"
    );
    let total = attached.mirror.cursor.next_byte;
    assert!(
        batches_bytes < total / 2,
        "most of the {total}-byte flood was replaced, not queued ({batches_bytes} streamed)"
    );
    daemon.shutdown().await;
    // The shell is still running in the independent host.
    let Ok(fl_prochost_proto::Response::Process(info)) = prochost
        .request(&fl_prochost_proto::Request::Info {
            terminal,
            incarnation,
        })
        .await
    else {
        panic!("process info")
    };
    assert_eq!(info.state, ProcessState::Running);
    let _ = prochost
        .request(&fl_prochost_proto::Request::Terminate {
            terminal,
            incarnation,
            grace_ms: 0,
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quic_streams_and_connections_bind_channels_with_session_tickets() {
    let fixture = Fixture::new().await;
    let daemon = fixture.start_with(true).await;
    let (device, token) = DeviceRegistry::new(fixture.home.devices_path())
        .enroll("pc")
        .unwrap();
    let addr = daemon.quic_addr.unwrap();
    let (_endpoint, conn) = quic::connect(addr, daemon.cert_sha256, WAIT).await.unwrap();
    let (send, recv) = quic::open_ctl(&conn).await.unwrap();
    let (reader, writer) = framed(recv, send);
    let mut control =
        Control::hello(reader, writer, Some((device, token)), &daemon.socket_path).await;

    async fn bind_stream(
        conn: &quinn::Connection,
        grant: &ChannelGrant,
    ) -> (FrameReader, FrameWriter, ServerMsg) {
        let (send, recv) = quic::open_ctl(conn).await.unwrap();
        let (mut reader, mut writer) = framed(recv, send);
        writer
            .send(&ClientMsg::BindChannel(grant.hello()))
            .await
            .unwrap();
        let reply = recv_server(&mut reader).await;
        (reader, writer, reply)
    }

    // A second stream on the authenticated connection.
    let grant = control.grant(ChannelRole::TerminalHot).await;
    let (reader, writer, reply) = bind_stream(&conn, &grant).await;
    assert!(
        matches!(reply, ServerMsg::ChannelReady(ready) if ready.server_session_id == control.session)
    );
    let mut channel = TermChannel::new((reader, writer));
    let id = channel.id();
    channel
        .send(TerminalClientMsg::Subscribe {
            id,
            subscription: SubscriptionId::new(),
            terminal: TerminalId::new(),
            incarnation: IncarnationId::new(),
            from: None,
        })
        .await;
    assert!(matches!(
        channel.recv().await,
        TerminalServerMsg::Error {
            error: TerminalError::NotFound,
            ..
        }
    ));

    // A separate connection carrying only a ticket-bound channel: no Hello.
    let grant = control.grant(ChannelRole::Bulk).await;
    let (_endpoint2, conn2) = quic::connect(addr, daemon.cert_sha256, WAIT).await.unwrap();
    let (_r, _w, reply) = bind_stream(&conn2, &grant).await;
    assert!(matches!(reply, ServerMsg::ChannelReady(ready) if ready.role == ChannelRole::Bulk));
    // Reusing that ticket on yet another connection fails.
    let (_endpoint3, conn3) = quic::connect(addr, daemon.cert_sha256, WAIT).await.unwrap();
    let (_, _, reply) = bind_stream(&conn3, &grant).await;
    assert_eq!(reply, ServerMsg::Rejected(RejectReason::InvalidCapability));

    // The control stream keeps working alongside its channels.
    assert_eq!(control.capabilities().await.engine_id, ENGINE_ID);
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_relaunches_missing_processes_and_finishes_closed_intents() {
    let fixture = Fixture::new().await;
    let prochost = support::ensure_prochost(fixture.home.root()).await;
    let daemon = fixture.start().await;
    let mut control = Control::connect(&daemon.socket_path).await;
    let layout = control.layout().await;
    let lost_terminal = TerminalId::new();
    control
        .apply(new_tab(&layout, lost_terminal).1)
        .await
        .unwrap();
    let layout = control.layout().await;
    let closed_terminal = TerminalId::new();
    let (closed_tab, closed_op) = new_tab(&layout, closed_terminal);
    control.apply(closed_op).await.unwrap();
    let before = control.processes().await;
    assert_eq!(before.len(), 2);
    let old = before
        .iter()
        .find(|p| p.terminal == lost_terminal)
        .unwrap()
        .clone();
    let closed = before
        .iter()
        .find(|p| p.terminal == closed_terminal)
        .unwrap()
        .clone();
    drop(control);
    daemon.shutdown().await;

    // While the daemon is down, one child disappears from the process host
    // and the other tab's close is committed without being executed.
    let terminate = fl_prochost_proto::Request::Terminate {
        terminal: old.terminal,
        incarnation: old.incarnation,
        grace_ms: 0,
    };
    prochost.request(&terminate).await.unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let forget = fl_prochost_proto::Request::Forget {
                terminal: old.terminal,
                incarnation: old.incarnation,
            };
            if prochost.request(&forget).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    {
        let mut database = fld_store::database::Database::open(&fixture.store_root()).unwrap();
        let layout = database.default_layout().unwrap();
        let applied = database
            .apply_layout(
                OperationId::new(),
                layout.rev,
                LayoutOp::CloseTab {
                    view: layout.active,
                    tab: closed_tab,
                },
                &fixture.config(false).launch,
            )
            .unwrap();
        assert_eq!(applied.closed.len(), 1);
    }
    assert!(alive(closed.pid));

    let daemon = fixture.start().await;
    let mut control = Control::connect(&daemon.socket_path).await;
    let after = control.processes().await;
    assert_eq!(
        after.len(),
        1,
        "only the desired terminal remains: {after:?}"
    );
    let relaunched = &after[0];
    assert_eq!(relaunched.terminal, old.terminal);
    assert_ne!(
        relaunched.incarnation, old.incarnation,
        "a cold relaunch is a new incarnation"
    );
    assert_ne!(relaunched.pid, old.pid);
    assert_eq!(relaunched.state, ProcessState::Running);
    assert_eq!(
        control
            .rpc(Request::ProcessInfo {
                terminal: old.terminal,
                incarnation: old.incarnation,
            })
            .await
            .unwrap_err()
            .code,
        RpcErrorCode::WrongIncarnation
    );
    // The relaunched shell is usable.
    let (mut attached, _) = attach(
        &mut control,
        ChannelRole::TerminalHot,
        relaunched.terminal,
        relaunched.incarnation,
        None,
    )
    .await;
    let reply = attached
        .input(
            control.client,
            1,
            ViewId::new(),
            (relaunched.terminal, relaunched.incarnation),
            b"echo relaunched-$((1+1))\n",
            now_ns() + 4_000_000_000,
        )
        .await;
    assert!(
        matches!(reply, TerminalServerMsg::InputAck(_)),
        "got {reply:?}"
    );
    attached.until(|m| m.text().contains("relaunched-2")).await;

    // The committed close intent was executed: terminated, forgotten, removed.
    tokio::time::timeout(WAIT, async {
        loop {
            let Ok(fl_prochost_proto::Response::Processes(list)) =
                prochost.request(&fl_prochost_proto::Request::List).await
            else {
                panic!("process-host list")
            };
            let records = fld_store::database::Database::open(&fixture.store_root())
                .unwrap()
                .list_terminals()
                .unwrap();
            if !alive(closed.pid)
                && list.iter().all(|p| p.terminal != closed.terminal)
                && records.iter().all(|e| e.spec.terminal != closed.terminal)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("closed intent finished at startup");
    daemon.shutdown().await;
}

/// Bell and OSC 52 stores reach the subscribers attached when the output is
/// parsed, exactly once, with the engine's effect sequence; a subscriber that
/// attaches afterwards sees the screen but never a replayed notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_output_notices_reach_attached_subscribers_once() {
    let fixture = Fixture::new().await;
    let daemon = fixture.start().await;
    let mut control = Control::connect(&daemon.socket_path).await;
    let layout = control.layout().await;
    let terminal = TerminalId::new();
    let (_, op) = new_tab(&layout, terminal);
    control
        .apply_at(OperationId::new(), layout.rev, op)
        .await
        .unwrap();
    let incarnation = control.processes().await[0].incarnation;
    let target = (terminal, incarnation);
    let (mut live, _) = attach(
        &mut control,
        ChannelRole::TerminalHot,
        terminal,
        incarnation,
        None,
    )
    .await;
    let view = ViewId::new();

    // `aGVsbG8gw6k=` is base64 for "hello é".
    let id = live.channel.id();
    live.channel
        .send(TerminalClientMsg::Input {
            id,
            terminal,
            incarnation,
            input: InputId {
                client: control.client,
                sequence: 1,
            },
            view,
            bytes: b"printf '\\a\\033]52;c;aGVsbG8gw6k=\\a'; echo notice-$((40+2))\n".to_vec(),
            deadline_ns: now_ns() + 4_000_000_000,
        })
        .await;
    let mut notices = Vec::new();
    tokio::time::timeout(WAIT, async {
        while notices.len() < 2 || !live.mirror.text().contains("notice-42") {
            match live.channel.recv().await {
                TerminalServerMsg::RecordBatch(batch) => live.mirror.apply(&batch),
                TerminalServerMsg::Notice {
                    terminal: t,
                    incarnation: i,
                    sequence,
                    notice,
                } => {
                    assert_eq!((t, i), target);
                    notices.push((sequence, notice));
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("notices {notices:?}; screen:\n{}", live.mirror.text()));
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert_eq!(notices[0].1, TerminalNotice::Bell);
    assert_eq!(
        notices[1].1,
        TerminalNotice::ClipboardStore {
            target: fl_proto::ClipboardTarget::Clipboard,
            text: "hello é".into(),
        }
    );
    assert!(notices[0].0 < notices[1].0, "effect sequences increase");

    // A later subscriber reconstructs the screen and receives no notice for
    // output it did not see live.
    let (mut late, _) = attach(
        &mut control,
        ChannelRole::TerminalBackground,
        terminal,
        incarnation,
        None,
    )
    .await;
    assert!(late.mirror.text().contains("notice-42"));
    live.input(
        control.client,
        2,
        view,
        target,
        b"echo after-$((1+1))\n",
        now_ns() + 4_000_000_000,
    )
    .await;
    tokio::time::timeout(WAIT, async {
        while !late.mirror.text().contains("after-2") {
            match late.channel.recv().await {
                TerminalServerMsg::RecordBatch(batch) => late.mirror.apply(&batch),
                TerminalServerMsg::Notice { notice, .. } => {
                    panic!("replayed notice {notice:?}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("late subscriber follows live output");
    daemon.shutdown().await;
}
