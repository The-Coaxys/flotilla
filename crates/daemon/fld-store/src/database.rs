//! Synchronous SQLite ownership. Callers run this work off runtime/UI workers.
//!
//! Every counter is encoded in a bounded postcard BLOB, not a SQLite INTEGER:
//! revisions and journal cursors must retain the entire unsigned 64-bit range.
use crate::{
    AppliedLayout, BlobHash, CheckpointRef, LaunchDefaults, Result, StoreError, TerminalEntry,
    TerminalKey,
};
use fl_model::{
    IncarnationId, LogCursor, OperationId, ProcessInfo, ProcessState, Size, TerminalId, ViewId,
    WorkspaceId, WorktreeId,
    layout::{Layout, LayoutError, LayoutOp, MAX_TERMINALS},
    workspace::LayoutScope,
};
use fl_prochost_proto::SpawnSpec;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, params, types::ValueRef,
};
use serde::{Deserialize, Deserializer, de};
use sha2::{Digest, Sha256};
use smallvec::SmallVec;
use std::{
    collections::BTreeMap,
    ffi::CString,
    fmt,
    fs::{File, Permissions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    time::Duration,
};

const SCHEMA_VERSION: u32 = 8;
const MAX_LAYOUT_BYTES: usize = 128 * 1024;
const MAX_ENTRY_BYTES: usize = 512 * 1024;
const MAX_TEXT_BYTES: usize = 4096;
const MAX_ARG_COUNT: usize = 256;
const MAX_ARGV_BYTES: usize = 64 * 1024;
const MAX_ENV_COUNT: usize = 128;
const MAX_ENV_BYTES: usize = 128 * 1024;
const MAX_ENGINE_BYTES: usize = 128;

// Version 1 held layout and terminal intents. Version 2 adds durable request
// deduplication. Both steps and the initial default workspace commit together.
const SCHEMA_V1: &str = "
    CREATE TABLE layout (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        state BLOB NOT NULL CHECK(length(state) BETWEEN 1 AND 131072)
    ) STRICT;
    CREATE TABLE terminals (
        terminal BLOB PRIMARY KEY NOT NULL CHECK(length(terminal) = 16),
        incarnation BLOB NOT NULL CHECK(length(incarnation) = 16),
        desired INTEGER NOT NULL CHECK(desired IN (0, 1)),
        entry BLOB NOT NULL CHECK(length(entry) BETWEEN 1 AND 524288)
    ) STRICT;
";
const SCHEMA_V2: &str = "
    CREATE TABLE operations (
        operation BLOB PRIMARY KEY NOT NULL CHECK(length(operation) = 16),
        payload_hash BLOB NOT NULL CHECK(length(payload_hash) = 32)
    ) STRICT;
";
// Version 3 adds workspaces, projects and worktrees (M2). Each worktree owns
// one layout with the home layout's revision and deduplication semantics;
// script terminals belong to a worktree but to no layout.
const SCHEMA_V3: &str = "
    CREATE TABLE workspace_meta (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        tree_rev BLOB NOT NULL CHECK(length(tree_rev) BETWEEN 1 AND 10)
    ) STRICT;
    CREATE TABLE workspaces (
        id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 16),
        name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 4096),
        ord INTEGER NOT NULL CHECK(ord >= 0)
    ) STRICT;
    CREATE TABLE projects (
        id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 16),
        workspace BLOB NOT NULL REFERENCES workspaces(id),
        name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 4096),
        path TEXT NOT NULL UNIQUE CHECK(length(path) BETWEEN 1 AND 4096),
        kind TEXT NOT NULL CHECK(kind IN ('git', 'folder')),
        default_branch TEXT CHECK(default_branch IS NULL OR length(default_branch) <= 4096),
        worktree_root TEXT NOT NULL CHECK(length(worktree_root) BETWEEN 1 AND 4096),
        ord INTEGER NOT NULL CHECK(ord >= 0)
    ) STRICT;
    CREATE TABLE worktrees (
        id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 16),
        project BLOB NOT NULL REFERENCES projects(id),
        path TEXT NOT NULL UNIQUE CHECK(length(path) BETWEEN 1 AND 4096),
        branch TEXT CHECK(branch IS NULL OR length(branch) <= 4096),
        target TEXT CHECK(target IS NULL OR length(target) <= 4096),
        label TEXT NOT NULL CHECK(length(label) <= 4096),
        is_primary INTEGER NOT NULL CHECK(is_primary IN (0, 1)),
        created_ms INTEGER NOT NULL CHECK(created_ms >= 0),
        last_active_ms INTEGER NOT NULL CHECK(last_active_ms >= 0)
    ) STRICT;
    CREATE TABLE worktree_layouts (
        worktree BLOB PRIMARY KEY NOT NULL REFERENCES worktrees(id),
        state BLOB NOT NULL CHECK(length(state) BETWEEN 1 AND 131072)
    ) STRICT;
    CREATE TABLE scripts (
        worktree BLOB NOT NULL REFERENCES worktrees(id),
        kind TEXT NOT NULL CHECK(length(kind) BETWEEN 1 AND 4200),
        terminal BLOB NOT NULL UNIQUE REFERENCES terminals(terminal),
        PRIMARY KEY (worktree, kind)
    ) STRICT;
";

/// The owning worker serializes access. Writes require exclusive ownership;
/// no method performs process I/O. Spawn/close results are committed intents.
pub struct Database {
    connection: Connection,
    // Retain the directory descriptor used by SQLite's /proc/self/fd path.
    _root: File,
}

impl Database {
    pub fn open(root: &Path) -> Result<Self> {
        let root = private_root(root)?;
        let path = descriptor_path(&root).join("state.db");
        let (database_file, existed) = private_file(&root, "state.db", true)?
            .ok_or_else(|| corrupt("state.db was not created"))?;
        // Reject pre-existing unsafe sidecars before SQLite can open them.
        for name in ["state.db-wal", "state.db-shm", "state.db-journal"] {
            private_file(&root, name, false)?;
        }
        let mut connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let version = schema_version(&connection)?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::FutureSchema(version));
        }
        connection.execute_batch(
            "PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;",
        )?;
        let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(corrupt("SQLite did not enable WAL journaling"));
        }
        migrate(&mut connection, &root, &path, existed)?;
        // The file name itself must survive a power failure after first open.
        database_file.sync_all()?;
        root.sync_all()?;
        Ok(Self {
            connection,
            _root: root,
        })
    }

    pub fn default_layout(&self) -> Result<Layout> {
        self.scoped_layout(LayoutScope::Home)
    }

    /// The home layout, or one worktree's layout.
    pub fn scoped_layout(&self, scope: LayoutScope) -> Result<Layout> {
        let transaction = self.connection.unchecked_transaction()?;
        let state = load_state(&transaction)?;
        transaction.commit()?;
        state.into_layout(scope)
    }

    /// Includes closed intents until explicit successful cleanup removes them.
    pub fn list_terminals(&self) -> Result<Vec<TerminalEntry>> {
        let transaction = self.connection.unchecked_transaction()?;
        let state = load_state(&transaction)?;
        transaction.commit()?;
        Ok(state.terminals)
    }

    pub fn terminal(&self, key: TerminalKey) -> Result<TerminalEntry> {
        let transaction = self.connection.unchecked_transaction()?;
        let state = load_state(&transaction)?;
        let entry = state
            .terminals
            .into_iter()
            .find(|entry| entry.spec.terminal == key.terminal)
            .ok_or(StoreError::NotFound(key.terminal))?;
        require_incarnation(&entry, key)?;
        transaction.commit()?;
        Ok(entry)
    }

    /// The home layout operation (M1 `ApplyLayout`).
    pub fn apply_layout(
        &mut self,
        operation: OperationId,
        base_rev: u64,
        op: LayoutOp,
        defaults: &LaunchDefaults,
    ) -> Result<AppliedLayout> {
        self.apply_scoped_layout(LayoutScope::Home, operation, base_rev, op, defaults)
    }

    /// Commit one layout operation of `scope` with its spawn/close intents.
    /// Terminals spawned by a worktree layout record that worktree.
    pub fn apply_scoped_layout(
        &mut self,
        scope: LayoutScope,
        operation: OperationId,
        base_rev: u64,
        op: LayoutOp,
        defaults: &LaunchDefaults,
    ) -> Result<AppliedLayout> {
        op.validate()?;
        // Defaults are server policy, not part of the client's operation. A
        // retry after changing that policy still denotes the original request.
        let payload = match scope {
            LayoutScope::Home => postcard::to_stdvec(&(base_rev, &op))?,
            LayoutScope::Worktree(worktree) => postcard::to_stdvec(&(worktree, base_rev, &op))?,
        };
        let hash: BlobHash = Sha256::digest(&payload).into();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut state = load_state(&transaction)?;
        let worktree = match scope {
            LayoutScope::Home => None,
            LayoutScope::Worktree(worktree) => Some(worktree),
        };
        if let Some(previous) = operation_hash(&transaction, operation)? {
            if previous != hash {
                return Err(StoreError::OperationConflict(operation));
            }
            transaction.commit()?;
            return Ok(AppliedLayout {
                layout: state.into_layout(scope)?,
                spawned: None,
                closed: SmallVec::new(),
                replayed: true,
            });
        }
        let terminals = &mut state.terminals;
        let layout = match scope {
            LayoutScope::Home => &mut state.home,
            LayoutScope::Worktree(id) => state
                .scoped
                .iter_mut()
                .find(|(owner, _)| *owner == id)
                .map(|(_, layout)| layout)
                .ok_or_else(|| StoreError::Missing(format!("worktree {id} not found")))?,
        };
        if layout.rev != base_rev {
            return Err(StoreError::RevisionConflict {
                expected: layout.rev,
                got: base_rev,
            });
        }
        let effect = layout.apply(op)?;
        let spawned = if let Some(spawn) = effect.spawn {
            // A closed incarnation remains an explicit cleanup obligation; do
            // not overwrite its process/checkpoint by reusing its terminal ID.
            if terminals
                .iter()
                .any(|entry| entry.spec.terminal == spawn.terminal)
            {
                return Err(LayoutError::DuplicateTerminal(spawn.terminal).into());
            }
            if terminals.len() == MAX_TERMINALS {
                return Err(LayoutError::TooManyTerminals.into());
            }
            let entry = TerminalEntry {
                spec: SpawnSpec {
                    terminal: spawn.terminal,
                    incarnation: IncarnationId::new(),
                    argv: defaults.argv.clone(),
                    cwd: defaults.cwd.clone(),
                    env: defaults.env.clone(),
                    size: spawn.size,
                    quota_bytes: defaults.quota_bytes,
                },
                desired: true,
                worktree,
                process: None,
                launch_error: None,
                checkpoint: None,
            };
            validate_entry(&entry).map_err(StoreError::Invalid)?;
            let bytes = encode_entry(&entry)?;
            transaction.execute(
                "INSERT INTO terminals(terminal, incarnation, desired, entry) VALUES (?1, ?2, 1, ?3)",
                params![entry.spec.terminal.as_bytes().as_slice(), entry.spec.incarnation.as_bytes().as_slice(), bytes],
            )?;
            Some(entry)
        } else {
            None
        };
        let mut closed = SmallVec::new();
        for terminal in effect.close {
            let entry = terminals
                .iter_mut()
                .find(|entry| entry.spec.terminal == terminal)
                .ok_or_else(|| corrupt("layout close refers to an absent terminal"))?;
            entry.desired = false;
            write_entry(&transaction, entry)?;
            closed.push(key_of(entry));
        }
        match worktree {
            None => write_layout(&transaction, layout)?,
            Some(id) => {
                write_scoped_layout(&transaction, id, layout)?;
                transaction.execute(
                    "UPDATE worktrees SET last_active_ms=?2 WHERE id=?1",
                    params![id.as_bytes().as_slice(), sql_ms(now_ms())?],
                )?;
            }
        }
        let layout = layout.clone();
        transaction.execute(
            "INSERT INTO operations(operation, payload_hash) VALUES (?1, ?2)",
            params![operation.as_bytes().as_slice(), hash.as_slice()],
        )?;
        transaction.commit()?;
        Ok(AppliedLayout {
            layout,
            spawned,
            closed,
            replayed: false,
        })
    }

    pub fn update_process(&mut self, info: &ProcessInfo) -> Result<()> {
        validate_process(info).map_err(StoreError::Invalid)?;
        self.mutate_terminal(
            TerminalKey {
                terminal: info.terminal,
                incarnation: info.incarnation,
            },
            |entry| {
                entry.process = Some(info.clone());
                Ok(())
            },
        )
    }

    pub fn set_launch_error(&mut self, key: TerminalKey, message: Option<&str>) -> Result<()> {
        if let Some(message) = message {
            validate_text("launch error", message, MAX_TEXT_BYTES).map_err(StoreError::Invalid)?;
        }
        self.mutate_terminal(key, |entry| {
            entry.launch_error = message.map(str::to_owned);
            Ok(())
        })
    }

    pub fn save_checkpoint_ref(
        &mut self,
        key: TerminalKey,
        reference: &CheckpointRef,
    ) -> Result<()> {
        validate_checkpoint(reference).map_err(StoreError::Invalid)?;
        self.mutate_terminal(key, |entry| {
            if let Some(previous) = &entry.checkpoint
                && (reference.cursor.next_record < previous.cursor.next_record
                    || reference.cursor.next_byte < previous.cursor.next_byte
                    || (reference.cursor.next_record == previous.cursor.next_record
                        && reference.cursor.next_byte != previous.cursor.next_byte))
            {
                return Err(StoreError::StaleCheckpoint);
            }
            entry.checkpoint = Some(reference.clone());
            Ok(())
        })
    }

    pub fn remove_terminal(&mut self, key: TerminalKey) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let terminals = load_state(&transaction)?.terminals;
        let entry = terminals
            .iter()
            .find(|entry| entry.spec.terminal == key.terminal)
            .ok_or(StoreError::NotFound(key.terminal))?;
        require_incarnation(entry, key)?;
        if entry.desired {
            return Err(StoreError::TerminalActive);
        }
        let changed = transaction.execute(
            "DELETE FROM terminals WHERE terminal=?1 AND incarnation=?2 AND desired=0",
            params![
                key.terminal.as_bytes().as_slice(),
                key.incarnation.as_bytes().as_slice()
            ],
        )?;
        if changed != 1 {
            return Err(corrupt(
                "terminal cleanup did not remove exactly one closed intent",
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Replace a lost child incarnation of a still-desired terminal. The old
    /// incarnation's offsets, checkpoint and input identities can never be
    /// accepted again; its checkpoint blob is reclaimed by later GC.
    pub fn relaunch_terminal(&mut self, key: TerminalKey) -> Result<TerminalEntry> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let terminals = load_state(&transaction)?.terminals;
        let mut entry = terminals
            .into_iter()
            .find(|entry| entry.spec.terminal == key.terminal)
            .ok_or(StoreError::NotFound(key.terminal))?;
        require_incarnation(&entry, key)?;
        if !entry.desired {
            return Err(StoreError::Invalid(
                "terminal was closed; it cannot be relaunched".into(),
            ));
        }
        entry.spec.incarnation = IncarnationId::new();
        entry.process = None;
        entry.checkpoint = None;
        entry.launch_error = None;
        validate_entry(&entry).map_err(StoreError::Invalid)?;
        let bytes = encode_entry(&entry)?;
        let changed = transaction.execute(
            "UPDATE terminals SET incarnation=?3,entry=?4 WHERE terminal=?1 AND incarnation=?2 AND desired=1",
            params![
                key.terminal.as_bytes().as_slice(),
                key.incarnation.as_bytes().as_slice(),
                entry.spec.incarnation.as_bytes().as_slice(),
                bytes
            ],
        )?;
        if changed != 1 {
            return Err(corrupt(
                "terminal relaunch did not replace exactly one incarnation",
            ));
        }
        transaction.commit()?;
        Ok(entry)
    }

    pub fn checkpoint_hashes(&self) -> Result<Vec<BlobHash>> {
        let mut hashes: Vec<_> = self
            .list_terminals()?
            .into_iter()
            .filter_map(|entry| entry.checkpoint.map(|reference| reference.hash))
            .collect();
        hashes.sort_unstable();
        hashes.dedup();
        Ok(hashes)
    }

    fn mutate_terminal(
        &mut self,
        key: TerminalKey,
        update: impl FnOnce(&mut TerminalEntry) -> Result<()>,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut terminals = load_state(&transaction)?.terminals;
        let entry = terminals
            .iter_mut()
            .find(|entry| entry.spec.terminal == key.terminal)
            .ok_or(StoreError::NotFound(key.terminal))?;
        require_incarnation(entry, key)?;
        update(entry)?;
        validate_entry(entry).map_err(StoreError::Invalid)?;
        write_entry(&transaction, entry)?;
        transaction.commit()?;
        Ok(())
    }
}

fn migrate(connection: &mut Connection, root: &File, path: &Path, existed: bool) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version = schema_version(&transaction)?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::FutureSchema(version));
    }
    if version < SCHEMA_VERSION {
        if existed {
            // A distinct reader is required: SQLite's backup API rejects a
            // source connection with an active write transaction. The reserved
            // writer lock above prevents any writer changing the backed-up
            // version before this transaction commits its migration.
            backup_before_upgrade(root, path, version)?;
        }
        if version == 0 {
            let objects: i64 = transaction.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if objects != 0 {
                return Err(corrupt("unversioned database contains an unknown schema"));
            }
            transaction.execute_batch(SCHEMA_V1)?;
            let layout = Layout::new(WorkspaceId::new(), ViewId::new());
            let bytes = encode_layout(&layout)?;
            transaction.execute("INSERT INTO layout(singleton,state) VALUES(1,?1)", [bytes])?;
        }
        if version < 2 {
            transaction.execute_batch(SCHEMA_V2)?;
        }
        if version < 3 {
            // The home layout's workspace becomes the default workspace, so
            // the M1 layout and its terminals stay where they were.
            transaction.execute_batch(SCHEMA_V3)?;
            let home = load_home_layout(&transaction)?;
            transaction.execute(
                "INSERT INTO workspaces(id,name,ord) VALUES(?1,'Default',0)",
                [home.workspace.as_bytes().as_slice()],
            )?;
            transaction.execute(
                "INSERT INTO workspace_meta(singleton,tree_rev) VALUES(1,?1)",
                [postcard::to_stdvec(&1u64)?],
            )?;
        }
        if version < 4 {
            transaction.execute_batch(sections::SCHEMA_V4)?;
            let initial = fl_model::sections::SectionsState {
                rev: 1,
                ..Default::default()
            };
            transaction.execute(
                "INSERT INTO sections(singleton,state) VALUES(1,?1)",
                [postcard::to_stdvec(&initial)?],
            )?;
        }
        if version < 5 {
            transaction.execute_batch(review::SCHEMA_V5)?;
        }
        if version < 6 {
            transaction.execute_batch(routing::SCHEMA_V6)?;
            transaction.execute(
                "INSERT INTO routing(singleton,state) VALUES(1,?1)",
                [routing::initial_routing()?],
            )?;
        }
        if version < 7 {
            transaction.execute_batch(chat::SCHEMA_V7)?;
            let fts5: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_module_list WHERE name='fts5')",
                [],
                |row| row.get(0),
            )?;
            if fts5 {
                transaction.execute_batch(chat::FTS_V7)?;
            } else {
                transaction.execute_batch(chat::FALLBACK_V7)?;
            }
            transaction.execute(
                "INSERT INTO chat_meta(singleton,fts5) VALUES(1,?1)",
                [chat::initial_chat_meta(fts5)],
            )?;
        }
        if version < 8 {
            transaction.execute_batch(cli::SCHEMA_V8)?;
            transaction.execute("INSERT OR IGNORE INTO cli_meta(singleton) VALUES(1)", [])?;
        }
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    // Never initialize over a missing/corrupt default or partially valid rows.
    load_state(&transaction)?;
    load_tree(&transaction)?;
    sections::validate_references(
        &sections::load_sections(&transaction)?,
        &load_tree(&transaction)?,
    )?;
    let invalid_operations: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE typeof(operation)!='blob' OR length(operation)!=16 OR typeof(payload_hash)!='blob' OR length(payload_hash)!=32)",
        [], |row| row.get(0),
    )?;
    if invalid_operations {
        return Err(corrupt("invalid persisted operation identity or hash"));
    }
    chat::validate_chat_integrity(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn backup_before_upgrade(root: &File, path: &Path, version: u32) -> Result<()> {
    let name = format!(
        "state.db.before-v{version}-to-v{SCHEMA_VERSION}-{}.db",
        IncarnationId::new()
    );
    let backup_path = descriptor_path(root).join(&name);
    let file = create_private_file(root, &name)?;
    let source = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    source.busy_timeout(Duration::from_secs(5))?;
    {
        let mut destination = Connection::open(&backup_path)?;
        rusqlite::backup::Backup::new(&source, &mut destination)?.run_to_completion(
            256,
            Duration::ZERO,
            None,
        )?;
        // The copied header inherits the source's WAL mode. Convert it so the
        // backup is one self-contained file that restores without sidecars.
        destination.pragma_update(None, "journal_mode", "DELETE")?;
    }
    file.sync_all()?;
    root.sync_all()?;
    Ok(())
}

fn schema_version(connection: &Connection) -> Result<u32> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    u32::try_from(version).map_err(|_| corrupt("invalid negative database schema version"))
}

/// Every layout, terminal intent and script owner, cross-checked: a desired
/// terminal is owned by exactly one layout pane or one script; a closed
/// intent is owned by nothing.
pub(crate) struct State {
    pub(crate) home: Layout,
    pub(crate) scoped: Vec<(WorktreeId, Layout)>,
    pub(crate) terminals: Vec<TerminalEntry>,
    pub(crate) scripts: Vec<ScriptRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScriptRow {
    pub(crate) worktree: WorktreeId,
    pub(crate) kind: String,
    pub(crate) terminal: TerminalId,
}

impl State {
    fn into_layout(self, scope: LayoutScope) -> Result<Layout> {
        match scope {
            LayoutScope::Home => Ok(self.home),
            LayoutScope::Worktree(id) => self
                .scoped
                .into_iter()
                .find(|(owner, _)| *owner == id)
                .map(|(_, layout)| layout)
                .ok_or_else(|| StoreError::Missing(format!("worktree {id} not found"))),
        }
    }
}

fn load_home_layout(connection: &Connection) -> Result<Layout> {
    let mut statement = connection.prepare("SELECT singleton,state FROM layout")?;
    let mut rows = statement.query([])?;
    let row = rows
        .next()?
        .ok_or_else(|| corrupt("default workspace is absent"))?;
    if row.get::<_, i64>(0)? != 1 {
        return Err(corrupt("invalid default workspace singleton"));
    }
    let bytes = row_blob(row, 1, MAX_LAYOUT_BYTES)?;
    let layout: Layout = decode(bytes, "layout")?;
    layout
        .validate()
        .map_err(|error| corrupt(format!("invalid layout: {error}")))?;
    if rows.next()?.is_some() {
        return Err(corrupt("more than one default workspace is persisted"));
    }
    Ok(layout)
}

fn panes(layout: &Layout) -> impl Iterator<Item = TerminalId> + '_ {
    layout
        .views
        .iter()
        .flat_map(|view| &view.tabs)
        .flat_map(|tab| &tab.panes)
        .map(|pane| pane.terminal)
}

fn load_state(connection: &Connection) -> Result<State> {
    let home = load_home_layout(connection)?;
    let mut scoped = Vec::new();
    {
        let mut statement = connection.prepare(&format!(
            "SELECT worktree,state FROM worktree_layouts ORDER BY worktree LIMIT {}",
            MAX_WORKTREES + 1
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if scoped.len() == MAX_WORKTREES {
                return Err(corrupt("persisted worktree layout count exceeds its bound"));
            }
            let worktree =
                WorktreeId::from_bytes(fixed_bytes(row_blob(row, 0, 16)?, "worktree ID")?);
            let layout: Layout = decode(row_blob(row, 1, MAX_LAYOUT_BYTES)?, "worktree layout")?;
            layout
                .validate()
                .map_err(|error| corrupt(format!("invalid worktree layout: {error}")))?;
            scoped.push((worktree, layout));
        }
    }
    let mut scripts = Vec::new();
    {
        let mut statement = connection.prepare(&format!(
            "SELECT worktree,kind,terminal FROM scripts ORDER BY worktree,kind LIMIT {}",
            MAX_TERMINALS + 1
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if scripts.len() == MAX_TERMINALS {
                return Err(corrupt("persisted script count exceeds 128"));
            }
            scripts.push(ScriptRow {
                worktree: WorktreeId::from_bytes(fixed_bytes(
                    row_blob(row, 0, 16)?,
                    "worktree ID",
                )?),
                kind: row.get(1)?,
                terminal: TerminalId::from_bytes(fixed_bytes(
                    row_blob(row, 2, 16)?,
                    "terminal ID",
                )?),
            });
        }
    }
    let mut statement = connection.prepare(
        "SELECT terminal,incarnation,desired,entry FROM terminals ORDER BY terminal LIMIT 129",
    )?;
    let mut rows = statement.query([])?;
    let mut terminals = Vec::new();
    while let Some(row) = rows.next()? {
        if terminals.len() == MAX_TERMINALS {
            return Err(corrupt("persisted terminal count exceeds 128"));
        }
        let terminal = TerminalId::from_bytes(fixed_bytes(row_blob(row, 0, 16)?, "terminal ID")?);
        let incarnation =
            IncarnationId::from_bytes(fixed_bytes(row_blob(row, 1, 16)?, "incarnation ID")?);
        let desired = match row.get::<_, i64>(2)? {
            0 => false,
            1 => true,
            _ => return Err(corrupt("invalid terminal desired flag")),
        };
        let entry = decode_entry(row_blob(row, 3, MAX_ENTRY_BYTES)?)?;
        if entry.spec.terminal != terminal
            || entry.spec.incarnation != incarnation
            || entry.desired != desired
        {
            return Err(corrupt(
                "terminal row disagrees with its serialized identity or intent",
            ));
        }
        terminals.push(entry);
    }
    drop(rows);
    drop(statement);
    // Owner of every referenced terminal: the worktree it belongs to (None
    // for the home layout).
    let mut owners: BTreeMap<TerminalId, Option<WorktreeId>> = BTreeMap::new();
    let references = panes(&home)
        .map(|terminal| (terminal, None))
        .chain(scoped.iter().flat_map(|(worktree, layout)| {
            panes(layout).map(move |terminal| (terminal, Some(*worktree)))
        }))
        .chain(
            scripts
                .iter()
                .map(|script| (script.terminal, Some(script.worktree))),
        );
    for (terminal, owner) in references {
        if owners.insert(terminal, owner).is_some() {
            return Err(corrupt(format!(
                "terminal {terminal} has more than one owner"
            )));
        }
        if !terminals.iter().any(|entry| {
            entry.spec.terminal == terminal && entry.desired && entry.worktree == owner
        }) {
            return Err(corrupt(format!(
                "layout or script references missing, closed or foreign terminal {terminal}"
            )));
        }
    }
    for entry in &terminals {
        if entry.desired != owners.contains_key(&entry.spec.terminal) {
            return Err(corrupt(format!(
                "terminal {} intent disagrees with layout",
                entry.spec.terminal
            )));
        }
    }
    Ok(State {
        home,
        scoped,
        terminals,
        scripts,
    })
}

fn operation_hash(connection: &Connection, operation: OperationId) -> Result<Option<BlobHash>> {
    let mut statement =
        connection.prepare("SELECT payload_hash FROM operations WHERE operation=?1")?;
    let mut rows = statement.query([operation.as_bytes().as_slice()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(fixed_bytes(row_blob(row, 0, 32)?, "operation hash")?))
}

fn write_layout(connection: &Connection, layout: &Layout) -> Result<()> {
    let bytes = encode_layout(layout)?;
    if connection.execute("UPDATE layout SET state=?1 WHERE singleton=1", [bytes])? != 1 {
        return Err(corrupt(
            "layout update did not affect exactly one default workspace",
        ));
    }
    Ok(())
}

fn write_scoped_layout(
    connection: &Connection,
    worktree: WorktreeId,
    layout: &Layout,
) -> Result<()> {
    let bytes = encode_layout(layout)?;
    if connection.execute(
        "UPDATE worktree_layouts SET state=?2 WHERE worktree=?1",
        params![worktree.as_bytes().as_slice(), bytes],
    )? != 1
    {
        return Err(corrupt(
            "worktree layout update did not affect exactly one row",
        ));
    }
    Ok(())
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn sql_ms(value: u64) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| StoreError::Invalid("timestamp exceeds the SQLite range".into()))
}

fn write_entry(connection: &Connection, entry: &TerminalEntry) -> Result<()> {
    let bytes = encode_entry(entry)?;
    let changed = connection.execute(
        "UPDATE terminals SET desired=?3,entry=?4 WHERE terminal=?1 AND incarnation=?2",
        params![
            entry.spec.terminal.as_bytes().as_slice(),
            entry.spec.incarnation.as_bytes().as_slice(),
            entry.desired,
            bytes
        ],
    )?;
    if changed != 1 {
        return Err(corrupt(
            "terminal update did not affect exactly one incarnation",
        ));
    }
    Ok(())
}

fn encode_layout(layout: &Layout) -> Result<Vec<u8>> {
    let bytes = postcard::to_stdvec(layout)?;
    if bytes.len() > MAX_LAYOUT_BYTES {
        return Err(StoreError::Invalid("encoded layout exceeds 128 KiB".into()));
    }
    Ok(bytes)
}

fn encode_entry(entry: &TerminalEntry) -> Result<Vec<u8>> {
    let bytes = postcard::to_stdvec(entry)?;
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err(StoreError::Invalid(
            "encoded terminal metadata exceeds 512 KiB".into(),
        ));
    }
    Ok(bytes)
}

fn row_blob<'a>(row: &'a Row<'_>, column: usize, max: usize) -> Result<&'a [u8]> {
    match row.get_ref(column)? {
        ValueRef::Blob(bytes) if !bytes.is_empty() && bytes.len() <= max => Ok(bytes),
        ValueRef::Blob(_) => Err(corrupt(
            "persisted BLOB is empty or exceeds its encoded bound",
        )),
        _ => Err(corrupt("persisted value is not a BLOB")),
    }
}

fn fixed_bytes<const N: usize>(bytes: &[u8], field: &str) -> Result<[u8; N]> {
    bytes
        .try_into()
        .map_err(|_| corrupt(format!("invalid {field} length")))
}

fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8], field: &str) -> Result<T> {
    let (value, suffix) = postcard::take_from_bytes(bytes)
        .map_err(|error| corrupt(format!("invalid {field} encoding: {error}")))?;
    if !suffix.is_empty() {
        return Err(corrupt(format!("trailing bytes in {field}")));
    }
    Ok(value)
}

fn key_of(entry: &TerminalEntry) -> TerminalKey {
    TerminalKey {
        terminal: entry.spec.terminal,
        incarnation: entry.spec.incarnation,
    }
}

fn require_incarnation(entry: &TerminalEntry, key: TerminalKey) -> Result<()> {
    if entry.spec.incarnation != key.incarnation {
        return Err(StoreError::WrongIncarnation);
    }
    Ok(())
}

fn validate_text(field: &str, text: &str, max: usize) -> std::result::Result<(), String> {
    if text.len() > max || text.contains('\0') {
        return Err(format!("{field} exceeds {max} bytes or contains NUL"));
    }
    Ok(())
}

fn validate_launch(argv: &[String], cwd: &str) -> std::result::Result<(), String> {
    if argv.len() > MAX_ARG_COUNT {
        return Err("argv exceeds 256 arguments".into());
    }
    let mut total = 0usize;
    for arg in argv {
        validate_text("argument", arg, MAX_TEXT_BYTES)?;
        total += arg.len();
    }
    if total > MAX_ARGV_BYTES {
        return Err("argv exceeds 64 KiB".into());
    }
    validate_text("cwd", cwd, MAX_TEXT_BYTES)
}

fn validate_process(info: &ProcessInfo) -> std::result::Result<(), String> {
    info.size.validate().map_err(|error| error.to_string())?;
    validate_launch(&info.argv, &info.cwd)?;
    validate_cursor(info.tail)?;
    if let ProcessState::Backpressured {
        reason,
        quota_bytes,
        ..
    } = &info.state
    {
        validate_text("backpressure reason", reason, MAX_TEXT_BYTES)?;
        if *quota_bytes == 0 {
            return Err("backpressure quota is zero".into());
        }
    }
    Ok(())
}

fn validate_cursor(cursor: LogCursor) -> std::result::Result<(), String> {
    if cursor.next_byte
        > cursor
            .next_record
            .saturating_mul(fl_prochost_proto::MAX_OUTPUT_BYTES as u64)
    {
        return Err("journal byte cursor exceeds its record capacity".into());
    }
    Ok(())
}

fn validate_checkpoint(reference: &CheckpointRef) -> std::result::Result<(), String> {
    reference
        .size
        .validate()
        .map_err(|error| error.to_string())?;
    validate_cursor(reference.cursor)?;
    validate_text("engine identity", &reference.engine_id, MAX_ENGINE_BYTES)?;
    if reference.engine_id.trim().is_empty() || reference.engine_id.chars().any(char::is_control) {
        return Err("engine identity is blank or contains control characters".into());
    }
    Ok(())
}

fn validate_entry(entry: &TerminalEntry) -> std::result::Result<(), String> {
    entry.spec.validate().map_err(|error| error.to_string())?;
    if entry.spec.env.len() > MAX_ENV_COUNT {
        return Err("environment exceeds 128 overrides".into());
    }
    let mut total = 0usize;
    for (key, value) in &entry.spec.env {
        total = total.saturating_add(key.len()).saturating_add(value.len());
        if key.is_empty()
            || key.contains(['=', '\0'])
            || value.contains('\0')
            || total > MAX_ENV_BYTES
        {
            return Err("environment contains an invalid override or exceeds 128 KiB".into());
        }
    }
    if let Some(process) = &entry.process {
        if process.terminal != entry.spec.terminal || process.incarnation != entry.spec.incarnation
        {
            return Err("process metadata belongs to another terminal incarnation".into());
        }
        validate_process(process)?;
    }
    if let Some(message) = &entry.launch_error {
        validate_text("launch error", message, MAX_TEXT_BYTES)?;
    }
    if let Some(reference) = &entry.checkpoint {
        validate_checkpoint(reference)?;
    }
    Ok(())
}

// Borrow all strings before validation. Collection visitors reject a bogus
// postcard length before reserving memory, including a tiny BLOB declaring an
// enormous argv/environment. This wire shape is identical to TerminalEntry.
#[derive(Deserialize)]
struct EntryWire<'a> {
    #[serde(borrow)]
    spec: SpecWire<'a>,
    desired: bool,
    worktree: Option<fl_model::WorktreeId>,
    #[serde(borrow)]
    process: Option<ProcessWire<'a>>,
    launch_error: Option<&'a str>,
    #[serde(borrow)]
    checkpoint: Option<CheckpointWire<'a>>,
}

#[derive(Deserialize)]
struct SpecWire<'a> {
    terminal: TerminalId,
    incarnation: IncarnationId,
    #[serde(borrow, deserialize_with = "deserialize_argv")]
    argv: Vec<&'a str>,
    cwd: &'a str,
    #[serde(borrow, deserialize_with = "deserialize_env")]
    env: BTreeMap<&'a str, &'a str>,
    size: Size,
    quota_bytes: u64,
}

#[derive(Deserialize)]
struct ProcessWire<'a> {
    terminal: TerminalId,
    incarnation: IncarnationId,
    pid: u32,
    size: Size,
    #[serde(borrow, deserialize_with = "deserialize_argv")]
    argv: Vec<&'a str>,
    cwd: &'a str,
    #[serde(borrow)]
    state: StateWire<'a>,
    tail: LogCursor,
    echo: bool,
}

#[derive(Deserialize)]
enum StateWire<'a> {
    Running,
    Backpressured {
        retained_bytes: u64,
        quota_bytes: u64,
        reason: &'a str,
    },
    Draining(fl_model::ExitStatus),
    Exited(fl_model::ExitStatus),
    Lost,
}

#[derive(Deserialize)]
struct CheckpointWire<'a> {
    hash: BlobHash,
    cursor: LogCursor,
    size: Size,
    engine_id: &'a str,
}

fn decode_entry(bytes: &[u8]) -> Result<TerminalEntry> {
    let wire: EntryWire<'_> = decode(bytes, "terminal metadata")?;
    let entry = TerminalEntry {
        spec: SpawnSpec {
            terminal: wire.spec.terminal,
            incarnation: wire.spec.incarnation,
            argv: wire.spec.argv.into_iter().map(str::to_owned).collect(),
            cwd: wire.spec.cwd.into(),
            env: wire
                .spec
                .env
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
            size: wire.spec.size,
            quota_bytes: wire.spec.quota_bytes,
        },
        desired: wire.desired,
        worktree: wire.worktree,
        process: wire.process.map(|process| ProcessInfo {
            terminal: process.terminal,
            incarnation: process.incarnation,
            pid: process.pid,
            size: process.size,
            argv: process.argv.into_iter().map(str::to_owned).collect(),
            cwd: process.cwd.into(),
            state: match process.state {
                StateWire::Running => ProcessState::Running,
                StateWire::Backpressured {
                    retained_bytes,
                    quota_bytes,
                    reason,
                } => ProcessState::Backpressured {
                    retained_bytes,
                    quota_bytes,
                    reason: reason.into(),
                },
                StateWire::Draining(status) => ProcessState::Draining(status),
                StateWire::Exited(status) => ProcessState::Exited(status),
                StateWire::Lost => ProcessState::Lost,
            },
            tail: process.tail,
            echo: process.echo,
        }),
        launch_error: wire.launch_error.map(str::to_owned),
        checkpoint: wire.checkpoint.map(|checkpoint| CheckpointRef {
            hash: checkpoint.hash,
            cursor: checkpoint.cursor,
            size: checkpoint.size,
            engine_id: checkpoint.engine_id.into(),
        }),
    };
    validate_entry(&entry)
        .map_err(|error| corrupt(format!("invalid terminal metadata: {error}")))?;
    Ok(entry)
}

fn deserialize_argv<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<&'de str>, D::Error> {
    struct Arguments;
    impl<'de> de::Visitor<'de> for Arguments {
        type Value = Vec<&'de str>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("at most 256 arguments")
        }
        fn visit_seq<A: de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            if sequence
                .size_hint()
                .is_some_and(|length| length > MAX_ARG_COUNT)
            {
                return Err(de::Error::custom("too many arguments"));
            }
            let mut values = Vec::new();
            while let Some(value) = sequence.next_element()? {
                if values.len() == MAX_ARG_COUNT {
                    return Err(de::Error::custom("too many arguments"));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Arguments)
}

fn deserialize_env<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<BTreeMap<&'de str, &'de str>, D::Error> {
    struct Environment;
    impl<'de> de::Visitor<'de> for Environment {
        type Value = BTreeMap<&'de str, &'de str>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("at most 128 unique environment overrides")
        }
        fn visit_map<A: de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            if map.size_hint().is_some_and(|length| length > MAX_ENV_COUNT) {
                return Err(de::Error::custom("too many environment overrides"));
            }
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry()? {
                if values.len() == MAX_ENV_COUNT || values.insert(key, value).is_some() {
                    return Err(de::Error::custom(
                        "too many or duplicate environment overrides",
                    ));
                }
            }
            Ok(values)
        }
    }
    deserializer.deserialize_map(Environment)
}

fn corrupt(message: impl Into<String>) -> StoreError {
    StoreError::Corrupt(message.into())
}

fn descriptor_path(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

fn private_root(path: &Path) -> Result<File> {
    let mut directory = File::open(if path.is_absolute() { "/" } else { "." })?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name.as_bytes(),
            Component::ParentDir => b"..",
            Component::Prefix(_) => {
                return Err(StoreError::Invalid("unsupported state path prefix".into()));
            }
        };
        let name = CString::new(name)
            .map_err(|_| StoreError::Invalid("state path contains NUL".into()))?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mut fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            let created = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if created < 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                return Err(io::Error::last_os_error().into());
            }
            if created == 0 {
                directory.sync_all()?;
            }
            fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        }
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    let metadata = directory.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(corrupt("state directory belongs to another user"));
    }
    directory.set_permissions(Permissions::from_mode(0o700))?;
    Ok(directory)
}

fn private_file(directory: &File, name: &str, create: bool) -> Result<Option<(File, bool)>> {
    let name = CString::new(name).map_err(|_| corrupt("invalid private filename"))?;
    let flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let mut fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    let mut existed = true;
    if fd < 0 && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
        if !create {
            return Ok(None);
        }
        fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
        };
        if fd < 0 && io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists {
            fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        } else {
            existed = false;
        }
    }
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    check_private_file(&file)?;
    Ok(Some((file, existed)))
}

fn create_private_file(directory: &File, name: &str) -> Result<File> {
    let name = CString::new(name).map_err(|_| corrupt("invalid backup filename"))?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    check_private_file(&file)?;
    Ok(file)
}

fn check_private_file(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(corrupt(
            "state file is not a singly-linked regular file owned by this user",
        ));
    }
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(())
}

#[path = "tree.rs"]
mod tree;
use tree::{MAX_WORKTREES, load_tree};
#[path = "review.rs"]
mod review;

#[path = "routing.rs"]
mod routing;

#[path = "chat.rs"]
pub mod chat;

pub use chat::{ChatAppend, ChatEventEffects, NewInboxItem, PendingKind, PendingRequest};

#[path = "sections.rs"]
mod sections;

#[path = "cli.rs"]
mod cli;

#[cfg(test)]
#[path = "database_tests.rs"]
mod tests;
