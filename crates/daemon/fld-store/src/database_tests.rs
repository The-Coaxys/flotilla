use super::*;
use fl_model::{PaneId, SplitId, TabId, layout::Axis};
use std::{fs, os::unix::fs::symlink};
use tempfile::TempDir;

fn defaults() -> LaunchDefaults {
    LaunchDefaults {
        cwd: "/srv/work".into(),
        argv: vec!["/bin/sh".into(), "-l".into()],
        env: BTreeMap::from([("TERM".into(), "xterm-256color".into())]),
        quota_bytes: fl_prochost_proto::DEFAULT_QUOTA_BYTES,
    }
}

fn new_tab(layout: &Layout, terminal: TerminalId) -> LayoutOp {
    LayoutOp::NewTab {
        view: layout.active,
        tab: TabId::new(),
        pane: PaneId::new(),
        terminal,
        name: Some("Shell".into()),
        size: Size::new(80, 24),
    }
}

fn create_terminal(database: &mut Database) -> (OperationId, LayoutOp, TerminalEntry) {
    let layout = database.default_layout().unwrap();
    let operation = OperationId::new();
    let op = new_tab(&layout, TerminalId::new());
    let applied = database
        .apply_layout(operation, layout.rev, op.clone(), &defaults())
        .unwrap();
    assert!(!applied.replayed);
    assert!(applied.closed.is_empty());
    (operation, op, applied.spawned.unwrap())
}

fn process(entry: &TerminalEntry) -> ProcessInfo {
    ProcessInfo {
        terminal: entry.spec.terminal,
        incarnation: entry.spec.incarnation,
        pid: 1234,
        size: entry.spec.size,
        argv: entry.spec.argv.clone(),
        cwd: entry.spec.cwd.clone(),
        state: ProcessState::Running,
        tail: LogCursor {
            next_record: 10,
            next_byte: 80,
        },
        echo: true,
    }
}

fn checkpoint(record: u64, byte: u64, hash: u8) -> CheckpointRef {
    CheckpointRef {
        hash: [hash; 32],
        cursor: LogCursor {
            next_record: record,
            next_byte: byte,
        },
        size: Size::new(80, 24),
        engine_id: "test-engine/1".into(),
    }
}

fn snapshot(database: &Database) -> (Layout, Vec<u8>, u64) {
    (
        database.default_layout().unwrap(),
        postcard::to_stdvec(&database.list_terminals().unwrap()).unwrap(),
        database
            .connection
            .query_row("SELECT count(*) FROM operations", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as u64)
            .unwrap(),
    )
}

fn close_tab(database: &mut Database, tab: TabId) -> AppliedLayout {
    let layout = database.default_layout().unwrap();
    database
        .apply_layout(
            OperationId::new(),
            layout.rev,
            LayoutOp::CloseTab {
                view: layout.active,
                tab,
            },
            &defaults(),
        )
        .unwrap()
}

fn tab_id(op: &LayoutOp) -> TabId {
    match op {
        LayoutOp::NewTab { tab, .. } => *tab,
        _ => unreachable!(),
    }
}

fn backups(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("state.db.before-")
        })
        .collect()
}

fn version_one(root: &Path, layout: Option<&Layout>) -> Connection {
    let connection = Connection::open(root.join("state.db")).unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    connection.execute_batch(SCHEMA_V1).unwrap();
    if let Some(layout) = layout {
        connection
            .execute(
                "INSERT INTO layout(singleton,state) VALUES (1,?1)",
                [encode_layout(layout).unwrap()],
            )
            .unwrap();
    }
    connection.pragma_update(None, "user_version", 1).unwrap();
    connection
}

#[test]
fn opens_private_real_sqlite_in_wal_full_mode() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("nested/state");
    let database = Database::open(&root).unwrap();
    let mode: String = database
        .connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    let synchronous: i64 = database
        .connection
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(synchronous, 2);
    assert_eq!(
        schema_version(&database.connection).unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
    for name in ["state.db", "state.db-wal", "state.db-shm"] {
        assert_eq!(fs::metadata(root.join(name)).unwrap().mode() & 0o777, 0o600);
    }
    assert!(backups(&root).is_empty());
}

#[test]
fn default_workspace_and_view_ids_survive_reopening() {
    let temporary = TempDir::new().unwrap();
    let first = {
        let database = Database::open(temporary.path()).unwrap();
        let layout = database.default_layout().unwrap();
        assert_eq!(layout.rev, 0);
        assert_eq!(layout.views.len(), 1);
        assert!(layout.views[0].tabs.is_empty());
        assert_eq!(
            layout.split.leaves().collect::<Vec<_>>(),
            vec![layout.active]
        );
        layout
    };
    let second = Database::open(temporary.path()).unwrap();
    assert_eq!(second.default_layout().unwrap(), first);
    assert!(second.list_terminals().unwrap().is_empty());
    assert!(backups(temporary.path()).is_empty());
}

#[test]
fn spawn_intent_spec_and_process_metadata_survive_restart_without_spawning() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, _, entry) = create_terminal(&mut database);
    assert!(entry.desired);
    assert!(entry.process.is_none());
    assert!(entry.launch_error.is_none());
    assert_eq!(entry.spec.argv, defaults().argv);
    assert_eq!(entry.spec.env, defaults().env);
    let key = key_of(&entry);
    let info = process(&entry);
    database.update_process(&info).unwrap();
    database
        .set_launch_error(key, Some("launch attempt interrupted"))
        .unwrap();
    let before = snapshot(&database);
    drop(database);
    let mut database = Database::open(temporary.path()).unwrap();
    assert_eq!(snapshot(&database), before);
    let restored = database.terminal(key).unwrap();
    assert_eq!(restored.spec, entry.spec);
    assert_eq!(restored.process.as_ref(), Some(&info));
    assert_eq!(
        restored.launch_error.as_deref(),
        Some("launch attempt interrupted")
    );
    database.set_launch_error(key, None).unwrap();
    assert!(database.terminal(key).unwrap().launch_error.is_none());
}

#[test]
fn stable_retry_returns_current_layout_without_spawn_close_or_policy_side_effects() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (operation, op, entry) = create_terminal(&mut database);
    let layout = database.default_layout().unwrap();
    let renamed = database
        .apply_layout(
            OperationId::new(),
            layout.rev,
            LayoutOp::RenameTab {
                view: layout.active,
                tab: tab_id(&op),
                name: Some("Renamed".into()),
            },
            &defaults(),
        )
        .unwrap()
        .layout;
    let before = snapshot(&database);
    let mut changed_defaults = defaults();
    changed_defaults.argv = vec!["\0".into()];
    changed_defaults.quota_bytes = 0;
    let retry = database
        .apply_layout(operation, 0, op.clone(), &changed_defaults)
        .unwrap();
    assert!(retry.replayed);
    assert_eq!(retry.layout, renamed);
    assert!(retry.spawned.is_none());
    assert!(retry.closed.is_empty());
    assert_eq!(database.terminal(key_of(&entry)).unwrap().spec, entry.spec);
    assert_eq!(snapshot(&database), before);
    drop(database);
    let mut database = Database::open(temporary.path()).unwrap();
    assert!(
        database
            .apply_layout(operation, 0, op, &defaults())
            .unwrap()
            .replayed
    );
    assert_eq!(snapshot(&database), before);
}

#[test]
fn reused_operation_id_with_changed_payload_or_base_revision_is_rejected() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (operation, op, _) = create_terminal(&mut database);
    let before = snapshot(&database);
    assert!(
        matches!(database.apply_layout(operation, 1, op.clone(), &defaults()), Err(StoreError::OperationConflict(id)) if id == operation)
    );
    let other = new_tab(&before.0, TerminalId::new());
    assert!(
        matches!(database.apply_layout(operation, 0, other, &defaults()), Err(StoreError::OperationConflict(id)) if id == operation)
    );
    assert_eq!(snapshot(&database), before);
}

#[test]
fn revision_conflict_across_real_connections_does_not_record_or_mutate_the_operation() {
    let temporary = TempDir::new().unwrap();
    let mut first = Database::open(temporary.path()).unwrap();
    let mut second = Database::open(temporary.path()).unwrap();
    let stale = second.default_layout().unwrap();
    create_terminal(&mut first);
    let operation = OperationId::new();
    let op = LayoutOp::SelectView { view: stale.active };
    let before = snapshot(&first);
    assert!(matches!(
        second.apply_layout(operation, stale.rev, op.clone(), &defaults()),
        Err(StoreError::RevisionConflict {
            expected: 1,
            got: 0
        })
    ));
    assert_eq!(snapshot(&first), before);
    let applied = second.apply_layout(operation, 1, op, &defaults()).unwrap();
    assert!(!applied.replayed);
    assert_eq!(applied.layout.rev, 2);
    assert_eq!(first.default_layout().unwrap(), applied.layout);
}

#[test]
fn invalid_defaults_leave_no_layout_row_terminal_or_dedup_mutation() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let before = snapshot(&database);
    let operation = OperationId::new();
    let op = new_tab(&before.0, TerminalId::new());
    let mut invalid = defaults();
    invalid.env.insert("INVALID=KEY".into(), "x".into());
    assert!(matches!(
        database.apply_layout(operation, 0, op.clone(), &invalid),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(snapshot(&database), before);
    assert!(
        database
            .apply_layout(operation, 0, op, &defaults())
            .unwrap()
            .spawned
            .is_some()
    );
}

#[test]
fn sqlite_failure_after_layout_and_spawn_writes_rolls_back_everything() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let before = snapshot(&database);
    database.connection.execute_batch("CREATE TRIGGER fail_operation BEFORE INSERT ON operations BEGIN SELECT RAISE(ABORT, 'injected commit failure'); END;").unwrap();
    let operation = OperationId::new();
    let op = new_tab(&before.0, TerminalId::new());
    assert!(matches!(
        database.apply_layout(operation, 0, op.clone(), &defaults()),
        Err(StoreError::Sqlite(_))
    ));
    assert_eq!(snapshot(&database), before);
    database
        .connection
        .execute_batch("DROP TRIGGER fail_operation")
        .unwrap();
    let applied = database
        .apply_layout(operation, 0, op, &defaults())
        .unwrap();
    assert!(!applied.replayed);
    assert!(applied.spawned.is_some());
}

#[test]
fn failed_close_rolls_back_desired_intent_and_layout_together() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, op, entry) = create_terminal(&mut database);
    let before = snapshot(&database);
    database.connection.execute_batch("CREATE TRIGGER fail_operation BEFORE INSERT ON operations BEGIN SELECT RAISE(ABORT, 'injected close failure'); END;").unwrap();
    let close = LayoutOp::CloseTab {
        view: before.0.active,
        tab: tab_id(&op),
    };
    assert!(matches!(
        database.apply_layout(OperationId::new(), before.0.rev, close, &defaults()),
        Err(StoreError::Sqlite(_))
    ));
    assert_eq!(snapshot(&database), before);
    assert!(database.terminal(key_of(&entry)).unwrap().desired);
}

#[test]
fn approved_view_tab_pane_hierarchy_and_close_intents_are_persisted() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, first_op, first) = create_terminal(&mut database);
    let layout = database.default_layout().unwrap();
    let view = layout.active;
    let tab = tab_id(&first_op);
    let source = layout.tab(view, tab).unwrap().active;
    let second = database
        .apply_layout(
            OperationId::new(),
            layout.rev,
            LayoutOp::SplitPane {
                view,
                tab,
                source,
                pane: PaneId::new(),
                terminal: TerminalId::new(),
                split: SplitId::new(),
                axis: Axis::Horizontal,
                size: Size::new(40, 24),
            },
            &defaults(),
        )
        .unwrap()
        .spawned
        .unwrap();
    assert!(matches!(
        database.remove_terminal(key_of(&first)),
        Err(StoreError::TerminalActive)
    ));
    let layout = database.default_layout().unwrap();
    let third = database
        .apply_layout(
            OperationId::new(),
            layout.rev,
            LayoutOp::SplitView {
                source: view,
                view: ViewId::new(),
                tab: TabId::new(),
                pane: PaneId::new(),
                terminal: TerminalId::new(),
                split: SplitId::new(),
                axis: Axis::Vertical,
                size: Size::new(80, 24),
            },
            &defaults(),
        )
        .unwrap()
        .spawned
        .unwrap();
    let layout = database.default_layout().unwrap();
    assert_eq!(layout.views.len(), 2);
    assert_eq!(layout.tab(view, tab).unwrap().panes.len(), 2);
    let applied = database
        .apply_layout(
            OperationId::new(),
            layout.rev,
            LayoutOp::CloseView { view },
            &defaults(),
        )
        .unwrap();
    assert_eq!(
        applied.closed.as_slice(),
        &[key_of(&first), key_of(&second)]
    );
    assert!(!database.terminal(key_of(&first)).unwrap().desired);
    assert!(!database.terminal(key_of(&second)).unwrap().desired);
    assert!(database.terminal(key_of(&third)).unwrap().desired);
    let before = snapshot(&database);
    drop(database);
    let mut database = Database::open(temporary.path()).unwrap();
    assert_eq!(snapshot(&database), before);
    database.remove_terminal(key_of(&first)).unwrap();
    assert!(matches!(
        database.terminal(key_of(&first)),
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(database.list_terminals().unwrap().len(), 2);
    assert!(matches!(
        database.remove_terminal(key_of(&third)),
        Err(StoreError::TerminalActive)
    ));
}

#[test]
fn close_retry_after_cleanup_does_not_recreate_or_close_anything() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (spawn_operation, spawn_op, entry) = create_terminal(&mut database);
    let layout = database.default_layout().unwrap();
    let operation = OperationId::new();
    let op = LayoutOp::CloseTab {
        view: layout.active,
        tab: tab_id(&spawn_op),
    };
    let applied = database
        .apply_layout(operation, layout.rev, op.clone(), &defaults())
        .unwrap();
    assert_eq!(applied.closed.as_slice(), &[key_of(&entry)]);
    database.remove_terminal(key_of(&entry)).unwrap();
    let before = snapshot(&database);
    let replay = database
        .apply_layout(operation, layout.rev, op, &defaults())
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.closed.is_empty());
    assert!(replay.spawned.is_none());
    let replay = database
        .apply_layout(spawn_operation, 0, spawn_op, &defaults())
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.spawned.is_none());
    assert_eq!(snapshot(&database), before);
}

#[test]
fn every_terminal_mutation_compare_and_swaps_incarnation() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, _, entry) = create_terminal(&mut database);
    let wrong = TerminalKey {
        terminal: entry.spec.terminal,
        incarnation: IncarnationId::new(),
    };
    let mut stale_process = process(&entry);
    stale_process.incarnation = wrong.incarnation;
    let before = snapshot(&database);
    assert!(matches!(
        database.terminal(wrong),
        Err(StoreError::WrongIncarnation)
    ));
    assert!(matches!(
        database.set_launch_error(wrong, Some("stale")),
        Err(StoreError::WrongIncarnation)
    ));
    assert!(matches!(
        database.update_process(&stale_process),
        Err(StoreError::WrongIncarnation)
    ));
    assert!(matches!(
        database.save_checkpoint_ref(wrong, &checkpoint(10, 80, 1)),
        Err(StoreError::WrongIncarnation)
    ));
    assert!(matches!(
        database.remove_terminal(wrong),
        Err(StoreError::WrongIncarnation)
    ));
    assert_eq!(snapshot(&database), before);
}

#[test]
fn reusing_a_terminal_id_requires_cleanup_and_gets_a_fresh_incarnation() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, first_op, first) = create_terminal(&mut database);
    close_tab(&mut database, tab_id(&first_op));
    let layout = database.default_layout().unwrap();
    let op = new_tab(&layout, first.spec.terminal);
    let operation = OperationId::new();
    assert!(matches!(
        database.apply_layout(operation, layout.rev, op.clone(), &defaults()),
        Err(StoreError::Layout(LayoutError::DuplicateTerminal(_)))
    ));
    database.remove_terminal(key_of(&first)).unwrap();
    let next = database
        .apply_layout(operation, layout.rev, op, &defaults())
        .unwrap()
        .spawned
        .unwrap();
    assert_ne!(next.spec.incarnation, first.spec.incarnation);
    assert!(next.desired);
    assert!(next.checkpoint.is_none());
    assert!(next.process.is_none());
    assert!(matches!(
        database.save_checkpoint_ref(key_of(&first), &checkpoint(10, 80, 1)),
        Err(StoreError::WrongIncarnation)
    ));
    assert!(matches!(
        database.update_process(&process(&first)),
        Err(StoreError::WrongIncarnation)
    ));
    assert!(matches!(
        database.set_launch_error(key_of(&first), Some("old launch")),
        Err(StoreError::WrongIncarnation)
    ));
    assert!(matches!(
        database.remove_terminal(key_of(&first)),
        Err(StoreError::WrongIncarnation)
    ));
}

#[test]
fn checkpoint_cursors_are_monotonic_and_same_record_cannot_change_byte_count() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, _, entry) = create_terminal(&mut database);
    let key = key_of(&entry);
    let first = checkpoint(10, 80, 1);
    database.save_checkpoint_ref(key, &first).unwrap();
    let before = snapshot(&database);
    for reference in [
        checkpoint(9, 80, 2),
        checkpoint(11, 79, 2),
        checkpoint(10, 81, 2),
        checkpoint(10, 79, 2),
    ] {
        assert!(matches!(
            database.save_checkpoint_ref(key, &reference),
            Err(StoreError::StaleCheckpoint)
        ));
        assert_eq!(snapshot(&database), before);
    }
    database.save_checkpoint_ref(key, &first).unwrap();
    let control_only = checkpoint(11, 80, 2);
    database.save_checkpoint_ref(key, &control_only).unwrap();
    drop(database);
    let database = Database::open(temporary.path()).unwrap();
    assert_eq!(
        database.terminal(key).unwrap().checkpoint,
        Some(control_only)
    );
    assert_eq!(database.checkpoint_hashes().unwrap(), vec![[2; 32]]);
}

#[test]
fn checkpoint_hashes_include_closed_cleanup_obligations_and_are_unique() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, first_op, first) = create_terminal(&mut database);
    let (_, _, second) = create_terminal(&mut database);
    database
        .save_checkpoint_ref(key_of(&first), &checkpoint(10, 80, 1))
        .unwrap();
    database
        .save_checkpoint_ref(key_of(&second), &checkpoint(10, 80, 1))
        .unwrap();
    close_tab(&mut database, tab_id(&first_op));
    assert_eq!(database.checkpoint_hashes().unwrap(), vec![[1; 32]]);
    database.remove_terminal(key_of(&first)).unwrap();
    assert_eq!(database.checkpoint_hashes().unwrap(), vec![[1; 32]]);
    database
        .save_checkpoint_ref(key_of(&second), &checkpoint(11, 90, 2))
        .unwrap();
    assert_eq!(database.checkpoint_hashes().unwrap(), vec![[2; 32]]);
}

#[test]
fn all_u64_counter_bits_survive_sqlite_and_revision_overflow_rolls_back() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, _, entry) = create_terminal(&mut database);
    let mut info = process(&entry);
    info.tail = LogCursor {
        next_record: u64::MAX,
        next_byte: u64::MAX,
    };
    info.state = ProcessState::Backpressured {
        retained_bytes: u64::MAX,
        quota_bytes: u64::MAX,
        reason: "disk full".into(),
    };
    database.update_process(&info).unwrap();
    database
        .save_checkpoint_ref(key_of(&entry), &checkpoint(u64::MAX, u64::MAX, 1))
        .unwrap();
    let mut layout = database.default_layout().unwrap();
    layout.rev = u64::MAX - 1;
    write_layout(&database.connection, &layout).unwrap();
    let operation = OperationId::new();
    let op = LayoutOp::SelectView {
        view: layout.active,
    };
    let applied = database
        .apply_layout(operation, layout.rev, op.clone(), &defaults())
        .unwrap();
    assert_eq!(applied.layout.rev, u64::MAX);
    assert!(
        database
            .apply_layout(operation, layout.rev, op.clone(), &defaults())
            .unwrap()
            .replayed
    );
    let before = snapshot(&database);
    assert!(matches!(
        database.apply_layout(OperationId::new(), u64::MAX, op, &defaults()),
        Err(StoreError::Layout(LayoutError::RevisionOverflow))
    ));
    assert_eq!(snapshot(&database), before);
    drop(database);
    let database = Database::open(temporary.path()).unwrap();
    assert_eq!(snapshot(&database), before);
    let restored = database.terminal(key_of(&entry)).unwrap();
    assert_eq!(restored.process, Some(info));
    assert_eq!(restored.checkpoint.unwrap().cursor.next_byte, u64::MAX);
}

#[test]
fn metadata_bounds_reject_inputs_without_mutation() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, _, entry) = create_terminal(&mut database);
    let before = snapshot(&database);
    let mut info = process(&entry);
    info.argv = vec!["x".into(); MAX_ARG_COUNT + 1];
    assert!(matches!(
        database.update_process(&info),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        database.set_launch_error(key_of(&entry), Some(&"x".repeat(MAX_TEXT_BYTES + 1))),
        Err(StoreError::Invalid(_))
    ));
    let mut reference = checkpoint(10, 80, 1);
    reference.engine_id = "invalid\nengine".into();
    assert!(matches!(
        database.save_checkpoint_ref(key_of(&entry), &reference),
        Err(StoreError::Invalid(_))
    ));
    reference.engine_id = "engine".into();
    reference.size.columns = 0;
    assert!(matches!(
        database.save_checkpoint_ref(key_of(&entry), &reference),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(snapshot(&database), before);
}

#[test]
fn missing_cross_references_fail_closed_without_reinitializing_layout() {
    let temporary = TempDir::new().unwrap();
    let database = Database::open(temporary.path()).unwrap();
    let mut layout = database.default_layout().unwrap();
    layout.apply(new_tab(&layout, TerminalId::new())).unwrap();
    write_layout(&database.connection, &layout).unwrap();
    assert!(matches!(
        database.default_layout(),
        Err(StoreError::Corrupt(_))
    ));
    drop(database);
    assert!(matches!(
        Database::open(temporary.path()),
        Err(StoreError::Corrupt(_))
    ));
    let connection = Connection::open(temporary.path().join("state.db")).unwrap();
    let bytes: Vec<u8> = connection
        .query_row("SELECT state FROM layout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(decode::<Layout>(&bytes, "layout").unwrap(), layout);
}

#[test]
fn malformed_terminal_identity_encoding_bounds_and_suffix_are_rejected() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, _, entry) = create_terminal(&mut database);
    let mut wrong = entry.clone();
    wrong.spec.incarnation = IncarnationId::new();
    database
        .connection
        .execute(
            "UPDATE terminals SET entry=?1",
            [postcard::to_stdvec(&wrong).unwrap()],
        )
        .unwrap();
    assert!(matches!(
        database.list_terminals(),
        Err(StoreError::Corrupt(_))
    ));
    let mut oversized = entry.clone();
    oversized.spec.argv = vec![String::new(); MAX_ARG_COUNT + 1];
    database
        .connection
        .execute(
            "UPDATE terminals SET entry=?1",
            [postcard::to_stdvec(&oversized).unwrap()],
        )
        .unwrap();
    assert!(matches!(
        database.list_terminals(),
        Err(StoreError::Corrupt(_))
    ));
    let mut bytes = postcard::to_stdvec(&entry).unwrap();
    bytes.push(0);
    database
        .connection
        .execute("UPDATE terminals SET entry=?1", [bytes])
        .unwrap();
    assert!(matches!(
        database.list_terminals(),
        Err(StoreError::Corrupt(_))
    ));
    drop(database);
    assert!(matches!(
        Database::open(temporary.path()),
        Err(StoreError::Corrupt(_))
    ));
}

#[test]
fn migration_online_backup_contains_live_wal_contents_and_preupgrade_schema() {
    let temporary = TempDir::new().unwrap();
    let layout = Layout::new(WorkspaceId::new(), ViewId::new());
    let source = version_one(temporary.path(), Some(&layout));
    assert!(
        fs::metadata(temporary.path().join("state.db-wal"))
            .unwrap()
            .len()
            > 0
    );
    let database = Database::open(temporary.path()).unwrap();
    assert_eq!(database.default_layout().unwrap(), layout);
    assert_eq!(
        schema_version(&database.connection).unwrap(),
        SCHEMA_VERSION
    );
    let files = backups(temporary.path());
    assert_eq!(files.len(), 1);
    assert_eq!(fs::metadata(&files[0]).unwrap().mode() & 0o777, 0o600);
    let backup = Connection::open_with_flags(&files[0], OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(schema_version(&backup).unwrap(), 1);
    let bytes: Vec<u8> = backup
        .query_row("SELECT state FROM layout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(decode::<Layout>(&bytes, "backup layout").unwrap(), layout);
    let operations: i64 = backup
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='operations'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(operations, 0);
    drop(source);
    drop(database);
    let reopened = Database::open(temporary.path()).unwrap();
    assert_eq!(reopened.default_layout().unwrap(), layout);
    assert_eq!(backups(temporary.path()).len(), 1);
}

#[test]
fn failed_migration_rolls_back_schema_version_and_ddl_but_retains_backup() {
    let temporary = TempDir::new().unwrap();
    let source = version_one(temporary.path(), None);
    assert!(matches!(
        Database::open(temporary.path()),
        Err(StoreError::Corrupt(_))
    ));
    assert_eq!(schema_version(&source).unwrap(), 1);
    let operations: i64 = source
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='operations'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(operations, 0);
    let layouts: i64 = source
        .query_row("SELECT count(*) FROM layout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(layouts, 0);
    let files = backups(temporary.path());
    assert_eq!(files.len(), 1);
    let backup = Connection::open_with_flags(&files[0], OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(schema_version(&backup).unwrap(), 1);
}

#[test]
fn future_schema_is_rejected_without_migration_or_backup() {
    let temporary = TempDir::new().unwrap();
    let source = Connection::open(temporary.path().join("state.db")).unwrap();
    source
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    assert!(
        matches!(Database::open(temporary.path()), Err(StoreError::FutureSchema(version)) if version == SCHEMA_VERSION + 1)
    );
    assert_eq!(schema_version(&source).unwrap(), SCHEMA_VERSION + 1);
    let mode: String = source
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
    assert!(backups(temporary.path()).is_empty());
}

#[test]
fn unknown_unversioned_schema_is_backed_up_but_never_overwritten() {
    let temporary = TempDir::new().unwrap();
    let source = Connection::open(temporary.path().join("state.db")).unwrap();
    source
        .execute_batch(
            "CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES('preserve');",
        )
        .unwrap();
    assert!(matches!(
        Database::open(temporary.path()),
        Err(StoreError::Corrupt(_))
    ));
    assert_eq!(schema_version(&source).unwrap(), 0);
    let value: String = source
        .query_row("SELECT value FROM unrelated", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "preserve");
    assert_eq!(backups(temporary.path()).len(), 1);
}

#[test]
fn state_paths_reject_symlink_components_database_and_sidecars() {
    let temporary = TempDir::new().unwrap();
    let actual = temporary.path().join("actual");
    fs::create_dir(&actual).unwrap();
    let link = temporary.path().join("link");
    symlink(&actual, &link).unwrap();
    assert!(matches!(Database::open(&link), Err(StoreError::Io(_))));
    assert!(matches!(
        Database::open(&link.join("nested")),
        Err(StoreError::Io(_))
    ));
    let target = temporary.path().join("target");
    fs::write(&target, b"preserved").unwrap();
    symlink(&target, actual.join("state.db")).unwrap();
    assert!(matches!(Database::open(&actual), Err(StoreError::Io(_))));
    assert_eq!(fs::read(&target).unwrap(), b"preserved");
    fs::remove_file(actual.join("state.db")).unwrap();
    symlink(&target, actual.join("state.db-wal")).unwrap();
    assert!(matches!(Database::open(&actual), Err(StoreError::Io(_))));
    assert_eq!(fs::read(&target).unwrap(), b"preserved");
}

#[test]
fn relaunch_replaces_incarnation_clears_state_and_rejects_stale_keys() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, op, entry) = create_terminal(&mut database);
    let old = key_of(&entry);
    database.update_process(&process(&entry)).unwrap();
    database
        .set_launch_error(old, Some("previous failure"))
        .unwrap();
    database
        .save_checkpoint_ref(old, &checkpoint(10, 80, 7))
        .unwrap();
    let relaunched = database.relaunch_terminal(old).unwrap();
    assert_eq!(relaunched.spec.terminal, entry.spec.terminal);
    assert_ne!(relaunched.spec.incarnation, entry.spec.incarnation);
    assert_eq!(relaunched.spec.argv, entry.spec.argv);
    assert_eq!(relaunched.spec.cwd, entry.spec.cwd);
    assert_eq!(relaunched.spec.env, entry.spec.env);
    assert_eq!(relaunched.spec.size, entry.spec.size);
    assert_eq!(relaunched.spec.quota_bytes, entry.spec.quota_bytes);
    assert_eq!(relaunched.worktree, entry.worktree);
    assert!(relaunched.desired);
    assert!(relaunched.process.is_none());
    assert!(relaunched.checkpoint.is_none());
    assert!(relaunched.launch_error.is_none());
    assert!(database.checkpoint_hashes().unwrap().is_empty());
    // Durable across a real reopen, and the old key is permanently stale.
    drop(database);
    let mut database = Database::open(temporary.path()).unwrap();
    let new = key_of(&relaunched);
    assert_eq!(
        postcard::to_stdvec(&database.terminal(new).unwrap()).unwrap(),
        postcard::to_stdvec(&relaunched).unwrap()
    );
    assert!(matches!(
        database.terminal(old),
        Err(StoreError::WrongIncarnation)
    ));
    assert!(matches!(
        database.relaunch_terminal(old),
        Err(StoreError::WrongIncarnation)
    ));
    assert!(matches!(
        database.update_process(&process(&entry)),
        Err(StoreError::WrongIncarnation)
    ));
    assert!(matches!(
        database.relaunch_terminal(TerminalKey {
            terminal: TerminalId::new(),
            incarnation: new.incarnation,
        }),
        Err(StoreError::NotFound(_))
    ));
    // A closed intent is a cleanup obligation, never a relaunch candidate.
    close_tab(&mut database, tab_id(&op));
    let before = snapshot(&database);
    assert!(matches!(
        database.relaunch_terminal(new),
        Err(StoreError::Invalid(message)) if message.contains("closed")
    ));
    assert_eq!(snapshot(&database), before);
}

#[test]
fn failed_relaunch_rolls_back_the_incarnation_row() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (_, _, entry) = create_terminal(&mut database);
    database
        .save_checkpoint_ref(key_of(&entry), &checkpoint(4, 16, 3))
        .unwrap();
    let before = snapshot(&database);
    database.connection.execute_batch("CREATE TRIGGER fail_relaunch AFTER UPDATE ON terminals BEGIN SELECT RAISE(ABORT, 'injected relaunch failure'); END;").unwrap();
    assert!(matches!(
        database.relaunch_terminal(key_of(&entry)),
        Err(StoreError::Sqlite(_))
    ));
    assert_eq!(snapshot(&database), before);
    database
        .connection
        .execute_batch("DROP TRIGGER fail_relaunch")
        .unwrap();
    let unchanged = database.terminal(key_of(&entry)).unwrap();
    assert_eq!(unchanged.checkpoint, Some(checkpoint(4, 16, 3)));
    assert!(database.relaunch_terminal(key_of(&entry)).is_ok());
}

// --- Schema v3: workspaces, projects, worktrees, scoped layouts, scripts ---

use fl_model::ProjectId;
use fl_model::workspace::{Project, ProjectKind, Worktree};

fn project(database: &Database, path: &str) -> (Project, Worktree) {
    let workspace = database.workspace_tree().unwrap().workspaces[0].id;
    let project = Project {
        id: ProjectId::new(),
        workspace,
        name: "repo".into(),
        path: path.into(),
        kind: ProjectKind::Git,
        default_branch: Some("main".into()),
        worktree_root: format!("{path}-worktrees"),
        order: 99,
    };
    let primary = worktree(&project, path, true);
    (project, primary)
}

fn worktree(project: &Project, path: &str, is_primary: bool) -> Worktree {
    Worktree {
        id: WorktreeId::new(),
        project: project.id,
        path: path.into(),
        branch: Some("main".into()),
        target: (!is_primary).then(|| "main".into()),
        label: "main".into(),
        is_primary,
        created_ms: 1,
        last_active_ms: 1,
    }
}

fn worktree_defaults(path: &str) -> LaunchDefaults {
    let mut launch = defaults();
    launch.cwd = path.into();
    launch
        .env
        .insert("FLOTILLA_WORKTREE_PATH".into(), path.into());
    launch
}

fn scoped_tab(
    database: &mut Database,
    scope: LayoutScope,
    path: &str,
) -> (OperationId, LayoutOp, AppliedLayout) {
    let layout = database.scoped_layout(scope).unwrap();
    let operation = OperationId::new();
    let op = new_tab(&layout, TerminalId::new());
    let applied = database
        .apply_scoped_layout(
            scope,
            operation,
            layout.rev,
            op.clone(),
            &worktree_defaults(path),
        )
        .unwrap();
    (operation, op, applied)
}

fn script_spec(path: &str) -> SpawnSpec {
    SpawnSpec {
        terminal: TerminalId::new(),
        incarnation: IncarnationId::new(),
        argv: vec!["/bin/sh".into(), "-lc".into(), "make setup".into()],
        cwd: path.into(),
        env: BTreeMap::new(),
        size: Size::new(80, 24),
        quota_bytes: fl_prochost_proto::DEFAULT_QUOTA_BYTES,
    }
}

/// A schema-2 database exactly as M1 wrote it, with one live terminal.
fn version_two(root: &Path) -> (Layout, Vec<TerminalEntry>) {
    let (layout, terminals) = {
        let mut database = Database::open(root).unwrap();
        create_terminal(&mut database);
        (
            database.default_layout().unwrap(),
            database.list_terminals().unwrap(),
        )
    };
    let connection = Connection::open(root.join("state.db")).unwrap();
    connection
        .execute_batch(
            "DROP TABLE routing; DROP TABLE reviews; DROP TABLE sections; DROP TABLE scripts; DROP TABLE worktree_layouts; DROP TABLE worktrees; \
             DROP TABLE projects; DROP TABLE workspaces; DROP TABLE workspace_meta; \
             PRAGMA user_version=2;",
        )
        .unwrap();
    (layout, terminals)
}

#[test]
fn v2_migration_keeps_the_home_layout_and_terminals_and_backs_up_v2() {
    let temporary = TempDir::new().unwrap();
    let (layout, terminals) = version_two(temporary.path());
    let database = Database::open(temporary.path()).unwrap();
    assert_eq!(
        schema_version(&database.connection).unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(database.default_layout().unwrap(), layout);
    assert_eq!(
        postcard::to_stdvec(&database.list_terminals().unwrap()).unwrap(),
        postcard::to_stdvec(&terminals).unwrap()
    );
    let tree = database.workspace_tree().unwrap();
    assert_eq!(tree.rev, 1);
    assert_eq!(tree.workspaces.len(), 1);
    assert_eq!(tree.workspaces[0].name, "Default");
    assert_eq!(tree.workspaces[0].id, layout.workspace);
    assert!(tree.projects.is_empty() && tree.worktrees.is_empty());
    let files = backups(temporary.path());
    assert_eq!(files.len(), 1);
    assert!(
        files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&format!("state.db.before-v2-to-v{SCHEMA_VERSION}-"))
    );
    let backup = Connection::open_with_flags(&files[0], OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(schema_version(&backup).unwrap(), 2);
    drop(database);
    // Idempotent: reopening neither migrates nor backs up again.
    let reopened = Database::open(temporary.path()).unwrap();
    assert_eq!(reopened.default_layout().unwrap(), layout);
    assert_eq!(backups(temporary.path()).len(), 1);
}

#[test]
fn projects_and_worktrees_persist_with_revisions_and_reject_duplicates() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (project, primary) = project(&database, "/srv/repo");
    let (stored, rev) = database
        .add_project(project.clone(), std::slice::from_ref(&primary))
        .unwrap();
    assert_eq!(rev, 2);
    assert_eq!(stored.order, 0);
    let linked = worktree(&stored, "/srv/repo-worktrees/feature", false);
    assert_eq!(database.add_worktree(&linked).unwrap(), 3);
    let tree = database.workspace_tree().unwrap();
    assert_eq!(tree.projects, vec![stored.clone()]);
    assert_eq!(tree.worktrees, vec![primary.clone(), linked.clone()]);
    assert!(matches!(
        database.add_project(project.clone(), std::slice::from_ref(&primary)),
        Err(StoreError::Duplicate(_))
    ));
    assert!(matches!(
        database.add_worktree(&Worktree {
            id: WorktreeId::new(),
            ..linked.clone()
        }),
        Err(StoreError::Duplicate(_))
    ));
    let mut foreign = project.clone();
    foreign.workspace = WorkspaceId::new();
    foreign.path = "/srv/other".into();
    assert!(matches!(
        database.add_project(foreign, &[]),
        Err(StoreError::Missing(_))
    ));
    let (labelled, rev) = database.set_worktree_label(linked.id, "Feature").unwrap();
    assert_eq!((labelled.label.as_str(), rev), ("Feature", 4));
    assert_eq!(
        database
            .set_worktree_branch(linked.id, Some("main"))
            .unwrap(),
        None
    );
    assert_eq!(
        database
            .set_worktree_branch(linked.id, Some("other"))
            .unwrap(),
        Some(5)
    );
    assert!(matches!(
        database.remove_worktree(primary.id),
        Err(StoreError::Invalid(_))
    ));
    drop(database);
    let database = Database::open(temporary.path()).unwrap();
    let tree = database.workspace_tree().unwrap();
    assert_eq!(tree.rev, 5);
    assert_eq!(tree.worktrees[1].label, "Feature");
    assert_eq!(tree.worktrees[1].branch.as_deref(), Some("other"));
    assert_eq!(database.worktree(linked.id).unwrap().1, stored);
}

#[test]
fn worktree_layouts_have_independent_revisions_dedup_and_spawn_in_the_worktree() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (project, primary) = project(&database, "/srv/repo");
    database
        .add_project(project, std::slice::from_ref(&primary))
        .unwrap();
    let scope = LayoutScope::Worktree(primary.id);
    let empty = database.scoped_layout(scope).unwrap();
    assert_eq!(empty.rev, 0);
    assert!(empty.views[0].tabs.is_empty());
    let home = database.default_layout().unwrap();
    let (operation, op, applied) = scoped_tab(&mut database, scope, "/srv/repo");
    let entry = applied.spawned.unwrap();
    assert_eq!(entry.worktree, Some(primary.id));
    assert_eq!(entry.spec.cwd, "/srv/repo");
    assert_eq!(
        entry
            .spec
            .env
            .get("FLOTILLA_WORKTREE_PATH")
            .map(String::as_str),
        Some("/srv/repo")
    );
    assert_eq!(applied.layout.rev, 1);
    // The home layout is untouched by worktree operations.
    assert_eq!(database.default_layout().unwrap(), home);
    // Exact retry replays; same ID with another scope or payload conflicts.
    let replay = database
        .apply_scoped_layout(scope, operation, 0, op.clone(), &defaults())
        .unwrap();
    assert!(replay.replayed && replay.spawned.is_none());
    assert_eq!(replay.layout, applied.layout);
    assert!(matches!(
        database.apply_scoped_layout(LayoutScope::Home, operation, 0, op, &defaults()),
        Err(StoreError::OperationConflict(_))
    ));
    let stale = new_tab(&applied.layout, TerminalId::new());
    assert!(matches!(
        database.apply_scoped_layout(scope, OperationId::new(), 0, stale, &defaults()),
        Err(StoreError::RevisionConflict {
            expected: 1,
            got: 0
        })
    ));
    assert!(matches!(
        database.scoped_layout(LayoutScope::Worktree(WorktreeId::new())),
        Err(StoreError::Missing(_))
    ));
    drop(database);
    let database = Database::open(temporary.path()).unwrap();
    assert_eq!(database.scoped_layout(scope).unwrap(), applied.layout);
    assert!(database.terminal(key_of(&entry)).unwrap().desired);
}

#[test]
fn scripts_replace_previous_runs_and_removal_closes_every_owned_terminal() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (project, primary) = project(&database, "/srv/repo");
    let (project, _) = database
        .add_project(project, std::slice::from_ref(&primary))
        .unwrap();
    let linked = worktree(&project, "/srv/repo-worktrees/a", false);
    database.add_worktree(&linked).unwrap();
    let (_, _, applied) = scoped_tab(
        &mut database,
        LayoutScope::Worktree(linked.id),
        &linked.path,
    );
    let pane = applied.spawned.unwrap();
    let (first, replaced) = database
        .start_script(linked.id, "setup", script_spec(&linked.path))
        .unwrap();
    assert!(replaced.is_none());
    assert_eq!(first.worktree, Some(linked.id));
    let (second, replaced) = database
        .start_script(linked.id, "setup", script_spec(&linked.path))
        .unwrap();
    assert_eq!(replaced, Some(key_of(&first)));
    assert!(!database.terminal(key_of(&first)).unwrap().desired);
    database.remove_terminal(key_of(&first)).unwrap();
    let (run, _) = database
        .start_script(linked.id, "run", script_spec(&linked.path))
        .unwrap();
    let scripts = database.scripts().unwrap();
    assert_eq!(
        scripts
            .iter()
            .map(|s| (s.kind.as_str(), s.key))
            .collect::<Vec<_>>(),
        vec![("run", key_of(&run)), ("setup", key_of(&second))]
    );
    assert_eq!(
        database.close_script(linked.id, "run").unwrap(),
        Some(key_of(&run))
    );
    assert_eq!(database.close_script(linked.id, "run").unwrap(), None);
    // Home terminals survive worktree removal; owned ones are closed.
    let (_, _, home) = create_terminal(&mut database);
    let removed = database.remove_worktree(linked.id).unwrap();
    assert_eq!(removed.worktrees, vec![linked.id]);
    let mut closed = removed.closed.clone();
    closed.sort_by_key(|key| key.terminal);
    let mut expected = vec![key_of(&pane), key_of(&second)];
    expected.sort_by_key(|key| key.terminal);
    assert_eq!(closed, expected);
    for key in &expected {
        assert!(!database.terminal(*key).unwrap().desired);
    }
    assert!(database.terminal(key_of(&home)).unwrap().desired);
    assert!(database.scripts().unwrap().is_empty());
    assert!(matches!(
        database.scoped_layout(LayoutScope::Worktree(linked.id)),
        Err(StoreError::Missing(_))
    ));
    // Removing the project closes the primary's terminals and forgets it.
    let (_, _, primary_pane) = scoped_tab(
        &mut database,
        LayoutScope::Worktree(primary.id),
        &primary.path,
    );
    let removed = database.remove_project(project.id).unwrap();
    assert_eq!(removed.closed, vec![key_of(&primary_pane.spawned.unwrap())]);
    let tree = database.workspace_tree().unwrap();
    assert!(tree.projects.is_empty() && tree.worktrees.is_empty());
    drop(database);
    let database = Database::open(temporary.path()).unwrap();
    assert!(database.workspace_tree().unwrap().projects.is_empty());
}

#[test]
fn a_terminal_owned_by_two_layouts_fails_closed_on_open() {
    let temporary = TempDir::new().unwrap();
    let mut database = Database::open(temporary.path()).unwrap();
    let (project, primary) = project(&database, "/srv/repo");
    database
        .add_project(project, std::slice::from_ref(&primary))
        .unwrap();
    let (_, _, applied) = scoped_tab(
        &mut database,
        LayoutScope::Worktree(primary.id),
        "/srv/repo",
    );
    // Copy the worktree's layout (and its terminal) into the home layout.
    write_layout(&database.connection, &applied.layout).unwrap();
    drop(database);
    assert!(matches!(
        Database::open(temporary.path()),
        Err(StoreError::Corrupt(_))
    ));
}
