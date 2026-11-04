//! Durable chat sessions, append-only transcripts, pending decisions and the
//! inbox (schema v6 → v7).
//!
//! Events are appended before publication and keyed by a strictly increasing
//! per-session sequence. The adapter's own sequence is incarnation-local, so
//! the daemon assigns the durable one; a repeat of the newest record is
//! idempotent, while a gap or a conflicting duplicate is refused.
//!
//! Search uses SQLite FTS5 over the event's extracted text. When the build
//! lacks FTS5 the migration creates a bounded substring index and search falls
//! back to `LIKE`; both modes are recorded in `chat_meta.fts5`.
use super::*;
use fl_model::chat::{
    ChatEventPage, ChatHit, ChatSession, EventRecord, InboxItem, InboxKind, InboxState,
    ResumeTarget, SessionState, StatusSource,
};

const MAX_CHAT_EVENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_CHAT_PAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHAT_TITLE_BYTES: usize = 4096;
const MAX_CHAT_REQUEST_BYTES: usize = 1024;
const MAX_CHAT_PROVIDER_BYTES: usize = 256;
const MAX_CHAT_MODEL_BYTES: usize = 1024;
const MAX_CHAT_LABEL_BYTES: usize = 4096;
const MAX_CHAT_RESUME_BYTES: usize = 64 * 1024;
const MAX_CHAT_QUERY_BYTES: usize = 1024;

pub(super) const SCHEMA_V7: &str = "
    CREATE TABLE chat_meta (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        fts5 INTEGER NOT NULL CHECK(fts5 IN (0, 1))
    ) STRICT;
    CREATE TABLE chat_sessions (
        id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 16),
        worktree BLOB NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
        provider TEXT NOT NULL CHECK(length(provider) BETWEEN 1 AND 256),
        label TEXT NOT NULL CHECK(length(label) BETWEEN 1 AND 4096),
        status BLOB NOT NULL CHECK(length(status) BETWEEN 1 AND 4096),
        status_source BLOB NOT NULL CHECK(length(status_source) = 1),
        model TEXT CHECK(model IS NULL OR length(model) <= 1024),
        thinking TEXT CHECK(thinking IS NULL OR length(thinking) <= 1024),
        permission_mode BLOB NOT NULL CHECK(length(permission_mode) = 1),
        resume BLOB CHECK(resume IS NULL OR length(resume) BETWEEN 1 AND 65536),
        seq BLOB NOT NULL CHECK(length(seq) = 8),
        incarnation BLOB NOT NULL CHECK(length(incarnation) = 16),
        archived INTEGER NOT NULL CHECK(archived IN (0, 1)),
        created_ms INTEGER NOT NULL CHECK(created_ms >= 0),
        updated_ms INTEGER NOT NULL CHECK(updated_ms >= 0)
    ) STRICT;
    CREATE INDEX chat_sessions_by_worktree ON chat_sessions(worktree, created_ms);
    CREATE TABLE chat_events (
        session BLOB NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
        sequence BLOB NOT NULL CHECK(length(sequence) = 8),
        record BLOB NOT NULL CHECK(length(record) BETWEEN 1 AND 2097152),
        search_text TEXT NOT NULL CHECK(length(search_text) <= 65536),
        PRIMARY KEY (session, sequence)
    ) STRICT;
    CREATE TABLE chat_pending (
        session BLOB NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
        request TEXT NOT NULL CHECK(length(request) BETWEEN 1 AND 1024),
        kind INTEGER NOT NULL CHECK(kind IN (0, 1)),
        payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 2097152),
        created_ms INTEGER NOT NULL CHECK(created_ms >= 0),
        PRIMARY KEY (session, request)
    ) STRICT;
    CREATE TABLE chat_inbox (
        id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 16),
        session BLOB NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
        worktree BLOB NOT NULL CHECK(length(worktree) = 16),
        kind INTEGER NOT NULL CHECK(kind BETWEEN 0 AND 3),
        state INTEGER NOT NULL CHECK(state BETWEEN 0 AND 2),
        title TEXT NOT NULL CHECK(length(title) BETWEEN 1 AND 4096),
        dedupe TEXT NOT NULL UNIQUE CHECK(length(dedupe) BETWEEN 1 AND 1024),
        created_ms INTEGER NOT NULL CHECK(created_ms >= 0)
    ) STRICT;
    CREATE INDEX chat_inbox_by_session ON chat_inbox(session, created_ms);
";

/// External-content FTS5 index over [`chat_events::search_text`].
pub(super) const FTS_V7: &str = "
    CREATE VIRTUAL TABLE chat_fts USING fts5(
        search_text,
        content='chat_events',
        content_rowid='rowid'
    );
    CREATE TRIGGER chat_events_index_insert AFTER INSERT ON chat_events BEGIN
        INSERT INTO chat_fts(rowid, search_text) VALUES (new.rowid, new.search_text);
    END;
    CREATE TRIGGER chat_events_index_delete AFTER DELETE ON chat_events BEGIN
        INSERT INTO chat_fts(chat_fts, rowid, search_text)
        VALUES ('delete', old.rowid, old.search_text);
    END;
";

/// Substring fallback when SQLite was built without FTS5.
pub(super) const FALLBACK_V7: &str = "
    CREATE TABLE chat_search (
        rowid INTEGER PRIMARY KEY,
        session BLOB NOT NULL CHECK(length(session) = 16),
        sequence BLOB NOT NULL CHECK(length(sequence) = 8),
        text TEXT NOT NULL CHECK(length(text) <= 65536)
    ) STRICT;
    CREATE TRIGGER chat_events_index_insert AFTER INSERT ON chat_events BEGIN
        INSERT INTO chat_search(rowid, session, sequence, text)
        VALUES (new.rowid, new.session, new.sequence, new.search_text);
    END;
    CREATE TRIGGER chat_events_index_delete AFTER DELETE ON chat_events BEGIN
        DELETE FROM chat_search WHERE rowid = old.rowid;
    END;
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Approval,
    Question,
}

/// One durable approval or question request. Its payload is the normalized
/// [`ChatEvent`] so a reconnect can show the exact card again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRequest {
    pub request: String,
    pub kind: PendingKind,
    pub payload: Vec<u8>,
    pub created_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInboxItem {
    pub kind: InboxKind,
    pub title: String,
    /// Stable identity: replayed events never duplicate an item.
    pub dedupe: String,
    pub created_ms: u64,
}

/// Everything one event changes, committed with the event itself.
#[derive(Debug, Clone, Default)]
pub struct ChatEventEffects {
    pub status: Option<(SessionState, StatusSource)>,
    pub resume: Option<ResumeTarget>,
    pub add_pending: Vec<PendingRequest>,
    pub resolve_pending: Vec<String>,
    /// Dedupe keys of inbox cards to dismiss with this event.
    pub dismiss_inbox: Vec<String>,
    pub inbox: Vec<NewInboxItem>,
    /// Cold restore could not validate the stored resume target.
    pub archive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatAppend {
    pub session: ChatSession,
    /// False when this was a byte-identical replay of the newest event.
    pub inserted: bool,
}

impl Database {
    pub fn create_chat_session(&mut self, session: &ChatSession) -> Result<()> {
        validate_session(session)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM worktrees WHERE id=?1)",
            [session.worktree.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StoreError::Missing("chat worktree not found".into()));
        }
        transaction.execute(
            "INSERT INTO chat_sessions(
                id,worktree,provider,label,status,status_source,model,thinking,
                permission_mode,resume,seq,incarnation,archived,created_ms,updated_ms
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,0,?13,?14)",
            params![
                session.id.as_bytes().as_slice(),
                session.worktree.as_bytes().as_slice(),
                session.provider,
                session.label,
                postcard::to_stdvec(&session.status)?,
                postcard::to_stdvec(&session.status_source)?,
                session.model,
                session.thinking,
                postcard::to_stdvec(&session.permission_mode)?,
                session
                    .resume
                    .as_ref()
                    .map(postcard::to_stdvec)
                    .transpose()?,
                sequence_key(session.seq),
                uuid::Uuid::new_v4().as_bytes().as_slice(),
                sql_ms(session.created_ms)?,
                sql_ms(session.updated_ms)?,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn chat_sessions(&self, worktree: WorktreeId) -> Result<Vec<ChatSession>> {
        let mut statement = self.connection.prepare(
            "SELECT id,worktree,provider,label,status,status_source,model,thinking,
                    permission_mode,resume,seq,created_ms,updated_ms
             FROM chat_sessions WHERE worktree=?1 ORDER BY created_ms,id",
        )?;
        let rows = statement.query_map([worktree.as_bytes().as_slice()], decode_session)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    pub fn all_chat_sessions(&self) -> Result<Vec<ChatSession>> {
        let mut statement = self.connection.prepare(
            "SELECT id,worktree,provider,label,status,status_source,model,thinking,
                    permission_mode,resume,seq,created_ms,updated_ms
             FROM chat_sessions ORDER BY created_ms,id",
        )?;
        let rows = statement.query_map([], decode_session)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    pub fn chat_session(&self, id: uuid::Uuid) -> Result<ChatSession> {
        Ok(self.chat_row(id)?.0)
    }

    /// Archived sessions keep their transcript but accept no new turns.
    pub fn chat_archived(&self, id: uuid::Uuid) -> Result<bool> {
        Ok(self.chat_row(id)?.1)
    }

    fn chat_row(&self, id: uuid::Uuid) -> Result<(ChatSession, bool)> {
        let transaction = self.connection.unchecked_transaction()?;
        let found = load_chat(&transaction, id)?;
        transaction.commit()?;
        found.ok_or_else(|| StoreError::Missing("chat session not found".into()))
    }

    /// Append one event and its effects atomically. The session row, pending
    /// decisions and inbox items commit together, before any caller publishes.
    pub fn append_chat_event(
        &mut self,
        id: uuid::Uuid,
        record: &EventRecord,
        effects: ChatEventEffects,
    ) -> Result<ChatAppend> {
        if record.session_id != id {
            return Err(StoreError::Invalid(
                "chat event belongs to another session".into(),
            ));
        }
        let bytes = postcard::to_stdvec(record)?;
        if bytes.len() > MAX_CHAT_EVENT_BYTES {
            return Err(StoreError::Invalid("chat event exceeds byte limit".into()));
        }
        let search = fl_model::chat::search_text(&record.event);
        if search.len() > fl_model::chat::MAX_CHAT_TEXT {
            return Err(StoreError::Invalid("chat search text exceeds limit".into()));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut session, archived) = load_chat(&transaction, id)?
            .ok_or_else(|| StoreError::Missing("chat session not found".into()))?;
        if archived {
            return Err(StoreError::Invalid("chat session is archived".into()));
        }
        if record.sequence == session.seq {
            let existing: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT record FROM chat_events WHERE session=?1 AND sequence=?2",
                    params![id.as_bytes().as_slice(), sequence_key(record.sequence)],
                    |row| row.get(0),
                )
                .optional()?;
            match existing {
                Some(existing) if existing == bytes => {
                    transaction.commit()?;
                    return Ok(ChatAppend {
                        session,
                        inserted: false,
                    });
                }
                _ => {
                    return Err(StoreError::Duplicate(
                        "chat event sequence already exists".into(),
                    ));
                }
            }
        }
        if record.sequence < session.seq {
            return Err(StoreError::Duplicate(format!(
                "chat event sequence {} is behind the durable cursor {}",
                record.sequence, session.seq
            )));
        }
        let expected = session
            .seq
            .checked_add(1)
            .ok_or_else(|| StoreError::Invalid("chat sequence overflow".into()))?;
        if record.sequence != expected {
            return Err(StoreError::Invalid(format!(
                "chat event sequence gap: expected {expected}, got {}",
                record.sequence
            )));
        }
        transaction.execute(
            "INSERT INTO chat_events(session,sequence,record,search_text) VALUES(?1,?2,?3,?4)",
            params![
                id.as_bytes().as_slice(),
                sequence_key(record.sequence),
                bytes,
                search
            ],
        )?;
        session.seq = record.sequence;
        session.updated_ms = session.updated_ms.max(record.timestamp_ms);
        let incarnation = record.incarnation.as_bytes().to_vec();
        if let Some(resume) = &effects.resume {
            session.resume = Some(resume.clone());
        }
        if let Some((status, source)) = &effects.status {
            session.status = status.clone();
            session.status_source = *source;
        }
        if effects.archive {
            session.status = SessionState::Closed;
            session.status_source = StatusSource::Structured;
        }
        if let Some(target) = &effects.resume {
            validate_resume(target)?;
        }
        transaction.execute(
            "UPDATE chat_sessions SET seq=?2,updated_ms=?3,incarnation=?4,status=?5,
                    status_source=?6,resume=?7,archived=?8 WHERE id=?1",
            params![
                id.as_bytes().as_slice(),
                sequence_key(session.seq),
                sql_ms(session.updated_ms)?,
                incarnation.as_slice(),
                postcard::to_stdvec(&session.status)?,
                postcard::to_stdvec(&session.status_source)?,
                session
                    .resume
                    .as_ref()
                    .map(postcard::to_stdvec)
                    .transpose()?,
                effects.archive as i64,
            ],
        )?;
        for request in effects.resolve_pending {
            transaction.execute(
                "DELETE FROM chat_pending WHERE session=?1 AND request=?2",
                params![id.as_bytes().as_slice(), request],
            )?;
        }
        for pending in effects.add_pending {
            validate_pending(&pending)?;
            transaction.execute(
                "INSERT INTO chat_pending(session,request,kind,payload,created_ms)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(session,request) DO UPDATE SET
                    kind=excluded.kind,payload=excluded.payload",
                params![
                    id.as_bytes().as_slice(),
                    pending.request,
                    pending_kind_code(pending.kind),
                    pending.payload,
                    sql_ms(pending.created_ms)?,
                ],
            )?;
        }
        for dedupe in effects.dismiss_inbox {
            dismiss_inbox_dedupe(&transaction, &dedupe)?;
        }
        for item in effects.inbox {
            insert_inbox(&transaction, id, session.worktree, &item)?;
        }
        transaction.commit()?;
        Ok(ChatAppend {
            session,
            inserted: true,
        })
    }

    /// Page durable events from `from` (inclusive). `more` means at least one
    /// later event exists; the page is additionally bounded by encoded bytes.
    pub fn chat_event_page(&self, id: uuid::Uuid, from: u64, limit: u16) -> Result<ChatEventPage> {
        let transaction = self.connection.unchecked_transaction()?;
        let _ = load_chat(&transaction, id)?
            .ok_or_else(|| StoreError::Missing("chat session not found".into()))?;
        let incarnation = chat_incarnation(&transaction, id)?;
        let limit = usize::from(limit.clamp(1, fl_model::chat::MAX_CHAT_MESSAGES as u16));
        let start = sequence_key(from.max(1));
        let mut statement = transaction.prepare(
            "SELECT record FROM chat_events
             WHERE session=?1 AND sequence>=?2 ORDER BY sequence LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![id.as_bytes().as_slice(), start, (limit + 1) as i64],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        let mut events = Vec::new();
        let mut bytes = 0usize;
        let mut more = false;
        for row in rows {
            let record = row?;
            if events.len() >= limit || bytes + record.len() > MAX_CHAT_PAGE_BYTES {
                more = true;
                break;
            }
            let event: EventRecord = postcard::from_bytes(&record)?;
            if event.session_id != id {
                return Err(corrupt("chat event belongs to another session"));
            }
            bytes += record.len();
            events.push(event);
        }
        drop(statement);
        if events.len() == limit {
            let last = events.last().map(|event| sequence_key(event.sequence));
            if let Some(last) = last {
                let exists: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM chat_events WHERE session=?1 AND sequence>?2)",
                    params![id.as_bytes().as_slice(), last],
                    |row| row.get(0),
                )?;
                more |= exists;
            }
        }
        transaction.commit()?;
        Ok(ChatEventPage {
            session: id,
            incarnation,
            events,
            more,
        })
    }

    pub fn chat_pending(&self, id: uuid::Uuid) -> Result<Vec<PendingRequest>> {
        let mut statement = self.connection.prepare(
            "SELECT request,kind,payload,created_ms FROM chat_pending
             WHERE session=?1 ORDER BY created_ms,request",
        )?;
        let rows = statement.query_map([id.as_bytes().as_slice()], |row| {
            let created: i64 = row.get(3)?;
            let kind = pending_kind(row.get(1)?).map_err(to_sqlite)?;
            let created_ms = u64::try_from(created)
                .map_err(|_| corrupt("negative pending time"))
                .map_err(to_sqlite)?;
            Ok(PendingRequest {
                request: row.get(0)?,
                kind,
                payload: row.get(2)?,
                created_ms,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn clear_chat_pending(&mut self, id: uuid::Uuid) -> Result<()> {
        self.connection.execute(
            "DELETE FROM chat_pending WHERE session=?1",
            [id.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    /// Remove one pending decision and dismiss its inbox card. Called after the
    /// provider acknowledged an explicit human decision, so a reconnect can
    /// never answer it again.
    pub fn resolve_chat_pending(
        &mut self,
        id: uuid::Uuid,
        request: &str,
        dedupe: &str,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM chat_pending WHERE session=?1 AND request=?2",
            params![id.as_bytes().as_slice(), request],
        )?;
        dismiss_inbox_dedupe(&transaction, dedupe)?;
        transaction.commit()?;
        Ok(())
    }

    /// Mark a session failed without fabricating a provider turn. Pending
    /// decisions become unanswerable and are cleared.
    pub fn fail_chat_session(
        &mut self,
        id: uuid::Uuid,
        message: &str,
        now: u64,
    ) -> Result<ChatSession> {
        validate_chat_text("chat failure", message, MAX_CHAT_TITLE_BYTES)?;
        let status = SessionState::Failed {
            message: message.to_owned(),
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE chat_sessions SET status=?2,status_source=?3,updated_ms=max(updated_ms,?4)
             WHERE id=?1",
            params![
                id.as_bytes().as_slice(),
                postcard::to_stdvec(&status)?,
                postcard::to_stdvec(&StatusSource::Heuristic)?,
                sql_ms(now)?,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Missing("chat session not found".into()));
        }
        transaction.execute(
            "DELETE FROM chat_pending WHERE session=?1",
            [id.as_bytes().as_slice()],
        )?;
        transaction.execute(
            "UPDATE chat_inbox SET state=?2 WHERE session=?1 AND kind IN (?3,?4) AND state != ?2",
            params![
                id.as_bytes().as_slice(),
                inbox_state_code(InboxState::Dismissed),
                inbox_kind_code(InboxKind::Approval),
                inbox_kind_code(InboxKind::Question),
            ],
        )?;
        let (session, _) = load_chat(&transaction, id)?
            .ok_or_else(|| StoreError::Missing("chat session not found".into()))?;
        transaction.commit()?;
        Ok(session)
    }

    /// Mark a session closed by explicit user action (or a clean shutdown).
    pub fn close_chat_session(&mut self, id: uuid::Uuid, now: u64) -> Result<ChatSession> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE chat_sessions SET status=?2,status_source=?3,updated_ms=max(updated_ms,?4)
             WHERE id=?1",
            params![
                id.as_bytes().as_slice(),
                postcard::to_stdvec(&SessionState::Closed)?,
                postcard::to_stdvec(&StatusSource::Structured)?,
                sql_ms(now)?,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Missing("chat session not found".into()));
        }
        transaction.execute(
            "DELETE FROM chat_pending WHERE session=?1",
            [id.as_bytes().as_slice()],
        )?;
        transaction.execute(
            "UPDATE chat_inbox SET state=?2 WHERE session=?1 AND kind IN (?3,?4) AND state != ?2",
            params![
                id.as_bytes().as_slice(),
                inbox_state_code(InboxState::Dismissed),
                inbox_kind_code(InboxKind::Approval),
                inbox_kind_code(InboxKind::Question),
            ],
        )?;
        let (session, _) = load_chat(&transaction, id)?
            .ok_or_else(|| StoreError::Missing("chat session not found".into()))?;
        transaction.commit()?;
        Ok(session)
    }

    pub fn dismiss_chat_inbox(&mut self, id: uuid::Uuid) -> Result<InboxItem> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = load_inbox(&transaction, id)?
            .ok_or_else(|| StoreError::Missing("inbox item not found".into()))?;
        transaction.execute(
            "UPDATE chat_inbox SET state=?2 WHERE id=?1",
            params![
                id.as_bytes().as_slice(),
                inbox_state_code(InboxState::Dismissed)
            ],
        )?;
        transaction.commit()?;
        Ok(InboxItem {
            state: InboxState::Dismissed,
            ..item
        })
    }

    /// Add one inbox card for a session. `dedupe` makes replays idempotent.
    pub fn add_chat_inbox(&mut self, session: uuid::Uuid, item: NewInboxItem) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let worktree: Vec<u8> = transaction
            .query_row(
                "SELECT worktree FROM chat_sessions WHERE id=?1",
                [session.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(|_| StoreError::Missing("chat session not found".into()))?;
        let worktree = WorktreeId::from_bytes(array16(&worktree)?);
        insert_inbox(&transaction, session, worktree, &item)?;
        transaction.commit()?;
        Ok(())
    }

    /// Unread and read items, newest first. Dismissed items are omitted.
    pub fn chat_inbox(&self) -> Result<Vec<InboxItem>> {
        let mut statement = self.connection.prepare(
            "SELECT id,session,worktree,kind,state,title,created_ms FROM chat_inbox
             WHERE state != ?1 ORDER BY created_ms DESC,id LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                inbox_state_code(InboxState::Dismissed),
                fl_model::chat::MAX_CHAT_HISTORY as i64
            ],
            decode_inbox,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    /// Normalize every session after a daemon restart. No live adapter exists,
    /// so pending decisions are unanswerable: they are cleared, their inbox
    /// cards are dismissed, and a previously working session reads idle. This
    /// never manufactures a successful turn.
    pub fn reset_chats_after_restart(&mut self, now: u64) -> Result<Vec<ChatSession>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM chat_pending", [])?;
        transaction.execute(
            "UPDATE chat_inbox SET state=?1 WHERE kind IN (?2,?3) AND state != ?1",
            params![
                inbox_state_code(InboxState::Dismissed),
                inbox_kind_code(InboxKind::Approval),
                inbox_kind_code(InboxKind::Question),
            ],
        )?;
        transaction.execute(
            "UPDATE chat_sessions SET status=?1,status_source=?2,updated_ms=max(updated_ms,?3)
             WHERE archived=0 AND status != ?4",
            params![
                postcard::to_stdvec(&SessionState::Idle)?,
                postcard::to_stdvec(&StatusSource::Heuristic)?,
                sql_ms(now)?,
                postcard::to_stdvec(&SessionState::Closed)?,
            ],
        )?;
        let mut statement = transaction.prepare(
            "SELECT id,worktree,provider,label,status,status_source,model,thinking,
                    permission_mode,resume,seq,created_ms,updated_ms
             FROM chat_sessions ORDER BY created_ms,id",
        )?;
        let rows = statement.query_map([], decode_session)?;
        let sessions = rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        drop(statement);
        transaction.commit()?;
        Ok(sessions)
    }

    /// Mark a session archived read-only: its transcript stays searchable and
    /// pageable, but the daemon will not relaunch it.
    pub fn archive_chat(&mut self, id: uuid::Uuid, now: u64) -> Result<ChatSession> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE chat_sessions SET archived=1,status=?2,status_source=?3,updated_ms=max(updated_ms,?4)
             WHERE id=?1",
            params![
                id.as_bytes().as_slice(),
                postcard::to_stdvec(&SessionState::Closed)?,
                postcard::to_stdvec(&StatusSource::Structured)?,
                sql_ms(now)?,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::Missing("chat session not found".into()));
        }
        let (session, _) = load_chat(&transaction, id)?
            .ok_or_else(|| StoreError::Missing("chat session not found".into()))?;
        transaction.commit()?;
        Ok(session)
    }

    /// Set the stored resume target (after a validated cold restore).
    pub fn set_chat_resume(
        &mut self,
        id: uuid::Uuid,
        resume: &ResumeTarget,
    ) -> Result<ChatSession> {
        validate_resume(resume)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE chat_sessions SET resume=?2 WHERE id=?1",
            params![id.as_bytes().as_slice(), postcard::to_stdvec(resume)?],
        )?;
        if changed == 0 {
            return Err(StoreError::Missing("chat session not found".into()));
        }
        let (session, _) = load_chat(&transaction, id)?
            .ok_or_else(|| StoreError::Missing("chat session not found".into()))?;
        transaction.commit()?;
        Ok(session)
    }

    pub fn search_chats(&self, query: &str, limit: u16) -> Result<Vec<ChatHit>> {
        if query.trim().is_empty() || query.len() > MAX_CHAT_QUERY_BYTES {
            return Err(StoreError::Invalid(
                "chat query must be 1..1024 bytes".into(),
            ));
        }
        let limit = usize::from(limit.clamp(1, fl_model::chat::MAX_CHAT_HISTORY as u16));
        let fts5 = chat_fts_mode(&self.connection)?;
        if fts5 {
            self.search_fts(query, limit)
        } else {
            self.search_like(query, limit)
        }
    }

    fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<ChatHit>> {
        let phrase = fts_phrase(query);
        let mut statement = self.connection.prepare(
            "SELECT e.session,e.sequence,s.worktree,
                    snippet(chat_fts,0,'','',' … ',16)
             FROM chat_fts
             JOIN chat_events e ON e.rowid=chat_fts.rowid
             JOIN chat_sessions s ON s.id=e.session
             WHERE chat_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = statement.query_map(params![phrase, limit as i64], decode_hit)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    fn search_like(&self, query: &str, limit: usize) -> Result<Vec<ChatHit>> {
        let pattern = format!("%{}%", like_escape(query.trim()));
        let mut statement = self.connection.prepare(
            "SELECT c.session,c.sequence,c.worktree,c.text
             FROM chat_search c JOIN chat_sessions s ON s.id=c.session
             WHERE c.text LIKE ?1 ESCAPE '\\'
             ORDER BY c.rowid DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![pattern, limit as i64], |row| {
            let sequence: Vec<u8> = row.get(1)?;
            let text: String = row.get(3)?;
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                sequence_u64(&sequence).map_err(to_sqlite)?,
                row.get::<_, Vec<u8>>(2)?,
                text,
            ))
        })?;
        let mut hits = Vec::new();
        for row in rows {
            let (session, sequence, worktree, text) = row?;
            hits.push(ChatHit {
                session: uuid_from(&session)?,
                worktree: WorktreeId::from_bytes(array16(&worktree)?),
                sequence,
                snippet: substring_snippet(&text, query.trim()),
            });
        }
        Ok(hits)
    }
}

fn validate_session(session: &ChatSession) -> Result<()> {
    if session.id.is_nil() {
        return Err(StoreError::Invalid("nil chat session id".into()));
    }
    validate_chat_text("chat provider", &session.provider, MAX_CHAT_PROVIDER_BYTES)?;
    validate_chat_text("chat label", &session.label, MAX_CHAT_LABEL_BYTES)?;
    for value in [&session.model, &session.thinking].into_iter().flatten() {
        validate_chat_text("chat setting", value, MAX_CHAT_MODEL_BYTES)?;
    }
    let status = postcard::to_stdvec(&session.status)?;
    if status.len() > MAX_CHAT_TITLE_BYTES {
        return Err(StoreError::Invalid("chat status exceeds byte limit".into()));
    }
    if let Some(resume) = &session.resume {
        validate_resume(resume)?;
    }
    if session.created_ms > session.updated_ms {
        return Err(StoreError::Invalid(
            "chat session updated before it was created".into(),
        ));
    }
    if session.seq != 0 {
        return Err(StoreError::Invalid(
            "a new chat session starts at sequence zero".into(),
        ));
    }
    Ok(())
}

fn validate_resume(resume: &ResumeTarget) -> Result<()> {
    validate_chat_text(
        "resume provider",
        &resume.provider_id,
        MAX_CHAT_PROVIDER_BYTES,
    )?;
    let native = &resume.native_id;
    if native.is_empty() || native.len() > 512 {
        return Err(StoreError::Invalid(
            "resume identity must be 1..512 bytes".into(),
        ));
    }
    if resume.cwd.as_os_str().is_empty() || resume.cwd.to_str().is_none() {
        return Err(StoreError::Invalid("resume cwd is not UTF-8".into()));
    }
    if postcard::to_stdvec(resume)?.len() > MAX_CHAT_RESUME_BYTES {
        return Err(StoreError::Invalid(
            "resume target exceeds byte limit".into(),
        ));
    }
    Ok(())
}

fn validate_pending(pending: &PendingRequest) -> Result<()> {
    validate_chat_text("pending request", &pending.request, MAX_CHAT_REQUEST_BYTES)?;
    if pending.payload.len() > MAX_CHAT_EVENT_BYTES {
        return Err(StoreError::Invalid(
            "pending request exceeds byte limit".into(),
        ));
    }
    Ok(())
}

/// Bounded text column (NUL-free, nonempty when required by the caller).
fn validate_chat_text(field: &str, value: &str, max: usize) -> Result<()> {
    if value.len() > max || value.contains('\0') {
        return Err(StoreError::Invalid(format!(
            "{field} is invalid or exceeds {max} bytes"
        )));
    }
    Ok(())
}

fn dismiss_inbox_dedupe(connection: &Connection, dedupe: &str) -> Result<()> {
    connection.execute(
        "UPDATE chat_inbox SET state=?2 WHERE dedupe=?1 AND state != ?2",
        params![dedupe, inbox_state_code(InboxState::Dismissed)],
    )?;
    Ok(())
}

fn insert_inbox(
    connection: &Connection,
    session: uuid::Uuid,
    worktree: WorktreeId,
    item: &NewInboxItem,
) -> Result<()> {
    validate_chat_text("inbox title", &item.title, MAX_CHAT_TITLE_BYTES)?;
    if item.title.trim().is_empty() {
        return Err(StoreError::Invalid("empty inbox title".into()));
    }
    validate_chat_text("inbox dedupe", &item.dedupe, MAX_CHAT_REQUEST_BYTES)?;
    connection.execute(
        "INSERT INTO chat_inbox(id,session,worktree,kind,state,title,dedupe,created_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(dedupe) DO NOTHING",
        params![
            uuid::Uuid::new_v4().as_bytes().as_slice(),
            session.as_bytes().as_slice(),
            worktree.as_bytes().as_slice(),
            inbox_kind_code(item.kind),
            inbox_state_code(InboxState::Unread),
            item.title,
            item.dedupe,
            sql_ms(item.created_ms)?,
        ],
    )?;
    Ok(())
}

fn load_chat(connection: &Connection, id: uuid::Uuid) -> Result<Option<(ChatSession, bool)>> {
    let row = connection
        .query_row(
            "SELECT id,worktree,provider,label,status,status_source,model,thinking,
                    permission_mode,resume,seq,created_ms,updated_ms,archived
             FROM chat_sessions WHERE id=?1",
            [id.as_bytes().as_slice()],
            |row| Ok((decode_session(row), row.get::<_, i64>(13)?)),
        )
        .optional()?;
    match row {
        Some((session, archived)) => {
            let session = session??;
            Ok(Some((session, archived != 0)))
        }
        None => Ok(None),
    }
}

fn decode_session(row: &Row<'_>) -> rusqlite::Result<Result<ChatSession>> {
    let status: Vec<u8> = row.get(4)?;
    let source: Vec<u8> = row.get(5)?;
    let permission: Vec<u8> = row.get(8)?;
    let resume: Option<Vec<u8>> = row.get(9)?;
    let sequence: Vec<u8> = row.get(10)?;
    let created: i64 = row.get(11)?;
    let updated: i64 = row.get(12)?;
    Ok((|| {
        Ok(ChatSession {
            id: uuid_from(&row.get::<_, Vec<u8>>(0)?)?,
            worktree: WorktreeId::from_bytes(array16(&row.get::<_, Vec<u8>>(1)?)?),
            provider: row.get(2)?,
            label: row.get(3)?,
            status: postcard::from_bytes(&status)?,
            status_source: postcard::from_bytes(&source)?,
            model: row.get(6)?,
            thinking: row.get(7)?,
            permission_mode: postcard::from_bytes(&permission)?,
            resume: resume
                .map(|bytes| postcard::from_bytes(&bytes))
                .transpose()?,
            seq: sequence_u64(&sequence)?,
            created_ms: u64::try_from(created)
                .map_err(|_| corrupt("negative chat creation time"))?,
            updated_ms: u64::try_from(updated).map_err(|_| corrupt("negative chat update time"))?,
        })
    })())
}

fn decode_inbox(row: &Row<'_>) -> rusqlite::Result<Result<InboxItem>> {
    let id: Vec<u8> = row.get(0)?;
    let session: Vec<u8> = row.get(1)?;
    let worktree: Vec<u8> = row.get(2)?;
    let created: i64 = row.get(6)?;
    Ok((|| {
        Ok(InboxItem {
            id: uuid_from(&id)?,
            session: uuid_from(&session)?,
            worktree: WorktreeId::from_bytes(array16(&worktree)?),
            kind: inbox_kind(row.get::<_, i64>(3)?)?,
            state: inbox_state(row.get::<_, i64>(4)?)?,
            title: row.get(5)?,
            created_ms: u64::try_from(created).map_err(|_| corrupt("negative inbox time"))?,
        })
    })())
}

fn decode_hit(row: &Row<'_>) -> rusqlite::Result<Result<ChatHit>> {
    let session: Vec<u8> = row.get(0)?;
    let sequence: Vec<u8> = row.get(1)?;
    let worktree: Vec<u8> = row.get(2)?;
    let snippet: String = row.get(3)?;
    Ok((|| {
        Ok(ChatHit {
            session: uuid_from(&session)?,
            worktree: WorktreeId::from_bytes(array16(&worktree)?),
            sequence: sequence_u64(&sequence)?,
            snippet,
        })
    })())
}

fn load_inbox(connection: &Connection, id: uuid::Uuid) -> Result<Option<InboxItem>> {
    let row = connection
        .query_row(
            "SELECT id,session,worktree,kind,state,title,created_ms
             FROM chat_inbox WHERE id=?1",
            [id.as_bytes().as_slice()],
            decode_inbox,
        )
        .optional()?;
    row.transpose()
}

fn to_sqlite(error: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(error))
}

fn chat_incarnation(connection: &Connection, id: uuid::Uuid) -> Result<uuid::Uuid> {
    let bytes: Vec<u8> = connection.query_row(
        "SELECT incarnation FROM chat_sessions WHERE id=?1",
        [id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    uuid_from(&bytes)
}

fn chat_fts_mode(connection: &Connection) -> Result<bool> {
    let mode: Option<i64> = connection
        .query_row("SELECT fts5 FROM chat_meta WHERE singleton=1", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(mode.unwrap_or(0) != 0)
}

pub(super) fn initial_chat_meta(fts5: bool) -> i64 {
    fts5 as i64
}

/// Shape checks run on every open, after migration. They catch a hand-edited
/// or truncated database before a reader trusts cursors or pending cards.
pub(super) fn validate_chat_integrity(connection: &Connection) -> Result<()> {
    let meta: i64 = connection.query_row(
        "SELECT count(*) FROM chat_meta WHERE singleton=1 AND fts5 IN (0,1)",
        [],
        |row| row.get(0),
    )?;
    if meta != 1 {
        return Err(corrupt("chat_meta is missing or duplicated"));
    }
    let fts5 = chat_fts_mode(connection)?;
    let index: i64 = if fts5 {
        connection.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='chat_fts' AND type='table'",
            [],
            |row| row.get(0),
        )?
    } else {
        connection.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='chat_search' AND type='table'",
            [],
            |row| row.get(0),
        )?
    };
    if index != 1 {
        return Err(corrupt("chat search index does not match chat_meta"));
    }
    let invalid_sessions: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM chat_sessions
            WHERE typeof(id)!='blob' OR length(id)!=16
               OR typeof(worktree)!='blob' OR length(worktree)!=16
               OR typeof(status)!='blob' OR length(status) NOT BETWEEN 1 AND 4096
               OR typeof(status_source)!='blob' OR length(status_source)!=1
               OR typeof(permission_mode)!='blob' OR length(permission_mode)!=1
               OR typeof(seq)!='blob' OR length(seq)!=8
               OR typeof(incarnation)!='blob' OR length(incarnation)!=16
               OR typeof(created_ms)!='integer' OR typeof(updated_ms)!='integer'
        )",
        [],
        |row| row.get(0),
    )?;
    if invalid_sessions {
        return Err(corrupt("invalid persisted chat session"));
    }
    let invalid_events: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM chat_events
            WHERE typeof(session)!='blob' OR length(session)!=16
               OR typeof(sequence)!='blob' OR length(sequence)!=8
               OR typeof(record)!='blob' OR length(record) NOT BETWEEN 1 AND 2097152
        )",
        [],
        |row| row.get(0),
    )?;
    if invalid_events {
        return Err(corrupt("invalid persisted chat event"));
    }
    let cursor_mismatch: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM chat_sessions s
            WHERE s.seq != COALESCE(
                (SELECT max(e.sequence) FROM chat_events e WHERE e.session=s.id),
                zeroblob(8)
            )
        )",
        [],
        |row| row.get(0),
    )?;
    if cursor_mismatch {
        return Err(corrupt("chat session cursor disagrees with its events"));
    }
    Ok(())
}

fn sequence_key(value: u64) -> [u8; 8] {
    value.to_be_bytes()
}

fn sequence_u64(bytes: &[u8]) -> Result<u64> {
    let bytes = array8(bytes)?;
    Ok(u64::from_be_bytes(bytes))
}

fn array8(bytes: &[u8]) -> Result<[u8; 8]> {
    bytes
        .try_into()
        .map_err(|_| corrupt("chat sequence must be 8 bytes"))
}

fn array16(bytes: &[u8]) -> Result<[u8; 16]> {
    bytes
        .try_into()
        .map_err(|_| corrupt("chat identity must be 16 bytes"))
}

fn uuid_from(bytes: &[u8]) -> Result<uuid::Uuid> {
    Ok(uuid::Uuid::from_bytes(array16(bytes)?))
}

fn pending_kind_code(kind: PendingKind) -> i64 {
    match kind {
        PendingKind::Approval => 0,
        PendingKind::Question => 1,
    }
}

fn pending_kind(code: i64) -> Result<PendingKind> {
    match code {
        0 => Ok(PendingKind::Approval),
        1 => Ok(PendingKind::Question),
        _ => Err(corrupt("unknown pending request kind")),
    }
}

fn inbox_kind_code(kind: InboxKind) -> i64 {
    match kind {
        InboxKind::Approval => 0,
        InboxKind::Question => 1,
        InboxKind::Completed => 2,
        InboxKind::Error => 3,
    }
}

fn inbox_kind(code: i64) -> Result<InboxKind> {
    match code {
        0 => Ok(InboxKind::Approval),
        1 => Ok(InboxKind::Question),
        2 => Ok(InboxKind::Completed),
        3 => Ok(InboxKind::Error),
        _ => Err(corrupt("unknown inbox kind")),
    }
}

fn inbox_state_code(state: InboxState) -> i64 {
    match state {
        InboxState::Unread => 0,
        InboxState::Read => 1,
        InboxState::Dismissed => 2,
    }
}

fn inbox_state(code: i64) -> Result<InboxState> {
    match code {
        0 => Ok(InboxState::Unread),
        1 => Ok(InboxState::Read),
        2 => Ok(InboxState::Dismissed),
        _ => Err(corrupt("unknown inbox state")),
    }
}

/// FTS5 query: every whitespace-separated term becomes a quoted prefix term
/// joined by implicit AND. Quotes are doubled, so user text cannot inject
/// operators.
fn fts_phrase(query: &str) -> String {
    let mut phrase = String::new();
    for term in query.split_whitespace().take(16) {
        let term: String = term.chars().filter(|c| !c.is_control()).take(64).collect();
        if term.is_empty() {
            continue;
        }
        if !phrase.is_empty() {
            phrase.push(' ');
        }
        phrase.push('"');
        phrase.push_str(&term.replace('"', "\"\""));
        phrase.push('"');
    }
    phrase
}

fn like_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn substring_snippet(text: &str, query: &str) -> String {
    let lower = text.to_lowercase();
    let needle = query.to_lowercase();
    let start = lower.find(&needle).unwrap_or(0);
    let mut begin = start.saturating_sub(48);
    while !text.is_char_boundary(begin) {
        begin -= 1;
    }
    let mut end = (start + needle.len() + 96).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut snippet = String::new();
    if begin > 0 {
        snippet.push('…');
    }
    snippet.push_str(&text[begin..end]);
    if end < text.len() {
        snippet.push('…');
    }
    snippet
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::chat::{ChatEvent, PermissionMode, TextUpdate};

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn seed_worktree(db: &Database) -> (WorktreeId, uuid::Uuid) {
        let workspace = db.workspace_tree().unwrap().workspaces[0].id;
        let project = fl_model::ProjectId::new();
        let worktree = WorktreeId::new();
        let primary = WorktreeId::new();
        db.connection
            .execute(
                "INSERT INTO projects(id,workspace,name,path,kind,default_branch,worktree_root,ord)
                 VALUES(?1,?2,'repo','/repo','git','main','/root',0)",
                params![
                    project.as_bytes().as_slice(),
                    workspace.as_bytes().as_slice()
                ],
            )
            .unwrap();
        // Every project has exactly one primary checkout; the chat session
        // belongs to a removable target worktree.
        db.connection
            .execute(
                "INSERT INTO worktrees(id,project,path,branch,target,label,is_primary,created_ms,last_active_ms)
                 VALUES(?1,?2,'/repo','main','main','repo',1,1,1)",
                params![primary.as_bytes().as_slice(), project.as_bytes().as_slice()],
            )
            .unwrap();
        db.connection
            .execute(
                "INSERT INTO worktrees(id,project,path,branch,target,label,is_primary,created_ms,last_active_ms)
                 VALUES(?1,?2,'/repo/wt','feature','main','wt',0,1,1)",
                params![worktree.as_bytes().as_slice(), project.as_bytes().as_slice()],
            )
            .unwrap();
        let layout: Vec<u8> = db
            .connection
            .query_row("SELECT state FROM layout", [], |row| row.get(0))
            .unwrap();
        for id in [primary, worktree] {
            db.connection
                .execute(
                    "INSERT INTO worktree_layouts(worktree,state) VALUES(?1,?2)",
                    params![id.as_bytes().as_slice(), layout],
                )
                .unwrap();
        }
        let id = uuid::Uuid::new_v4();
        (worktree, id)
    }

    fn session(worktree: WorktreeId, id: uuid::Uuid) -> ChatSession {
        ChatSession {
            id,
            worktree,
            provider: "claude".into(),
            label: "Claude Code".into(),
            status: SessionState::Idle,
            status_source: StatusSource::Structured,
            model: Some("sonnet".into()),
            thinking: None,
            permission_mode: PermissionMode::Ask,
            resume: None,
            seq: 0,
            created_ms: 10,
            updated_ms: 10,
        }
    }

    fn record(id: uuid::Uuid, incarnation: uuid::Uuid, sequence: u64) -> EventRecord {
        EventRecord {
            schema_version: fl_model::chat::CHAT_SCHEMA_VERSION,
            session_id: id,
            incarnation,
            sequence,
            timestamp_ms: 100 + sequence,
            native_session_id: Some("native".into()),
            turn_id: None,
            event: ChatEvent::AssistantText {
                id: format!("m{sequence}"),
                update: TextUpdate::Final,
                text: format!("durable event {sequence}"),
            },
        }
    }

    fn append(db: &mut Database, id: uuid::Uuid, record: EventRecord) -> Result<ChatAppend> {
        db.append_chat_event(id, &record, ChatEventEffects::default())
    }

    #[test]
    fn sessions_and_events_survive_reopen_and_sequence_gaps_are_refused() {
        let dir = root();
        let mut db = Database::open(dir.path()).unwrap();
        let (worktree, id) = seed_worktree(&db);
        db.create_chat_session(&session(worktree, id)).unwrap();
        let incarnation = uuid::Uuid::new_v4();
        append(&mut db, id, record(id, incarnation, 1)).unwrap();
        append(&mut db, id, record(id, incarnation, 2)).unwrap();
        let gap = append(&mut db, id, record(id, incarnation, 4)).unwrap_err();
        assert!(
            matches!(&gap, StoreError::Invalid(message) if message.contains("gap")),
            "{gap:?}"
        );
        drop(db);
        let db = Database::open(dir.path()).unwrap();
        let stored = db.chat_session(id).unwrap();
        assert_eq!(stored.seq, 2);
        assert_eq!(db.chat_sessions(worktree).unwrap().len(), 1);
        let page = db.chat_event_page(id, 0, 16).unwrap();
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[1].sequence, 2);
        assert!(!page.more);
    }

    #[test]
    fn repeated_newest_event_is_idempotent_and_conflicts_are_rejected() {
        let dir = root();
        let mut db = Database::open(dir.path()).unwrap();
        let (worktree, id) = seed_worktree(&db);
        db.create_chat_session(&session(worktree, id)).unwrap();
        let incarnation = uuid::Uuid::new_v4();
        let first = record(id, incarnation, 1);
        let appended = append(&mut db, id, first.clone()).unwrap();
        assert!(appended.inserted);
        let replay = append(&mut db, id, first.clone()).unwrap();
        assert!(!replay.inserted);
        let mut conflicting = first;
        if let ChatEvent::AssistantText { text, .. } = &mut conflicting.event {
            *text = "different".into();
        }
        let error = append(&mut db, id, conflicting).unwrap_err();
        assert!(matches!(error, StoreError::Duplicate(_)), "{error:?}");
        assert_eq!(db.chat_event_page(id, 1, 16).unwrap().events.len(), 1);
    }

    #[test]
    fn deleting_a_worktree_cascades_sessions_events_pending_and_inbox() {
        let dir = root();
        let mut db = Database::open(dir.path()).unwrap();
        let (worktree, id) = seed_worktree(&db);
        db.create_chat_session(&session(worktree, id)).unwrap();
        let incarnation = uuid::Uuid::new_v4();
        let effects = ChatEventEffects {
            add_pending: vec![PendingRequest {
                request: "req-1".into(),
                kind: PendingKind::Approval,
                payload: postcard::to_stdvec(&record(id, incarnation, 1).event).unwrap(),
                created_ms: 1,
            }],
            inbox: vec![NewInboxItem {
                kind: InboxKind::Approval,
                title: "Approval needed".into(),
                dedupe: "approval:req-1".into(),
                created_ms: 1,
            }],
            ..Default::default()
        };
        db.append_chat_event(id, &record(id, incarnation, 1), effects)
            .unwrap();
        assert_eq!(db.chat_pending(id).unwrap().len(), 1);
        assert_eq!(db.chat_inbox().unwrap().len(), 1);
        db.remove_worktree(worktree).unwrap();
        for table in ["chat_sessions", "chat_events", "chat_pending", "chat_inbox"] {
            let count: i64 = db
                .connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table} was not cascaded");
        }
        let hits = db.search_chats("durable", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_returns_worktree_bound_hits_in_order() {
        let dir = root();
        let mut db = Database::open(dir.path()).unwrap();
        let (worktree, id) = seed_worktree(&db);
        db.create_chat_session(&session(worktree, id)).unwrap();
        let incarnation = uuid::Uuid::new_v4();
        append(&mut db, id, record(id, incarnation, 1)).unwrap();
        append(&mut db, id, record(id, incarnation, 2)).unwrap();
        let hits = db.search_chats("durable", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].worktree, worktree);
        assert_eq!(hits[0].session, id);
        assert!(hits.iter().all(|hit| hit.sequence >= 1));
        assert!(db.search_chats("   ", 10).is_err());
        assert!(db.search_chats("absentterm", 10).unwrap().is_empty());
    }

    #[test]
    fn restart_clears_pending_and_never_marks_a_turn_completed() {
        let dir = root();
        let mut db = Database::open(dir.path()).unwrap();
        let (worktree, id) = seed_worktree(&db);
        db.create_chat_session(&session(worktree, id)).unwrap();
        let incarnation = uuid::Uuid::new_v4();
        let effects = ChatEventEffects {
            status: Some((SessionState::Running, StatusSource::Structured)),
            add_pending: vec![PendingRequest {
                request: "req-1".into(),
                kind: PendingKind::Question,
                payload: postcard::to_stdvec(&record(id, incarnation, 1).event).unwrap(),
                created_ms: 1,
            }],
            inbox: vec![NewInboxItem {
                kind: InboxKind::Question,
                title: "Input needed".into(),
                dedupe: "question:req-1".into(),
                created_ms: 1,
            }],
            ..Default::default()
        };
        db.append_chat_event(id, &record(id, incarnation, 1), effects)
            .unwrap();
        let sessions = db.reset_chats_after_restart(500).unwrap();
        assert_eq!(sessions[0].status, SessionState::Idle);
        assert_eq!(sessions[0].status_source, StatusSource::Heuristic);
        assert!(db.chat_pending(id).unwrap().is_empty());
        assert!(db.chat_inbox().unwrap().is_empty());
    }

    #[test]
    fn archived_sessions_keep_search_but_refuse_appends() {
        let dir = root();
        let mut db = Database::open(dir.path()).unwrap();
        let (worktree, id) = seed_worktree(&db);
        db.create_chat_session(&session(worktree, id)).unwrap();
        let incarnation = uuid::Uuid::new_v4();
        append(&mut db, id, record(id, incarnation, 1)).unwrap();
        db.archive_chat(id, 200).unwrap();
        assert!(db.chat_archived(id).unwrap());
        let error = append(&mut db, id, record(id, incarnation, 2)).unwrap_err();
        assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");
        assert_eq!(db.search_chats("durable", 10).unwrap().len(), 1);
        assert_eq!(db.chat_event_page(id, 1, 16).unwrap().events.len(), 1);
    }
}
