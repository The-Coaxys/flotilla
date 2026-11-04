//! Coordination state and advisory locks over the durable store (M6).
//!
//! Every mutation is committed before the change is published, so a watcher
//! that reacts to `CoordinationChanged` always reads the new version.
use super::*;
use fl_model::coordination::{
    CoordinationLock, CoordinationScope, CoordinationSnapshot, CoordinationValue,
};

impl WorkspaceService {
    pub(crate) async fn coordination_get(
        &self,
        scope: CoordinationScope,
        key: String,
    ) -> Result<CoordinationSnapshot, RpcError> {
        let now = now_ms();
        self.store(move |store| store.database.coordination_get(scope, &key, now))
            .await
    }

    pub(crate) async fn coordination_set(
        &self,
        scope: CoordinationScope,
        key: String,
        value: String,
        if_version: Option<u64>,
    ) -> Result<CoordinationValue, RpcError> {
        let now = now_ms();
        let key_for_event = key.clone();
        let entry = self
            .store(move |store| {
                store
                    .database
                    .coordination_set(scope, &key, &value, if_version, now)
            })
            .await?;
        self.publish(DaemonEvent::CoordinationChanged {
            scope,
            key: key_for_event,
            version: entry.version,
        });
        Ok(entry)
    }

    pub(crate) async fn coordination_delete(
        &self,
        scope: CoordinationScope,
        key: String,
        if_version: Option<u64>,
    ) -> Result<(), RpcError> {
        let key_for_event = key.clone();
        self.store(move |store| {
            store
                .database
                .coordination_delete(scope, &key, if_version)
                .map(|_| ())
        })
        .await?;
        self.publish(DaemonEvent::CoordinationChanged {
            scope,
            key: key_for_event,
            version: 0,
        });
        Ok(())
    }

    pub(crate) async fn coordination_lock(
        &self,
        scope: CoordinationScope,
        key: String,
        owner: String,
        ttl_ms: u64,
    ) -> Result<CoordinationLock, RpcError> {
        let now = now_ms();
        let key_for_event = key.clone();
        let lock = self
            .store(move |store| {
                store
                    .database
                    .coordination_lock(scope, &key, &owner, ttl_ms, now)
            })
            .await?;
        self.publish(DaemonEvent::CoordinationChanged {
            scope,
            key: key_for_event,
            // Locks do not change the value version; 0 means "re-read".
            version: 0,
        });
        Ok(lock)
    }

    pub(crate) async fn coordination_unlock(
        &self,
        scope: CoordinationScope,
        key: String,
        owner: String,
    ) -> Result<(), RpcError> {
        let key_for_event = key.clone();
        self.store(move |store| store.database.coordination_unlock(scope, &key, &owner))
            .await?;
        self.publish(DaemonEvent::CoordinationChanged {
            scope,
            key: key_for_event,
            version: 0,
        });
        Ok(())
    }
}
