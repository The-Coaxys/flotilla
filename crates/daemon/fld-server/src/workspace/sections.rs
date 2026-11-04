//! Daemon-authoritative section definitions, manual assignments and pins.
use fl_model::sections::{SectionMutation, SectionsState};
use fl_model::workspace::DaemonEvent;
use fl_proto::RpcError;
use super::WorkspaceService;

impl WorkspaceService {
    pub(crate) async fn sections_state(&self) -> Result<SectionsState, RpcError> {
        self.store(|store| store.database.sections_state()).await
    }

    pub(crate) async fn mutate_sections(&self, expected_rev: u64, mutation: SectionMutation) -> Result<SectionsState, RpcError> {
        // Serialize publication with structural deletion and other section
        // writes. The durable store performs validation and CAS atomically.
        let _structure = self.structure.lock().await;
        let state = self.store(move |store| store.database.mutate_sections(expected_rev, mutation)).await?;
        self.publish(DaemonEvent::SectionsChanged(state.clone()));
        Ok(state)
    }
}
