//! Composition of one runnable agent from resolved configuration.
//!
//! Memory, skills, sub-agents, ChatGPT credentials, and the HTTP trace writer
//! are phases 5 to 7; the seams for them are named below and nothing else about
//! the composition order changes when they arrive.
//!
//! Safety: the redaction boundary decides everything. `boundary_redactor` must
//! leave the workspace path, the tool definitions, and the system prompt
//! byte-identical, or the whole run degrades to a closed boundary: no provider
//! client, no bash tool, no runtime identity in the status line.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use otto_core::agent::inbox::NotificationKind;
use otto_core::agent::progress::ProgressPolicy;
use otto_core::agent::redactor::Redactor;
use otto_core::agent::{
    Agent, AgentError, CompactionResult, CompactionSettings, Event, EventSink, Options,
};
use otto_core::config::resolve::{Overrides, Runtime, SessionDefaults};
use otto_core::config::{ConfigError, File, McpRuntime};
use otto_core::model::{Block, Message, OperationStopReason, ToolDefinition, Usage};
use otto_core::operation::OperationControl;
use otto_core::provider::{
    Provider, ProviderError, ProviderSettlement, Request, RequestSizer, StreamSink,
};
use otto_core::session::{
    CURRENT_VERSION, CompactionCheckpoint, CompactionMetadata, Header, MemorySession,
    RuntimeMetadata, Session, SessionError, Snapshot,
};
use otto_core::tool::ToolExecutor;
use tokio_util::sync::CancellationToken;

use crate::deadline::{Control, Deadline};
use crate::failover;
use crate::provider::openaicompat::Client;
use crate::sandbox::CommandExecutor;
pub use crate::session::SessionHandle;
#[cfg(test)]
use crate::session::Store;
use crate::skill::Catalog;
use crate::tool::registry::Registry;
use crate::tool::workspace::Workspace;
use crate::tool::{Tool, bash, edit, find, grep, ls, models, read, verification, write};

use super::boundary::{self, BoundaryInputs, FixedText};
use super::info::{SandboxInfo, SandboxMode, SandboxNetwork, SandboxReason};
use super::prompt::{subagent_model_guidance, system_prompt_for};
use super::sandbox_runtime::canonical_directory;
use super::workspace_context::{layered_workspace_context, split_workspace_instructions};

struct BuildTrace {
    last: Instant,
    entries: Vec<(&'static str, Duration)>,
}

impl BuildTrace {
    fn new() -> Self {
        Self {
            last: Instant::now(),
            entries: Vec::new(),
        }
    }

    fn mark(&mut self, label: &'static str) {
        let now = Instant::now();
        self.entries.push((label, now.duration_since(self.last)));
        self.last = now;
    }
}

fn mark_build_trace(trace: &mut Option<BuildTrace>, label: &'static str) {
    if let Some(trace) = trace {
        trace.mark(label);
    }
}

/// Every failure here is already redacted text, so a `String` carries all a
/// caller may show.
pub type BuildError = String;

/// Everything a frontend shows about the resolved runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub thinking: String,
    pub context_window: i64,
    pub sandbox: SandboxInfo,
}

/// A transcript that is never written to disk. It is what `--no-session`
/// selects; `otto_core::session::MemorySession` carries the transcript and this
/// wrapper carries the header and the name.
pub struct MemoryHandle {
    inner: MemorySession,
    state: Mutex<MemoryState>,
    writer_ownership: Option<Arc<dyn crate::session::WriterOwnership>>,
}

struct MemoryState {
    header: Header,
    name: String,
    custom_data: HashMap<String, String>,
}

impl MemoryHandle {
    pub fn new(mut header: Header) -> Self {
        header.version = CURRENT_VERSION;
        Self {
            inner: MemorySession::new(),
            state: Mutex::new(MemoryState {
                header,
                name: String::new(),
                custom_data: HashMap::new(),
            }),
            writer_ownership: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_writer_ownership(
        header: Header,
        writer_ownership: Arc<dyn crate::session::WriterOwnership>,
    ) -> Self {
        let mut handle = Self::new(header);
        handle.writer_ownership = Some(writer_ownership);
        handle
    }

    fn state(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state.lock().expect("memory session mutex")
    }
}

#[async_trait::async_trait]
impl Session for MemoryHandle {
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
        self.state()
            .custom_data
            .insert(custom_type.into(), data.into());
        Ok(())
    }
}

impl SessionHandle for MemoryHandle {
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

    fn lease(&self) -> Option<Arc<dyn crate::session::WriterOwnership>> {
        self.writer_ownership.clone()
    }
}

/// A session shared by the agent and the frontend.
///
/// Both need `&self` access to the same transcript for the whole run, which
/// is what `Arc` buys; every [`SessionHandle`] method already takes `&self`.
#[derive(Clone)]
pub struct SharedSession(
    Arc<dyn SessionHandle>,
    Arc<otto_core::agent::hooks::State>,
    Recorder,
    Arc<Mutex<Option<Arc<super::workflow_notifications::Binding>>>>,
);

pub(crate) struct WeakWorkflowSession(
    std::sync::Weak<dyn SessionHandle>,
    std::sync::Weak<otto_core::agent::hooks::State>,
    std::sync::Weak<std::sync::Mutex<Option<Vec<Message>>>>,
    std::sync::Weak<Mutex<Option<Arc<super::workflow_notifications::Binding>>>>,
);
impl WeakWorkflowSession {
    pub(crate) fn upgrade(&self) -> Option<SharedSession> {
        Some(SharedSession(
            self.0.upgrade()?,
            self.1.upgrade()?,
            self.2.upgrade()?,
            self.3.upgrade()?,
        ))
    }
}

/// Conversation messages appended while recording is on; `None` when off.
/// Shared by clones, so the frontend and the agent see one recorder.
type Recorder = Arc<std::sync::Mutex<Option<Vec<Message>>>>;

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

    fn record(&self, message: &Message) {
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
        binding: Arc<super::workflow_notifications::Binding>,
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

/// The provider an agent calls, or the refusal used when the redaction boundary
/// is closed.
///
/// The agent never reaches the client when the boundary is closed, because
/// `Run` checks the redactor first. This enum keeps the provider slot
/// non-optional, so the agent's type parameter stays concrete.
#[derive(Clone)]
pub enum ProviderClient {
    Compat {
        client: Arc<Client>,
        timeout: Option<Duration>,
        cancellation_grace: Duration,
    },
    ChatGpt {
        client: Arc<crate::provider::chatgpt::Client>,
        timeout: Option<Duration>,
        cancellation_grace: Duration,
    },
    Unavailable,
    /// Test seam. `Runner` is a concrete struct, so the seam sits one layer
    /// down.
    #[cfg(test)]
    Scripted(Arc<dyn Provider + Send + Sync>),
}

#[async_trait::async_trait]
impl Provider for ProviderClient {
    fn capabilities(&self) -> otto_core::provider::ProviderCapabilities {
        match self {
            Self::Compat { client, .. } => client.capabilities(),
            Self::ChatGpt { client, .. } => client.capabilities(),
            Self::Unavailable => otto_core::provider::ProviderCapabilities::default(),
            #[cfg(test)]
            Self::Scripted(provider) => provider.capabilities(),
        }
    }

    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.complete_observed(request, emit, control, &otto_core::diagnostics::Noop)
            .await
    }

    fn complete_observed<'a>(
        &'a self,
        request: &'a Request,
        emit: StreamSink<'a>,
        control: &'a dyn OperationControl,
        observer: &'a dyn otto_core::diagnostics::Observer,
    ) -> otto_core::provider::DiagnosticFuture<'a> {
        Box::pin(async move {
            match self {
                Self::Compat {
                    client,
                    timeout,
                    cancellation_grace,
                } => {
                    complete_with_timeout(
                        &ObservedProvider {
                            inner: client.as_ref(),
                            observer,
                        },
                        request,
                        emit,
                        control,
                        *timeout,
                        *cancellation_grace,
                    )
                    .await
                }
                Self::ChatGpt {
                    client,
                    timeout,
                    cancellation_grace,
                } => {
                    complete_with_timeout(
                        &ObservedProvider {
                            inner: client.as_ref(),
                            observer,
                        },
                        request,
                        emit,
                        control,
                        *timeout,
                        *cancellation_grace,
                    )
                    .await
                }
                Self::Unavailable => ProviderSettlement::failed(
                    ProviderError::Other(
                        "provider is unavailable: redaction is incomplete".to_string(),
                    ),
                    0,
                    otto_core::model::EffectCertainty::NotStarted,
                ),
                #[cfg(test)]
                Self::Scripted(provider) => {
                    provider
                        .complete_observed(request, emit, control, observer)
                        .await
                }
            }
        })
    }
}

struct ObservedProvider<'a, P: ?Sized> {
    inner: &'a P,
    observer: &'a dyn otto_core::diagnostics::Observer,
}
#[async_trait::async_trait]
impl<P: Provider + Sync + ?Sized> Provider for ObservedProvider<'_, P> {
    fn capabilities(&self) -> otto_core::provider::ProviderCapabilities {
        self.inner.capabilities()
    }
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.inner
            .complete_observed(request, emit, control, self.observer)
            .await
    }
}

fn provider_stop_settlement(reason: OperationStopReason) -> ProviderSettlement {
    ProviderSettlement::stopped(
        if reason == OperationStopReason::Deadline {
            ProviderError::DeadlineExceeded
        } else {
            ProviderError::Cancelled
        },
        0,
        otto_core::model::EffectCertainty::NotStarted,
        reason,
    )
}

async fn complete_with_timeout<P: Provider + ?Sized>(
    provider: &P,
    request: &Request,
    emit: StreamSink<'_>,
    parent: &dyn OperationControl,
    timeout: Option<Duration>,
    cancellation_grace: Duration,
) -> ProviderSettlement {
    if let Some(reason) = parent.admission_stop_reason() {
        return provider_stop_settlement(reason);
    }
    let control = Control::new(Deadline::child(parent.remaining(), timeout));
    let mut complete = std::pin::pin!(provider.complete(request, emit, &control));
    let deadline = control.deadline();
    let reason = tokio::select! {
        biased;
        result = &mut complete => return result,
        () = parent.cancellation_token().cancelled() => {
            let reason = parent
                .stop_reason()
                .unwrap_or(OperationStopReason::UserCancellation);
            control.stop(reason);
            reason
        }
        () = deadline.expired() => {
            control.stop(OperationStopReason::Deadline);
            OperationStopReason::Deadline
        }
    };
    match tokio::time::timeout(cancellation_grace, &mut complete).await {
        Ok(settlement) => settlement,
        Err(_) => ProviderSettlement::stopped(
            if reason == OperationStopReason::Deadline {
                ProviderError::DeadlineExceeded
            } else {
                ProviderError::Cancelled
            },
            1,
            otto_core::model::EffectCertainty::Unknown,
            reason,
        ),
    }
}

async fn drive_with_control<F, T>(
    future: F,
    parent_cancel: &CancellationToken,
    control: &Control,
) -> T
where
    F: Future<Output = T>,
{
    let mut future = std::pin::pin!(future);
    let deadline = control.deadline();
    tokio::select! {
        biased;
        result = &mut future => result,
        () = parent_cancel.cancelled() => {
            control.stop(OperationStopReason::UserCancellation);
            future.await
        }
        () = deadline.expired() => {
            control.stop(OperationStopReason::Deadline);
            future.await
        }
    }
}

/// The notice a frontend shows after an interrupted turn dropped guidance it
/// had accepted but never delivered.
///
/// The exact words matter: the user typed them and, with the turn gone, the
/// notice is the only place they can read them back. Each message is quoted,
/// bounded so a pathological queue cannot flood the transcript, and the
/// remainder is counted rather than hidden.
fn lost_steering_notice(lost: &[otto_core::agent::steering::Message]) -> String {
    const MAX_QUOTED: usize = 8;
    const MAX_QUOTED_CHARS: usize = 1_000;

    let mut notice = if lost.len() == 1 {
        "1 queued guidance message for the turn that just ended was not delivered: \
         the turn ended before the next safe boundary, so it was dropped. \
         Send it again if it is still needed."
            .to_string()
    } else {
        format!(
            "{} queued guidance messages for the turn that just ended were not \
             delivered: the turn ended before the next safe boundary, so they were \
             dropped. Send them again if they are still needed.",
            lost.len()
        )
    };
    for message in lost.iter().take(MAX_QUOTED) {
        notice.push_str("\n\n> ");
        let mut chars = message.text.chars();
        let quoted: String = chars.by_ref().take(MAX_QUOTED_CHARS).collect();
        notice.push_str(&quoted.split('\n').collect::<Vec<_>>().join("\n> "));
        if chars.next().is_some() {
            notice.push_str(" […]");
        }
    }
    if let Some(rest) = lost.len().checked_sub(MAX_QUOTED).filter(|rest| *rest > 0) {
        notice.push_str(&format!("\n\n({rest} more not shown)"));
    }
    notice
}

/// One composed agent, plus the two fixed strings a frontend may show.
/// The request sizer a [`Runner::scripted`] runner uses: the bytes of text, so
/// compaction can size its summary request without a real provider client.
#[cfg(test)]
struct ScriptedSizer;

/// Test providers keyed by home directory: a runner built by a [`Builder`]
/// whose `home` is listed uses that provider, so a test can drive the full
/// composition without a network and without affecting tests that run
/// concurrently under other homes.
#[cfg(test)]
pub(crate) static TEST_PROVIDERS: std::sync::Mutex<Vec<(String, Arc<dyn Provider + Send + Sync>)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
impl RequestSizer for ScriptedSizer {
    fn serialized_request_size(&self, request: &Request) -> Result<usize, ProviderError> {
        Ok(request.system_prompt.len()
            + request
                .messages
                .iter()
                .map(|message| message.text().len())
                .sum::<usize>())
    }
}

pub struct Runner {
    workflow_binding: Option<Arc<super::workflow_notifications::Binding>>,
    output_logs: Arc<crate::tool::bash_output::Logs>,
    authorization_run: Arc<Mutex<String>>,
    approvals: Option<(Arc<crate::approval::Service>, otto_core::approval::Scope)>,
    review_context: tokio::sync::Mutex<crate::approval::reviewer::ReviewContext>,
    pending_authorization: Mutex<Vec<String>>,
    agent: Agent<ProviderClient, Registry, SharedSession>,
    turn_timeout: Option<Duration>,
    unattended_turn_timeout: Option<Duration>,
    /// Set for `otto --prompt`: every turn is unattended.
    headless: bool,
    system_prompt: String,
    definitions: Vec<ToolDefinition>,
    usage: Option<crate::usage::Collector>,
    logger: Arc<crate::log::Logger>,
    /// The sub-agent task registry, absent when sub-agents are off. `/tasks`
    /// and `/task` read the active runner's task registry.
    pub(crate) tasks: Option<Arc<crate::subagent::tasks::Tasks>>,
    /// The child executor shared with durable workflows.
    pub(crate) subagents: Option<Arc<crate::subagent::runner::Runner>>,
    /// The timer registry, absent when the timer tools are not registered.
    /// `/timers` lists it and archiving the session clears it.
    pub(crate) reminders: Option<Arc<crate::tool::remind::Reminders>>,
    /// The skills discovered for this runner. `/skills` and `/skill` display this fixed catalog.
    pub(crate) skills: Catalog,
    pub(crate) discovered_skills: Catalog,
    /// Named child definitions used by durable workflow discovery.
    pub(crate) agents: crate::subagent::Catalog,
    /// The MCP servers connected for this runner. `/mcp` reads its status
    /// rows; `Runner::close` (best-effort, backgrounded) and
    /// `Runner::close_mcp` (awaited, bounded) shut its clients down.
    pub(crate) mcp: Arc<crate::mcp::Servers>,
    pub(crate) task_control: Arc<crate::app::task_control::TaskControl>,
    /// Adds advisory hints to memory candidates, absent when memory is off.
    /// Reflection reads it from the active runner.
    pub(crate) advisor: Option<Arc<crate::memory::advice::Advisor>>,
    /// The origin of the current user turn, read by the schedule tools.
    pub(crate) schedule_origin: Arc<crate::schedule::tools::TurnOrigin>,
    /// Exact passive-presentation origin for the current logical user turn.
    /// Unlike scheduling, a wake turn clears this instead of inheriting it.
    pub(crate) presentation_origin: Arc<crate::schedule::tools::TurnOrigin>,
    /// Where the controller binds the `reflect` tool's port. Present whether
    /// or not the tool is registered.
    pub(crate) reflection: Arc<crate::tool::reflect::Handle>,
}

/// The tool-less agent behind the session-model advice backend.
pub type AdviceAgent = Agent<ProviderClient, Registry, SharedSession>;

fn log_runtime_event(logger: &crate::log::Logger, session: &SharedSession, event: &Event) {
    match event {
        Event::ProviderApiCall {
            operation_id,
            kind,
            provider,
            model,
            duration,
            attempts,
            status,
            outcome,
        } => logger.info(
            "provider_operation_finished",
            &[
                ("operation_id", operation_id.to_string()),
                ("kind", kind.name().into()),
                ("provider", provider.clone()),
                ("model", model.clone()),
                ("attempts", attempts.to_string()),
                ("status", status.name().into()),
                (
                    "disposition",
                    crate::usage::disposition_name(outcome.disposition).into(),
                ),
                (
                    "effect_certainty",
                    crate::usage::effect_certainty_name(outcome.effect_certainty).into(),
                ),
                (
                    "stop_reason",
                    outcome
                        .stop_reason
                        .map(crate::usage::stop_reason_name)
                        .unwrap_or_default()
                        .into(),
                ),
                ("duration_ms", duration.as_millis().to_string()),
            ],
        ),
        // A no-op result has an empty checkpoint id, which can equal the id of
        // a remote checkpoint the provider returned without one.
        Event::CompactionCompleted { compaction } if !compaction.noop => {
            let Some(checkpoint) = session
                .remote_compaction()
                .filter(|checkpoint| checkpoint.id == compaction.checkpoint_id)
            else {
                return;
            };
            logger.info(
                "remote_compaction_finished",
                &[
                    ("checkpoint_id", checkpoint.id),
                    ("provider", checkpoint.provider),
                    ("reason", compaction.reason.name().into()),
                    ("automatic", compaction.automatic.to_string()),
                    (
                        "tail_messages",
                        session.post_remote_compaction_messages().len().to_string(),
                    ),
                    (
                        "tokens_before",
                        compaction
                            .tokens_before
                            .map(|value| value.to_string())
                            .unwrap_or_default(),
                    ),
                    ("usage_present", compaction.usage_present.to_string()),
                    (
                        "cached_input_tokens_present",
                        compaction.cached_input_tokens_present.to_string(),
                    ),
                ],
            );
        }
        _ => {}
    }
}

/// The step deadline: for an unattended turn the earlier of the two limits,
/// where `None` means no limit; otherwise `turn_timeout`.
fn turn_limit(
    turn_timeout: Option<Duration>,
    unattended_turn_timeout: Option<Duration>,
    unattended: bool,
) -> Option<Duration> {
    if !unattended {
        return turn_timeout;
    }
    match (turn_timeout, unattended_turn_timeout) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl Runner {
    /// A turn is unattended when this process is `otto --prompt` or the
    /// current user turn came from a chat.
    fn turn_deadline(&self) -> Deadline {
        let unattended = self.headless || self.schedule_origin.get().is_some_and(|o| o.is_chat());
        turn_limit(self.turn_timeout, self.unattended_turn_timeout, unattended)
            .map_or_else(Deadline::unlimited, Deadline::after)
    }

    /// Keep temporary output across all provider steps of one logical turn,
    /// including approval retries and queued follow-ups.
    pub(crate) fn retain_temporary_outputs(&self) -> crate::tool::bash_output::LogLease {
        self.output_logs.lease()
    }

    pub(crate) fn set_scheduled_authorization_run(&self, label: &str) {
        let mut run = self.authorization_run.lock().expect("authorization run");
        if !run.starts_with("schedule:") {
            *run = format!("schedule:{label}");
        }
    }
    pub(crate) async fn install_authorization_context(
        &self,
        mut context: crate::approval::reviewer::ReviewContext,
    ) -> Result<(), String> {
        let Some((service, scope)) = &self.approvals else {
            return Ok(());
        };
        if scope.session_id.is_empty() {
            return Ok(());
        }
        let mut current = self.review_context.lock().await;
        context.policy_digest = current.policy_digest.clone();
        context.refresh_digest();
        self.session()
            .append_custom(
                crate::approval::reviewer::CONTEXT_CUSTOM_TYPE,
                &serde_json::to_string(&context).map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())?;
        service
            .set_review_context(&scope.workspace, &scope.session_id, context.clone())
            .map_err(|error| error.to_string())?;
        *current = context;
        Ok(())
    }
    /// Captures only text admitted through a human input boundary, before any
    /// model/tool execution. Generated child, wake and scheduled prompts never call this.
    pub(crate) async fn record_user_authorization(&self, text: &str) -> Result<(), AgentError> {
        self.record_user_authorizations(&[text.to_owned()]).await
    }

    async fn record_user_authorizations(&self, texts: &[String]) -> Result<(), AgentError> {
        let Some((service, scope)) = &self.approvals else {
            return Ok(());
        };
        if scope.session_id.is_empty() {
            return Ok(());
        }
        let mut current = self.review_context.lock().await;
        let mut context = current.clone();
        if context
            .instructions
            .iter()
            .any(|instruction| instruction == crate::approval::reviewer::INCOMPLETE_EVIDENCE)
        {
            context.trusted_user_messages.clear();
            context.instructions.retain(|instruction| {
                instruction != crate::approval::reviewer::INCOMPLETE_EVIDENCE
            });
            context
                .instructions
                .push(crate::approval::reviewer::FRESH_EVIDENCE_ONLY.into());
        }
        context
            .trusted_user_messages
            .extend(texts.iter().map(|text| self.redact_text(text)));
        // Never select an arbitrary excerpt which could drop a prohibition.
        if serde_json::to_string(&context)
            .map_err(|error| AgentError::Other(error.to_string()))?
            .len()
            > crate::approval::reviewer::MAX_INPUT_BYTES
        {
            context.trusted_user_messages.clear();
            context.instructions.clear();
            context
                .instructions
                .push(crate::approval::reviewer::INCOMPLETE_EVIDENCE.into());
        }
        context.refresh_digest();
        let unavailable = crate::approval::reviewer::ReviewContext {
            evidence_digest: context.evidence_digest.clone(),
            policy_digest: "authorization-persistence-pending".into(),
            ..Default::default()
        };
        service
            .set_review_context(&scope.workspace, &scope.session_id, unavailable)
            .map_err(|error| AgentError::Other(error.to_string()))?;
        self.session()
            .append_custom(
                crate::approval::reviewer::CONTEXT_CUSTOM_TYPE,
                &serde_json::to_string(&context)
                    .map_err(|error| AgentError::Other(error.to_string()))?,
            )
            .await
            .map_err(|error| AgentError::Other(error.to_string()))?;
        {
            let pending = self
                .pending_authorization
                .lock()
                .expect("pending authorization");
            if pending.is_empty() {
                service
                    .set_review_context(&scope.workspace, &scope.session_id, context.clone())
                    .map_err(|error| AgentError::Other(error.to_string()))?;
            }
        }
        *current = context;
        Ok(())
    }

    /// Sync admission invalidates a review immediately. The monitor persists
    /// the original input before installing an assessable replacement context.
    pub(crate) fn queue_user_authorization(&self, text: &str) -> Result<(), String> {
        let Some((service, scope)) = &self.approvals else {
            return Ok(());
        };
        if scope.session_id.is_empty() {
            return Ok(());
        }
        let mut pending = self
            .pending_authorization
            .lock()
            .expect("pending authorization");
        pending.push(self.redact_text(text));
        let unavailable = crate::approval::reviewer::ReviewContext {
            evidence_digest: crate::approval::digest(&pending.join("\n")),
            policy_digest: "authorization-input-pending".into(),
            ..Default::default()
        };
        service
            .set_review_context(&scope.workspace, &scope.session_id, unavailable)
            .map_err(|error| error.to_string())
    }

    async fn flush_user_authorization(&self) -> Result<(), AgentError> {
        let pending = std::mem::take(
            &mut *self
                .pending_authorization
                .lock()
                .expect("pending authorization"),
        );
        if pending.is_empty() {
            Ok(())
        } else {
            self.record_user_authorizations(&pending).await
        }
    }
    pub async fn end_session_hooks(self: &Arc<Self>) {
        if self.agent.claim_end_hooks() {
            let runner = self.clone();
            tokio::spawn(async move {
                runner.agent.end_hooks().await;
            });
        }
        self.agent.wait_end_hooks().await;
    }
    /// Runs one turn.
    pub async fn run(
        &self,
        user_text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.run_with_system_context(user_text, "", emit, cancel)
            .await
    }

    /// Runs one turn with ephemeral system context that is not persisted.
    pub async fn run_with_system_context(
        &self,
        user_text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _output_lease = self.output_logs.lease();
        let control = Control::new(self.turn_deadline());
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let mut send = |event| {
            let _ = events.send(event);
        };
        let mut collecting = self.collecting(&mut send);
        let run = self.agent.run_with_system_context_control(
            user_text,
            system_context,
            &mut collecting,
            &control,
        );
        let result = self
            .monitor_approvals(
                drive_with_control(run, cancel, &control),
                received,
                &mut *emit,
            )
            .await;
        self.report_lost_steering(&mut *emit).await;
        result
    }

    /// Runs one model-originated follow-up as display-only context.
    pub async fn run_followup_with_system_context(
        &self,
        text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _output_lease = self.output_logs.lease();
        let control = Control::new(self.turn_deadline());
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let mut send = |event| {
            let _ = events.send(event);
        };
        let mut collecting = self.collecting(&mut send);
        let run = self.agent.run_followup_with_system_context_control(
            text,
            system_context,
            &mut collecting,
            &control,
        );
        let result = self
            .monitor_approvals(
                drive_with_control(run, cancel, &control),
                received,
                &mut *emit,
            )
            .await;
        self.report_lost_steering(&mut *emit).await;
        result
    }

    pub async fn run_with_image_system_context(
        &self,
        user_text: &str,
        image: Block,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _output_lease = self.output_logs.lease();
        let control = Control::new(self.turn_deadline());
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let mut send = |event| {
            let _ = events.send(event);
        };
        let mut collecting = self.collecting(&mut send);
        let run = self.agent.run_with_image_system_context_control(
            user_text,
            Some(image),
            system_context,
            &mut collecting,
            &control,
        );
        let result = self
            .monitor_approvals(
                drive_with_control(run, cancel, &control),
                received,
                &mut *emit,
            )
            .await;
        self.report_lost_steering(&mut *emit).await;
        result
    }

    /// Reports the guidance a finished run accepted but never delivered.
    ///
    /// An interrupted turn — the user pressed Esc, the provider failed, the
    /// deadline expired — drops guidance it never reached a safe boundary
    /// for. The user typed those words and must learn what became of them, so
    /// each loss is persisted as a visible `agent_steering` context message
    /// and announced on the turn's own event stream. A failed append still
    /// announces the text, with the failure appended: losing the user's input
    /// silently is exactly what this prevents.
    async fn report_lost_steering(&self, emit: EventSink<'_>) {
        let lost = self.steering().take_discarded();
        if lost.is_empty() {
            return;
        }
        let mut text = lost_steering_notice(&lost);
        if let Err(error) = self
            .session()
            .append(Message {
                role: otto_core::model::Role::Context,
                blocks: vec![Block::text(&text)],
                created_at: Utc::now(),
                context_type: "agent_steering".into(),
                display: true,
                ..Message::default()
            })
            .await
        {
            text.push_str(&format!("\n\n(It could not be saved: {error})"));
        }
        emit(Event::Notification {
            kind: Some(NotificationKind::TaskReport),
            task_id: "agent_steering".into(),
            text,
            usage: Usage::default(),
            present: false,
            cached_input_tokens_present: false,
        });
    }

    async fn monitor_approvals<T>(
        &self,
        future: impl Future<Output = T>,
        mut events: tokio::sync::mpsc::UnboundedReceiver<otto_core::agent::Event>,
        emit: EventSink<'_>,
    ) -> T {
        {
            let mut run = self.authorization_run.lock().expect("authorization run");
            if !run.starts_with("schedule:") {
                *run = self
                    .task_control()
                    .authorization_scope()
                    .unwrap_or_else(crate::runtime::sidecar_mailbox::new_input_id);
            }
        }
        tokio::pin!(future);
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut cursor = self
            .approvals
            .as_ref()
            .and_then(|(service, scope)| {
                service
                    .snapshot(&scope.workspace, Some(&scope.session_id))
                    .ok()
            })
            .map_or(0, |s| s.cursor);
        loop {
            tokio::select! {
                result=&mut future=>{
                    let _ = self.flush_user_authorization().await;
                    while let Ok(event)=events.try_recv(){emit(event);}
                    if let Some((service,scope))=&self.approvals {
                        while let Ok(changes)=service.events(&scope.workspace,Some(&scope.session_id),cursor) {
                            if changes.is_empty() {break;}
                            for change in changes {cursor=change.sequence;emit(otto_core::agent::Event::ApprovalChanged{request:Box::new(change.request)});}
                        }
                    }
                    return result
                },
                Some(event)=events.recv()=>emit(event),
                _=tick.tick(),if self.approvals.is_some()=>{
                    let _ = self.flush_user_authorization().await;
                    while let Ok(event)=events.try_recv(){emit(event);}
                    if let Some((service,scope))=&self.approvals && let Ok(changes)=service.events(&scope.workspace,Some(&scope.session_id),cursor) {
                        for change in changes {cursor=change.sequence;emit(otto_core::agent::Event::ApprovalChanged{request:Box::new(change.request)});}
                    }
                }
            }
        }
    }

    /// Compacts the transcript.
    pub async fn compact(
        &self,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, AgentError> {
        let control = Control::new(
            self.turn_timeout
                .map_or_else(Deadline::unlimited, Deadline::after),
        );
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let mut emit = self.collecting(emit);
        let compact = self.agent.compact_with_control(focus, &mut emit, &control);
        drive_with_control(compact, cancel, &control).await
    }

    /// Asks the model once with no tools, outside the turn loop, under the
    /// turn timeout and `cancel`. The provider usage is recorded against
    /// `task_id` so reflection cost is separable from turn cost.
    pub async fn complete_text(
        &self,
        request: &otto_core::agent::oneshot::TextRequest<'_>,
        task_id: &str,
        cancel: &CancellationToken,
    ) -> Result<otto_core::agent::oneshot::TextResponse, AgentError> {
        let control = Control::new(
            self.turn_timeout
                .map_or_else(Deadline::unlimited, Deadline::after),
        );
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let collector = self.usage.as_ref().map(|usage| usage.for_task(task_id));
        // The one-shot call emits its provider operation record, including
        // for a failed call, so operation and attempt counts include it.
        let mut emit = |event: otto_core::agent::Event| {
            if let Some(collector) = &collector {
                let _ = collector.record(&event);
            }
        };
        let call = self.agent.complete_text(request, &mut emit, &control);
        let response = drive_with_control(call, cancel, &control).await?;
        if let Some(collector) = &collector {
            let _ = collector.record(&otto_core::agent::Event::ProviderUsage {
                usage: response.usage,
                present: response.usage_present,
                cached_input_tokens_present: response.cached_input_tokens_present,
            });
        }
        Ok(response)
    }

    /// A bounded approval-only dialogue. It has an isolated memory transcript,
    /// the existing provider and usage collector, and no effectful task tools.
    pub async fn approval_dialogue(
        &self,
        text: &str,
        tools: Vec<Box<dyn crate::tool::Tool + Send + Sync>>,
        runtime: &Runtime,
        system_prompt: String,
        redaction_values: &[String],
        cancel: &CancellationToken,
    ) -> Result<String, AgentError> {
        let registry = Registry::new(tools).map_err(AgentError::Other)?;
        let options = Options {
            diagnostics: self.agent.diagnostics().cloned(),
            model: runtime.model.clone(),
            provider_name: runtime.provider.clone(),
            thinking: runtime.thinking.clone(),
            system_prompt,
            now: Box::new(Utc::now),
            new_operation_id: Box::new(new_operation_id),
            ..Options::default()
        };
        let agent = Agent::with_redactor(
            self.provider().clone(),
            registry,
            SharedSession::memory(self.session().header()),
            options,
            Redactor::new(redaction_values),
        );
        let control = Control::new(Deadline::after(Duration::from_secs(30)));
        let mut discard = |_| {};
        let mut sink = self.collecting(&mut discard);
        drive_with_control(
            agent.run_with_control(&self.redact_text(text), &mut sink, &control),
            cancel,
            &control,
        )
        .await?;
        Ok(agent
            .session()
            .messages()
            .iter()
            .rev()
            .find(|message| message.role == otto_core::model::Role::Assistant)
            .map_or_else(String::new, |message| self.redact_text(&message.text())))
    }

    /// Redacts transcript text exactly as a provider request would be.
    pub fn redact_text(&self, text: &str) -> String {
        self.agent.redact_text(text)
    }

    /// Whether transcript-derived text may be sent to the provider at all.
    pub fn allows_dynamic_content(&self) -> bool {
        self.agent.allows_dynamic_content()
    }

    fn collecting<'a>(
        &'a self,
        emit: EventSink<'a>,
    ) -> impl FnMut(otto_core::agent::Event) + Send + use<'a> {
        move |event| {
            log_runtime_event(&self.logger, self.session(), &event);
            if let Some(usage) = &self.usage {
                let _ = usage.record(&event);
            }
            emit(event);
        }
    }

    pub fn session(&self) -> &SharedSession {
        self.agent.session()
    }

    pub fn provider(&self) -> &ProviderClient {
        self.agent.provider()
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Records the client's input id for the next run's user message.
    pub fn set_next_input_id(&self, input_id: Option<String>) {
        self.agent.set_next_input_id(input_id);
    }

    pub(crate) fn task_control(&self) -> Arc<crate::app::task_control::TaskControl> {
        Arc::clone(&self.task_control)
    }

    pub(crate) fn steering(&self) -> Arc<otto_core::agent::steering::Steering> {
        self.task_control.steering()
    }

    /// What the next provider request contains.
    pub fn context_report(&self) -> otto_core::agent::context_report::ContextReport {
        self.agent.context_report()
    }

    /// The session mailbox shared with notification producers.
    pub fn mailbox(&self) -> &Arc<dyn otto_core::mailbox::Mailbox> {
        self.agent.mailbox()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    pub fn skills(&self) -> &Catalog {
        &self.skills
    }

    pub fn agents(&self) -> &crate::subagent::Catalog {
        &self.agents
    }

    pub fn subagents(&self) -> Option<Arc<crate::subagent::runner::Runner>> {
        self.subagents.clone()
    }

    /// Releases the agent's own resources and best-effort starts MCP
    /// shutdown in the background. The session is closed separately, by
    /// whoever owns it. A running Tokio runtime spawns
    /// [`crate::mcp::Servers::close`] in the background (closes stdio
    /// children, drops HTTP connections); its absence (a scripted test
    /// runner, or a caller outside `#[tokio::main]`) just skips it, since
    /// there is nothing to release for `Servers::default()` and no runtime to
    /// block on for a real one.
    ///
    /// This is enough for a runner displaced by `/new`, `/load`, or
    /// `/model`: nothing downstream needs its MCP servers gone by any
    /// particular deadline. It is not enough at process exit, where a
    /// dropped runtime can cancel the spawned task before it runs; call
    /// [`Self::close_mcp`] there too. Both paths await the same detached MCP
    /// cleanup owner, so cancellation or timeout of either waiter cannot lose
    /// the clients being closed.
    pub fn close(&self) {
        if let Some(binding) = &self.workflow_binding {
            binding.stop();
            let binding = binding.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    binding.join().await;
                });
            }
        }
        let _ = self.agent.close();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let mcp = Arc::clone(&self.mcp);
            handle.spawn(async move { mcp.close().await });
        }
    }

    /// Shuts every connected MCP server down, waiting up to 5 seconds.
    /// Idempotent with [`Self::close`]'s background close: all callers wait
    /// for the same detached cleanup owner. If this caller times out, cleanup
    /// continues and a later caller can await the same completion. There is
    /// nothing to release for `Servers::default()`, so this returns
    /// immediately for a runner built without MCP servers.
    pub async fn close_mcp(&self) {
        if let Some(binding) = &self.workflow_binding {
            binding.join().await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.mcp.close()).await;
    }

    /// Runs during a SIGTERM migration, after this session's turn was
    /// already cancelled: cancels every non-final sub-agent task, waits for
    /// each to reach a final status, and leaves one notification describing
    /// what moved (see [`crate::failover::recovery::notify_moved`]).
    /// Returns no warnings and does nothing when sub-agents are off
    /// (`self.tasks` is `None`) or the session has no persisted path
    /// (`--no-session`, so there is no children directory to scan).
    pub async fn migrate(&self) -> Vec<String> {
        let Some(tasks) = self.tasks.as_ref() else {
            return Vec::new();
        };
        let ids = tasks.begin_migration();
        tasks.wait_final(&ids).await;

        let path = self.session().path();
        if path.is_empty() {
            return Vec::new();
        }
        let children_dir = Path::new(&path).with_extension("");
        let max_output_bytes = self
            .subagents
            .as_ref()
            .map(|subagents| subagents.max_output_bytes())
            .unwrap_or(0);
        crate::failover::recovery::notify_moved(
            tasks.notifications().as_ref(),
            &children_dir,
            &ids,
            max_output_bytes,
            &self.session().messages(),
        )
    }

    /// A runner with no tools whose provider the test supplies, carrying the
    /// sub-agent registry the caller owns. See [`ProviderClient::Scripted`].
    ///
    /// `tasks` is wired to both [`Options::tasks`] and [`Options::inbox`], so
    /// an empty-text wake turn reaches the provider instead of being rejected
    /// as empty user text.
    #[cfg(test)]
    pub fn scripted(
        session: SharedSession,
        provider: Arc<dyn Provider + Send + Sync>,
        tasks: Arc<crate::subagent::tasks::Tasks>,
    ) -> Self {
        Self::scripted_with_tools(session, provider, tasks, Vec::new())
    }

    /// [`Self::scripted`] with `tools` registered, for a test that needs the
    /// provider's tool call to reach a tool double.
    #[cfg(test)]
    pub fn scripted_with_tools(
        session: SharedSession,
        provider: Arc<dyn Provider + Send + Sync>,
        tasks: Arc<crate::subagent::tasks::Tasks>,
        tools: Vec<Box<dyn crate::tool::Tool + Send + Sync>>,
    ) -> Self {
        let registry = Registry::new(tools).expect("test tool registry");
        let definitions = registry.definitions();
        let task_control = Arc::new(crate::app::task_control::TaskControl::default());
        let inbox = tasks.notifications().clone();
        let mailbox: Arc<dyn otto_core::mailbox::Mailbox> = inbox.clone();
        Self {
            workflow_binding: None,
            output_logs: Arc::default(),
            agent: Agent::new(
                ProviderClient::Scripted(provider),
                registry,
                session,
                Options {
                    model: "test-model".to_string(),
                    provider_name: "openai-compatible".to_string(),
                    request_sizer: Some(Arc::new(ScriptedSizer)),
                    now: Box::new(Utc::now),
                    mailbox: Arc::clone(&mailbox),
                    steering: task_control.steering(),
                    tasks: Some(Arc::clone(&tasks)
                        as Arc<dyn otto_core::agent::tasks::TaskRegistry + Send + Sync>),
                    ..Options::default()
                },
            ),
            approvals: None,
            authorization_run: Arc::default(),
            review_context: tokio::sync::Mutex::new(Default::default()),
            pending_authorization: Mutex::new(Vec::new()),
            turn_timeout: None,
            unattended_turn_timeout: None,
            headless: false,
            system_prompt: String::new(),
            definitions,
            usage: None,
            logger: Arc::new(crate::log::Logger::discard()),
            reminders: Some(Arc::new(crate::tool::remind::Reminders::new(inbox))),
            tasks: Some(tasks),
            subagents: None,
            skills: Catalog::default(),
            discovered_skills: Catalog::default(),
            agents: crate::subagent::Catalog::default(),
            mcp: Arc::new(crate::mcp::Servers::default()),
            task_control,
            advisor: None,
            schedule_origin: Default::default(),
            presentation_origin: Default::default(),
            reflection: Default::default(),
        }
    }

    /// Gives a [`Self::scripted`] runner an advisor.
    #[cfg(test)]
    pub fn with_advisor(mut self, advisor: Arc<crate::memory::advice::Advisor>) -> Self {
        self.advisor = Some(advisor);
        self
    }

    /// Replaces the MCP servers a [`Self::scripted`] runner was built with.
    /// Test-only: lets a close-ordering test inject a [`crate::mcp::Servers`]
    /// with a fake connected client instead of the empty default.
    #[cfg(test)]
    pub fn with_mcp(mut self, mcp: Arc<crate::mcp::Servers>) -> Self {
        self.mcp = mcp;
        self
    }
}

/// Everything the composition root builds once per process, independent of
/// any one workspace: config, environment, provider-runtime inputs, auth,
/// usage, task recording, the skill checker, and the session root. `Builder`
/// holds one of these behind an `Arc` and adds what one workspace needs.
///
/// See `docs/specs/2026-09-26-serve-multiple-workspaces.md` ("Server
/// structure") for why the split exists: a server process that later loads
/// more than one workspace builds this once and calls
/// [`Builder::for_workspace`] once per workspace.
#[derive(Clone)]
pub struct Shared {
    pub storage: Arc<super::storage::Storage>,
    pub approvals: Arc<crate::approval::Service>,
    pub config_path: PathBuf,
    pub config: File,
    pub environment: HashMap<String, String>,
    /// The resolved home directory, used to locate MCP OAuth token files
    /// (`crate::mcp::oauth::token_path`).
    pub home: String,
    pub session_root: PathBuf,
    pub shell: String,
    pub no_session: bool,
    /// Whether this process is `otto --prompt`; its runners treat every turn
    /// as unattended.
    pub headless: bool,
    pub overrides: Overrides,
    /// The host environment as captured at startup, byte for byte. Loading a
    /// second workspace's sandbox reuses this rather than re-reading the
    /// process environment, so every loaded workspace classifies the same
    /// snapshot.
    pub host_entries: Vec<Vec<u8>>,
    /// The redaction boundary's secret set before the sandbox for any one
    /// workspace opens and may add its own. `Builder::for_workspace` seeds
    /// the per-workspace `sandbox_secrets` from this.
    ///
    /// This baseline is captured once, from the startup workspace's own
    /// sandbox, and reused as the seed for every workspace loaded later. A
    /// workspace loaded at runtime does not get its own baseline recomputed
    /// from its own sandbox.
    pub sandbox_secrets_baseline: Vec<String>,
    pub sandbox_secrets_baseline_complete: bool,
    /// `--sandbox`, when the flag was passed. Process-wide: every workspace's
    /// `resolve_sandbox_settings` call uses this same override, so `otto
    /// serve --sandbox off` applies to a workspace loaded after startup too.
    pub sandbox_driver_override: Option<String>,
    /// Whether `--config` named the config file explicitly, vs. the default
    /// path. Each workspace's `SandboxReloader` re-reads this same file on
    /// `/sandbox reload`, so the flag is process-wide rather than per-workspace.
    pub explicit_config: bool,
    /// The captured `~/.otto/auth/chatgpt.json`.
    pub auth_path: String,
    /// The captured credentials, valid only when loaded is true.
    pub auth_credentials: crate::auth::Credentials,
    pub auth_credentials_loaded: bool,
    /// Process-wide append-only token usage storage. `None` keeps usage
    /// collection from affecting an otherwise usable runtime.
    pub usage: Option<Arc<dyn crate::usage::repository::UsageRepository>>,
    /// Process-wide sub-agent task recorder (`~/.otto/tasks.db`). `None` keeps
    /// `build_subagents` from recording task history, matching today's
    /// behaviour; `build_subagents` injects it into every `Tasks` registry it
    /// builds.
    pub task_recorder: Option<Arc<dyn crate::subagent::record::TaskRepository>>,
    /// The scheduled-delivery store (`~/.otto/schedule.db`). `None` when it
    /// could not be opened; the schedule tools then report it as unavailable.
    pub schedule: Option<Arc<dyn crate::schedule::ScheduleStore>>,
    /// The automatic skill contract check. `None` when the feature is disabled
    /// or its database could not be opened; either way
    /// `build_subagents` wires no checker in and delegation is unaffected.
    pub skill_checker: Option<Arc<crate::skill::check::Checker>>,
    /// The process-wide memory service, its user scope, and the recall
    /// limits: one file store for the whole process, shared by every
    /// loaded workspace. Each workspace's `Builder` keeps only its own
    /// `workspace_scope`.
    pub memory: super::wiring::MemoryWiring,
    /// The reflection use case: resolved `[reflection]` settings and, when
    /// enabled, the `~/.otto/reflection.db` store.
    pub reflector: Arc<crate::reflection::Reflector>,
    /// The process log, built once at the composition root. Interactive
    /// frontends get `Logger::discard()` because stderr would overwrite the
    /// screen.
    pub logger: Arc<crate::log::Logger>,
    /// Frontend-owned parent tools, installed only after the frontend has
    /// assembled any capabilities they need. Every workspace builder for this
    /// process shares this slot.
    pub parent_tool_factory: ParentToolSlot,
}

/// Context supplied to a frontend-owned parent-tool factory.
///
/// The factory receives identity and the append-only source transcript, not a
/// workspace handle or command executor. This lets a delivery service record a
/// text-only completion notice without granting it a second live filesystem
/// root or arbitrary source-session control.
#[derive(Clone)]
pub struct ParentToolContext {
    pub workspace: String,
    pub session_id: String,
    pub source_session: SharedSession,
}

/// Adds frontend-owned tools to parent runners only.
///
/// `runtime` defines this narrow seam but never implements it with CLI
/// dependencies. Child runners are composed from a separate tool list and do
/// not call this factory. Factories must return a fresh tool instance for each
/// runner build because controllers rebuild runners after session/profile/
/// thinking replacement.
pub trait ParentToolFactory: Send + Sync {
    fn tools(
        &self,
        context: &ParentToolContext,
    ) -> Result<Vec<Box<dyn Tool + Send + Sync>>, String>;
}

/// A process-shared, post-construction installation point for frontend-owned
/// parent tools. The slot is deliberately absent from child composition.
#[derive(Clone, Default)]
pub struct ParentToolSlot(Arc<std::sync::RwLock<Option<Arc<dyn ParentToolFactory>>>>);

impl ParentToolSlot {
    /// Installs the frontend's parent tools. The first installation wins: one
    /// process has one frontend, and a second would silently replace the tools
    /// every later parent runner composes. Returns whether this call installed.
    pub fn install(&self, factory: Arc<dyn ParentToolFactory>) -> bool {
        let mut slot = self.0.write().unwrap_or_else(|poison| poison.into_inner());
        if slot.is_some() {
            return false;
        }
        *slot = Some(factory);
        true
    }

    /// Whether a factory is installed. Test-only: production installs once and
    /// relies on the first-install-wins rule above.
    #[cfg(test)]
    pub(crate) fn installed(&self) -> bool {
        self.0
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_some()
    }

    fn get(&self) -> Option<Arc<dyn ParentToolFactory>> {
        self.0
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

/// The composition root for one workspace.
///
/// Optional seams: memory, skills, sub-agents, ChatGPT credentials, and the
/// `OTTO_TRACE` writer. None of them changes the order below; each only appends
/// tools or wraps the client.
pub struct Builder {
    /// One controller/lock for every parent runner in this workspace.
    pub(crate) workflows: tokio::sync::OnceCell<Result<Arc<crate::workflow::Controller>, String>>,
    /// Everything built once per process. Field access on `Builder` reaches
    /// these through `Deref`, so `self.config`, `self.environment`, and the
    /// rest of `Shared`'s fields read the same as before the split.
    pub shared: Arc<Shared>,
    /// Leaked for the process lifetime so the file tools, which borrow it,
    /// satisfy the registry's `'static` bound. See `leaked_workspace`.
    pub workspace: &'static Workspace,
    pub workspace_path: String,
    pub command_executor: Option<Arc<dyn CommandExecutor>>,
    pub bash_approvals: Option<Arc<bash::BashApprovals>>,
    pub grant_control: Option<Arc<dyn crate::app::SandboxControl>>,
    pub excluded_commands: Option<Arc<bash::ExcludedCommands>>,
    pub sandbox_environment: Option<Vec<String>>,
    pub sandbox_info: SandboxInfo,
    pub sandbox_secrets: Vec<String>,
    pub sandbox_secrets_complete: bool,
    /// This workspace's memory scope: its identity marker's adopted scope,
    /// or the legacy path/configured id (`workspace_memory_scope`). The service
    /// itself and the user scope are process-wide and live on `Shared`.
    pub workspace_scope: crate::memory::Scope,
    /// Host grants and the HTTP client of the `web_fetch` and `web_search`
    /// tools, shared by every runner built for this workspace.
    pub web: Arc<crate::tool::web::Runtime>,
    /// The resolved `[mcp]` configuration. `connect_mcp` reads this at
    /// `build_runner` time; the servers themselves are not connected until
    /// then.
    pub mcp: McpRuntime,
}

impl std::ops::Deref for Builder {
    type Target = Shared;

    fn deref(&self) -> &Shared {
        &self.shared
    }
}

impl Builder {
    /// Assembles the workspace-scoped half of the composition root over an
    /// already-built [`Shared`]. `cli::run` calls this once per process, for
    /// the TUI, REPL, and serve alike.
    ///
    /// ponytail: `workspace` and `mcp` are passed in already resolved rather
    /// than resolved here, because `cli::run` resolves them at specific
    /// points in its startup sequence to keep today's error-reporting order
    /// (the workspace lease happens before the archive/resume paths that
    /// never use it; mcp resolution happens before the usage and
    /// task-recorder stores open). A later slice that loads a second
    /// workspace at runtime has no such fixed sequence to preserve and
    /// should have `for_workspace` call `leaked_workspace` and `resolve_mcp`
    /// itself.
    pub fn for_workspace(
        shared: Arc<Shared>,
        workspace: &'static Workspace,
        workspace_path: String,
        mcp: McpRuntime,
    ) -> Builder {
        let sandbox_secrets = shared.sandbox_secrets_baseline.clone();
        let sandbox_secrets_complete = shared.sandbox_secrets_baseline_complete;
        let web = crate::tool::web::Runtime::new();
        Builder {
            workflows: tokio::sync::OnceCell::new(),
            shared,
            web,
            workspace,
            workspace_path,
            command_executor: None,
            bash_approvals: None,
            grant_control: None,
            excluded_commands: None,
            sandbox_environment: None,
            sandbox_info: SandboxInfo::default(),
            sandbox_secrets,
            sandbox_secrets_complete,
            workspace_scope: Default::default(),
            mcp,
        }
    }

    /// Mutable access to this `Builder`'s `Shared`, valid only while it is
    /// still uniquely owned. Startup uses this to fill in the memory
    /// service, its user scope, and the recall limits once they are resolved
    /// (`cli::run`), before any second workspace clones the `Arc`; test
    /// fixtures use it the same way, to tweak a field on a freshly built
    /// `Shared` before use. Once a workspace load clones `shared`, this
    /// panics rather than letting one `Builder` silently rewrite state every
    /// other loaded workspace already reads.
    pub(crate) fn shared_mut(&mut self) -> &mut Shared {
        Arc::get_mut(&mut self.shared).expect("shared: not uniquely owned")
    }

    /// The `lease_seconds` to pass as `create_lease` to
    /// `Prepared::prepare`/`prepare_listed` when opening a session for this
    /// workspace, resolved from `[failover]`: `Some(lease_seconds)` when
    /// enabled, `None` otherwise. `[failover]` is not validated at startup
    /// (unlike most tables it has no per-turn resolution path to piggyback
    /// on), so an invalid `lease_seconds` is reported here, at the point a
    /// session is opened, rather than at process start.
    pub fn create_lease(&self) -> Result<Option<u64>, String> {
        let runtime =
            otto_core::config::resolve_failover(&self.config).map_err(|error| error.to_string())?;
        Ok(runtime.enabled.then_some(runtime.lease_seconds))
    }
}

impl Builder {
    pub fn usage_summary(&self, session_id: Option<&str>) -> Result<crate::usage::Summary, String> {
        match &self.usage {
            Some(store) => store.summary(session_id).map_err(|error| error.to_string()),
            None => Ok(crate::usage::Summary::default()),
        }
    }

    pub fn usage_analysis(
        &self,
        days: u16,
        session_id: Option<&str>,
    ) -> Result<crate::usage::Analysis, String> {
        match &self.usage {
            Some(store) => store
                .daily(days, session_id)
                .map_err(|error| error.to_string()),
            None => crate::usage::Analysis::empty(days).map_err(|error| error.to_string()),
        }
    }

    pub fn tasks_list(
        &self,
        query: &crate::subagent::record::ListQuery,
    ) -> Result<crate::subagent::record::ListResult, String> {
        match &self.task_recorder {
            Some(store) => store.list(query).map_err(|error| error.to_string()),
            None => Ok(crate::subagent::record::ListResult::default()),
        }
    }

    pub fn tasks_get(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Option<crate::subagent::record::TaskRow>, String> {
        match &self.task_recorder {
            Some(store) => store
                .get(parent_session, task_id)
                .map_err(|error| error.to_string()),
            None => Ok(None),
        }
    }

    pub(crate) fn usage_collector(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
    ) -> Option<crate::usage::Collector> {
        self.usage.as_ref().map(|store| {
            let header = session.header();
            crate::usage::Collector::from_sink(
                Arc::clone(store) as Arc<dyn crate::usage::Sink>,
                crate::usage::Context {
                    workspace: header.workspace,
                    session_id: header.id,
                    provider: runtime.provider.clone(),
                    profile: runtime.profile.clone(),
                    model: runtime.model.clone(),
                    task_id: String::new(),
                },
            )
        })
    }

    pub fn boundary_inputs(&self) -> BoundaryInputs<'_> {
        BoundaryInputs {
            sandbox_secrets: &self.sandbox_secrets,
            sandbox_secrets_complete: self.sandbox_secrets_complete,
            config: &self.config,
            environment: &self.environment,
            overrides_base_url: &self.overrides.base_url,
        }
    }

    /// The definitions the boundary check must leave unchanged.
    ///
    /// The built-ins, bash when it is planned, the memory tools when memory is
    /// usable, and the skill and sub-agent definitions `build_catalogs` would
    /// register.
    fn boundary_tool_definitions(&self, runtime: Option<&Runtime>) -> Vec<ToolDefinition> {
        let max_output = match runtime {
            Some(runtime) if runtime.max_output_bytes > 0 => output_cap(runtime.max_output_bytes),
            _ => 1,
        };
        let mut definitions: Vec<ToolDefinition> = self
            .builtin_file_tools(max_output)
            .iter()
            .chain(self.web_tools(max_output, &[], runtime).iter())
            .map(|tool| tool.definition())
            .collect();
        definitions.push(verification::verification_definition());
        definitions.extend(self.boundary_memory_definitions(max_output));
        definitions.push(crate::tool::approval::request_definition());
        if self.planned_bash_available() {
            let private_home = self.sandbox_info.mode == SandboxMode::Seatbelt;
            definitions.push(match self.bash_approvals.is_some() {
                true => bash::bash_definition_with_approvals(private_home),
                false => bash::bash_definition(private_home),
            });
        }
        if runtime.is_some() {
            definitions.push(models::list_models_definition());
        }
        let dynamic =
            boundary::secret_redactor(&self.boundary_inputs(), runtime).allows_dynamic_content();
        definitions.extend(self.boundary_catalog_definitions(max_output, dynamic));
        definitions
    }

    /// The redactor for this run, closed when any fixed text would change.
    fn boundary_redactor(&self, runtime: Option<&Runtime>) -> Redactor {
        let definitions = self.boundary_tool_definitions(runtime);
        let (provider, model) = match runtime {
            Some(runtime) => (runtime.provider.as_str(), runtime.model.as_str()),
            None => ("", ""),
        };
        let endpoint_host = runtime
            .map(|runtime| boundary::endpoint_host_for(&runtime.base_url))
            .unwrap_or_default();
        let mut prompt = system_prompt_for(
            &definitions,
            self.planned_sandbox_info(),
            provider,
            &endpoint_host,
            model,
        );
        if definitions
            .iter()
            .any(|definition| definition.name == "agent")
            && let Some(runtime) = runtime
        {
            prompt.push_str(&subagent_model_guidance(&runtime.subagent_models));
            if let Some(diagnostic) =
                otto_core::config::resolve::subagent_model_diagnostic(&self.config, runtime)
            {
                prompt.push_str(&format!(
                    "\n<subagent_model_diagnostic>\n{}\n</subagent_model_diagnostic>\n",
                    diagnostic.reason
                ));
            }
        }
        boundary::boundary_redactor(
            &self.boundary_inputs(),
            runtime,
            &FixedText {
                workspace_path: &self.workspace_path,
                definitions: Some(&definitions),
                system_prompt: Some(&prompt),
            },
        )
    }

    fn review_instruction_snapshot(
        &self,
        workspace_instructions: &str,
        redactor: &Redactor,
    ) -> Vec<String> {
        let mut instructions = vec![redactor.redact_string(workspace_instructions)];
        if self.memory.extension.is_none() && self.config.memory.enabled != Some(false) {
            match self
                .memory
                .service
                .standing_records(&self.memory.user_scope)
            {
                Ok(records) => {
                    instructions.extend(records.into_iter().map(|record| {
                        serde_json::json!({
                            "constraint_only": true, "source": "accepted_standing_user_memory",
                            "id": record.id, "revision": record.revision, "kind": record.kind,
                            "key": record.key, "text": redactor.redact_string(&record.text),
                        })
                        .to_string()
                    }));
                }
                Err(error) if error.kind == crate::memory::ErrorKind::Disabled => {}
                Err(_) => instructions
                    .push(crate::approval::reviewer::STANDING_CONSTRAINTS_UNAVAILABLE.into()),
            }
        }
        instructions
    }

    pub fn boundary_allows_dynamic(&self, runtime: Option<&Runtime>) -> bool {
        self.boundary_redactor(runtime).allows_dynamic_content()
    }

    /// Every secret form the run must hide.
    pub fn secret_values(&self, runtime: Option<&Runtime>) -> Vec<String> {
        boundary::boundary_secret_values(&self.boundary_inputs(), runtime).0
    }

    /// Whether a bash tool will actually be built.
    pub fn bash_configured(&self) -> bool {
        self.boundary_allows_dynamic(None) && self.planned_bash_available()
    }

    fn planned_bash_available(&self) -> bool {
        self.sandbox_info.bash_available
            && self.sandbox_environment.is_some()
            && self.command_executor.is_some()
    }

    /// A sandbox that reported bash available but left no executor behind is a
    /// runtime failure, not a usable one.
    pub fn planned_sandbox_info(&self) -> SandboxInfo {
        if self.sandbox_info.bash_available && !self.planned_bash_available() {
            return SandboxInfo {
                mode: SandboxMode::Unavailable,
                network: SandboxNetwork::Unconfined,
                bash_available: false,
                reason: SandboxReason::RuntimeFailure,
            };
        }
        self.sandbox_info
    }

    /// A closed boundary disables bash and says only that the environment was
    /// rejected.
    pub fn effective_sandbox_info(&self) -> SandboxInfo {
        if !self.boundary_allows_dynamic(None) {
            return SandboxInfo {
                mode: SandboxMode::Unavailable,
                network: SandboxNetwork::Unconfined,
                bash_available: false,
                reason: SandboxReason::EnvironmentRejected,
            };
        }
        self.planned_sandbox_info()
    }

    /// A closed boundary reports no runtime identity, because provider,
    /// profile, and model are all model-visible text.
    pub fn runtime_info(&self, runtime: &Runtime) -> RuntimeInfo {
        let mut info = RuntimeInfo {
            provider: runtime.provider.clone(),
            profile: runtime.profile.clone(),
            model: runtime.model.clone(),
            thinking: runtime.thinking.clone(),
            context_window: runtime.compaction.context_window,
            sandbox: self.effective_sandbox_info(),
        };
        if !self.boundary_allows_dynamic(Some(runtime)) {
            info.provider = String::new();
            info.profile = String::new();
            info.model = String::new();
            info.thinking = String::new();
            info.context_window = 0;
        }
        info
    }

    pub(crate) fn plugin_runtime(
        &self,
        snapshot: &crate::plugin::Snapshot,
        secrets: &[String],
    ) -> Result<Arc<crate::plugin::hooks::Runtime>, String> {
        crate::plugin::hooks::Runtime::new(
            snapshot,
            Path::new(&self.home),
            &self.config.hooks,
            crate::plugin::hooks::Execution {
                executor: self.command_executor.clone(),
                environment: self.sandbox_environment.clone().unwrap_or_default(),
                workspace: self.workspace.root().into(),
                guard: crate::tool::project_guard::Guard::for_config(
                    &self.config,
                    &self.home,
                    &self.workspace_path,
                ),
                secrets: secrets.to_vec(),
                logger: self.logger.clone(),
            },
        )
    }

    pub(crate) fn builtin_file_tools(&self, max_output: usize) -> Vec<Box<dyn Tool + Send + Sync>> {
        vec![
            Box::new(read::ReadTool::new(self.workspace, max_output)),
            Box::new(grep::GrepTool::new(self.workspace, max_output)),
            Box::new(find::FindTool::new(self.workspace, max_output)),
            Box::new(ls::LsTool::new(self.workspace, max_output)),
            Box::new(write::WriteTool::new(self.workspace)),
            Box::new(edit::EditTool::new(self.workspace)),
        ]
    }

    /// The web tools: `web_fetch` and `web_search`; the search backend is
    /// chosen from the environment. `secrets` are values a URL or query may not contain.
    pub(crate) fn web_tools(
        &self,
        max_output: usize,
        secrets: &[String],
        runtime: Option<&Runtime>,
    ) -> Vec<Box<dyn Tool + Send + Sync>> {
        crate::tool::web::tools(
            &self.web,
            max_output,
            self.web_network_denied(),
            secrets,
            &|name| self.environment.get(name).cloned(),
            self.hosted_search(runtime),
        )
    }

    fn web_network_denied(&self) -> bool {
        self.planned_sandbox_info().network == SandboxNetwork::Denied
    }

    /// Whether the model provider runs web searches itself: the `chatgpt`
    /// provider's hosted `web_search`, used when no Google or Brave key is
    /// configured and `[sandbox].network` is not `deny`. The hosted search
    /// runs at OpenAI, outside Otto's host approvals.
    pub(crate) fn hosted_search(&self, runtime: Option<&Runtime>) -> bool {
        runtime.is_some_and(|runtime| runtime.provider == otto_core::config::PROVIDER_CHATGPT)
            && !self.web_network_denied()
            && crate::tool::web::search_is_keyless(&|name| self.environment.get(name).cloned())
    }

    /// Composes one runnable agent.
    ///
    /// This convenience entry point preserves the command/server behavior of
    /// reporting non-fatal discovery and MCP warnings to stderr. Interactive
    /// frontends must use [`Self::build_runner_quiet`] while they own the
    /// terminal screen.
    pub async fn build_runner(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
    ) -> Result<Runner, BuildError> {
        let mut warnings = std::io::stderr();
        self.build_runner_with_warnings(session, runtime, &mut warnings)
            .await
    }

    /// Composes a runner without writing diagnostics to the process terminal.
    ///
    /// The caller that owns an interactive terminal must not let a background
    /// or replacement build write ordinary text while the TUI has an alternate
    /// screen active: stdout and stderr commonly share the same terminal.
    pub async fn build_runner_quiet(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
    ) -> Result<Runner, BuildError> {
        let mut warnings = std::io::sink();
        self.build_runner_with_warnings(session, runtime, &mut warnings)
            .await
    }

    /// Composes one runnable agent, writing non-fatal build diagnostics to
    /// `warnings`.
    pub async fn build_runner_with_warnings(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
        warnings: &mut (dyn std::io::Write + Send),
    ) -> Result<Runner, BuildError> {
        self.build_runner_inner(session, runtime, None, true, true, warnings)
            .await
            .map(|(runner, _)| runner)
    }

    pub async fn build_runner_with_trace(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
        trace: bool,
    ) -> Result<(Runner, Vec<(&'static str, Duration)>), BuildError> {
        let trace = trace.then(BuildTrace::new);
        let mut warnings = std::io::stderr();
        self.build_runner_inner(session, runtime, trace, true, true, &mut warnings)
            .await
    }

    pub async fn build_runner_without_mcp_with_trace(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
        trace: bool,
    ) -> Result<(Runner, Vec<(&'static str, Duration)>), BuildError> {
        let trace = trace.then(BuildTrace::new);
        let mut warnings = std::io::stderr();
        self.build_runner_inner(session, runtime, trace, false, true, &mut warnings)
            .await
    }

    /// Composes the internal owner runner for durable workflows. Frontend
    /// parent-only tools (such as cross-workspace delivery) are intentionally
    /// absent: a workflow runtime is not a user parent session.
    pub async fn build_workflow_runner(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
        warnings: &mut (dyn std::io::Write + Send),
    ) -> Result<Runner, BuildError> {
        self.build_runner_inner(session, runtime, None, true, false, warnings)
            .await
            .map(|(runner, _)| runner)
    }

    /// The provider client for `runtime`; `Unavailable` when the redaction
    /// boundary is closed.
    fn provider_client(&self, runtime: &Runtime) -> Result<ProviderClient, BuildError> {
        #[cfg(test)]
        if let Some((_, provider)) = TEST_PROVIDERS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|(home, _)| *home == self.home)
        {
            return Ok(ProviderClient::Scripted(provider.clone()));
        }
        Ok(if !self.boundary_allows_dynamic(Some(runtime)) {
            ProviderClient::Unavailable
        } else if runtime.provider == otto_core::config::PROVIDER_CHATGPT {
            ProviderClient::ChatGpt {
                client: Arc::new(
                    chatgpt_client(
                        &self.auth_path,
                        &self.auth_credentials,
                        self.auth_credentials_loaded,
                    )?
                    .with_catalog_cache(
                        std::path::Path::new(&self.home).join(".otto/cache/chatgpt-models.json"),
                    )
                    .with_hosted_web_search(self.hosted_search(Some(runtime))),
                ),
                timeout: runtime.resilience.deadlines.provider_timeout,
                cancellation_grace: runtime.resilience.deadlines.cancellation_grace,
            }
        } else {
            ProviderClient::Compat {
                client: Arc::new(Client::new(&runtime.base_url, &runtime.api_key)),
                timeout: runtime.resilience.deadlines.provider_timeout,
                cancellation_grace: runtime.resilience.deadlines.cancellation_grace,
            }
        })
    }

    async fn build_runner_inner(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
        mut trace: Option<BuildTrace>,
        connect_mcp: bool,
        include_parent_tools: bool,
        warnings: &mut (dyn std::io::Write + Send),
    ) -> Result<(Runner, Vec<(&'static str, Duration)>), BuildError> {
        let redaction_values = self.secret_values(Some(runtime));
        let max_output = output_cap(runtime.max_output_bytes);
        let output_logs = Arc::new(crate::tool::bash_output::Logs::default());
        let schedule_origin = Arc::new(crate::schedule::tools::TurnOrigin::default());
        let presentation_origin = Arc::new(crate::schedule::tools::TurnOrigin::default());
        let skills = otto_core::config::resolve_skills(
            &self.config,
            &self.environment,
            &self.workspace_path,
        );
        if skills.enabled
            && let Some(roots) = crate::reflection::skill_roots(&self.home, &skills.roots)
        {
            self.reflector
                .attach_approvals(&self.approvals, &self.workspace_path, roots)?;
        }
        let mut tools = self.builtin_file_tools(max_output);
        // Parent-only: child registries are built before their session exists,
        // so only the parent can bind verdict evidence to authoritative history.
        tools.push(Box::new(verification::SubmitVerification::new(
            session.clone(),
        )));
        tools.extend(self.web_tools(max_output, &redaction_values, Some(runtime)));
        if self.bash_configured() {
            let executor = self
                .command_executor
                .clone()
                .expect("bash_configured implies an executor");
            let mut tool = bash::BashTool::new_with_grace(
                self.workspace,
                executor,
                &self.shell,
                self.sandbox_environment.clone().unwrap_or_default(),
                shell_timeout(runtime.shell_timeout),
                runtime.resilience.deadlines.cancellation_grace,
                max_output,
                &redaction_values,
            )
            .map_err(|error| format!("create bash tool: {error}"))?;
            tool = tool.with_output_logs(Arc::clone(&output_logs));
            if self.sandbox_info.mode == SandboxMode::Seatbelt {
                tool = tool.with_private_home();
            }
            if let Some(approvals) = &self.bash_approvals {
                tool = tool.with_approvals(session.header().id, Arc::clone(approvals));
            }
            if let Some(control) = &self.grant_control {
                tool = tool.with_grant_control(self.config_path.clone(), control.clone());
            }
            if let Some(excluded) = &self.excluded_commands {
                tool = tool.with_excluded_commands(Arc::clone(excluded));
            }
            tools.push(Box::new(tool));
        }
        let redactor = self.boundary_redactor(Some(runtime));
        let model_diagnostic =
            otto_core::config::resolve::subagent_model_diagnostic(&self.config, runtime)
                .map(|diagnostic| redactor.redact_string(&diagnostic.reason));
        if let Some(reason) = &model_diagnostic
            && redactor.allows_dynamic_content()
        {
            let _ = writeln!(warnings, "warning: {reason}");
        }
        let client = self.provider_client(runtime)?;
        let advisor = if self.memory_usable()
            && self.memory.extension.is_none()
            && self.boundary_allows_dynamic(Some(runtime))
        {
            crate::approval::memory::attach(
                &self.approvals,
                &self.memory.service,
                vec![self.memory.user_scope.clone(), self.workspace_scope.clone()],
                &self.workspace_path,
            )
            .map_err(|e| e.to_string())?;
            crate::approval::memory::reconcile(
                &self.approvals,
                &self.memory.service,
                &[self.memory.user_scope.clone(), self.workspace_scope.clone()],
                &self.workspace_path,
            )
            .map_err(|e| e.to_string())?;
            let advice_agent = Agent::with_redactor(
                client.clone(),
                Registry::new(Vec::new()).map_err(|error| format!("advice registry: {error}"))?,
                SharedSession::memory(session.header()),
                Options {
                    diagnostics: self.logger.diagnostic_scope(
                        &session.header().id,
                        "",
                        self.boundary_redactor(Some(runtime)),
                    ),
                    model: runtime.model.clone(),
                    provider_name: runtime.provider.clone(),
                    thinking: runtime.thinking.clone(),
                    now: Box::new(Utc::now),
                    new_operation_id: Box::new(new_operation_id),
                    ..Options::default()
                },
                self.boundary_redactor(Some(runtime)),
            );
            let advisor = self.advisor(
                advice_agent,
                &runtime.model,
                self.usage_collector(session, runtime),
            );
            tools.extend(self.memory_tools(
                max_output,
                advisor.clone(),
                Some((
                    session.header().id.as_str(),
                    Arc::clone(&presentation_origin),
                )),
            ));
            advisor
        } else {
            if self.memory.extension.is_some()
                && self.memory_usable()
                && self.boundary_allows_dynamic(Some(runtime))
                && let Some(client) = &self.memory.client
            {
                tools.extend(crate::tool::memory_service::tools(
                    client.clone(),
                    self.memory_context(
                        otto_core::memory::Origin::Model,
                        Some(session.header().id),
                    ),
                    max_output,
                ));
            }
            None
        };
        mark_build_trace(&mut trace, "runner/setup");
        let plugins =
            crate::plugin::Snapshot::load(Path::new(&self.home), &self.config, &self.environment);
        let extensions = self.plugin_runtime(&plugins, &redaction_values)?;
        let hook_guard = crate::plugin::hooks::session_guard(
            {
                let session = session.clone();
                Arc::new(move || session.lease())
            },
            PathBuf::from(&self.workspace_path),
            session.1.closed.clone(),
            otto_core::config::resolve_failover(&self.config)
                .map_err(|e| e.to_string())?
                .enabled,
        );
        if redactor.allows_dynamic_content() {
            tools.extend(extensions.tools());
        }
        let mut catalogs =
            self.build_catalogs_with_plugins(&mut tools, max_output, warnings, &plugins)?;
        // Disabling conversational delegation must not disable named workflow
        // execution. This internal runner never exposes parent tools.
        if !include_parent_tools {
            let mut discovery = self.config.clone();
            discovery.agents.enabled = Some(true);
            catalogs.agents = otto_core::config::resolve_agents(
                &discovery,
                &self.environment,
                &self.workspace_path,
            )
            .map_err(|error| error.to_string())?;
            let roots = crate::config::layered::catalog_roots(
                &discovery,
                Path::new(&self.home),
                Path::new(&self.workspace_path),
                &catalogs.agents.roots,
                "agents",
            )?;
            let paths: Vec<PathBuf> = roots.into_iter().map(PathBuf::from).collect();
            let (catalog, diagnostics) = crate::subagent::Catalog::discover(&paths);
            catalogs.agent_catalog = catalog;
            for warning in diagnostics {
                let _ = writeln!(warnings, "warning: {warning}");
            }
        }
        mark_build_trace(&mut trace, "runner/catalogs");
        let (mcp_tools, mcp_connected, mcp_servers) = if connect_mcp {
            self.connect_mcp_with_plugins(
                max_output,
                runtime.resilience.deadlines.cancellation_grace,
                warnings,
                &plugins,
            )
            .await
        } else {
            (Vec::new(), Vec::new(), self.connecting_mcp_servers())
        };
        tools.extend(mcp_tools);
        if let Some(guard) = crate::tool::project_guard::Guard::for_config(
            &self.config,
            &self.home,
            &self.workspace_path,
        ) && guard.check_trust
        {
            guard.watch_mcp(&mcp_servers);
        }
        mark_build_trace(&mut trace, "runner/mcp");

        match &client {
            ProviderClient::Compat { client, .. } => {
                tools.push(Box::new(models::ListModelsTool::new(
                    Arc::clone(client),
                    max_output,
                )));
            }
            ProviderClient::ChatGpt { client, .. } => {
                tools.push(Box::new(models::ListModelsTool::chatgpt(
                    Arc::clone(client),
                    max_output,
                )));
            }
            _ => {}
        }
        mark_build_trace(&mut trace, "runner/provider");

        // The workspace context runs `git status` through the sandbox, so it
        // may only reach the executor when both the boundary is open and a
        // bash tool exists to hold it.
        let context_executor = if redactor.allows_dynamic_content() && self.bash_configured() {
            self.command_executor.as_ref()
        } else {
            None
        };
        let environment = layered_workspace_context(
            Path::new(&self.home),
            &self.workspace_path,
            Utc::now(),
            context_executor,
            self.sandbox_environment.as_deref(),
            self.workspace,
        )
        .await?;
        let prompt_tail = redactor.redact_string(&(environment.clone() + &catalogs.skill_section));
        mark_build_trace(&mut trace, "runner/workspace-context");
        let parent_agent_section = redactor.redact_string(&catalogs.agent_section);
        let endpoint_host = boundary::endpoint_host_for(&runtime.base_url);
        let mut child_tools = self.child_tools_with_output_logs(
            runtime,
            max_output,
            &redaction_values,
            &catalogs.skills,
            Arc::clone(&output_logs),
        )?;
        child_tools.extend(super::wiring::mcp_child_tools(&mcp_connected, max_output));
        if redactor.allows_dynamic_content() {
            child_tools.extend(extensions.tools());
        }
        if let Some(guard) = crate::tool::project_guard::Guard::for_config(
            &self.config,
            &self.home,
            &self.workspace_path,
        ) {
            child_tools = guard.tools(child_tools);
        }
        let mut workflow_binding = None;
        let mut subagents = self.build_subagents(
            &mut tools,
            &catalogs,
            &client,
            &redaction_values,
            &redactor,
            runtime,
            session,
            self.child_prompt_for(runtime, &endpoint_host, &prompt_tail),
            child_tools,
            Some(extensions.clone()),
            Some(hook_guard.clone()),
            warnings,
        )?;
        if let Some(runner) = &subagents.runner {
            runner.bind_output_logs(Arc::clone(&output_logs));
            runner.bind_scratch_root(
                self.workspace
                    .root_fs()
                    .try_clone()
                    .map_err(|error| format!("pin child research workspace: {error}"))?,
            );
        }
        mark_build_trace(&mut trace, "runner/subagents");

        let task_control = Arc::new(crate::app::task_control::TaskControl::default());
        if let Some(journal) = self.session_journal(session, "inputs.json", warnings) {
            task_control.steering().set_journal(journal);
        }
        if let Some(journal) = self.session_journal(session, "followups.json", warnings) {
            task_control.set_followup_journal(journal);
        }
        if let Some(journal) = self.session_journal(session, "turns.json", warnings) {
            task_control.set_turn_queue_journal(journal);
        }
        let reflection = Arc::new(crate::tool::reflect::Handle::default());
        if self.boundary_allows_dynamic(Some(runtime)) {
            tools.push(Box::new(crate::tool::approval::RequestApproval {
                service: self.approvals.clone(),
                scope: otto_core::approval::Scope {
                    workspace: self.workspace_path.clone(),
                    session_id: session.header().id,
                    task_id: String::new(),
                    run_id: String::new(),
                },
                turn_origin: Some(Arc::clone(&presentation_origin)),
            }));
            for revoke in [false, true] {
                tools.push(Box::new(crate::tool::approval::Control {
                    service: self.approvals.clone(),
                    scope: otto_core::approval::Scope {
                        workspace: self.workspace_path.clone(),
                        session_id: session.header().id,
                        task_id: String::new(),
                        run_id: String::new(),
                    },
                    revoke,
                    expected_id: None,
                    legacy: self.bash_approvals.clone(),
                }));
            }
            tools.extend(crate::schedule::tools::tools_with_session(
                &crate::schedule::tools::Context {
                    store: self.schedule.clone(),
                    origin: Arc::clone(&schedule_origin),
                    session_id: session.header().id.clone(),
                    workspace: self.workspace_path.clone(),
                },
                session.handle().clone(),
            ));
            tools.push(Box::new(crate::tool::task_control::TaskQueue::new(
                Arc::clone(&task_control),
                &redaction_values,
            )));
            // Parent-only: a child's plan would be shown nowhere.
            tools.push(Box::new(crate::tool::plan::UpdatePlan::new(
                Arc::new(session.clone()),
                &redaction_values,
            )));
            // Parent-only, like the timer tools: a child has no session file
            // of its own to reflect on.
            if self.reflector.enabled() && !self.no_session {
                tools.push(Box::new(crate::tool::reflect::ReflectTool::new(
                    Arc::clone(&reflection),
                )));
            }
            if include_parent_tools {
                if subagents.inbox.is_none() {
                    let inbox = Arc::new(otto_core::agent::inbox::Inbox::default());
                    let source = session.clone();
                    let no_session = self.no_session;
                    crate::runtime::sidecar_mailbox::install(&inbox, ".inbox.json", move || {
                        let path = source.path();
                        (!no_session && !path.is_empty())
                            .then(|| Path::new(&path).with_extension("inbox.json"))
                    });
                    let path = session.path();
                    if !self.no_session && !path.is_empty() {
                        if let Some(warning) = crate::runtime::sidecar_mailbox::load(
                            &inbox,
                            &Path::new(&path).with_extension("inbox.json"),
                        ) {
                            let _ = writeln!(warnings, "warning: {warning}");
                        }
                    }
                    subagents.inbox = Some(inbox);
                }
                let controller = Box::pin(crate::runtime::workflow::shared_controller(
                    self, runtime, warnings,
                ))
                .await;
                if !self.no_session
                    && let (Ok(controller), Some(mailbox)) = (&controller, &subagents.inbox)
                {
                    workflow_binding = Some(crate::runtime::workflow_notifications::bind(
                        controller,
                        session.clone(),
                        mailbox,
                    ));
                }
                let parent = crate::tool::workflow::Parent::new(
                    controller,
                    session.header().id.clone(),
                    !self.no_session,
                );
                tools.extend(parent.tools());
            }
            if include_parent_tools && let Some(factory) = self.shared.parent_tool_factory.get() {
                tools.extend(factory.tools(&ParentToolContext {
                    workspace: self.workspace_path.clone(),
                    session_id: session.header().id.clone(),
                    source_session: session.clone(),
                })?);
            }
            let mut definitions: Vec<_> = tools.iter().map(|tool| tool.definition()).collect();
            definitions.push(
                crate::tool::otto::Help {
                    definitions: Vec::new(),
                }
                .definition(),
            );
            tools.push(Box::new(crate::tool::otto::Help { definitions }));
        }

        if let Some(guard) = crate::tool::project_guard::Guard::for_config(
            &self.config,
            &self.home,
            &self.workspace_path,
        ) {
            tools = guard.tools(tools);
        }
        let authorization_run = Arc::new(Mutex::new(String::new()));
        let registry =
            Registry::new(tools).map_err(|error| format!("create tool registry: {error}"))?;
        let registry = registry.with_approval_origin(
            self.approvals.clone(),
            otto_core::approval::Scope {
                workspace: self.workspace_path.clone(),
                session_id: session.header().id,
                task_id: String::new(),
                run_id: String::new(),
            },
            Some(Arc::clone(&presentation_origin)),
        );
        let registry = with_lease_guard(registry, session, &self.workspace_path)
            .with_authorization_run(authorization_run.clone());
        let definitions = registry.definitions();
        let base_prompt = system_prompt_for(
            &definitions,
            self.effective_sandbox_info(),
            &runtime.provider,
            &endpoint_host,
            &runtime.model,
        );
        let mut model_guidance = if subagents.runner.is_some() {
            redactor.redact_string(&subagent_model_guidance(&runtime.subagent_models))
        } else {
            String::new()
        };
        if subagents.runner.is_some()
            && redactor.allows_dynamic_content()
            && let Some(reason) = model_diagnostic
        {
            model_guidance.push_str(&redactor.redact_string(&format!(
                "\n<subagent_model_diagnostic>\n{reason}\n</subagent_model_diagnostic>\n"
            )));
        }
        let system_prompt = base_prompt.clone()
            + &model_guidance
            + &prompt_tail
            + &parent_agent_section
            + "</otto_system_prompt>";
        let (environment, instructions) = split_workspace_instructions(&environment);
        self.approvals
            .set_policy_config(&self.workspace_path, self.config.approval.clone())
            .map_err(|error| error.to_string())?;
        let forced = self
            .config
            .policy
            .as_ref()
            .and_then(|policy| policy.force.get("approval"))
            .cloned()
            .map(toml::Value::try_into::<otto_core::config::ApprovalConfig>)
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        self.approvals
            .set_managed_policy(&self.workspace_path, forced)
            .map_err(|error| error.to_string())?;
        if !session.header().id.is_empty() {
            let review_runtime = match self.config.approval.reviewer_profile.as_deref() {
                Some(profile) => {
                    if !self.config.profiles.contains_key(profile) {
                        return Err("approval reviewer profile does not exist".into());
                    }
                    self.resolve_profile(profile)?
                }
                None => runtime.clone(),
            };
            let review_agent = Agent::with_redactor(
                match self.config.approval.reviewer_profile {
                    Some(_) => self.provider_client(&review_runtime)?,
                    None => client.clone(),
                },
                Registry::new(Vec::new())?,
                SharedSession::memory(session.header()),
                Options {
                    diagnostics: self.logger.diagnostic_scope(
                        &session.header().id,
                        "",
                        self.boundary_redactor(Some(&review_runtime)),
                    ),
                    model: review_runtime.model.clone(),
                    provider_name: review_runtime.provider.clone(),
                    thinking: review_runtime.thinking.clone(),
                    now: Box::new(Utc::now),
                    new_operation_id: Box::new(new_operation_id),
                    ..Options::default()
                },
                self.boundary_redactor(Some(&review_runtime)),
            );
            self.approvals
                .register_reviewer(
                    &self.workspace_path,
                    &session.header().id,
                    Arc::new(
                        crate::approval::reviewer::SessionReviewer::new(review_agent)
                            .with_usage(self.usage_collector(session, &review_runtime)),
                    ),
                )
                .map_err(|error| error.to_string())?;
        }
        let mut review_context = session
            .handle()
            .custom_data(crate::approval::reviewer::CONTEXT_CUSTOM_TYPE)
            .and_then(|data| {
                serde_json::from_str::<crate::approval::reviewer::ReviewContext>(&data).ok()
            })
            .unwrap_or_else(|| {
                let mut constraints = self.review_instruction_snapshot(instructions, &redactor);
                if !session.messages().is_empty() {
                    constraints.push(crate::approval::reviewer::INCOMPLETE_EVIDENCE.into());
                }
                crate::approval::reviewer::ReviewContext {
                    instructions: constraints,
                    ..Default::default()
                }
            });
        review_context.policy_digest = crate::approval::digest(
            &serde_json::to_string(&self.config.approval).map_err(|error| error.to_string())?,
        );
        review_context.refresh_digest();
        if !session.header().id.is_empty() {
            self.approvals
                .set_review_context(
                    &self.workspace_path,
                    &session.header().id,
                    review_context.clone(),
                )
                .map_err(|error| error.to_string())?;
        }
        let system_prompt_parts: Vec<(String, String)> = [
            ("Base", base_prompt),
            ("Sub-agent models", model_guidance),
            ("Environment", redactor.redact_string(environment)),
            (
                "Workspace instructions",
                redactor.redact_string(instructions),
            ),
            ("Skills", redactor.redact_string(&catalogs.skill_section)),
            ("Agents", parent_agent_section),
            ("Document end", "</otto_system_prompt>".to_string()),
        ]
        .into_iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(label, text)| (label.to_string(), text))
        .collect();

        let request_sizer = match &client {
            ProviderClient::Compat { client, .. } => {
                Some(client.clone() as Arc<dyn RequestSizer + Send + Sync>)
            }
            ProviderClient::ChatGpt { client, .. } => {
                Some(client.clone() as Arc<dyn RequestSizer + Send + Sync>)
            }
            ProviderClient::Unavailable => None,
            #[cfg(test)]
            ProviderClient::Scripted(_) => None,
        };
        let options = Options {
            diagnostics: self.logger.diagnostic_scope(
                &session.header().id,
                "",
                self.boundary_redactor(Some(runtime)),
            ),
            hooks: (include_parent_tools
                && extensions.has_hooks()
                && redactor.allows_dynamic_content())
            .then(|| {
                extensions.bind(
                    session.header().id,
                    None,
                    String::new(),
                    if session.path().is_empty() {
                        "new"
                    } else {
                        "resume"
                    },
                    Some(hook_guard),
                )
            }),
            hook_state: session.1.clone(),
            model: runtime.model.clone(),
            provider_name: runtime.provider.clone(),
            system_prompt: system_prompt.clone(),
            system_prompt_parts,
            thinking: runtime.thinking.clone(),
            cache_key: session.header().id,
            now: Box::new(Utc::now),
            new_operation_id: Box::new(new_operation_id),
            request_sizer,
            compaction: CompactionSettings {
                auto: runtime.compaction.auto,
                hard_input_window: runtime.compaction.hard_input_window,
                working_window: runtime.compaction.working_window,
                reserve_tokens: runtime.compaction.reserve_tokens,
                keep_recent_tokens: runtime.compaction.keep_recent_tokens,
            },
            progress: ProgressPolicy {
                repeated_calls: runtime.progress.repeated_calls,
                consecutive_failures: runtime.progress.consecutive_failures,
            },
            memory: match self.memory_usable() && self.boundary_allows_dynamic(Some(runtime)) {
                true => Some(self.bind_selected_memory(session.header().id)?),
                false => None,
            },
            memory_recall_limit: self.memory.recall_limit,
            memory_recall_token_budget: self.memory.recall_token_budget,
            tasks: subagents
                .tasks
                .clone()
                .map(|tasks| tasks as Arc<dyn otto_core::agent::tasks::TaskRegistry + Send + Sync>),
            mailbox: subagents
                .inbox
                .unwrap_or_else(|| Arc::new(otto_core::mailbox::InMemoryMailbox::default())),
            steering: task_control.steering(),
            version: env!("CARGO_PKG_VERSION").into(),
            ..Options::default()
        };
        let hook_port = options.hooks.clone();
        let hook_state = options.hook_state.clone();
        mark_build_trace(&mut trace, "runner/registry-and-options");
        if !plugins.packages.is_empty() {
            session
                .append_custom("otto.plugins", &plugins.provenance())
                .await
                .map_err(|e| e.to_string())?;
        }
        let trace_entries = trace.map(|trace| trace.entries).unwrap_or_default();
        let runner = Runner {
            workflow_binding,
            output_logs,
            authorization_run,
            review_context: tokio::sync::Mutex::new(review_context),
            pending_authorization: Mutex::new(Vec::new()),
            approvals: Some((
                self.approvals.clone(),
                otto_core::approval::Scope {
                    workspace: self.workspace_path.clone(),
                    session_id: session.header().id,
                    task_id: String::new(),
                    run_id: String::new(),
                },
            )),
            agent: Agent::with_redactor(client, registry, session.clone(), options, redactor),
            turn_timeout: runtime.resilience.deadlines.turn_timeout,
            unattended_turn_timeout: runtime.resilience.deadlines.unattended_turn_timeout,
            headless: self.headless,
            system_prompt,
            definitions,
            usage: self.usage_collector(session, runtime),
            logger: self.logger.clone(),
            tasks: subagents.tasks,
            subagents: subagents.runner,
            reminders: subagents.reminders,
            skills: catalogs.skills.clone(),
            discovered_skills: catalogs.discovered_skills.clone(),
            agents: catalogs.agent_catalog.clone(),
            mcp: mcp_servers,
            task_control,
            advisor,
            schedule_origin,
            presentation_origin,
            reflection,
        };
        if include_parent_tools && extensions.has_hooks() && runner.allows_dynamic_content() {
            if hook_state.claim_start() {
                let state = hook_state.clone();
                let port = hook_port.expect("enabled hooks have a port");
                tokio::spawn(async move {
                    let result = port
                        .run(
                            "session_start",
                            serde_json::json!({"turn_id":""}),
                            &state.closed.child_token(),
                        )
                        .await;
                    state.complete_start(result);
                    if state.closed.is_cancelled()
                        && !state
                            .end_requested
                            .swap(true, std::sync::atomic::Ordering::SeqCst)
                        && !state.ended.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        let _ = port
                            .run(
                                "session_end",
                                serde_json::json!({"turn_id":""}),
                                &CancellationToken::new(),
                            )
                            .await;
                        state.end_done.cancel();
                    }
                });
            }
            let response = match hook_state.wait_start().await {
                Ok(r) => r,
                Err(message) => {
                    runner.agent.end_hooks().await;
                    runner.close();
                    runner.close_mcp().await;
                    return Err(message);
                }
            };
            if response.decision == Some(otto_core::agent::hooks::Decision::Deny) {
                runner.agent.end_hooks().await;
                runner.close();
                runner.close_mcp().await;
                return Err(response
                    .reason
                    .unwrap_or_else(|| "Hook denied session".into()));
            }
        }
        Ok((runner, trace_entries))
    }

    /// The text a caller may print for `message`.
    ///
    /// A closed boundary yields the empty string, deliberately, so no
    /// diagnostic escapes when the redaction set is incomplete.
    pub fn redact_error(&self, message: &str, runtime: Option<&Runtime>) -> String {
        let redactor = boundary::secret_redactor(&self.boundary_inputs(), runtime);
        if !redactor.allows_dynamic_content() {
            return String::new();
        }
        redactor.redact_string(message)
    }

    /// Resolves a replacement runtime from the provenance a session carries.
    pub fn resolve_session(&self, metadata: &RuntimeMetadata) -> Result<Runtime, BuildError> {
        let overrides = Overrides {
            profile: metadata.profile.clone(),
            provider: metadata.provider.clone(),
            model: metadata.model.clone(),
            ..self.overrides.clone()
        };
        otto_core::config::resolve::resolve(
            &self.config,
            &resume_environment(&self.environment),
            &SessionDefaults::default(),
            &overrides,
        )
        .map_err(|error| self.redact_error(&error.to_string(), None))
    }

    /// The named profile is an explicit override, so its own provider, model
    /// and base URL win.
    pub fn resolve_profile(&self, profile: &str) -> Result<Runtime, BuildError> {
        let overrides = Overrides {
            profile: profile.to_string(),
            provider: String::new(),
            base_url: String::new(),
            model: String::new(),
            ..self.overrides.clone()
        };
        otto_core::config::resolve::resolve(
            &self.config,
            &resume_environment(&self.environment),
            &SessionDefaults::default(),
            &overrides,
        )
        .map_err(|error| self.redact_error(&error.to_string(), None))
    }

    /// Records the resolved provenance on the session when it differs from the
    /// header's.
    pub fn update_session_runtime(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
    ) -> Result<(), BuildError> {
        if !self.boundary_allows_dynamic(Some(runtime)) {
            return Ok(());
        }
        let header = session.header();
        if header.profile == runtime.profile
            && header.provider == runtime.provider
            && header.model == runtime.model
        {
            return Ok(());
        }
        session.update_runtime(&RuntimeMetadata {
            profile: runtime.profile.clone(),
            provider: runtime.provider.clone(),
            model: runtime.model.clone(),
        })
    }

    /// `Store::create_lazy` defers the file until the first user message, so
    /// starting Otto and quitting without a prompt leaves nothing behind.
    pub fn create_session(&self, runtime: &Runtime) -> Result<SharedSession, BuildError> {
        let id = random_id().map_err(|error| format!("create session id: {error}"))?;
        self.create_session_with_id(runtime, id)
    }

    pub fn create_session_with_id(
        &self,
        runtime: &Runtime,
        id: String,
    ) -> Result<SharedSession, BuildError> {
        let header = Header {
            version: CURRENT_VERSION,
            id,
            workspace: self.workspace_path.clone(),
            provider: runtime.provider.clone(),
            profile: runtime.profile.clone(),
            model: runtime.model.clone(),
            created_at: Utc::now(),
        };
        if self.no_session {
            return Ok(SharedSession::memory(header));
        }
        let failover_seconds = self
            .create_lease()
            .map_err(|error| self.redact_error(&error, Some(runtime)))?;
        let session_root = self
            .storage
            .session_root(&self.session_root)
            .map_err(|error| self.redact_error(&error, Some(runtime)))?;
        let handle = self
            .storage
            .sessions()
            .and_then(|repository| repository.create_lazy(&session_root, header, failover_seconds))
            .map_err(|error| self.redact_error(&error, Some(runtime)))?;
        Ok(SharedSession::new(handle))
    }
}

impl Builder {
    /// Writes a new session forked from the session file `reference`; see
    /// [`crate::session::SessionRepository::fork`].
    pub fn fork_session_file(
        &self,
        reference: &str,
        before: Option<&str>,
    ) -> Result<crate::session::Forked, otto_core::session::ForkError> {
        let failed = |message: String| {
            otto_core::session::ForkError::Session(otto_core::session::PiError::other(message))
        };
        let id = random_id().map_err(|error| failed(format!("create session id: {error}")))?;
        self.fork_session_file_with_id(reference, before, &id, None)
    }

    pub fn fork_session_file_with_id(
        &self,
        reference: &str,
        before: Option<&str>,
        id: &str,
        request_id: Option<&str>,
    ) -> Result<crate::session::Forked, otto_core::session::ForkError> {
        let failed = |message: String| {
            otto_core::session::ForkError::Session(otto_core::session::PiError::other(message))
        };
        self.storage
            .sessions()
            .map_err(failed)?
            .fork(reference, before, id, request_id)
    }

    /// The places the session file `reference` can be forked, oldest first.
    pub fn session_fork_points(
        &self,
        reference: &str,
    ) -> Result<Vec<otto_core::session::ForkPoint>, String> {
        self.storage.sessions()?.fork_points(reference)
    }
}

/// Environment for an explicit selection change or active-runtime rebuild.
/// Drop runtime selectors so they cannot override the selection already chosen
/// by the caller.
pub fn resume_environment(environment: &HashMap<String, String>) -> HashMap<String, String> {
    environment
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "OTTO_PROVIDER" | "OTTO_PROFILE" | "OTTO_MODEL" | "OTTO_UI"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// 16 random bytes from `/dev/urandom`, hex encoded.
pub(crate) fn random_id() -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// A fresh opaque operation identity for durable tool-attempt facts.
pub(crate) fn new_operation_id() -> Result<otto_core::model::OperationId, String> {
    let random = random_id().map_err(|error| format!("read operation randomness: {error}"))?;
    otto_core::model::OperationId::new(format!("op_{random}")).map_err(|error| error.to_string())
}

/// Leaks one workspace for the process lifetime.
///
/// ponytail: the file tools borrow `&'a Workspace` and the registry stores
/// `Box<dyn Tool + 'static>`, so the workspace must outlive both. The CLI
/// builds exactly one per process and never drops it before exit, so a leak
/// costs one allocation. Swap for `Arc<Workspace>` only if the tools ever
/// need to own their workspace.
pub fn leaked_workspace(root: &Path) -> Result<&'static Workspace, std::io::Error> {
    Ok(Box::leak(Box::new(Workspace::new(root)?)))
}

/// Installs the session-lease commit guard: a tool call through `registry`
/// refuses to run once `session`'s lease has been lost, and each call syncs
/// `workspace_path` to disk once it finishes. `session.lease()` returns
/// `None` for a session that is not lease-managed, so the guard is then a
/// no-op (see `failover::CommitGuard`).
fn with_lease_guard(registry: Registry, session: &SharedSession, workspace_path: &str) -> Registry {
    let session = session.clone();
    let lease: failover::LeaseSource = Arc::new(move || session.lease());
    registry.with_guard(Arc::new(failover::CommitGuard::new(
        lease,
        PathBuf::from(workspace_path),
    )))
}

/// A negative or oversized cap is clamped rather than wrapped, and the tools
/// reject zero themselves.
fn output_cap(max_output_bytes: i64) -> usize {
    usize::try_from(max_output_bytes).unwrap_or(0)
}

/// `BashTool::new` rejects a zero timeout, which configuration resolution never
/// produces, and a negative `Duration` cannot exist in Rust.
pub(crate) fn shell_timeout(timeout: Duration) -> Duration {
    timeout
}

/// Resolves process selection from CLI, environment and configuration.
/// Session metadata is history, not an implicit runtime selection.
pub fn resolve_initial_runtime(
    file: &File,
    environment: &HashMap<String, String>,
    _metadata: Option<&RuntimeMetadata>,
    overrides: &Overrides,
) -> Result<Runtime, ConfigError> {
    otto_core::config::resolve::resolve(file, environment, &SessionDefaults::default(), overrides)
}

/// A session may be resumed in its original directory or a recorded workspace move.
pub fn validate_session_workspace(session_workspace: &str, workspace: &str) -> Result<(), String> {
    if crate::workspace::read(Path::new(workspace))
        .map_err(|error| error.to_string())?
        .is_some()
        && crate::workspace::owns_session(workspace, session_workspace)
            .map_err(|error| error.to_string())?
    {
        return Ok(());
    }
    let header_workspace = canonical_directory(Path::new(session_workspace))
        .map_err(|error| format!("resolve session workspace: {error}"))?;
    if header_workspace.as_os_str() != workspace {
        return Err(format!(
            "session workspace {:?} does not match cwd",
            header_workspace.to_string_lossy()
        ));
    }
    Ok(())
}

/// The ChatGPT provider client for the signed-in credentials at `path`.
fn chatgpt_client(
    path: &str,
    credentials: &crate::auth::Credentials,
    loaded: bool,
) -> Result<crate::provider::chatgpt::Client, String> {
    if !loaded {
        return Err(crate::auth::AuthError::NoCredentials.to_string());
    }
    let path = path.trim();
    if path.is_empty() {
        return Err(crate::auth::AuthError::CredentialsUnavailable.to_string());
    }
    // `build_runner` has no token to pass, so the source gets a fresh
    // never-cancelled base and every request still aborts on the per-call token
    // the agent hands `Provider::complete`.
    let tokens = crate::auth::token::TokenSource::new(
        credentials.clone(),
        std::path::PathBuf::from(path),
        tokio_util::sync::CancellationToken::new(),
    );
    Ok(crate::provider::chatgpt::Client::new(
        tokens,
        &credentials.account_id,
    ))
}

#[cfg(test)]
mod tests;
