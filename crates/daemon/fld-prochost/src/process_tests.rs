//! These tests use real Linux PTYs, shell children, kernel termios/winsize,
//! process groups and filesystem journals through Manager's production API.
use super::*;
use fl_model::{InputId, TerminalRecord};
use fl_prochost_proto::{DEFAULT_QUOTA_BYTES, MIN_QUOTA_BYTES};
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

struct Fixture {
    manager: Arc<Manager>,
    root: TempDir,
    terminal: TerminalId,
    incarnation: IncarnationId,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let manager = Manager::open(root.path()).unwrap();
        Self {
            manager,
            root,
            terminal: TerminalId::new(),
            incarnation: IncarnationId::new(),
        }
    }

    async fn spawn(&self, script: &str, quota_bytes: u64) -> ProcessInfo {
        let response = self
            .manager
            .dispatch(Request::Spawn(SpawnSpec {
                terminal: self.terminal,
                incarnation: self.incarnation,
                argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
                cwd: self.root.path().to_str().unwrap().into(),
                env: BTreeMap::new(),
                size: Size::new(80, 24),
                quota_bytes,
            }))
            .await
            .unwrap();
        let Response::Process(info) = response else {
            panic!("unexpected spawn response: {response:?}");
        };
        assert_ne!(info.pid, 0);
        assert_eq!(unsafe { libc::getsid(info.pid as i32) }, info.pid as i32);
        info
    }

    async fn info(&self) -> ProcessInfo {
        info(&self.manager, self.terminal, self.incarnation).await
    }

    async fn until_state(&self, predicate: impl Fn(&ProcessState) -> bool) -> ProcessInfo {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let info = self.info().await;
                if predicate(&info.state) {
                    return info;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("native PTY did not reach expected lifecycle state")
    }

    async fn records(
        &self,
        from: LogCursor,
        wait_ms: u16,
    ) -> (Vec<TerminalRecord>, LogCursor, ProcessState) {
        records(
            &self.manager,
            self.terminal,
            self.incarnation,
            from,
            wait_ms,
        )
        .await
    }

    async fn read_until(&self, cursor: &mut LogCursor, needle: &[u8]) -> Vec<TerminalRecord> {
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut collected = Vec::new();
            loop {
                let (batch, next, _) = self.records(*cursor, 1000).await;
                *cursor = next;
                collected.extend(batch);
                if output(&collected)
                    .windows(needle.len())
                    .any(|window| window == needle)
                {
                    return collected;
                }
            }
        })
        .await
        .expect("expected bytes did not arrive from native child")
    }

    async fn write(&self, id: WriteId, bytes: &[u8]) -> Response {
        write_all(&self.manager, self.terminal, self.incarnation, id, bytes).await
    }

    async fn terminate(&self) {
        self.manager
            .dispatch(Request::Terminate {
                terminal: self.terminal,
                incarnation: self.incarnation,
                grace_ms: 30,
            })
            .await
            .unwrap();
        self.until_state(|state| matches!(state, ProcessState::Exited(_)))
            .await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Panic cleanup also reaches the same production command actor; dropping
        // the manager itself intentionally does not kill a live terminal.
        let (reply, _) = oneshot::channel();
        let _ = self.manager.commands.try_send(Command {
            request: Request::Terminate {
                terminal: self.terminal,
                incarnation: self.incarnation,
                grace_ms: 0,
            },
            reply,
        });
        wake_actor(&self.manager.wake);
    }
}

async fn info(
    manager: &Arc<Manager>,
    terminal: TerminalId,
    incarnation: IncarnationId,
) -> ProcessInfo {
    let response = manager
        .dispatch(Request::Info {
            terminal,
            incarnation,
        })
        .await
        .unwrap();
    let Response::Process(info) = response else {
        panic!("unexpected info response: {response:?}");
    };
    info
}

async fn records(
    manager: &Arc<Manager>,
    terminal: TerminalId,
    incarnation: IncarnationId,
    from: LogCursor,
    wait_ms: u16,
) -> (Vec<TerminalRecord>, LogCursor, ProcessState) {
    let response = manager
        .dispatch(Request::Read {
            terminal,
            incarnation,
            from,
            max_bytes: 512 * 1024,
            wait_ms,
        })
        .await
        .unwrap();
    response.validate().unwrap();
    let Response::Records {
        records,
        next,
        state,
        ..
    } = response
    else {
        panic!("unexpected read response: {response:?}");
    };
    if let Some(first) = records.first() {
        assert_eq!(first.at, from);
    }
    (records, next, state)
}

async fn write_all(
    manager: &Arc<Manager>,
    terminal: TerminalId,
    incarnation: IncarnationId,
    id: WriteId,
    bytes: &[u8],
) -> Response {
    let deadline_ns =
        matches!(id, WriteId::Client(_)).then(|| crate::clock_ns().unwrap() + 5_000_000_000);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match manager
                .dispatch(Request::Write {
                    terminal,
                    incarnation,
                    id,
                    bytes: bytes.to_vec(),
                    deadline_ns,
                })
                .await
            {
                Ok(
                    response @ Response::Written {
                        accepted, expired, ..
                    },
                ) => {
                    assert!(!expired, "fresh native input unexpectedly expired");
                    if accepted as usize == bytes.len() {
                        return response;
                    }
                }
                Err(error) if error.code == ErrorCode::Backpressured => {}
                result => panic!("unexpected write response: {result:?}"),
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("native PTY input did not accept its complete prefix")
}

fn output(records: &[TerminalRecord]) -> Vec<u8> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            TerminalRecordKind::Output { bytes, .. } => Some(bytes.as_ref()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}

fn client_id(client: ClientSessionId, sequence: u64) -> WriteId {
    WriteId::Client(InputId { client, sequence })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_output_input_resize_echo_and_sync_are_ordered() {
    let fixture = Fixture::new();
    let spawned = fixture.spawn(
        "stty -echo; printf 'READY\\n'; IFS= read -r text; printf 'INPUT:%s\\n' \"$text\"; stty size; printf 'TAIL\\n'; exit 7",
        DEFAULT_QUOTA_BYTES,
    ).await;
    let mut cursor = LogCursor::default();
    let ready = fixture.read_until(&mut cursor, b"READY").await;
    assert!(
        ready
            .iter()
            .any(|record| matches!(record.kind, TerminalRecordKind::Echo(false)))
    );
    assert!(ready.iter().any(|record| matches!(&record.kind,
        TerminalRecordKind::Output { bytes, echo: false } if bytes.windows(5).any(|part| part == b"READY"))));
    let size = Size {
        columns: 101,
        rows: 37,
        cell_width: 9,
        cell_height: 18,
    };
    let resized = fixture
        .manager
        .dispatch(Request::Resize {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
            size,
        })
        .await
        .unwrap();
    let Response::Cursor(after_resize) = resized else {
        panic!("resize did not return a cursor");
    };
    let flushed = fixture
        .manager
        .dispatch(Request::FlushSync {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
        })
        .await
        .unwrap();
    let Response::Cursor(after_flush) = flushed else {
        panic!("flush did not return a cursor");
    };
    assert!(after_flush.next_record > after_resize.next_record);
    assert_eq!(after_flush.next_byte, after_resize.next_byte);
    let written = fixture
        .write(client_id(ClientSessionId::new(), 1), b"native-input\n")
        .await;
    assert!(matches!(
        written,
        Response::Written {
            echo: false,
            accepted: 13,
            expired: false,
            ..
        }
    ));
    let stopped = fixture
        .until_state(|state| matches!(state, ProcessState::Exited(_)))
        .await;
    assert_eq!(
        stopped.state,
        ProcessState::Exited(ExitStatus {
            code: Some(7),
            signal: None
        })
    );
    let (tail, next, _) = fixture.records(cursor, 0).await;
    assert_eq!(next, stopped.tail);
    assert!(
        matches!(tail.first().map(|record| &record.kind), Some(TerminalRecordKind::Resize(found)) if *found == size)
    );
    assert!(matches!(
        tail.get(1).map(|record| &record.kind),
        Some(TerminalRecordKind::SyncFlush)
    ));
    let bytes = output(&tail);
    assert!(bytes.windows(18).any(|part| part == b"INPUT:native-input"));
    assert!(bytes.windows(6).any(|part| part == b"37 101"));
    assert!(matches!(
        tail.last().map(|record| &record.kind),
        Some(TerminalRecordKind::Exit(_))
    ));
    assert_eq!(spawned.argv[0], "/bin/sh");
    assert_eq!(fixture.info().await.size, size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_and_engine_replays_never_reinject_native_input() {
    let fixture = Fixture::new();
    fixture.spawn("stty -echo; printf 'READY\\n'; while IFS= read -r text; do printf '[%s]\\n' \"$text\"; done", DEFAULT_QUOTA_BYTES).await;
    let mut cursor = LogCursor::default();
    fixture.read_until(&mut cursor, b"READY").await;
    let client = ClientSessionId::new();
    fixture.write(client_id(client, 10), b"alpha\n").await;
    fixture.write(client_id(client, 10), b"alpha\n").await;
    let conflict = fixture
        .manager
        .dispatch(Request::Write {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
            id: client_id(client, 10),
            bytes: b"different\n".to_vec(),
            deadline_ns: Some(crate::clock_ns().unwrap() + 1_000_000_000),
        })
        .await
        .unwrap_err();
    assert_eq!(conflict.code, ErrorCode::InputConflict);
    fixture.write(client_id(client, 11), b"beta\n").await;
    let old = fixture
        .manager
        .dispatch(Request::Write {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
            id: client_id(client, 10),
            bytes: b"alpha\n".to_vec(),
            deadline_ns: Some(crate::clock_ns().unwrap() + 1_000_000_000),
        })
        .await
        .unwrap_err();
    assert_eq!(old.code, ErrorCode::InputConflict);
    fixture.write(WriteId::Effect(7), b"gamma\n").await;
    fixture.write(WriteId::Effect(7), b"gamma\n").await;
    fixture.write(WriteId::Effect(8), b"delta\n").await;
    fixture.write(WriteId::Effect(7), b"gamma\n").await;
    let received = fixture.read_until(&mut cursor, b"[delta]").await;
    fixture.terminate().await;
    let (remaining, _, _) = fixture.records(cursor, 0).await;
    let bytes = [output(&received), output(&remaining)].concat();
    for needle in [b"[alpha]".as_slice(), b"[beta]", b"[gamma]", b"[delta]"] {
        assert_eq!(
            bytes
                .windows(needle.len())
                .filter(|window| *window == needle)
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_drain_retains_more_than_hot_ring_without_a_reader() {
    let fixture = Fixture::new();
    let count = 3 * 1024 * 1024;
    fixture
        .spawn(
            &format!("head -c {count} /dev/zero | tr '\\000' x; printf 'FINAL'; exit 9"),
            8 * 1024 * 1024,
        )
        .await;
    let info = fixture
        .until_state(|state| matches!(state, ProcessState::Exited(_)))
        .await;
    assert_eq!(
        info.state,
        ProcessState::Exited(ExitStatus {
            code: Some(9),
            signal: None
        })
    );
    let mut cursor = LogCursor::default();
    let mut bytes = Vec::new();
    let mut last = None;
    while cursor != info.tail {
        let (records, next, _) = fixture.records(cursor, 0).await;
        assert_ne!(next, cursor);
        bytes.extend(output(&records));
        last = records.last().map(|record| record.kind.clone());
        cursor = next;
    }
    assert_eq!(bytes.len(), count + 5);
    assert!(bytes[..count].iter().all(|byte| *byte == b'x'));
    assert_eq!(&bytes[count..], b"FINAL");
    assert!(matches!(last, Some(TerminalRecordKind::Exit(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_pauses_and_checkpoint_ack_resumes_without_losing_output() {
    let fixture = Fixture::new();
    let count = 1024 * 1024;
    fixture
        .spawn(
            &format!("head -c {count} /dev/zero | tr '\\000' q; printf 'DONE'"),
            MIN_QUOTA_BYTES,
        )
        .await;
    let paused = fixture
        .until_state(|state| matches!(state, ProcessState::Backpressured { .. }))
        .await;
    let retained = match paused.state {
        ProcessState::Backpressured {
            retained_bytes,
            quota_bytes,
            ..
        } => {
            assert_eq!(quota_bytes, MIN_QUOTA_BYTES);
            retained_bytes
        }
        _ => unreachable!(),
    };
    assert!(retained > 0 && retained <= MIN_QUOTA_BYTES);
    let mut cursor = LogCursor::default();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (batch, next, state) = fixture.records(cursor, 1000).await;
            bytes.extend(output(&batch));
            cursor = next;
            fixture
                .manager
                .dispatch(Request::AckCheckpoint {
                    terminal: fixture.terminal,
                    incarnation: fixture.incarnation,
                    through: cursor,
                })
                .await
                .unwrap();
            if matches!(state, ProcessState::Exited(_)) && cursor == fixture.info().await.tail {
                break;
            }
        }
    })
    .await
    .expect("checkpoint reclamation did not unblock native child");
    assert_eq!(bytes.len(), count + 4);
    assert!(bytes[..count].iter().all(|byte| *byte == b'q'));
    assert_eq!(&bytes[count..], b"DONE");
    let expired = fixture
        .manager
        .dispatch(Request::Read {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
            from: LogCursor::default(),
            max_bytes: 65536,
            wait_ms: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(expired.code, ErrorCode::CursorExpired);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raising_quota_unblocks_pending_bytes_and_idle_shutdown_excludes_spawn() {
    let fixture = Fixture::new();
    let count = 512 * 1024;
    fixture
        .spawn(
            &format!("head -c {count} /dev/zero | tr '\\000' r"),
            MIN_QUOTA_BYTES,
        )
        .await;
    fixture
        .until_state(|state| matches!(state, ProcessState::Backpressured { .. }))
        .await;
    let denied = fixture
        .manager
        .dispatch(Request::ShutdownIfIdle)
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::NotStopped);
    assert!(!fixture.manager.shutdown_token().is_cancelled());
    fixture
        .manager
        .dispatch(Request::SetQuota {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
            bytes: 2 * 1024 * 1024,
        })
        .await
        .unwrap();
    let info = fixture
        .until_state(|state| matches!(state, ProcessState::Exited(_)))
        .await;
    let mut cursor = LogCursor::default();
    let mut bytes = Vec::new();
    while cursor != info.tail {
        let (batch, next, _) = fixture.records(cursor, 0).await;
        bytes.extend(output(&batch));
        cursor = next;
    }
    assert_eq!(bytes, vec![b'r'; count]);
    fixture
        .manager
        .dispatch(Request::Forget {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
        })
        .await
        .unwrap();
    assert!(
        matches!(fixture.manager.dispatch(Request::List).await.unwrap(), Response::Processes(entries) if entries.is_empty())
    );
    fixture
        .manager
        .dispatch(Request::ShutdownIfIdle)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.manager.shutdown_token().cancelled(),
    )
    .await
    .unwrap();
    assert!(
        fixture
            .manager
            .dispatch(Request::Spawn(SpawnSpec {
                terminal: TerminalId::new(),
                incarnation: IncarnationId::new(),
                argv: vec!["/bin/true".into()],
                cwd: "/".into(),
                env: BTreeMap::new(),
                size: Size::new(80, 24),
                quota_bytes: MIN_QUOTA_BYTES,
            }))
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_all_rpc_owners_preserves_child_and_recovery_marks_lost() {
    let root = tempfile::tempdir().unwrap();
    let manager = Manager::open(root.path()).unwrap();
    let terminal = TerminalId::new();
    let incarnation = IncarnationId::new();
    manager.dispatch(Request::Spawn(SpawnSpec {
        terminal, incarnation,
        argv: vec!["/bin/sh".into(), "-c".into(),
            "stty -echo; printf 'READY'; sleep 0.15; printf 'SURVIVED'; printf yes > survived; sleep 0.15".into()],
        cwd: root.path().to_str().unwrap().into(), env: BTreeMap::new(),
        size: Size::new(80, 24), quota_bytes: MIN_QUOTA_BYTES,
    })).await.unwrap();
    let initial = info(&manager, terminal, incarnation).await;
    let dir = root
        .path()
        .join("state/prochost")
        .join(terminal.to_string())
        .join(incarnation.to_string());
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(dir.join("process.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(manager);
    assert!(
        Manager::open(root.path()).is_err(),
        "a disconnected live owner must retain its singleton lock"
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.path().join("survived").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dropping RPC ownership killed the native child");
    let recovered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match Manager::open(root.path()) {
                Ok(manager) => return manager,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("drained detached owner did not release the singleton lock");
    let recovered_info = info(&recovered, terminal, incarnation).await;
    assert_eq!(recovered_info.pid, initial.pid);
    assert_eq!(recovered_info.state, ProcessState::Lost);
    let (records, next, state) =
        records(&recovered, terminal, incarnation, LogCursor::default(), 0).await;
    assert_eq!(next, recovered_info.tail);
    assert_eq!(state, ProcessState::Lost);
    assert_eq!(output(&records), b"READYSURVIVED");
    assert!(matches!(
        records.last().map(|record| &record.kind),
        Some(TerminalRecordKind::Exit(_))
    ));
    recovered
        .dispatch(Request::Forget {
            terminal,
            incarnation,
        })
        .await
        .unwrap();
    recovered.dispatch(Request::ShutdownIfIdle).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_escalates_across_real_background_session_members() {
    let fixture = Fixture::new();
    fixture.spawn("stty -echo; trap '' HUP TERM; (trap '' HUP TERM; while :; do sleep 1; done) & printf 'BACKGROUND:%s\\n' \"$!\"; wait", DEFAULT_QUOTA_BYTES).await;
    let mut cursor = LogCursor::default();
    let ready = fixture.read_until(&mut cursor, b"\n").await;
    let bytes = String::from_utf8(output(&ready)).unwrap();
    let pid: i32 = bytes
        .split("BACKGROUND:")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let background = process_identity(pid).unwrap();
    assert_eq!(background.session, fixture.info().await.pid as i32);
    fixture.terminate().await;
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        assert!(
            stat.rsplit_once(')')
                .unwrap()
                .1
                .trim_start()
                .starts_with('Z'),
            "background job is still alive: {stat}"
        );
    }
    let (tail, _, _) = fixture.records(cursor, 0).await;
    assert!(matches!(
        tail.last().map(|record| &record.kind),
        Some(TerminalRecordKind::Exit(ExitStatus {
            signal: Some(libc::SIGKILL),
            ..
        }))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_partial_input_keeps_prefix_and_cannot_extend_original_deadline() {
    let fixture = Fixture::new();
    fixture.spawn(
        "stty raw -echo; printf READY; while [ ! -f start-reading ]; do sleep 0.01; done; dd bs=1 count=2097153 of=received 2>/dev/null",
        DEFAULT_QUOTA_BYTES,
    ).await;
    let mut cursor = LogCursor::default();
    fixture.read_until(&mut cursor, b"READY").await;
    let client = ClientSessionId::new();
    let bytes = vec![b'a'; 65536];
    let mut fully_accepted = 0usize;
    let mut partial = None;
    // Fill the actual kernel queue while the native reader is gated. Do not
    // assume a particular Linux PTY buffer capacity or manufacture a short write.
    for sequence in 1..=32 {
        let response = fixture
            .manager
            .dispatch(Request::Write {
                terminal: fixture.terminal,
                incarnation: fixture.incarnation,
                id: client_id(client, sequence),
                bytes: bytes.clone(),
                deadline_ns: Some(crate::clock_ns().unwrap() + 1_000_000_000),
            })
            .await
            .unwrap();
        let Response::Written {
            accepted,
            expired: false,
            ..
        } = response
        else {
            panic!("unexpected native write: {response:?}");
        };
        if accepted < 65536 {
            partial = Some((sequence, accepted));
            break;
        }
        fully_accepted += accepted as usize;
    }
    let (sequence, accepted) = partial.expect("native PTY queue did not backpressure within 2 MiB");
    let total = fully_accepted + accepted as usize;
    assert!(total > 0);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let expired = fixture
        .manager
        .dispatch(Request::Write {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
            id: client_id(client, sequence),
            bytes,
            deadline_ns: Some(crate::clock_ns().unwrap() + 5_000_000_000),
        })
        .await
        .unwrap();
    assert!(
        matches!(expired, Response::Written { accepted: prefix, expired: true, .. } if prefix == accepted)
    );
    fs::write(fixture.root.path().join("start-reading"), []).unwrap();
    fixture.write(client_id(client, sequence + 1), b"Z").await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if fs::metadata(fixture.root.path().join("received"))
                .is_ok_and(|metadata| metadata.len() == total as u64 + 1)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native reader did not receive the accepted prefix and later sequence");
    fixture.terminate().await;
    let received = fs::read(fixture.root.path().join("received")).unwrap();
    assert_eq!(received.len(), total + 1);
    assert!(received[..total].iter().all(|byte| *byte == b'a'));
    assert_eq!(received[total], b'Z');
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn already_expired_input_is_never_written_and_same_id_conflicts_remain_checked() {
    let fixture = Fixture::new();
    fixture
        .spawn(
            "stty -echo; printf READY; IFS= read -r text; printf 'GOT:%s' \"$text\"",
            DEFAULT_QUOTA_BYTES,
        )
        .await;
    let mut cursor = LogCursor::default();
    fixture.read_until(&mut cursor, b"READY").await;
    let client = ClientSessionId::new();
    let expired = fixture
        .manager
        .dispatch(Request::Write {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
            id: client_id(client, 1),
            bytes: b"must-not-run\n".to_vec(),
            deadline_ns: Some(0),
        })
        .await
        .unwrap();
    assert!(matches!(
        expired,
        Response::Written {
            accepted: 0,
            expired: true,
            ..
        }
    ));
    let conflict = fixture
        .manager
        .dispatch(Request::Write {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
            id: client_id(client, 1),
            bytes: b"changed\n".to_vec(),
            deadline_ns: Some(crate::clock_ns().unwrap() + 5_000_000_000),
        })
        .await
        .unwrap_err();
    assert_eq!(conflict.code, ErrorCode::InputConflict);
    fixture.write(client_id(client, 2), b"fresh\n").await;
    fixture
        .until_state(|state| matches!(state, ProcessState::Exited(_)))
        .await;
    let (tail, _, _) = fixture.records(cursor, 0).await;
    assert_eq!(output(&tail), b"GOT:fresh");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_during_draining_kills_slave_holders_after_leader_was_reaped() {
    let fixture = Fixture::new();
    fixture.spawn(
        "stty -echo; (trap '' HUP TERM; printf HOLDER; touch armed; sleep 30) & while [ ! -f armed ]; do sleep 0.01; done; exit 0",
        DEFAULT_QUOTA_BYTES,
    ).await;
    let draining = fixture
        .until_state(|state| matches!(state, ProcessState::Draining(_)))
        .await;
    assert_eq!(
        draining.state,
        ProcessState::Draining(ExitStatus {
            code: Some(0),
            signal: None
        })
    );
    let denied = fixture
        .manager
        .dispatch(Request::Forget {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
        })
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::NotStopped);
    fixture.terminate().await;
    let (records, next, _) = fixture.records(LogCursor::default(), 0).await;
    assert_eq!(next, fixture.info().await.tail);
    assert_eq!(output(&records), b"HOLDER");
    assert!(matches!(
        records.last().map(|record| &record.kind),
        Some(TerminalRecordKind::Exit(ExitStatus {
            code: Some(0),
            signal: None
        }))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_native_launch_does_not_register_a_process() {
    let fixture = Fixture::new();
    fixture
        .manager
        .dispatch(Request::Spawn(SpawnSpec {
            terminal: fixture.terminal,
            incarnation: fixture.incarnation,
            argv: vec![
                fixture
                    .root
                    .path()
                    .join("missing-program")
                    .to_str()
                    .unwrap()
                    .into(),
            ],
            cwd: fixture.root.path().to_str().unwrap().into(),
            env: BTreeMap::new(),
            size: Size::new(80, 24),
            quota_bytes: MIN_QUOTA_BYTES,
        }))
        .await
        .unwrap_err();
    assert!(
        matches!(fixture.manager.dispatch(Request::List).await.unwrap(),
        Response::Processes(entries) if entries.is_empty())
    );
}
