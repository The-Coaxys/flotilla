//! Durable coordination state and advisory locks (schema v9).
//!
//! One row per (scope, key). A write bumps the version by exactly one, and a
//! compare-and-set write fails with the current version when it does not match.
//! Locks are separate rows with an owner and an expiry; an expired lock is
//! reported as absent and may be taken by anyone.
use super::*;
use crate::{Result, StoreError};
use fl_model::coordination::{
    CoordinationLock, CoordinationScope, CoordinationSnapshot, CoordinationValue,
    MAX_COORDINATION_KEY_BYTES, MAX_COORDINATION_OWNER_BYTES, MAX_COORDINATION_VALUE_BYTES,
    MAX_LOCK_TTL_MS, MIN_LOCK_TTL_MS,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

/// Coordination values and locks share the scope namespace.
pub(crate) const SCHEMA_V9: &str = "
    CREATE TABLE coordination_state (
        scope_kind INTEGER NOT NULL CHECK(scope_kind IN (0, 1, 2)),
        scope_id BLOB NOT NULL CHECK(length(scope_id) = 16),
        key TEXT NOT NULL CHECK(length(key) BETWEEN 1 AND 256),
        value TEXT NOT NULL CHECK(length(value) <= 65536),
        version BLOB NOT NULL CHECK(length(version) = 8),
        updated_ms INTEGER NOT NULL CHECK(updated_ms >= 0),
        PRIMARY KEY(scope_kind, scope_id, key)
    ) STRICT;
    CREATE TABLE coordination_locks (
        scope_kind INTEGER NOT NULL CHECK(scope_kind IN (0, 1, 2)),
        scope_id BLOB NOT NULL CHECK(length(scope_id) = 16),
        key TEXT NOT NULL CHECK(length(key) BETWEEN 1 AND 256),
        owner TEXT NOT NULL CHECK(length(owner) BETWEEN 1 AND 256),
        acquired_ms INTEGER NOT NULL CHECK(acquired_ms >= 0),
        expires_ms INTEGER NOT NULL CHECK(expires_ms >= 0),
        PRIMARY KEY(scope_kind, scope_id, key)
    ) STRICT;
";

fn scope_parts(scope: CoordinationScope) -> (i64, [u8; 16]) {
    match scope {
        CoordinationScope::Global => (0, [0; 16]),
        CoordinationScope::Workspace(id) => (1, *id.as_bytes()),
        CoordinationScope::Worktree(id) => (2, *id.as_bytes()),
    }
}

fn version_bytes(value: u64) -> [u8; 8] {
    value.to_be_bytes()
}

fn version_of(bytes: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = bytes
        .try_into()
        .map_err(|_| corrupt("stored coordination version has the wrong length"))?;
    Ok(u64::from_be_bytes(bytes))
}

fn check_key(key: &str) -> Result<()> {
    if key.trim().is_empty() || key.len() > MAX_COORDINATION_KEY_BYTES || key.contains('\0') {
        return Err(StoreError::Invalid(format!(
            "coordination key must be 1-{MAX_COORDINATION_KEY_BYTES} bytes without NUL"
        )));
    }
    Ok(())
}

fn check_owner(owner: &str) -> Result<()> {
    if owner.trim().is_empty() || owner.len() > MAX_COORDINATION_OWNER_BYTES || owner.contains('\0')
    {
        return Err(StoreError::Invalid(format!(
            "lock owner must be 1-{MAX_COORDINATION_OWNER_BYTES} bytes without NUL"
        )));
    }
    Ok(())
}

/// The scope must name an entity that still exists.
fn require_scope(transaction: &rusqlite::Transaction<'_>, scope: CoordinationScope) -> Result<()> {
    let (kind, id) = scope_parts(scope);
    match scope {
        CoordinationScope::Global => Ok(()),
        CoordinationScope::Workspace(_) => {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?1)",
                [id.as_slice()],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StoreError::Missing(format!(
                    "workspace {} not found",
                    WorkspaceId::from_bytes(id)
                )));
            }
            let _ = kind;
            Ok(())
        }
        CoordinationScope::Worktree(_) => {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM worktrees WHERE id=?1)",
                [id.as_slice()],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(StoreError::Missing(format!(
                    "worktree {} not found",
                    WorktreeId::from_bytes(id)
                )));
            }
            let _ = kind;
            Ok(())
        }
    }
}

fn load_value(
    transaction: &rusqlite::Transaction<'_>,
    scope: CoordinationScope,
    key: &str,
) -> Result<Option<CoordinationValue>> {
    let (kind, id) = scope_parts(scope);
    let row: Option<(String, Vec<u8>, i64)> = transaction
        .query_row(
            "SELECT value,version,updated_ms FROM coordination_state
             WHERE scope_kind=?1 AND scope_id=?2 AND key=?3",
            params![kind, id.as_slice(), key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((value, version, updated_ms)) = row else {
        return Ok(None);
    };
    Ok(Some(CoordinationValue {
        scope,
        key: key.to_owned(),
        value,
        version: version_of(&version)?,
        updated_ms: u64::try_from(updated_ms)
            .map_err(|_| corrupt("stored coordination timestamp is negative"))?,
    }))
}

fn load_lock(
    transaction: &rusqlite::Transaction<'_>,
    scope: CoordinationScope,
    key: &str,
    now_ms: u64,
) -> Result<Option<CoordinationLock>> {
    let (kind, id) = scope_parts(scope);
    let row: Option<(String, i64, i64)> = transaction
        .query_row(
            "SELECT owner,acquired_ms,expires_ms FROM coordination_locks
             WHERE scope_kind=?1 AND scope_id=?2 AND key=?3",
            params![kind, id.as_slice(), key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((owner, acquired_ms, expires_ms)) = row else {
        return Ok(None);
    };
    let acquired_ms =
        u64::try_from(acquired_ms).map_err(|_| corrupt("stored lock timestamp is negative"))?;
    let expires_ms =
        u64::try_from(expires_ms).map_err(|_| corrupt("stored lock expiry is negative"))?;
    if expires_ms <= now_ms {
        return Ok(None);
    }
    Ok(Some(CoordinationLock {
        scope,
        key: key.to_owned(),
        owner,
        acquired_ms,
        expires_ms,
    }))
}

impl Database {
    pub fn coordination_get(
        &self,
        scope: CoordinationScope,
        key: &str,
        now_ms: u64,
    ) -> Result<CoordinationSnapshot> {
        check_key(key)?;
        let transaction = self.connection.unchecked_transaction()?;
        require_scope(&transaction, scope)?;
        let snapshot = CoordinationSnapshot {
            value: load_value(&transaction, scope, key)?,
            lock: load_lock(&transaction, scope, key, now_ms)?,
        };
        transaction.commit()?;
        Ok(snapshot)
    }

    /// Write one value, bumping its version. `if_version` is compare-and-set;
    /// `Some(0)` creates only, and a mismatched version is a conflict.
    pub fn coordination_set(
        &mut self,
        scope: CoordinationScope,
        key: &str,
        value: &str,
        if_version: Option<u64>,
        now_ms: u64,
    ) -> Result<CoordinationValue> {
        check_key(key)?;
        if value.len() > MAX_COORDINATION_VALUE_BYTES {
            return Err(StoreError::Invalid(format!(
                "coordination value exceeds {MAX_COORDINATION_VALUE_BYTES} bytes"
            )));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_scope(&transaction, scope)?;
        let current = load_value(&transaction, scope, key)?.map(|entry| entry.version);
        if let Some(expected) = if_version {
            let found = current.unwrap_or(0);
            if expected != found {
                return Err(StoreError::Conflict(format!(
                    "coordination key `{key}` is at version {found}, not {expected}; re-read it and retry"
                )));
            }
        }
        let version = current.unwrap_or(0) + 1;
        let (kind, id) = scope_parts(scope);
        transaction.execute(
            "INSERT INTO coordination_state(scope_kind,scope_id,key,value,version,updated_ms)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(scope_kind,scope_id,key) DO UPDATE SET
                value=excluded.value,version=excluded.version,updated_ms=excluded.updated_ms",
            params![
                kind,
                id.as_slice(),
                key,
                value,
                version_bytes(version).as_slice(),
                sql_ms(now_ms)?
            ],
        )?;
        transaction.commit()?;
        Ok(CoordinationValue {
            scope,
            key: key.to_owned(),
            value: value.to_owned(),
            version,
            updated_ms: now_ms,
        })
    }

    pub fn coordination_delete(
        &mut self,
        scope: CoordinationScope,
        key: &str,
        if_version: Option<u64>,
    ) -> Result<u64> {
        check_key(key)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_scope(&transaction, scope)?;
        let current = load_value(&transaction, scope, key)?;
        let Some(current) = current else {
            return Err(StoreError::Missing(format!(
                "coordination key `{key}` does not exist"
            )));
        };
        if let Some(expected) = if_version
            && expected != current.version
        {
            return Err(StoreError::Conflict(format!(
                "coordination key `{key}` is at version {}, not {expected}; re-read it and retry",
                current.version
            )));
        }
        let (kind, id) = scope_parts(scope);
        let removed = transaction.execute(
            "DELETE FROM coordination_state WHERE scope_kind=?1 AND scope_id=?2 AND key=?3",
            params![kind, id.as_slice(), key],
        )?;
        if removed != 1 {
            return Err(corrupt("coordination delete removed no entry"));
        }
        transaction.commit()?;
        Ok(0)
    }

    /// Acquire or extend a lock. A live lock held by another owner is refused
    /// with that owner and its expiry.
    pub fn coordination_lock(
        &mut self,
        scope: CoordinationScope,
        key: &str,
        owner: &str,
        ttl_ms: u64,
        now_ms: u64,
    ) -> Result<CoordinationLock> {
        check_key(key)?;
        check_owner(owner)?;
        if !(MIN_LOCK_TTL_MS..=MAX_LOCK_TTL_MS).contains(&ttl_ms) {
            return Err(StoreError::Invalid(format!(
                "lock TTL must be {MIN_LOCK_TTL_MS}-{MAX_LOCK_TTL_MS} ms"
            )));
        }
        let expires_ms = now_ms.saturating_add(ttl_ms);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_scope(&transaction, scope)?;
        if let Some(existing) = load_lock(&transaction, scope, key, now_ms)?
            && existing.owner != owner
        {
            let remaining = existing.expires_ms.saturating_sub(now_ms) / 1_000;
            return Err(StoreError::Conflict(format!(
                "coordination lock `{key}` is held by `{}` for another {remaining}s",
                existing.owner
            )));
        }
        let (kind, id) = scope_parts(scope);
        transaction.execute(
            "INSERT INTO coordination_locks(scope_kind,scope_id,key,owner,acquired_ms,expires_ms)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(scope_kind,scope_id,key) DO UPDATE SET
                owner=excluded.owner,acquired_ms=excluded.acquired_ms,expires_ms=excluded.expires_ms",
            params![
                kind,
                id.as_slice(),
                key,
                owner,
                sql_ms(now_ms)?,
                sql_ms(expires_ms)?
            ],
        )?;
        transaction.commit()?;
        Ok(CoordinationLock {
            scope,
            key: key.to_owned(),
            owner: owner.to_owned(),
            acquired_ms: now_ms,
            expires_ms,
        })
    }

    /// Release a lock. A different owner — or an absent lock — is refused.
    pub fn coordination_unlock(
        &mut self,
        scope: CoordinationScope,
        key: &str,
        owner: &str,
    ) -> Result<()> {
        check_key(key)?;
        check_owner(owner)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_scope(&transaction, scope)?;
        let (kind, id) = scope_parts(scope);
        let stored: Option<(String, i64)> = transaction
            .query_row(
                "SELECT owner,expires_ms FROM coordination_locks
                 WHERE scope_kind=?1 AND scope_id=?2 AND key=?3",
                params![kind, id.as_slice(), key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((stored_owner, _)) = stored else {
            return Err(StoreError::Missing(format!(
                "coordination key `{key}` has no lock"
            )));
        };
        if stored_owner != owner {
            return Err(StoreError::NotOwner(format!(
                "coordination lock `{key}` is held by `{stored_owner}`, not `{owner}`"
            )));
        }
        let removed = transaction.execute(
            "DELETE FROM coordination_locks WHERE scope_kind=?1 AND scope_id=?2 AND key=?3",
            params![kind, id.as_slice(), key],
        )?;
        if removed != 1 {
            return Err(corrupt("coordination unlock removed no lock"));
        }
        transaction.commit()?;
        Ok(())
    }
}

/// Check every persisted coordination row; called by the schema opener.
pub(super) fn validate_integrity(connection: &Connection) -> Result<()> {
    let invalid: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM coordination_state
            WHERE scope_kind NOT IN (0,1,2) OR length(scope_id)!=16
               OR length(key) NOT BETWEEN 1 AND 256 OR length(value) > 65536
               OR length(version)!=8 OR updated_ms < 0
        ) OR EXISTS(
            SELECT 1 FROM coordination_locks
            WHERE scope_kind NOT IN (0,1,2) OR length(scope_id)!=16
               OR length(key) NOT BETWEEN 1 AND 256
               OR length(owner) NOT BETWEEN 1 AND 256
               OR acquired_ms < 0 OR expires_ms < acquired_ms
        )",
        [],
        |row| row.get(0),
    )?;
    if invalid {
        return Err(corrupt("invalid persisted coordination state"));
    }
    Ok(())
}
