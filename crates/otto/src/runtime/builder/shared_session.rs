//! Shared session handle used by the agent and the frontend.

use super::*;

impl SharedSession {
    pub fn new(handle: Arc<dyn SessionHandle>) -> Self {
        crate::failover::ownership::register_writer_session(&handle);
        Self(handle, Arc::default(), Arc::default(), Arc::default())
    }

    /// A `--no-session` transcript.
    pub fn memory(header: Header) -> Self {
        Self(
            Arc::new(MemoryHandle::new(header)),
            Arc::default(),
            Arc::default(),
            Arc::default(),
        )
    }

    /// Starts collecting appended conversation messages for a turn upload.
    pub fn start_recording(&self) {
        *self.2.lock().unwrap_or_else(|p| p.into_inner()) = Some(Vec::new());
    }

    /// Stops recording and returns what was collected.
    pub fn finish_recording(&self) -> Vec<Message> {
        self.2
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_default()
    }

    pub(super) fn record(&self, message: &Message) {
        if let Some(recorded) = self.2.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            recorded.push(message.clone());
        }
    }

    pub fn handle(&self) -> &Arc<dyn SessionHandle> {
        &self.0
    }

    pub fn header(&self) -> Header {
        self.0.header()
    }

    pub fn name(&self) -> String {
        self.0.name()
    }

    pub fn path(&self) -> String {
        self.0.path()
    }

    pub fn rename(&self, name: &str) -> Result<(), String> {
        self.0.rename(name)
    }

    pub fn update_thinking_level(&self, thinking: &str) -> Result<(), String> {
        self.0.update_thinking_level(thinking)
    }

    pub fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String> {
        self.0.update_runtime(runtime)
    }

    pub(crate) fn workflow_closed(&self) -> tokio_util::sync::CancellationToken {
        self.1.closed.clone()
    }
    pub(crate) fn downgrade_workflow(&self) -> WeakWorkflowSession {
        WeakWorkflowSession(
            Arc::downgrade(&self.0),
            Arc::downgrade(&self.1),
            Arc::downgrade(&self.2),
            Arc::downgrade(&self.3),
        )
    }
    pub(crate) fn replace_workflow_binding(
        &self,
        binding: Arc<super::super::workflow_notifications::Binding>,
    ) {
        let mut current = self.3.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(old) = current.replace(binding) {
            old.stop();
            tokio::spawn(async move {
                old.join().await;
            });
        }
    }
    pub fn close(&self) -> Result<(), String> {
        if let Some(binding) = self.3.lock().unwrap_or_else(|p| p.into_inner()).take() {
            binding.stop();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    binding.join().await;
                });
            }
        }
        self.1.closed.cancel();
        self.0.close()
    }

    pub fn archive(
        &self,
        root: &Path,
        workspace: &str,
    ) -> Result<crate::session::ArchiveResult, String> {
        self.0.archive(root, workspace)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.0.snapshot()
    }

    pub fn lease(&self) -> Option<Arc<dyn crate::session::WriterOwnership>> {
        self.0.lease()
    }

    pub fn take_recovery(&self) -> Option<crate::session::Recovery> {
        self.0.take_recovery()
    }

    pub fn forked_from(&self) -> Option<otto_core::session::ForkRecord> {
        self.0.forked_from()
    }
}

#[async_trait::async_trait]
impl Session for SharedSession {
    fn messages(&self) -> Vec<Message> {
        self.0.messages()
    }

    fn model_messages(&self) -> Vec<Message> {
        self.0.model_messages()
    }

    async fn append(&self, message: Message) -> Result<(), SessionError> {
        self.0.append(message.clone()).await?;
        self.record(&message);
        Ok(())
    }

    fn latest_compaction(&self) -> Option<CompactionMetadata> {
        self.0.latest_compaction()
    }

    fn remote_compaction(&self) -> Option<otto_core::session::RemoteCompaction> {
        self.0.remote_compaction()
    }

    fn post_remote_compaction_messages(&self) -> Vec<Message> {
        self.0.post_remote_compaction_messages()
    }

    async fn append_remote_compaction(
        &self,
        checkpoint: otto_core::session::RemoteCompaction,
    ) -> Result<(), SessionError> {
        self.0.append_remote_compaction(checkpoint).await
    }

    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError> {
        self.0.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: otto_core::session::OperationFact,
    ) -> Result<(), SessionError> {
        self.0.append_operation_fact(fact).await
    }

    fn operation_history(&self) -> Vec<otto_core::session::OperationFact> {
        self.0.operation_history()
    }

    fn operation_ledger(&self) -> otto_core::session::OperationLedger {
        self.0.operation_ledger()
    }

    fn observed_capabilities(&self) -> Option<otto_core::session::Capabilities> {
        self.0.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<otto_core::session::InterruptedTurn> {
        self.0.interrupted_turn()
    }

    fn plan(&self) -> Vec<otto_core::session::PlanStep> {
        self.0.plan()
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: otto_core::model::Message,
    ) -> Result<otto_core::session::AppendOutcome, SessionError> {
        let outcome = self
            .0
            .append_input_if_absent(input_id, message.clone())
            .await?;
        if matches!(outcome, otto_core::session::AppendOutcome::Appended) {
            self.record(&message);
        }
        Ok(outcome)
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.0.has_input(input_id)
    }

    async fn append_custom(&self, custom_type: &str, data: &str) -> Result<(), SessionError> {
        self.0.append_custom(custom_type, data).await
    }
}
