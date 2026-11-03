use fl_model::{
    LogCursor, OperationId, PaneId, Size, TabId, TerminalId, TerminalRecordKind, layout::LayoutOp,
};
use fl_prochost_proto::{DEFAULT_QUOTA_BYTES, MAX_READ_BYTES};
use fl_term::{EffectMode, Engine};
use fld_prochost::journal::Journal;
use fld_store::{LaunchDefaults, Store, TerminalKey};
use std::collections::BTreeMap;

#[test]
fn committed_checkpoint_survives_missing_ack_and_replays_exact_suffix() {
    let root = tempfile::tempdir().unwrap();
    let mut store = Store::open(root.path()).unwrap();
    let layout = store.database.default_layout().unwrap();
    let terminal = TerminalId::new();
    let applied = store
        .database
        .apply_layout(
            OperationId::new(),
            0,
            LayoutOp::NewTab {
                view: layout.active,
                tab: TabId::new(),
                pane: PaneId::new(),
                terminal,
                name: None,
                size: Size::new(40, 5),
            },
            &LaunchDefaults {
                cwd: root.path().to_str().unwrap().into(),
                argv: Vec::new(),
                env: BTreeMap::new(),
                quota_bytes: DEFAULT_QUOTA_BYTES,
            },
        )
        .unwrap();
    let entry = applied.spawned.unwrap();
    let key = TerminalKey {
        terminal,
        incarnation: entry.spec.incarnation,
    };
    let mut journal = Journal::open(&root.path().join("journal"), DEFAULT_QUOTA_BYTES).unwrap();
    let mut original = Engine::new(entry.spec.size, EffectMode::Authoritative).unwrap();
    let prefix = b"\x1b[32mremote \xe7";
    let first = journal
        .append(&TerminalRecordKind::Output {
            bytes: prefix.as_slice().into(),
            echo: true,
        })
        .unwrap();
    original.feed(0, prefix).unwrap();
    let saved = first.end().unwrap();
    store.checkpoint(key, saved, &original).unwrap();
    // Crash window: SQLite committed the reference, but prochost never received
    // the checkpoint acknowledgment. A later output record is already durable.
    let suffix = b"\x95\x8c\x1b[0m\r\n\x1b[6n";
    let second = journal
        .append(&TerminalRecordKind::Output {
            bytes: suffix.as_slice().into(),
            echo: true,
        })
        .unwrap();
    let expected_effects = original.feed(saved.next_byte, suffix).unwrap();
    let expected_state = original.checkpoint().unwrap();
    drop(store);
    let recovered = Store::open(root.path()).unwrap();
    let (mut engine, cursor) = recovered.restore(key, EffectMode::Authoritative).unwrap();
    assert_eq!(cursor, saved);
    journal.acknowledge(cursor).unwrap();
    let records = journal.read(cursor, MAX_READ_BYTES).unwrap();
    let mut resumed = cursor;
    let mut effects = Vec::new();
    for record in records {
        assert_eq!(record.at, resumed);
        if let TerminalRecordKind::Output { bytes, .. } = &record.kind {
            effects.extend(engine.feed(record.at.next_byte, bytes).unwrap());
        }
        resumed = record.end().unwrap();
    }
    assert_eq!(resumed, second.end().unwrap());
    assert_eq!(effects, expected_effects);
    assert_eq!(engine.checkpoint().unwrap(), expected_state);
    assert_ne!(resumed, LogCursor::default());
}
