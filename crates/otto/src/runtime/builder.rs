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
use crate::runtime::verification;
use crate::sandbox::CommandExecutor;
pub use crate::session::SessionHandle;
#[cfg(test)]
use crate::session::Store;
use crate::skill::Catalog;
use crate::tool::registry::Registry;
use crate::tool::{Tool, bash, edit, find, grep, ls, models, read, write};
use crate::workspace::confined::Workspace;

use super::boundary::{self, BoundaryInputs, FixedText};
use super::prompt::{subagent_model_guidance, system_prompt_for};
use super::workspace_context::{layered_workspace_context, split_workspace_instructions};
use crate::fsops::canonical_directory;
use crate::sandbox::info::{SandboxInfo, SandboxMode, SandboxNetwork, SandboxReason};

mod builder_inputs;
mod memory_session;
mod provider_client;
mod runner;
mod runner_assembly;
mod runner_authorization;
mod runner_entry;
mod session_lifecycle;
mod shared_session;

use provider_client::*;

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
pub(crate) type BuildError = String;

/// Everything a frontend shows about the resolved runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeInfo {
    /// The provider name from the selector.
    pub provider: String,
    /// The API type of the provider.
    pub provider_type: String,
    /// The `<provider>/<model_id>` selector.
    pub model_selector: String,
    /// The provider model ID.
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
    input_submission_events: Vec<String>,
    local_commands: Vec<otto_core::session::local_command::LocalCommandReceipt>,
}

/// A session shared by the agent and the frontend: a cloneable handle to one
/// `SessionState`.
///
/// Both need `&self` access to the same transcript for the whole run, which
/// is what `Arc` buys; every [`SessionHandle`] method already takes `&self`.
#[derive(Clone)]
pub struct SharedSession(Arc<SessionState>);

/// A non-owning reference to a session, held by background workers that must
/// not keep it alive.
pub(crate) struct WeakSharedSession(std::sync::Weak<SessionState>);

/// Everything one session owns. A runner borrows it and never closes it; see
/// [`super::session_resources`].
pub(crate) struct SessionState {
    transcript: Arc<dyn SessionHandle>,
    hooks: Arc<otto_core::agent::hooks::State>,
    /// Created by the first runner build for this session
    /// ([`Builder::session_resources`]); every later build reuses it.
    resources: std::sync::OnceLock<Arc<super::session_resources::SessionResources>>,
    /// Serializes the first creation of `resources` across concurrent builds.
    resources_creation: Mutex<()>,
}

impl WeakSharedSession {
    pub(crate) fn upgrade(&self) -> Option<SharedSession> {
        self.0.upgrade().map(SharedSession)
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
    Claude {
        client: Arc<crate::provider::claude::Client>,
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
    #[cfg(any(test, feature = "test-support"))]
    Scripted(Arc<dyn Provider + Send + Sync>),
}

/// One composed agent, plus the two fixed strings a frontend may show.
/// The request sizer a [`Runner::scripted`] runner uses: the bytes of text, so
/// compaction can size its summary request without a real provider client.
#[cfg(any(test, feature = "test-support"))]
struct ScriptedSizer;

/// Test providers keyed by home directory: a runner built by a [`Builder`]
/// whose `home` is listed uses that provider, so a test can drive the full
/// composition without a network and without affecting tests that run
/// concurrently under other homes.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_PROVIDERS: std::sync::Mutex<Vec<(String, Arc<dyn Provider + Send + Sync>)>> =
    std::sync::Mutex::new(Vec::new());

/// Builder homes whose scripted provider also backs a sub-agent runner.
/// Without an entry here a scripted runner has no sub-agent executor.
#[cfg(any(test, feature = "test-support"))]
pub(crate) static TEST_SCRIPTED_SUBAGENT_HOMES: std::sync::Mutex<Vec<String>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(any(test, feature = "test-support"))]
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
    /// The memory client of this runner; `/remember` uses it.
    pub(crate) memory_client: Option<Arc<crate::memory::client::Client>>,
    /// The lifecycle proposal generator over the advice backend; `/memory
    /// propose` uses it. Absent without a usable native memory store.
    pub(crate) memory_proposals: Option<Arc<crate::memory::proposals::Generator>>,
    authorization_run: Arc<Mutex<String>>,
    approvals: Option<(Arc<crate::approval::Service>, otto_core::approval::Scope)>,
    /// Reviewer registered for this runner; `close` unregisters it.
    review_provider: Option<Arc<dyn crate::approval::reviewer::ReviewProvider>>,
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
    /// The child executor shared with durable workflows.
    pub(crate) subagents: Option<Arc<crate::subagent::runner::Runner>>,
    /// The skills discovered for this runner. `/skills` and `/skill` display this fixed catalog.
    pub(crate) skills: Catalog,
    pub(crate) discovered_skills: Catalog,
    /// Named child definitions used by durable workflow discovery.
    pub(crate) agents: crate::subagent::Catalog,
    /// Status rows shown until the session's MCP servers are connected: set
    /// when this runner was built without connecting MCP. Once the session
    /// holds connected servers, [`Runner::mcp`] reads those instead.
    connecting_mcp: Option<Arc<crate::mcp::Servers>>,
    pub(crate) task_control: Arc<crate::runtime::task_control::TaskControl>,
    /// The origin of the current user turn, read by the schedule tools.
    pub(crate) schedule_origin: Arc<crate::session::origin::TurnOrigin>,
    /// Exact passive-presentation origin for the current logical user turn.
    /// Unlike scheduling, a wake turn clears this instead of inheriting it.
    pub(crate) presentation_origin: Arc<crate::session::origin::TurnOrigin>,
}

/// The tool-less agent behind the session-model advice backend.
pub(crate) type AdviceAgent = Agent<ProviderClient, Registry, SharedSession>;

/// Everything the composition root builds once per process, independent of
/// any one workspace: config, environment, provider-runtime inputs, auth,
/// usage, task recording, the skill checker, and the session root. `Builder`
/// holds one of these behind an `Arc` and adds what one workspace needs.
///
/// The split exists because a server process that later loads
/// more than one workspace builds this once and calls
/// `Builder::for_workspace` once per workspace.
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
    /// `Builder::session_resources` from recording task history, matching today's
    /// behaviour; `Builder::session_resources` injects it into every `Tasks` registry it
    /// builds.
    pub task_recorder: Option<Arc<dyn crate::subagent::record::TaskRepository>>,
    /// The scheduled-delivery store (`~/.otto/schedule.db`). `None` when it
    /// could not be opened; the schedule tools then report it as unavailable.
    pub schedule: Option<Arc<dyn crate::schedule::ScheduleStore>>,
    /// The automatic skill contract check. `None` when the feature is disabled
    /// or its database could not be opened; either way
    /// `Builder::session_resources` wires no checker in and delegation is unaffected.
    pub skill_checker: Option<Arc<crate::skill::check::Checker>>,
    /// The process-wide memory service, its user scope, and the recall
    /// limits: one file store for the whole process, shared by every
    /// loaded workspace. Each workspace's `Builder` keeps only its own
    /// `workspace_scope`.
    pub memory: super::wiring::MemoryWiring,
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn installed(&self) -> bool {
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
    pub(crate) workflows: tokio::sync::OnceCell<Arc<crate::workflow::Controller>>,
    /// Everything built once per process. Field access on `Builder` reaches
    /// these through `Deref`, so `self.config`, `self.environment`, and the
    /// rest of `Shared`'s fields read the same as before the split.
    pub shared: Arc<Shared>,
    pub workspace: Arc<Workspace>,
    pub workspace_path: String,
    pub command_executor: Option<Arc<dyn CommandExecutor>>,
    pub bash_approvals: Option<Arc<bash::BashApprovals>>,
    pub grant_control: Option<Arc<dyn crate::sandbox::control::SandboxControl>>,
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
    /// already-built [`Shared`]. `otto_cli::run` calls this once per process, for
    /// the TUI, REPL, and serve alike.
    ///
    /// ponytail: `workspace` and `mcp` are passed in already resolved rather
    /// than resolved here, because `otto_cli::run` resolves them at specific
    /// points in its startup sequence to keep today's error-reporting order
    /// (the workspace lease happens before the archive/resume paths that
    /// never use it; mcp resolution happens before the usage and
    /// task-recorder stores open). A later slice that loads a second
    /// workspace at runtime has no such fixed sequence to preserve and
    /// should have `for_workspace` resolve the workspace and `resolve_mcp`
    /// itself.
    pub(crate) fn for_workspace(
        shared: Arc<Shared>,
        workspace: Arc<Workspace>,
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
    /// (`otto_cli::run`), before any second workspace clones the `Arc`; test
    /// fixtures use it the same way, to tweak a field on a freshly built
    /// `Shared` before use. Once a workspace load clones `shared`, this
    /// panics rather than letting one `Builder` silently rewrite state every
    /// other loaded workspace already reads.
    pub fn shared_mut(&mut self) -> &mut Shared {
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

/// Environment for an explicit selection change or active-runtime rebuild.
/// Drop runtime selectors so they cannot override the selection already chosen
/// by the caller.
pub(crate) fn resume_environment(environment: &HashMap<String, String>) -> HashMap<String, String> {
    environment
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "OTTO_PROVIDER" | "OTTO_MODEL" | "OTTO_UI"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// A fresh opaque operation identity for durable tool-attempt facts.
pub(crate) fn new_operation_id() -> Result<otto_core::model::OperationId, String> {
    let random = crate::random::random_id()
        .map_err(|error| format!("read operation randomness: {error}"))?;
    otto_core::model::OperationId::new(format!("op_{random}")).map_err(|error| error.to_string())
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
