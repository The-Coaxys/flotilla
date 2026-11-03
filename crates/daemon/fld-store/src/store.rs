//! Publication order: durable blob -> SQLite FULL commit -> caller journal ack.
use crate::{
    CheckpointRef, Result, StoreError, TerminalKey, checkpoint::CheckpointFiles, database::Database,
};
use fl_model::LogCursor;
use fl_term::{ENGINE_ID, EffectMode, Engine};
use std::path::Path;

pub struct Store {
    pub database: Database,
    files: CheckpointFiles,
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        let database = Database::open(root)?;
        let files = CheckpointFiles::open(root)?;
        Ok(Self { database, files })
    }

    /// Successful return is the only permission to reclaim the covered journal.
    pub fn checkpoint(
        &mut self,
        key: TerminalKey,
        cursor: LogCursor,
        engine: &Engine,
    ) -> Result<CheckpointRef> {
        if engine.offset() != cursor.next_byte {
            return Err(StoreError::Corrupt(
                "engine offset does not match checkpoint cursor".into(),
            ));
        }
        // Reject obsolete incarnations before doing potentially large file IO.
        self.database.terminal(key)?;
        let bytes = engine.checkpoint()?;
        let hash = self.files.publish(&bytes)?;
        let reference = CheckpointRef {
            hash,
            cursor,
            size: engine.size(),
            engine_id: ENGINE_ID.into(),
        };
        // This method rechecks incarnation/cursor within its SQLite transaction.
        self.database.save_checkpoint_ref(key, &reference)?;
        Ok(reference)
    }

    pub fn restore(&self, key: TerminalKey, mode: EffectMode) -> Result<(Engine, LogCursor)> {
        let entry = self.database.terminal(key)?;
        match entry.checkpoint {
            Some(reference) => {
                if reference.engine_id != ENGINE_ID {
                    return Err(StoreError::Corrupt(format!(
                        "unsupported checkpoint engine {}",
                        reference.engine_id
                    )));
                }
                let bytes = self.files.load(reference.hash)?;
                let engine = Engine::restore(&bytes, mode)?;
                if engine.offset() != reference.cursor.next_byte || engine.size() != reference.size
                {
                    return Err(StoreError::Corrupt(
                        "checkpoint reference disagrees with engine state".into(),
                    ));
                }
                Ok((engine, reference.cursor))
            }
            None => Ok((Engine::new(entry.spec.size, mode)?, LogCursor::default())),
        }
    }

    /// Requires exclusive Store ownership so GC cannot race blob publication
    /// and the subsequent database reference commit.
    pub fn collect_checkpoint_garbage(&mut self) -> Result<usize> {
        let hashes = self.database.checkpoint_hashes()?;
        self.files.retain_only(&hashes)
    }
}
