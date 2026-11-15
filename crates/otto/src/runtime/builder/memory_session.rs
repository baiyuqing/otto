//! In-memory session handle: the `--no-session` transcript.

use super::*;

impl MemoryHandle {
    pub fn new(mut header: Header) -> Self {
        header.version = CURRENT_VERSION;
        Self {
            inner: MemorySession::new(),
            state: Mutex::new(MemoryState {
                header,
                name: String::new(),
                custom_data: HashMap::new(),
                input_submission_events: Vec::new(),
                local_commands: Vec::new(),
            }),
            writer_ownership: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_writer_ownership(
        header: Header,
        writer_ownership: Arc<dyn otto_session::WriterOwnership>,
    ) -> Self {
        let mut handle = Self::new(header);
        handle.writer_ownership = Some(writer_ownership);
        handle
    }

    pub(super) fn state(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state.lock().expect("memory session mutex")
    }
}

#[async_trait::async_trait]
impl Session for MemoryHandle {
    fn session_id(&self) -> Option<String> {
        Some(self.header().id)
    }

    fn memory_references(&self) -> Vec<otto_core::session::TurnMemoryReferences> {
        self.inner.memory_references()
    }

    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }

    async fn append(&self, message: Message) -> Result<(), SessionError> {
        self.inner.append(message).await
    }

    fn latest_compaction(&self) -> Option<CompactionMetadata> {
        self.inner.latest_compaction()
    }

    fn remote_compaction(&self) -> Option<otto_core::session::RemoteCompaction> {
        self.inner.remote_compaction()
    }

    fn post_remote_compaction_messages(&self) -> Vec<Message> {
        self.inner.post_remote_compaction_messages()
    }

    async fn append_remote_compaction(
        &self,
        checkpoint: otto_core::session::RemoteCompaction,
    ) -> Result<(), SessionError> {
        self.inner.append_remote_compaction(checkpoint).await
    }

    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError> {
        self.inner.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: otto_core::session::OperationFact,
    ) -> Result<(), SessionError> {
        self.inner.append_operation_fact(fact).await
    }

    fn operation_history(&self) -> Vec<otto_core::session::OperationFact> {
        self.inner.operation_history()
    }

    fn operation_ledger(&self) -> otto_core::session::OperationLedger {
        self.inner.operation_ledger()
    }

    fn observed_capabilities(&self) -> Option<otto_core::session::Capabilities> {
        self.inner.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<otto_core::session::InterruptedTurn> {
        self.inner.interrupted_turn()
    }

    fn plan(&self) -> Vec<otto_core::session::PlanStep> {
        self.inner.plan()
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: otto_core::model::Message,
    ) -> Result<otto_core::session::AppendOutcome, SessionError> {
        self.inner.append_input_if_absent(input_id, message).await
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.inner.has_input(input_id)
    }

    async fn append_custom(&self, custom_type: &str, data: &str) -> Result<(), SessionError> {
        self.inner.append_custom(custom_type, data).await?;
        if custom_type == otto_core::session::local_command::LOCAL_COMMAND_CUSTOM_TYPE {
            let receipt = serde_json::from_str(data)
                .map_err(|error| SessionError::Persist(error.to_string()))?;
            self.state().local_commands.push(receipt);
        }
        self.state()
            .custom_data
            .insert(custom_type.into(), data.into());
        Ok(())
    }
}

impl SessionHandle for MemoryHandle {
    fn append_input_submissions(&self, encoded: &str) -> Result<(), String> {
        self.state().input_submission_events.push(encoded.into());
        Ok(())
    }
    fn input_submission_events(&self) -> Result<Vec<String>, String> {
        Ok(self.state().input_submission_events.clone())
    }
    fn append_local_command(&self, encoded: &str) -> Result<(), String> {
        let receipt = serde_json::from_str(encoded).map_err(|error| error.to_string())?;
        self.state().local_commands.push(receipt);
        Ok(())
    }
    fn local_commands(&self) -> Vec<otto_core::session::local_command::LocalCommandReceipt> {
        otto_core::session::local_command::fold_local_commands(self.state().local_commands.clone())
    }
    fn custom_data(&self, custom_type: &str) -> Option<String> {
        self.state().custom_data.get(custom_type).cloned()
    }
    fn header(&self) -> Header {
        self.state().header.clone()
    }

    fn name(&self) -> String {
        self.state().name.clone()
    }

    /// An in-memory session has no file.
    fn path(&self) -> String {
        String::new()
    }

    fn rename(&self, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("session name is required".to_string());
        }
        self.state().name = name.to_string();
        Ok(())
    }

    fn update_thinking_level(&self, thinking: &str) -> Result<(), String> {
        if !matches!(thinking, "" | "low" | "medium" | "high" | "xhigh" | "max") {
            return Err(
                "invalid thinking: must be one of low, medium, high, xhigh, max".to_string(),
            );
        }
        Ok(())
    }

    fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String> {
        if runtime.provider.is_empty() || runtime.model.is_empty() {
            return Err("session is invalid".to_string());
        }
        let mut state = self.state();
        state.header.profile = runtime.profile.clone();
        state.header.provider = runtime.provider.clone();
        state.header.model = runtime.model.clone();
        Ok(())
    }

    fn close(&self) -> Result<(), String> {
        Ok(())
    }

    fn lease(&self) -> Option<Arc<dyn otto_session::WriterOwnership>> {
        self.writer_ownership.clone()
    }
}
