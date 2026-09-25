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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use otto_core::agent::redactor::Redactor;
use otto_core::agent::{
    Agent, AgentError, CompactionResult, CompactionSettings, EventSink, Options,
};
use otto_core::config::resolve::{Overrides, Runtime, SessionDefaults};
use otto_core::config::{ConfigError, File, McpRuntime};
use otto_core::model::{Block, Message, ToolDefinition};
use otto_core::provider::{Provider, ProviderError, Request, RequestSizer, Response, StreamSink};
use otto_core::session::{
    CURRENT_VERSION, CompactionCheckpoint, CompactionMetadata, Header, MemorySession,
    RuntimeMetadata, Session, SessionError, Snapshot,
};
use otto_core::tool::ToolExecutor;
use tokio_util::sync::CancellationToken;

use crate::provider::openaicompat::Client;
use crate::sandbox::CommandExecutor;
use crate::session::Store;
use crate::skill::Catalog;
use crate::tool::registry::Registry;
use crate::tool::workspace::Workspace;
use crate::tool::{Tool, bash, edit, find, grep, ls, read, write};

use super::boundary::{self, BoundaryInputs, FixedText};
use super::info::{SandboxInfo, SandboxMode, SandboxNetwork, SandboxReason};
use super::prompt::system_prompt_for;
use super::sandbox_runtime::canonical_directory;
use super::workspace_context::{split_workspace_instructions, workspace_context_for};

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

/// The session operations a frontend needs beyond the transcript itself.
pub trait SessionHandle: Session + Send + Sync {
    fn header(&self) -> Header;
    fn name(&self) -> String;
    fn path(&self) -> String;
    fn rename(&self, name: &str) -> Result<(), String>;
    fn thinking_level(&self) -> String;
    fn update_thinking_level(&self, thinking: &str) -> Result<(), String>;
    fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String>;
    fn close(&self) -> Result<(), String>;

    /// The usage and context-window counters a frontend displays. A transcript
    /// that keeps no counters reports zeroes.
    fn snapshot(&self) -> Snapshot {
        Snapshot::default()
    }
}

impl SessionHandle for Store {
    fn header(&self) -> Header {
        Store::header(self)
    }

    fn name(&self) -> String {
        Store::name(self)
    }

    fn path(&self) -> String {
        Store::path(self)
    }

    fn rename(&self, name: &str) -> Result<(), String> {
        Store::rename(self, name).map_err(|error| error.to_string())
    }

    fn thinking_level(&self) -> String {
        Store::thinking_level(self)
    }

    fn update_thinking_level(&self, thinking: &str) -> Result<(), String> {
        Store::update_thinking_level(self, thinking).map_err(|error| error.to_string())
    }

    fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String> {
        Store::update_runtime(self, runtime).map_err(|error| error.to_string())
    }

    fn close(&self) -> Result<(), String> {
        Store::close(self).map_err(|error| error.to_string())
    }

    fn snapshot(&self) -> Snapshot {
        Store::snapshot(self)
    }
}

/// A transcript that is never written to disk. It is what `--no-session`
/// selects; `otto_core::session::MemorySession` carries the transcript and this
/// wrapper carries the header and the name.
pub struct MemoryHandle {
    inner: MemorySession,
    state: Mutex<MemoryState>,
}

struct MemoryState {
    header: Header,
    name: String,
    thinking_level: String,
}

impl MemoryHandle {
    pub fn new(mut header: Header) -> Self {
        header.version = CURRENT_VERSION;
        Self {
            inner: MemorySession::new(),
            state: Mutex::new(MemoryState {
                header,
                name: String::new(),
                thinking_level: String::new(),
            }),
        }
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

    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError> {
        self.inner.append_compaction(checkpoint).await
    }
}

impl SessionHandle for MemoryHandle {
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

    fn thinking_level(&self) -> String {
        self.state().thinking_level.clone()
    }

    fn update_thinking_level(&self, thinking: &str) -> Result<(), String> {
        if !matches!(thinking, "" | "low" | "medium" | "high" | "xhigh" | "max") {
            return Err(
                "invalid thinking: must be one of low, medium, high, xhigh, max".to_string(),
            );
        }
        self.state().thinking_level = thinking.to_string();
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
}

/// A session shared by the agent and the frontend.
///
/// Both need `&self` access to the same transcript for the whole run, which
/// is what `Arc` buys; every [`SessionHandle`] method already takes `&self`.
#[derive(Clone)]
pub struct SharedSession(Arc<dyn SessionHandle>);

impl SharedSession {
    pub fn new(handle: Arc<dyn SessionHandle>) -> Self {
        Self(handle)
    }

    /// A `--no-session` transcript.
    pub fn memory(header: Header) -> Self {
        Self(Arc::new(MemoryHandle::new(header)))
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

    pub fn thinking_level(&self) -> String {
        self.0.thinking_level()
    }

    pub fn update_thinking_level(&self, thinking: &str) -> Result<(), String> {
        self.0.update_thinking_level(thinking)
    }

    pub fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String> {
        self.0.update_runtime(runtime)
    }

    pub fn close(&self) -> Result<(), String> {
        self.0.close()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.0.snapshot()
    }
}

#[async_trait::async_trait]
impl Session for SharedSession {
    fn messages(&self) -> Vec<Message> {
        self.0.messages()
    }

    async fn append(&self, message: Message) -> Result<(), SessionError> {
        self.0.append(message).await
    }

    fn latest_compaction(&self) -> Option<CompactionMetadata> {
        self.0.latest_compaction()
    }

    async fn append_compaction(
        &self,
        checkpoint: CompactionCheckpoint,
    ) -> Result<CompactionMetadata, SessionError> {
        self.0.append_compaction(checkpoint).await
    }
}

/// The provider an agent calls, or the refusal used when the redaction boundary
/// is closed.
///
/// The agent never reaches the client when the boundary is closed, because
/// `Run` checks the redactor first. This enum keeps the provider slot
/// non-optional, so the agent's type parameter stays concrete.
pub enum ProviderClient {
    Compat(Arc<Client>),
    ChatGpt(Arc<crate::provider::chatgpt::Client>),
    Unavailable,
    /// Test seam. `Runner` is a concrete struct, so the seam sits one layer
    /// down.
    #[cfg(test)]
    Scripted(Arc<dyn Provider + Send + Sync>),
}

#[async_trait::async_trait]
impl Provider for ProviderClient {
    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<Response, ProviderError> {
        match self {
            Self::Compat(client) => client.complete(request, emit, cancel).await,
            Self::ChatGpt(client) => client.complete(request, emit, cancel).await,
            Self::Unavailable => Err(ProviderError::Other(
                "provider is unavailable: redaction is incomplete".to_string(),
            )),
            #[cfg(test)]
            Self::Scripted(provider) => provider.complete(request, emit, cancel).await,
        }
    }
}

/// One composed agent, plus the two fixed strings a frontend may show.
pub struct Runner {
    agent: Agent<ProviderClient, Registry, SharedSession>,
    system_prompt: String,
    definitions: Vec<ToolDefinition>,
    usage: Option<crate::usage::Collector>,
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
    /// Named child definitions used by durable workflow discovery.
    pub(crate) agents: crate::subagent::Catalog,
    /// The MCP servers connected for this runner. `/mcp` reads its status
    /// rows; `Runner::close` (best-effort, backgrounded) and
    /// `Runner::close_mcp` (awaited, bounded) shut its clients down.
    pub(crate) mcp: Arc<crate::mcp::Servers>,
}

impl Runner {
    /// Runs one turn.
    pub async fn run(
        &self,
        user_text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let mut emit = self.collecting(emit);
        self.agent.run(user_text, &mut emit, cancel).await
    }

    pub async fn run_with_image(
        &self,
        user_text: &str,
        image: Block,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let mut emit = self.collecting(emit);
        self.agent
            .run_with_image(user_text, Some(image), &mut emit, cancel)
            .await
    }

    /// Compacts the transcript.
    pub async fn compact(
        &self,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, AgentError> {
        let mut emit = self.collecting(emit);
        self.agent.compact(focus, &mut emit, cancel).await
    }

    fn collecting<'a>(
        &'a self,
        emit: EventSink<'a>,
    ) -> impl FnMut(otto_core::agent::Event) + Send + use<'a> {
        move |event| {
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

    /// What the next provider request contains.
    pub fn context_report(&self) -> otto_core::agent::context_report::ContextReport {
        self.agent.context_report()
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
    /// [`Self::close_mcp`] there too, and call it *before* `close`, not
    /// after: [`crate::mcp::Servers::close`] uses `mem::take` internally, so
    /// whichever of the two runs first empties the client list and the
    /// other becomes a no-op.
    pub fn close(&self) {
        let _ = self.agent.close();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let mcp = Arc::clone(&self.mcp);
            handle.spawn(async move { mcp.close().await });
        }
    }

    /// Shuts every connected MCP server down, waiting up to 5 seconds.
    /// Idempotent with [`Self::close`]'s background close via `mem::take`,
    /// but only awaiting this one guarantees MCP is down before the caller
    /// proceeds. Call this *before* `close` at a process-exit site, so this
    /// call is the one that actually drains `Servers`' client list instead
    /// of racing a background task that a subsequent process exit could
    /// cancel before it runs. There is nothing to release for
    /// `Servers::default()`, so this returns immediately for a runner built
    /// without MCP servers.
    pub async fn close_mcp(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.mcp.close()).await;
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
        let registry = Registry::new(Vec::new()).expect("empty registry");
        let definitions = registry.definitions();
        Self {
            agent: Agent::new(
                ProviderClient::Scripted(provider),
                registry,
                session,
                Options {
                    model: "test-model".to_string(),
                    provider_name: "openai-compatible".to_string(),
                    now: Box::new(Utc::now),
                    inbox: Arc::clone(tasks.notifications()),
                    tasks: Some(Arc::clone(&tasks)
                        as Arc<dyn otto_core::agent::tasks::TaskRegistry + Send + Sync>),
                    ..Options::default()
                },
            ),
            system_prompt: String::new(),
            definitions,
            usage: None,
            reminders: Some(Arc::new(crate::tool::remind::Reminders::new(Arc::clone(
                tasks.notifications(),
            )))),
            tasks: Some(tasks),
            subagents: None,
            skills: Catalog::default(),
            agents: crate::subagent::Catalog::default(),
            mcp: Arc::new(crate::mcp::Servers::default()),
        }
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

/// The composition root.
///
/// Optional seams: memory, skills, sub-agents, ChatGPT credentials, and the
/// `OTTO_TRACE` writer. None of them changes the order below; each only appends
/// tools or wraps the client.
pub struct Builder {
    pub config_path: PathBuf,
    pub config: File,
    pub environment: HashMap<String, String>,
    /// The resolved home directory, used to locate MCP OAuth token files
    /// (`crate::mcp::oauth::token_path`).
    pub home: String,
    /// Leaked for the process lifetime so the file tools, which borrow it,
    /// satisfy the registry's `'static` bound. See `leaked_workspace`.
    pub workspace: &'static Workspace,
    pub workspace_path: String,
    pub session_root: PathBuf,
    pub shell: String,
    pub no_session: bool,
    pub overrides: Overrides,
    pub command_executor: Option<Arc<dyn CommandExecutor>>,
    pub bash_approvals: Option<Arc<bash::BashApprovals>>,
    pub sandbox_environment: Option<Vec<String>>,
    pub sandbox_info: SandboxInfo,
    pub sandbox_secrets: Vec<String>,
    pub sandbox_secrets_complete: bool,
    /// The captured `~/.otto/auth/chatgpt.json`.
    pub auth_path: String,
    /// The captured credentials, valid only when loaded is true.
    pub auth_credentials: crate::auth::Credentials,
    pub auth_credentials_loaded: bool,
    /// The process-wide memory service and its two scopes. The default is a
    /// null service reporting memory as disabled.
    pub memory: super::wiring::MemoryWiring,
    /// Process-wide append-only token usage storage. `None` keeps usage
    /// collection from affecting an otherwise usable runtime.
    pub usage: Option<Arc<crate::usage::Store>>,
    /// The resolved `[mcp]` configuration. `connect_mcp` reads this at
    /// `build_runner` time; the servers themselves are not connected until
    /// then.
    pub mcp: McpRuntime,
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

    pub(crate) fn usage_collector(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
    ) -> Option<crate::usage::Collector> {
        self.usage.as_ref().map(|store| {
            let header = session.header();
            crate::usage::Collector::new(
                Arc::clone(store),
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
            .map(|tool| tool.definition())
            .collect();
        definitions.extend(self.boundary_memory_definitions(max_output));
        if self.planned_bash_available() {
            definitions.push(match self.bash_approvals.is_some() {
                true => bash::bash_definition_with_approvals(),
                false => bash::bash_definition(),
            });
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
        let prompt = system_prompt_for(
            &definitions,
            self.planned_sandbox_info(),
            provider,
            &endpoint_host,
            model,
        );
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

    /// Composes one runnable agent.
    pub async fn build_runner(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
    ) -> Result<Runner, BuildError> {
        self.build_runner_inner(session, runtime, None, true)
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
        self.build_runner_inner(session, runtime, trace, true).await
    }

    pub async fn build_runner_without_mcp_with_trace(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
        trace: bool,
    ) -> Result<(Runner, Vec<(&'static str, Duration)>), BuildError> {
        let trace = trace.then(BuildTrace::new);
        self.build_runner_inner(session, runtime, trace, false)
            .await
    }

    async fn build_runner_inner(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
        mut trace: Option<BuildTrace>,
        connect_mcp: bool,
    ) -> Result<(Runner, Vec<(&'static str, Duration)>), BuildError> {
        let redaction_values = self.secret_values(Some(runtime));
        let max_output = output_cap(runtime.max_output_bytes);
        let mut tools = self.builtin_file_tools(max_output);
        if self.bash_configured() {
            let executor = self
                .command_executor
                .clone()
                .expect("bash_configured implies an executor");
            let mut tool = bash::BashTool::new(
                self.workspace,
                executor,
                &self.shell,
                self.sandbox_environment.clone().unwrap_or_default(),
                shell_timeout(runtime.shell_timeout),
                max_output,
                &redaction_values,
            )
            .map_err(|error| format!("create bash tool: {error}"))?;
            if let Some(approvals) = &self.bash_approvals {
                tool = tool.with_approvals(session.header().id, Arc::clone(approvals));
            }
            tools.push(Box::new(tool));
        }
        let mut warnings = std::io::stderr();
        if self.memory_usable() && self.boundary_allows_dynamic(Some(runtime)) {
            tools.extend(self.memory_tools(max_output));
        }
        mark_build_trace(&mut trace, "runner/setup");
        let catalogs = self.build_catalogs(&mut tools, max_output, &mut warnings)?;
        mark_build_trace(&mut trace, "runner/catalogs");
        let (mcp_tools, mcp_connected, mcp_servers) = if connect_mcp {
            self.connect_mcp(max_output, &mut warnings).await
        } else {
            (Vec::new(), Vec::new(), self.connecting_mcp_servers())
        };
        tools.extend(mcp_tools);
        mark_build_trace(&mut trace, "runner/mcp");

        let redactor = self.boundary_redactor(Some(runtime));
        let client = if !self.boundary_allows_dynamic(Some(runtime)) {
            ProviderClient::Unavailable
        } else if runtime.provider == otto_core::config::PROVIDER_CHATGPT {
            ProviderClient::ChatGpt(Arc::new(super::login::chatgpt_client(
                &self.auth_path,
                &self.auth_credentials,
                self.auth_credentials_loaded,
            )?))
        } else {
            ProviderClient::Compat(Arc::new(Client::new(&runtime.base_url, &runtime.api_key)))
        };
        mark_build_trace(&mut trace, "runner/provider");

        // The workspace context runs `git status` through the sandbox, so it
        // may only reach the executor when both the boundary is open and a
        // bash tool exists to hold it.
        let context_executor = if redactor.allows_dynamic_content() && self.bash_configured() {
            self.command_executor.as_ref()
        } else {
            None
        };
        let environment = workspace_context_for(
            &self.workspace_path,
            Utc::now(),
            context_executor,
            self.sandbox_environment.as_deref(),
            self.workspace,
        )
        .await;
        let prompt_tail = redactor.redact_string(&(environment.clone() + &catalogs.skill_section));
        mark_build_trace(&mut trace, "runner/workspace-context");
        let parent_agent_section = redactor.redact_string(&catalogs.agent_section);
        let endpoint_host = boundary::endpoint_host_for(&runtime.base_url);
        let mut child_tools =
            self.child_tools(runtime, max_output, &redaction_values, &catalogs.skills)?;
        child_tools.extend(super::wiring::mcp_child_tools(&mcp_connected, max_output));
        let subagents = self.build_subagents(
            &mut tools,
            &catalogs,
            &client,
            &redaction_values,
            &redactor,
            runtime,
            session,
            self.child_prompt_for(runtime, &endpoint_host, &prompt_tail),
            child_tools,
            &mut warnings,
        )?;
        mark_build_trace(&mut trace, "runner/subagents");

        let registry =
            Registry::new(tools).map_err(|error| format!("create tool registry: {error}"))?;
        let definitions = registry.definitions();
        let base_prompt = system_prompt_for(
            &definitions,
            self.effective_sandbox_info(),
            &runtime.provider,
            &endpoint_host,
            &runtime.model,
        );
        let system_prompt = base_prompt.clone() + &prompt_tail + &parent_agent_section;
        let (environment, instructions) = split_workspace_instructions(&environment);
        let system_prompt_parts: Vec<(String, String)> = [
            ("Base", base_prompt),
            ("Environment", redactor.redact_string(environment)),
            (
                "Workspace instructions",
                redactor.redact_string(instructions),
            ),
            ("Skills", redactor.redact_string(&catalogs.skill_section)),
            ("Agents", parent_agent_section),
        ]
        .into_iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(label, text)| (label.to_string(), text))
        .collect();

        let request_sizer = match &client {
            ProviderClient::Compat(client) => {
                Some(client.clone() as Arc<dyn RequestSizer + Send + Sync>)
            }
            ProviderClient::ChatGpt(client) => {
                Some(client.clone() as Arc<dyn RequestSizer + Send + Sync>)
            }
            ProviderClient::Unavailable => None,
            #[cfg(test)]
            ProviderClient::Scripted(_) => None,
        };
        let options = Options {
            model: runtime.model.clone(),
            provider_name: runtime.provider.clone(),
            system_prompt: system_prompt.clone(),
            system_prompt_parts,
            thinking: runtime.thinking.clone(),
            now: Box::new(Utc::now),
            request_sizer,
            compaction: CompactionSettings {
                auto: runtime.compaction.auto,
                hard_input_window: runtime.compaction.hard_input_window,
                working_window: runtime.compaction.working_window,
                reserve_tokens: runtime.compaction.reserve_tokens,
                keep_recent_tokens: runtime.compaction.keep_recent_tokens,
            },
            memory: match self.memory_usable() && self.boundary_allows_dynamic(Some(runtime)) {
                true => Some(Arc::new(self.bind_memory()?)),
                false => None,
            },
            memory_recall_limit: self.memory.recall_limit,
            memory_recall_token_budget: self.memory.recall_token_budget,
            tasks: subagents
                .tasks
                .clone()
                .map(|tasks| tasks as Arc<dyn otto_core::agent::tasks::TaskRegistry + Send + Sync>),
            inbox: subagents.inbox.unwrap_or_default(),
            ..Options::default()
        };
        mark_build_trace(&mut trace, "runner/registry-and-options");
        let trace_entries = trace.map(|trace| trace.entries).unwrap_or_default();
        Ok((
            Runner {
                agent: Agent::with_redactor(client, registry, session.clone(), options, redactor),
                system_prompt,
                definitions,
                usage: self.usage_collector(session, runtime),
                tasks: subagents.tasks,
                subagents: subagents.runner,
                reminders: subagents.reminders,
                skills: catalogs.skills.clone(),
                agents: catalogs.agent_catalog.clone(),
                mcp: mcp_servers,
            },
            trace_entries,
        ))
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
        resolve_initial_runtime(
            &self.config,
            &resume_environment(&self.environment),
            Some(metadata),
            &self.overrides,
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
        let store = Store::create_lazy(&self.session_root, header)
            .map_err(|error| self.redact_error(&error.to_string(), Some(runtime)))?;
        Ok(SharedSession::new(Arc::new(store)))
    }
}

/// The environment a session replacement resolves against. The four "pick a
/// runtime" variables are dropped so a replacement keeps the session's own
/// provider, profile and model instead of silently re-reading the process
/// environment.
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

/// A resumed session's stored provider and model win over its profile's, unless
/// `--profile` was given explicitly.
pub fn resolve_initial_runtime(
    file: &File,
    environment: &HashMap<String, String>,
    metadata: Option<&RuntimeMetadata>,
    overrides: &Overrides,
) -> Result<Runtime, ConfigError> {
    let Some(metadata) = metadata else {
        return otto_core::config::resolve::resolve(
            file,
            environment,
            &SessionDefaults::default(),
            overrides,
        );
    };
    let file = config_for_session_runtime(file, &metadata.profile, !overrides.profile.is_empty());
    otto_core::config::resolve::resolve(
        &file,
        environment,
        &SessionDefaults {
            provider: metadata.provider.clone(),
            model: metadata.model.clone(),
        },
        overrides,
    )
}

/// Selects the stored profile and blanks its provider and model so the session
/// defaults are what fills them in.
fn config_for_session_runtime(file: &File, stored_profile: &str, explicit_profile: bool) -> File {
    if explicit_profile {
        return file.clone();
    }
    let mut copy = file.clone();
    if !stored_profile.is_empty() {
        copy.default_profile = stored_profile.to_string();
    }
    let selected = copy.default_profile.clone();
    if selected.is_empty() {
        return copy;
    }
    if let Some(profile) = copy.profiles.get_mut(&selected) {
        profile.provider = String::new();
        profile.model = String::new();
    }
    copy
}

/// A session may only be resumed from the directory it was created in.
pub fn validate_session_workspace(session_workspace: &str, workspace: &str) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::direct::DirectDriver;
    use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy};
    use otto_core::config::Profile;
    use std::time::Duration;

    fn builder(root: &Path) -> Builder {
        let workspace = leaked_workspace(root).expect("workspace");
        Builder {
            config_path: root.join("config.toml"),
            config: File::default(),
            environment: HashMap::new(),
            home: root.to_string_lossy().into_owned(),
            workspace,
            workspace_path: workspace.root().to_string_lossy().into_owned(),
            session_root: root.join("sessions"),
            shell: "/bin/sh".to_string(),
            no_session: true,
            auth_path: String::new(),
            auth_credentials: crate::auth::Credentials::default(),
            auth_credentials_loaded: false,
            overrides: Overrides::default(),
            command_executor: None,
            bash_approvals: None,
            sandbox_environment: None,
            sandbox_info: SandboxInfo::unavailable(SandboxReason::SeatbeltMissing),
            sandbox_secrets: Vec::new(),
            sandbox_secrets_complete: true,
            memory: Default::default(),
            usage: None,
            mcp: McpRuntime {
                enabled: false,
                call_timeout_secs: 60,
                connect_timeout_secs: 20,
                servers: Vec::new(),
            },
        }
    }

    fn with_bash(builder: &mut Builder, root: &Path) {
        let executor = Executor::new(
            Arc::new(DirectDriver::new()),
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Allow,
            },
            root,
        )
        .expect("executor");
        builder.command_executor = Some(Arc::new(executor));
        builder.sandbox_environment = Some(vec!["PATH=/usr/bin:/bin".to_string()]);
        builder.sandbox_info = SandboxInfo {
            mode: SandboxMode::Off,
            network: SandboxNetwork::Unconfined,
            bash_available: true,
            reason: SandboxReason::None,
        };
    }

    fn runtime() -> Runtime {
        Runtime {
            profile: "default".to_string(),
            provider: "openai-compatible".to_string(),
            base_url: "https://gw.example.com/v1".to_string(),
            model: "gpt-test".to_string(),
            api_key: "sk-secret-value".to_string(),
            api_key_env: "OTTO_API_KEY".to_string(),
            shell_timeout: Duration::from_secs(30),
            max_output_bytes: 65536,
            ..Runtime::default()
        }
    }

    fn tool_names(runner: &Runner) -> Vec<String> {
        runner
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect()
    }

    #[tokio::test]
    async fn builtin_file_tools_are_registered_in_a_fixed_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let builder = builder(dir.path());
        let session = SharedSession::memory(Header::default());
        let runner = builder
            .build_runner(&session, &runtime())
            .await
            .expect("runner");
        assert_eq!(
            tool_names(&runner),
            vec![
                "read",
                "grep",
                "find",
                "ls",
                "write",
                "edit",
                "agent",
                "agent_wait",
                "agent_status",
                "remind",
                "remind_status",
                "remind_cancel"
            ]
        );
    }

    #[tokio::test]
    async fn build_runner_without_mcp_reports_connecting_and_skips_mcp_tools() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut builder = builder(dir.path());
        builder.mcp = otto_core::config::McpRuntime {
            enabled: true,
            call_timeout_secs: 5,
            connect_timeout_secs: 1,
            servers: vec![otto_core::config::McpServerRuntime {
                name: "slow".to_string(),
                enabled: true,
                transport: otto_core::config::McpTransport::Stdio {
                    command: "otto-mcp-test-nonexistent-command".to_string(),
                    args: Vec::new(),
                    env: Vec::new(),
                    cwd: dir.path().to_string_lossy().into_owned(),
                },
                secrets: Vec::new(),
            }],
        };
        let session = SharedSession::memory(Header::default());
        let (runner, _) = builder
            .build_runner_without_mcp_with_trace(&session, &runtime(), false)
            .await
            .expect("runner");

        let status = runner.mcp.status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].name, "slow");
        assert_eq!(status[0].state, crate::mcp::ServerState::Connecting);
        assert!(
            tool_names(&runner)
                .into_iter()
                .all(|name| !name.starts_with("mcp__"))
        );
    }

    #[tokio::test]
    async fn a_chatgpt_runtime_registers_remind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut builder = builder(dir.path());
        builder.auth_credentials_loaded = true;
        builder.auth_path = dir
            .path()
            .join("chatgpt.json")
            .to_string_lossy()
            .into_owned();
        builder.auth_credentials = crate::auth::Credentials {
            account_id: "acct".into(),
            access_token: "tok".into(),
            ..crate::auth::Credentials::default()
        };
        let mut runtime = runtime();
        runtime.provider = otto_core::config::PROVIDER_CHATGPT.to_string();
        let session = SharedSession::memory(Header::default());
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let names = tool_names(&runner);
        assert!(names.contains(&"remind".to_string()), "{names:?}");
        assert!(names.contains(&"agent".to_string()), "{names:?}");
    }

    #[tokio::test]
    async fn the_bash_tool_is_added_only_when_the_sandbox_is_usable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut builder = builder(dir.path());
        with_bash(&mut builder, dir.path());
        assert!(builder.bash_configured());
        let session = SharedSession::memory(Header::default());
        let runner = builder
            .build_runner(&session, &runtime())
            .await
            .expect("runner");
        let names = tool_names(&runner);
        assert_eq!(names.iter().position(|name| name == "bash"), Some(6));
    }

    #[tokio::test]
    async fn only_the_parent_bash_tool_advertises_temporary_elevation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut builder = builder(dir.path());
        with_bash(&mut builder, dir.path());
        let elevated = Executor::new(
            Arc::new(DirectDriver::new()),
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Allow,
            },
            dir.path(),
        )
        .expect("elevated executor");
        builder.bash_approvals = Some(Arc::new(bash::BashApprovals::new(
            Arc::new(elevated),
            vec!["HOME=/real-home".to_string()],
        )));
        let session = SharedSession::memory(Header {
            id: "session-1".to_string(),
            ..Header::default()
        });

        let runner = builder
            .build_runner(&session, &runtime())
            .await
            .expect("runner");
        let bash = runner
            .definitions()
            .into_iter()
            .find(|definition| definition.name == "bash")
            .expect("parent bash");
        assert!(
            bash.parameters
                .expect("schema")
                .get()
                .contains("require_escalated")
        );
    }

    #[test]
    fn planned_sandbox_info_downgrades_when_the_executor_is_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut builder = builder(dir.path());
        with_bash(&mut builder, dir.path());
        assert_eq!(builder.planned_sandbox_info(), builder.sandbox_info);

        builder.command_executor = None;
        let planned = builder.planned_sandbox_info();
        assert_eq!(planned.mode, SandboxMode::Unavailable);
        assert!(!planned.bash_available);
        assert_eq!(planned.reason, SandboxReason::RuntimeFailure);
        assert!(!builder.bash_configured());
    }

    #[test]
    fn effective_sandbox_info_reports_environment_rejected_when_the_boundary_is_closed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut builder = builder(dir.path());
        with_bash(&mut builder, dir.path());
        // A secret equal to the workspace path closes the boundary: redacting
        // it would rewrite fixed prompt text.
        builder.sandbox_secrets = vec![builder.workspace_path.clone()];
        assert!(!builder.boundary_allows_dynamic(None));
        let effective = builder.effective_sandbox_info();
        assert_eq!(effective.mode, SandboxMode::Unavailable);
        assert_eq!(effective.reason, SandboxReason::EnvironmentRejected);

        let info = builder.runtime_info(&runtime());
        assert_eq!(info.provider, "");
        assert_eq!(info.profile, "");
        assert_eq!(info.model, "");
        assert_eq!(info.context_window, 0);
    }

    #[tokio::test]
    async fn the_system_prompt_carries_the_workspace_context_and_hides_the_api_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("AGENTS.md"), "house rules\n").expect("write");
        let builder = builder(dir.path());
        let session = SharedSession::memory(Header::default());
        let runner = builder
            .build_runner(&session, &runtime())
            .await
            .expect("runner");
        let prompt = runner.system_prompt();
        assert!(prompt.starts_with("You are Otto, a concise coding agent."));
        assert!(prompt.contains(
            "Usable tools: read, grep, find, ls, write, edit, agent, agent_wait, agent_status, remind, remind_status, remind_cancel."
        ));
        assert!(prompt.contains("<workspace-instructions"), "{prompt}");
        assert!(prompt.contains("house rules"), "{prompt}");
        assert!(!prompt.contains("sk-secret-value"), "{prompt}");

        let report = runner.context_report();
        let parts = &report.sections[0].items;
        let labels: Vec<&str> = parts.iter().map(|part| part.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Base", "Environment", "Workspace instructions"],
            "the parts must concatenate to the prompt, or the report falls back to one part"
        );
        assert!(parts[2].text.contains("house rules"));
    }

    #[test]
    fn runtime_info_reports_the_resolved_runtime_when_the_boundary_is_open() {
        let dir = tempfile::tempdir().expect("temp dir");
        let builder = builder(dir.path());
        let mut runtime = runtime();
        runtime.compaction.context_window = 128_000;
        let info = builder.runtime_info(&runtime);
        assert_eq!(info.provider, "openai-compatible");
        assert_eq!(info.profile, "default");
        assert_eq!(info.model, "gpt-test");
        assert_eq!(info.context_window, 128_000);
        assert_eq!(info.sandbox.mode, SandboxMode::Unavailable);
    }

    #[test]
    fn resolve_initial_runtime_prefers_session_metadata_over_the_profile() {
        let mut file = File {
            default_profile: "work".to_string(),
            ..File::default()
        };
        file.profiles.insert(
            "work".to_string(),
            Profile {
                provider: "openai-compatible".to_string(),
                base_url: "https://gw.example.com/v1".to_string(),
                model: "profile-model".to_string(),
                api_key_env: "WORK_KEY".to_string(),
                ..Profile::default()
            },
        );
        let environment = HashMap::from([("WORK_KEY".to_string(), "sk-1".to_string())]);

        let fresh = resolve_initial_runtime(&file, &environment, None, &Overrides::default())
            .expect("fresh runtime");
        assert_eq!(fresh.model, "profile-model");

        let metadata = RuntimeMetadata {
            profile: "work".to_string(),
            provider: "openai-compatible".to_string(),
            model: "session-model".to_string(),
        };
        let resumed =
            resolve_initial_runtime(&file, &environment, Some(&metadata), &Overrides::default())
                .expect("resumed runtime");
        assert_eq!(resumed.model, "session-model");
        assert_eq!(resumed.profile, "work");

        let overrides = Overrides {
            profile: "work".to_string(),
            ..Overrides::default()
        };
        let overridden = resolve_initial_runtime(&file, &environment, Some(&metadata), &overrides)
            .expect("overridden runtime");
        assert_eq!(overridden.model, "profile-model");
    }

    #[test]
    fn validate_session_workspace_rejects_a_different_workspace() {
        let dir = tempfile::tempdir().expect("temp dir");
        let other = tempfile::tempdir().expect("temp dir");
        let canonical = canonical_directory(dir.path()).expect("canonical");
        let workspace = canonical.to_string_lossy().into_owned();

        validate_session_workspace(dir.path().to_str().expect("utf-8"), &workspace)
            .expect("same workspace");

        let error = validate_session_workspace(other.path().to_str().expect("utf-8"), &workspace)
            .expect_err("mismatch");
        assert!(error.starts_with("session workspace "), "{error}");
        assert!(error.ends_with("does not match cwd"), "{error}");

        let missing = validate_session_workspace(
            dir.path().join("nope").to_str().expect("utf-8"),
            &workspace,
        )
        .expect_err("missing");
        assert!(
            missing.starts_with("resolve session workspace: "),
            "{missing}"
        );
    }

    #[tokio::test]
    async fn a_closed_boundary_builds_no_provider_client() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut builder = builder(dir.path());
        builder.sandbox_secrets = vec![builder.workspace_path.clone()];
        let session = SharedSession::memory(Header::default());
        let runner = builder
            .build_runner(&session, &runtime())
            .await
            .expect("runner");
        assert!(matches!(runner.provider(), ProviderClient::Unavailable));
    }

    #[tokio::test]
    async fn a_memory_session_carries_its_header_and_name() {
        let session = SharedSession::memory(Header {
            id: "abc".to_string(),
            provider: "openai-compatible".to_string(),
            model: "gpt-test".to_string(),
            ..Header::default()
        });
        assert_eq!(session.header().id, "abc");
        assert_eq!(session.header().version, CURRENT_VERSION);
        assert_eq!(session.path(), "");
        assert_eq!(session.name(), "");
        session.rename("planning").expect("rename");
        assert_eq!(session.name(), "planning");
        session.rename("  ").expect_err("blank name");
        session
            .update_runtime(&RuntimeMetadata {
                profile: "work".to_string(),
                provider: "openai-compatible".to_string(),
                model: "gpt-next".to_string(),
            })
            .expect("update runtime");
        assert_eq!(session.header().model, "gpt-next");
    }

    #[test]
    fn usage_collector_is_bound_to_the_runtime_and_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut builder = builder(dir.path());
        let store = Arc::new(crate::usage::Store::open_in_memory().expect("usage store"));
        builder.usage = Some(Arc::clone(&store));
        let runtime = runtime();
        let session = SharedSession::memory(Header {
            id: "session-1".into(),
            workspace: builder.workspace_path.clone(),
            ..Header::default()
        });

        builder
            .usage_collector(&session, &runtime)
            .expect("collector")
            .record(&otto_core::agent::Event::ProviderUsage {
                usage: otto_core::model::Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    cached_input_tokens: 4,
                },
                present: true,
            })
            .expect("record");

        let summary = store.summary(Some("session-1")).expect("summary");
        assert_eq!(summary.input_tokens, 10);
        assert_eq!(summary.cached_input_tokens, 4);
    }

    /// A provider `Runner::scripted` needs a value for but this test never
    /// calls: `close_mcp`/`close` never send a turn.
    struct UnusedProvider;

    #[async_trait::async_trait]
    impl Provider for UnusedProvider {
        async fn complete(
            &self,
            _request: &Request,
            _emit: StreamSink<'_>,
            _cancel: &CancellationToken,
        ) -> Result<Response, ProviderError> {
            unreachable!("close-ordering test never sends a turn")
        }
    }

    /// Regression test for the close-ordering rule in the doc comments on
    /// [`Runner::close`] and [`Runner::close_mcp`]: `Servers::close` uses
    /// `mem::take`, so calling `close_mcp` (awaited) before `close`
    /// (backgrounded) is what makes the awaited call the one that actually
    /// closes the client, instead of racing `close`'s spawned task.
    #[tokio::test]
    async fn close_mcp_before_close_actually_waits_for_the_real_close() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let closed = Arc::new(AtomicBool::new(false));
        let client = crate::mcp::test_support::connected_client("fake", Arc::clone(&closed)).await;
        let mcp = crate::mcp::Servers::default();
        mcp.push(
            crate::mcp::ServerStatus {
                name: "fake".to_string(),
                transport: "stdio",
                era: Some(crate::mcp::Era::Modern),
                state: crate::mcp::ServerState::Connected { tools: 0 },
            },
            Some(Arc::new(client)),
        );

        let runner = Runner::scripted(
            SharedSession::memory(Header::default()),
            Arc::new(UnusedProvider) as Arc<dyn Provider + Send + Sync>,
            Arc::new(crate::subagent::tasks::Tasks::new()),
        )
        .with_mcp(Arc::new(mcp));

        runner.close_mcp().await;
        assert!(
            closed.load(Ordering::SeqCst),
            "close_mcp did not wait for the real close"
        );

        // `close`'s background spawn now finds `Servers` already emptied by
        // `mem::take`; it must be a harmless no-op, not a panic or a
        // double-close.
        runner.close();
    }
}
