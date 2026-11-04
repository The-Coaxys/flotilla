//! `fl` CLI selection state (schema v8).
//!
//! The default workspace is durable daemon state, not a client preference:
//! every `fl` invocation on this home sees the same selection.
use super::tree::load_tree;
use super::*;
use crate::{Result, StoreError};
use fl_model::WorkspaceId;
use fl_model::workspace::WorkspaceInfo;
use rusqlite::{OptionalExtension, TransactionBehavior, params};

pub(crate) const SCHEMA_V8: &str = "
    CREATE TABLE IF NOT EXISTS cli_meta (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        selected_workspace BLOB CHECK(selected_workspace IS NULL OR length(selected_workspace) = 16)
    ) STRICT;
";

impl Database {
    /// The selected workspace, when it still exists.
    pub fn selected_workspace(&self) -> Result<Option<WorkspaceInfo>> {
        let transaction = self.connection.unchecked_transaction()?;
        let tree = load_tree(&transaction)?;
        let bytes: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT selected_workspace FROM cli_meta WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        transaction.commit()?;
        let bytes = match bytes {
            Some(bytes) => bytes,
            None => return Ok(None),
        };
        if bytes.len() != 16 {
            return Err(StoreError::Invalid(
                "stored CLI workspace ID has the wrong length".into(),
            ));
        }
        let id = WorkspaceId::from_bytes(bytes.as_slice().try_into().map_err(|_| {
            StoreError::Invalid("stored CLI workspace ID has the wrong length".into())
        })?);
        Ok(tree
            .workspaces
            .into_iter()
            .find(|workspace| workspace.id == id))
    }

    /// Select an existing workspace and return it.
    pub fn set_selected_workspace(&mut self, id: WorkspaceId) -> Result<WorkspaceInfo> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tree = load_tree(&transaction)?;
        let workspace = tree
            .workspaces
            .into_iter()
            .find(|workspace| workspace.id == id)
            .ok_or_else(|| StoreError::Missing(format!("workspace {id} not found")))?;
        transaction.execute(
            "INSERT INTO cli_meta(singleton,selected_workspace) VALUES(1,?1) \
             ON CONFLICT(singleton) DO UPDATE SET selected_workspace=excluded.selected_workspace",
            params![id.as_bytes().as_slice()],
        )?;
        transaction.commit()?;
        Ok(workspace)
    }
}
