//! Durable custom commands (schema v10).
//!
//! One bounded snapshot with a revision; every edit is compare-and-set so two
//! writers cannot silently overwrite each other's commands. Duplicate and
//! missing definitions are refusals, not silent upserts.
use super::*;
use fl_model::command::{CommandMutation, CommandState, MAX_COMMAND_BODY_BYTES, MAX_COMMANDS};
use fl_model::routing::Scope;

const MAX_COMMANDS_BYTES: usize = 4 * 1024 * 1024;

pub(super) const SCHEMA_V10: &str = "
    CREATE TABLE commands (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        state BLOB NOT NULL CHECK(length(state) BETWEEN 1 AND 4194304)
    ) STRICT;
";

impl Database {
    pub fn command_state(&self) -> Result<CommandState> {
        let transaction = self.connection.unchecked_transaction()?;
        let state = load_commands(&transaction)?;
        transaction.commit()?;
        Ok(state)
    }

    pub fn mutate_commands(
        &mut self,
        expected_rev: u64,
        mutation: CommandMutation,
    ) -> Result<CommandState> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut state = load_commands(&transaction)?;
        if state.rev != expected_rev {
            return Err(StoreError::RevisionConflict {
                expected: state.rev,
                got: expected_rev,
            });
        }
        check_scope(&transaction, mutation.scope())?;
        state.apply(mutation).map_err(StoreError::Invalid)?;
        check_scope_references(&transaction, &state)?;
        let bytes = postcard::to_stdvec(&state)?;
        if bytes.len() > MAX_COMMANDS_BYTES {
            return Err(StoreError::Invalid(
                "command state exceeds its byte limit".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO commands(singleton,state) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET state=excluded.state",
            [bytes],
        )?;
        transaction.commit()?;
        Ok(state)
    }
}

fn check_scope(connection: &Connection, scope: Scope) -> Result<()> {
    match scope {
        Scope::Global => Ok(()),
        Scope::Workspace(id) => {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?1)",
                [id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if exists {
                Ok(())
            } else {
                Err(StoreError::Missing("command workspace not found".into()))
            }
        }
        Scope::Project(id) => {
            let exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",
                [id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if exists {
                Ok(())
            } else {
                Err(StoreError::Missing("command project not found".into()))
            }
        }
    }
}

fn check_scope_references(connection: &Connection, state: &CommandState) -> Result<()> {
    for scope in state.commands.iter().map(|command| command.scope) {
        check_scope(connection, scope)?;
    }
    Ok(())
}

fn load_commands(connection: &Connection) -> Result<CommandState> {
    let bytes: Option<Vec<u8>> = connection
        .query_row("SELECT state FROM commands WHERE singleton=1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(bytes) = bytes else {
        return Ok(CommandState::default());
    };
    if bytes.len() > MAX_COMMANDS_BYTES {
        return Err(corrupt("oversized command snapshot"));
    }
    let state: CommandState = postcard::from_bytes(&bytes)?;
    state.validate().map_err(corrupt)?;
    if state.commands.len() > MAX_COMMANDS
        || state
            .commands
            .iter()
            .any(|command| command.body.len() > MAX_COMMAND_BODY_BYTES)
    {
        return Err(corrupt("command snapshot exceeds entry limits"));
    }
    Ok(state)
}

pub(super) fn initial_commands() -> Result<Vec<u8>> {
    Ok(postcard::to_stdvec(&CommandState::default())?)
}

pub(super) fn validate_integrity(connection: &Connection) -> Result<()> {
    check_scope_references(connection, &load_commands(connection)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::command::CustomCommand;
    use fl_model::routing::Scope;

    fn command(scope: Scope, name: &str, body: &str) -> CustomCommand {
        CustomCommand {
            scope,
            name: name.into(),
            description: None,
            body: body.into(),
        }
    }

    #[test]
    fn commands_persist_with_compare_and_set_and_refusals() {
        let temporary = tempfile::tempdir().unwrap();
        let mut database = Database::open(temporary.path()).unwrap();
        assert_eq!(database.command_state().unwrap(), CommandState::default());
        let created = database
            .mutate_commands(
                0,
                CommandMutation::Create(command(Scope::Global, "deploy", "Ship {{branch}}")),
            )
            .unwrap();
        assert_eq!(created.rev, 1);
        assert_eq!(database.command_state().unwrap().commands[0].name, "deploy");

        // A stale revision is refused and next state is unchanged.
        assert!(matches!(
            database.mutate_commands(
                0,
                CommandMutation::Create(command(Scope::Global, "other", "body")),
            ),
            Err(StoreError::RevisionConflict { .. })
        ));
        // A duplicate name at the same scope is a refusal with the reason.
        let duplicate = database
            .mutate_commands(
                1,
                CommandMutation::Create(command(Scope::Global, "deploy", "again")),
            )
            .unwrap_err();
        assert!(
            duplicate.to_string().contains("already exists"),
            "{duplicate}"
        );

        let updated = database
            .mutate_commands(
                1,
                CommandMutation::Update(command(Scope::Global, "deploy", "Ship now")),
            )
            .unwrap();
        assert_eq!(updated.commands[0].body, "Ship now");
        let deleted = database
            .mutate_commands(
                2,
                CommandMutation::Delete {
                    scope: Scope::Global,
                    name: "deploy".into(),
                },
            )
            .unwrap();
        assert!(deleted.commands.is_empty());
        assert_eq!(deleted.rev, 3);
        assert!(matches!(
            database.mutate_commands(
                3,
                CommandMutation::Delete {
                    scope: Scope::Global,
                    name: "deploy".into(),
                },
            ),
            Err(StoreError::Invalid(_))
        ));
    }

    #[test]
    fn a_command_scope_must_reference_a_stored_project() {
        let temporary = tempfile::tempdir().unwrap();
        let mut database = Database::open(temporary.path()).unwrap();
        let missing = database
            .mutate_commands(
                0,
                CommandMutation::Create(command(
                    Scope::Project(fl_model::ProjectId::new()),
                    "deploy",
                    "body",
                )),
            )
            .unwrap_err();
        assert!(matches!(missing, StoreError::Missing(_)), "{missing}");
    }
}
