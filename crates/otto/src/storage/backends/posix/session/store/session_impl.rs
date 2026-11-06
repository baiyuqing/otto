//! The `Session` trait implementation for [`Store`].

use super::*;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Session for Store {
    fn memory_references(&self) -> Vec<otto_core::session::TurnMemoryReferences> {
        let state = self.lock().expect("session mutex");
        let leaf = state.leaf_id.as_deref().unwrap_or_default();
        build_context(&state.entries, leaf)
            .expect("stored session remains a valid context")
            .0
            .memory_references
    }

    fn messages(&self) -> Vec<Message> {
        Store::messages(self)
    }

    fn model_messages(&self) -> Vec<Message> {
        let state = self.lock().expect("session mutex");
        let leaf = state.leaf_id.as_deref().unwrap_or_default();
        build_context(&state.entries, leaf)
            .expect("stored session remains a valid context")
            .0
            .model_messages
    }

    async fn append(&self, message: Message) -> Result<(), SessionError> {
        self.append_message(&message)
            .map_err(|error| SessionError::Persist(error.to_string()))
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: Message,
    ) -> Result<otto_core::session::AppendOutcome, SessionError> {
        match Store::append_input_if_absent(self, input_id, &message) {
            Ok(true) => Ok(otto_core::session::AppendOutcome::Appended),
            Ok(false) => Ok(otto_core::session::AppendOutcome::AlreadyPresent),
            Err(error) => Err(SessionError::Persist(error.to_string())),
        }
    }

    fn has_input(&self, input_id: &str) -> bool {
        Store::has_input(self, input_id)
    }

    fn latest_compaction(&self) -> Option<CompactionMetadata> {
        Store::latest_compaction(self)
    }

    fn remote_compaction(&self) -> Option<otto_core::session::RemoteCompaction> {
        let state = self.lock().expect("session mutex");
        let leaf = state.leaf_id.as_deref().unwrap_or_default();
        build_context(&state.entries, leaf)
            .expect("stored session remains a valid context")
            .0
            .remote_compaction
    }

    fn post_remote_compaction_messages(&self) -> Vec<Message> {
        let state = self.lock().expect("session mutex");
        let leaf = state.leaf_id.as_deref().unwrap_or_default();
        build_context(&state.entries, leaf)
            .expect("stored session remains a valid context")
            .0
            .post_remote_compaction_messages
    }

    async fn append_remote_compaction(
        &self,
        checkpoint: otto_core::session::RemoteCompaction,
    ) -> Result<(), SessionError> {
        let encoded = otto_core::session::encode_remote_compaction(&checkpoint)
            .map_err(|error| SessionError::Persist(error.to_string()))?;
        Store::append_custom_entry(
            self,
            otto_core::session::REMOTE_COMPACTION_CUSTOM_TYPE,
            &encoded,
        )
        .map_err(|error| SessionError::Persist(error.to_string()))
    }

    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError> {
        Store::append_compaction(self, &checkpoint)
            .map_err(|error| SessionError::Persist(error.to_string()))
    }

    async fn append_operation_fact(&self, fact: OperationFact) -> Result<(), SessionError> {
        Store::append_operation_fact(self, fact)
            .map_err(|error| SessionError::Persist(error.to_string()))
    }

    fn operation_ledger(&self) -> OperationLedger {
        Store::operation_ledger(self)
    }

    fn observed_capabilities(&self) -> Option<otto_core::session::Capabilities> {
        let state = self.lock().expect("session mutex");
        let leaf = state.leaf_id.as_deref().unwrap_or_default();
        build_context(&state.entries, leaf)
            .expect("stored session remains a valid context")
            .0
            .observed_capabilities
    }

    fn interrupted_turn(&self) -> Option<otto_core::session::InterruptedTurn> {
        let state = self.lock().expect("session mutex");
        let leaf = state.leaf_id.as_deref().unwrap_or_default();
        build_context(&state.entries, leaf)
            .expect("stored session remains a valid context")
            .0
            .interrupted_turn
    }

    fn plan(&self) -> Vec<otto_core::session::PlanStep> {
        self.lock()
            .map(|state| state.plan.clone())
            .unwrap_or_default()
    }

    async fn append_custom(&self, custom_type: &str, data: &str) -> Result<(), SessionError> {
        Store::append_custom_entry(self, custom_type, data)
            .map_err(|error| SessionError::Persist(error.to_string()))
    }
}
