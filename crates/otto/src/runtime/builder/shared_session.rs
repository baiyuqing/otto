//! Shared session handle used by the agent and the frontend.

use super::*;

impl SessionState {
    fn new(transcript: Arc<dyn SessionHandle>) -> Self {
        Self {
            transcript,
            hooks: Arc::default(),
            resources: std::sync::OnceLock::new(),
            resources_creation: Mutex::new(()),
        }
    }
}

impl SharedSession {
    pub fn new(handle: Arc<dyn SessionHandle>) -> Self {
        crate::failover::ownership::register_writer_session(&handle);
        Self(Arc::new(SessionState::new(handle)))
    }

    /// A `--no-session` transcript.
    pub fn memory(header: Header) -> Self {
        Self(Arc::new(SessionState::new(Arc::new(MemoryHandle::new(
            header,
        )))))
    }

    /// The session's resources, absent until the first runner build.
    pub(crate) fn resources(
        &self,
    ) -> Option<&Arc<super::super::session_resources::SessionResources>> {
        self.0.resources.get()
    }

    /// The hook lifecycle state of the session; shared by every runner.
    pub(crate) fn hooks(&self) -> &Arc<otto_core::agent::hooks::State> {
        &self.0.hooks
    }

    /// Returns the session's resources, calling `create` for the first caller
    /// only. Concurrent first callers wait for it.
    pub(crate) fn resources_or_create(
        &self,
        create: impl FnOnce() -> Arc<super::super::session_resources::SessionResources>,
    ) -> Arc<super::super::session_resources::SessionResources> {
        let _creating = self
            .0
            .resources_creation
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = self.0.resources.get() {
            return Arc::clone(existing);
        }
        let created = create();
        let _ = self.0.resources.set(Arc::clone(&created));
        created
    }

    /// Installs resources built by a test around a fixed registry; ignored
    /// when the session already has resources.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn install_test_resources(
        &self,
        resources: Arc<super::super::session_resources::SessionResources>,
    ) {
        let _ = self.resources_or_create(|| resources);
    }

    pub fn handle(&self) -> &Arc<dyn SessionHandle> {
        &self.0.transcript
    }

    pub fn header(&self) -> Header {
        self.0.transcript.header()
    }

    pub fn name(&self) -> String {
        self.0.transcript.name()
    }

    pub fn path(&self) -> String {
        self.0.transcript.path()
    }

    pub fn rename(&self, name: &str) -> Result<(), String> {
        self.0.transcript.rename(name)
    }

    pub fn update_thinking_level(&self, thinking: &str) -> Result<(), String> {
        self.0.transcript.update_thinking_level(thinking)
    }

    /// Appends a plain custom entry that is written when the session file is
    /// created (see [`SessionHandle::append_custom_when_created`]), or at once
    /// when the file already exists. A fresh lazy session gets no file here.
    pub async fn append_custom_when_created(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), SessionError> {
        self.0
            .transcript
            .append_custom_when_created(custom_type, data)
            .await
    }

    pub fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String> {
        self.0.transcript.update_runtime(runtime)
    }

    pub(crate) fn workflow_closed(&self) -> tokio_util::sync::CancellationToken {
        self.0.hooks.closed.clone()
    }
    pub(crate) fn downgrade(&self) -> WeakSharedSession {
        WeakSharedSession(Arc::downgrade(&self.0))
    }
    /// Makes `binding` the session's workflow delivery worker and stops the
    /// previous one. A session without resources cannot hold the worker, so
    /// the binding is stopped.
    pub(crate) fn replace_workflow_binding(
        &self,
        binding: Arc<super::super::workflow_notifications::Binding>,
    ) {
        match self.resources() {
            Some(resources) => resources.replace_workflow_binding(binding),
            None => binding.stop(),
        }
    }
    /// Ends the session: stops its resources without waiting, cancels the
    /// hook state and closes the transcript. Use [`Self::close_resources`]
    /// as well when the caller must wait for MCP shutdown.
    pub fn close(&self) -> Result<(), String> {
        self.stop_resources();
        self.0.hooks.closed.cancel();
        self.0.transcript.close()
    }

    pub fn permanent_delete(&self, root: &Path, workspace: &str) -> Result<(), String> {
        self.0.transcript.permanent_delete(root, workspace)
    }

    pub fn archive(
        &self,
        root: &Path,
        workspace: &str,
    ) -> Result<crate::session::ArchiveResult, String> {
        self.0.transcript.archive(root, workspace)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.0.transcript.snapshot()
    }

    pub fn lease(&self) -> Option<Arc<dyn crate::session::WriterOwnership>> {
        self.0.transcript.lease()
    }

    pub fn take_recovery(&self) -> Option<crate::session::Recovery> {
        self.0.transcript.take_recovery()
    }

    pub fn forked_from(&self) -> Option<otto_core::session::ForkRecord> {
        self.0.transcript.forked_from()
    }
}

#[async_trait::async_trait]
impl Session for SharedSession {
    fn session_id(&self) -> Option<String> {
        Some(self.header().id)
    }

    fn memory_references(&self) -> Vec<otto_core::session::TurnMemoryReferences> {
        self.0.transcript.memory_references()
    }

    fn messages(&self) -> Vec<Message> {
        self.0.transcript.messages()
    }

    fn model_messages(&self) -> Vec<Message> {
        self.0.transcript.model_messages()
    }

    async fn append(&self, message: Message) -> Result<(), SessionError> {
        self.0.transcript.append(message).await
    }

    fn latest_compaction(&self) -> Option<CompactionMetadata> {
        self.0.transcript.latest_compaction()
    }

    fn remote_compaction(&self) -> Option<otto_core::session::RemoteCompaction> {
        self.0.transcript.remote_compaction()
    }

    fn post_remote_compaction_messages(&self) -> Vec<Message> {
        self.0.transcript.post_remote_compaction_messages()
    }

    async fn append_remote_compaction(
        &self,
        checkpoint: otto_core::session::RemoteCompaction,
    ) -> Result<(), SessionError> {
        self.0.transcript.append_remote_compaction(checkpoint).await
    }

    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError> {
        self.0.transcript.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: otto_core::session::OperationFact,
    ) -> Result<(), SessionError> {
        self.0.transcript.append_operation_fact(fact).await
    }

    fn operation_history(&self) -> Vec<otto_core::session::OperationFact> {
        self.0.transcript.operation_history()
    }

    fn operation_ledger(&self) -> otto_core::session::OperationLedger {
        self.0.transcript.operation_ledger()
    }

    fn observed_capabilities(&self) -> Option<otto_core::session::Capabilities> {
        self.0.transcript.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<otto_core::session::InterruptedTurn> {
        self.0.transcript.interrupted_turn()
    }

    fn plan(&self) -> Vec<otto_core::session::PlanStep> {
        self.0.transcript.plan()
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: otto_core::model::Message,
    ) -> Result<otto_core::session::AppendOutcome, SessionError> {
        self.0
            .transcript
            .append_input_if_absent(input_id, message)
            .await
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.0.transcript.has_input(input_id)
    }

    async fn append_custom(&self, custom_type: &str, data: &str) -> Result<(), SessionError> {
        self.0.transcript.append_custom(custom_type, data).await
    }
}
