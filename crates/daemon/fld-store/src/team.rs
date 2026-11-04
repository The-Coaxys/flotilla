//! Durable team runs and their roles (schema v9).
//!
//! Run and role state is a constrained enum. A run settles from its roles:
//! all finished → finished, any failed → failed, otherwise running. The single
//! lead fan-in is claimed with `fan_in_sent` in the same transaction that
//! settles the run, so a crash cannot send it twice.
use super::*;
use crate::{Result, StoreError};
use fl_model::team::{
    MAX_TEAM_RESULT_FILE_BYTES, MAX_TEAM_ROLES, MAX_TEAM_RUNS, MAX_TEAM_SUMMARY_BYTES, TeamReport,
    TeamRole, TeamRun, TeamState,
};
use fl_model::{TabId, WorktreeId};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

pub(crate) const SCHEMA_V9: &str = "
    CREATE TABLE team_runs (
        id BLOB PRIMARY KEY NOT NULL CHECK(length(id) = 16),
        worktree BLOB NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
        lead TEXT NOT NULL CHECK(length(lead) BETWEEN 1 AND 64),
        state BLOB NOT NULL CHECK(length(state) BETWEEN 1 AND 1024),
        fan_in_sent INTEGER NOT NULL CHECK(fan_in_sent IN (0, 1)),
        created_ms INTEGER NOT NULL CHECK(created_ms >= 0),
        updated_ms INTEGER NOT NULL CHECK(updated_ms >= 0)
    ) STRICT;
    CREATE INDEX team_runs_by_worktree ON team_runs(worktree, created_ms);
    CREATE TABLE team_roles (
        run BLOB NOT NULL REFERENCES team_runs(id) ON DELETE CASCADE,
        idx INTEGER NOT NULL CHECK(idx BETWEEN 0 AND 7),
        label TEXT NOT NULL CHECK(length(label) BETWEEN 1 AND 64),
        provider TEXT NOT NULL CHECK(length(provider) BETWEEN 1 AND 256),
        session BLOB CHECK(session IS NULL OR length(session) = 16),
        tab BLOB CHECK(tab IS NULL OR length(tab) = 16),
        state BLOB NOT NULL CHECK(length(state) BETWEEN 1 AND 1024),
        summary TEXT CHECK(summary IS NULL OR length(summary) BETWEEN 1 AND 16384),
        result_file TEXT CHECK(result_file IS NULL OR length(result_file) BETWEEN 1 AND 4096),
        submitted_ms INTEGER CHECK(submitted_ms IS NULL OR submitted_ms >= 0),
        PRIMARY KEY(run, idx)
    ) STRICT;
";

fn state_bytes(state: TeamState) -> Result<Vec<u8>> {
    Ok(postcard::to_stdvec(&state)?)
}

fn state_of(bytes: &[u8]) -> Result<TeamState> {
    postcard::from_bytes(bytes).map_err(|_| corrupt("stored team state is unknown"))
}

fn role_index(index: u16) -> i64 {
    i64::from(index)
}

fn decode_role(row: &Row<'_>) -> rusqlite::Result<Result<TeamRole>> {
    let index: i64 = row.get("idx")?;
    let label: String = row.get("label")?;
    let provider: String = row.get("provider")?;
    let session: Option<Vec<u8>> = row.get("session")?;
    let tab: Option<Vec<u8>> = row.get("tab")?;
    let state: Vec<u8> = row.get("state")?;
    let summary: Option<String> = row.get("summary")?;
    let result_file: Option<String> = row.get("result_file")?;
    let submitted_ms: Option<i64> = row.get("submitted_ms")?;
    Ok((|| {
        let session = session
            .map(|bytes| -> Result<uuid::Uuid> {
                let bytes: [u8; 16] = bytes
                    .try_into()
                    .map_err(|_| corrupt("stored team session has the wrong length"))?;
                Ok(uuid::Uuid::from_bytes(bytes))
            })
            .transpose()?;
        let tab = tab
            .map(|bytes| -> Result<TabId> {
                let bytes: [u8; 16] = bytes
                    .try_into()
                    .map_err(|_| corrupt("stored team tab has the wrong length"))?;
                Ok(TabId::from_bytes(bytes))
            })
            .transpose()?;
        let state = state_of(&state)?;
        let report = match (summary, submitted_ms) {
            (Some(summary), Some(submitted_ms)) => Some(TeamReport {
                summary,
                result_file,
                submitted_ms: u64::try_from(submitted_ms)
                    .map_err(|_| corrupt("stored team report time is negative"))?,
            }),
            (None, None) => None,
            _ => return Err(corrupt("stored team report is incomplete")),
        };
        Ok(TeamRole {
            index: u16::try_from(index)
                .map_err(|_| corrupt("stored team role index is out of range"))?,
            label,
            provider,
            session,
            tab,
            state,
            report,
        })
    })())
}

fn load_run(transaction: &rusqlite::Transaction<'_>, id: uuid::Uuid) -> Result<TeamRun> {
    let (worktree, lead, state, fan_in_sent, created_ms, updated_ms): (
        Vec<u8>,
        String,
        Vec<u8>,
        i64,
        i64,
        i64,
    ) = transaction
        .query_row(
            "SELECT worktree,lead,state,fan_in_sent,created_ms,updated_ms
             FROM team_runs WHERE id=?1",
            [id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::Missing(format!("team run {id} not found")))?;
    let worktree: [u8; 16] = worktree
        .as_slice()
        .try_into()
        .map_err(|_| corrupt("stored team worktree has the wrong length"))?;
    let mut statement = transaction.prepare(
        "SELECT idx,label,provider,session,tab,state,summary,result_file,submitted_ms
         FROM team_roles WHERE run=?1 ORDER BY idx",
    )?;
    let rows = statement.query_map([id.as_bytes().as_slice()], decode_role)?;
    let roles = rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    Ok(TeamRun {
        id,
        worktree: WorktreeId::from_bytes(worktree),
        lead,
        state: state_of(&state)?,
        roles,
        created_ms: u64::try_from(created_ms)
            .map_err(|_| corrupt("stored team run time is negative"))?,
        updated_ms: u64::try_from(updated_ms)
            .map_err(|_| corrupt("stored team run time is negative"))?,
        fan_in_sent: fan_in_sent != 0,
    })
}

impl Database {
    pub fn create_team_run(&mut self, run: &TeamRun) -> Result<()> {
        if run.roles.is_empty() || run.roles.len() > MAX_TEAM_ROLES {
            return Err(StoreError::Invalid(
                "a team run needs 1-8 roles".to_string(),
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO team_runs(id,worktree,lead,state,fan_in_sent,created_ms,updated_ms)
             VALUES(?1,?2,?3,?4,0,?5,?6)",
            params![
                run.id.as_bytes().as_slice(),
                run.worktree.as_bytes().as_slice(),
                run.lead,
                state_bytes(run.state)?,
                sql_ms(run.created_ms)?,
                sql_ms(run.updated_ms)?
            ],
        )?;
        for role in &run.roles {
            transaction.execute(
                "INSERT INTO team_roles(run,idx,label,provider,session,tab,state)
                 VALUES(?1,?2,?3,?4,NULL,NULL,?5)",
                params![
                    run.id.as_bytes().as_slice(),
                    role_index(role.index),
                    role.label,
                    role.provider,
                    state_bytes(role.state)?
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn team_run(&self, id: uuid::Uuid) -> Result<TeamRun> {
        let transaction = self.connection.unchecked_transaction()?;
        let run = load_run(&transaction, id)?;
        transaction.commit()?;
        Ok(run)
    }

    /// Newest runs of one worktree, newest first.
    pub fn team_runs(&self, worktree: WorktreeId) -> Result<Vec<TeamRun>> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut statement = transaction.prepare(
            "SELECT id FROM team_runs WHERE worktree=?1
             ORDER BY created_ms DESC, id DESC LIMIT ?2",
        )?;
        let ids = statement
            .query_map(
                params![worktree.as_bytes().as_slice(), MAX_TEAM_RUNS as i64],
                |row| {
                    let bytes: Vec<u8> = row.get(0)?;
                    Ok(bytes)
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut runs = Vec::with_capacity(ids.len());
        for bytes in ids {
            let bytes: [u8; 16] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| corrupt("stored team run has the wrong length"))?;
            runs.push(load_run(&transaction, uuid::Uuid::from_bytes(bytes))?);
        }
        transaction.commit()?;
        Ok(runs)
    }

    /// The newest run of `worktree`, if any.
    pub fn latest_team_run(&self, worktree: WorktreeId) -> Result<Option<TeamRun>> {
        Ok(self.team_runs(worktree)?.into_iter().next())
    }

    /// Record the session and tab a launched role owns, and mark it running.
    pub fn team_role_started(
        &mut self,
        run: uuid::Uuid,
        index: u16,
        session: uuid::Uuid,
        tab: Option<TabId>,
        now_ms: u64,
    ) -> Result<TeamRun> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE team_roles SET session=?3,tab=?4,state=?5 WHERE run=?1 AND idx=?2",
            params![
                run.as_bytes().as_slice(),
                role_index(index),
                session.as_bytes().as_slice(),
                tab.map(|tab| tab.as_bytes().to_vec()),
                state_bytes(TeamState::Running)?
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Missing(format!(
                "team run {run} has no role {index}"
            )));
        }
        transaction.execute(
            "UPDATE team_runs SET state=?2,updated_ms=?3 WHERE id=?1 AND state=?4",
            params![
                run.as_bytes().as_slice(),
                state_bytes(TeamState::Running)?,
                sql_ms(now_ms)?,
                state_bytes(TeamState::Starting)?
            ],
        )?;
        transaction.commit()?;
        self.team_run(run)
    }

    /// Update one role state, then settle the run from its roles.
    pub fn team_role_state(
        &mut self,
        run: uuid::Uuid,
        index: u16,
        state: TeamState,
        now_ms: u64,
    ) -> Result<(TeamRun, bool)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE team_roles SET state=?3 WHERE run=?1 AND idx=?2",
            params![
                run.as_bytes().as_slice(),
                role_index(index),
                state_bytes(state)?
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Missing(format!(
                "team run {run} has no role {index}"
            )));
        }
        transaction.commit()?;
        self.settle_team_run(run, now_ms)
    }

    /// Recompute a run's state from its roles and claim any pending fan-in.
    /// Returns the run and whether this call must deliver the fan-in.
    pub fn settle_team_run(&mut self, run: uuid::Uuid, now_ms: u64) -> Result<(TeamRun, bool)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut current = load_run(&transaction, run)?;
        if current.state == TeamState::Cancelled {
            transaction.commit()?;
            return Ok((current, false));
        }
        let all_terminal = current.roles.iter().all(|role| role.state.is_terminal());
        let any_failed = current
            .roles
            .iter()
            .any(|role| role.state == TeamState::Failed);
        let all_finished = current
            .roles
            .iter()
            .all(|role| role.state == TeamState::Finished);
        let state = if all_finished {
            TeamState::Finished
        } else if any_failed {
            TeamState::Failed
        } else if all_terminal {
            TeamState::Cancelled
        } else {
            TeamState::Running
        };
        let send_fan_in = all_terminal && state != TeamState::Cancelled && !current.fan_in_sent;
        if state != current.state || send_fan_in {
            transaction.execute(
                "UPDATE team_runs SET state=?2,fan_in_sent=?3,updated_ms=?4 WHERE id=?1",
                params![
                    run.as_bytes().as_slice(),
                    state_bytes(state)?,
                    i64::from(send_fan_in),
                    sql_ms(now_ms)?
                ],
            )?;
            current.state = state;
            current.fan_in_sent |= send_fan_in;
            current.updated_ms = now_ms;
        }
        transaction.commit()?;
        Ok((current, send_fan_in))
    }

    /// Record one role's report. The role must belong to the run.
    pub fn record_team_report(
        &mut self,
        run: uuid::Uuid,
        role: &str,
        summary: &str,
        result_file: Option<&str>,
        now_ms: u64,
    ) -> Result<TeamRun> {
        if summary.trim().is_empty() || summary.len() > MAX_TEAM_SUMMARY_BYTES {
            return Err(StoreError::Invalid(format!(
                "team report summary must be 1-{MAX_TEAM_SUMMARY_BYTES} bytes"
            )));
        }
        if let Some(path) = result_file
            && (path.trim().is_empty() || path.len() > MAX_TEAM_RESULT_FILE_BYTES)
        {
            return Err(StoreError::Invalid(format!(
                "team result file must be 1-{MAX_TEAM_RESULT_FILE_BYTES} bytes"
            )));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE team_roles SET summary=?3,result_file=?4,submitted_ms=?5
             WHERE run=?1 AND label=?2",
            params![
                run.as_bytes().as_slice(),
                role,
                summary,
                result_file,
                sql_ms(now_ms)?
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Missing(format!(
                "team run {run} has no role `{role}`"
            )));
        }
        transaction.execute(
            "UPDATE team_runs SET updated_ms=?2 WHERE id=?1",
            params![run.as_bytes().as_slice(), sql_ms(now_ms)?],
        )?;
        transaction.commit()?;
        self.team_run(run)
    }

    /// Cancel a run: non-terminal roles become cancelled in one commit.
    pub fn cancel_team_run(&mut self, run: uuid::Uuid, now_ms: u64) -> Result<TeamRun> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = load_run(&transaction, run)?;
        if current.state == TeamState::Cancelled {
            transaction.commit()?;
            return Ok(current);
        }
        for role in &current.roles {
            if !role.state.is_terminal() {
                transaction.execute(
                    "UPDATE team_roles SET state=?3 WHERE run=?1 AND idx=?2",
                    params![
                        run.as_bytes().as_slice(),
                        role_index(role.index),
                        state_bytes(TeamState::Cancelled)?
                    ],
                )?;
            }
        }
        transaction.execute(
            "UPDATE team_runs SET state=?2,updated_ms=?3 WHERE id=?1",
            params![
                run.as_bytes().as_slice(),
                state_bytes(TeamState::Cancelled)?,
                sql_ms(now_ms)?
            ],
        )?;
        transaction.commit()?;
        self.team_run(run)
    }

    /// Session identities of a run's roles; used to cancel them.
    pub fn team_sessions(&self, run: uuid::Uuid) -> Result<Vec<(u16, uuid::Uuid)>> {
        let team = self.team_run(run)?;
        Ok(team
            .roles
            .into_iter()
            .filter_map(|role| role.session.map(|session| (role.index, session)))
            .collect())
    }

    /// The (run, role index) a chat session belongs to, if any.
    pub fn team_role_by_session(&self, session: uuid::Uuid) -> Result<Option<(uuid::Uuid, u16)>> {
        let transaction = self.connection.unchecked_transaction()?;
        let found: Option<(Vec<u8>, i64)> = transaction
            .query_row(
                "SELECT run,idx FROM team_roles WHERE session=?1",
                [session.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        transaction.commit()?;
        let Some((run, index)) = found else {
            return Ok(None);
        };
        let run: [u8; 16] = run
            .as_slice()
            .try_into()
            .map_err(|_| corrupt("stored team run has the wrong length"))?;
        let index =
            u16::try_from(index).map_err(|_| corrupt("stored team role index is out of range"))?;
        Ok(Some((uuid::Uuid::from_bytes(run), index)))
    }
}

/// Check every persisted team row; called by the schema opener.
pub(super) fn validate_integrity(connection: &Connection) -> Result<()> {
    let invalid: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM team_runs
            WHERE length(id)!=16 OR length(worktree)!=16
               OR length(lead) NOT BETWEEN 1 AND 64
               OR length(state) NOT BETWEEN 1 AND 1024
               OR fan_in_sent NOT IN (0,1)
               OR created_ms < 0 OR updated_ms < 0
        ) OR EXISTS(
            SELECT 1 FROM team_roles
            WHERE length(run)!=16 OR idx NOT BETWEEN 0 AND 7
               OR length(label) NOT BETWEEN 1 AND 64
               OR length(provider) NOT BETWEEN 1 AND 256
               OR (session IS NOT NULL AND length(session)!=16)
               OR (tab IS NOT NULL AND length(tab)!=16)
               OR length(state) NOT BETWEEN 1 AND 1024
               OR (summary IS NOT NULL AND length(summary) NOT BETWEEN 1 AND 16384)
               OR (result_file IS NOT NULL AND length(result_file) NOT BETWEEN 1 AND 4096)
               OR (submitted_ms IS NOT NULL AND submitted_ms < 0)
               OR ((summary IS NULL) != (submitted_ms IS NULL))
        )",
        [],
        |row| row.get(0),
    )?;
    if invalid {
        return Err(corrupt("invalid persisted team state"));
    }
    Ok(())
}
