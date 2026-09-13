//! Composition of one runnable agent from resolved configuration.
//!
//! Port of the `buildRunner` half of `cmd/otto/runtime_builder.go`. Memory,
//! skills, sub-agents, ChatGPT credentials, and the HTTP trace writer are
//! phases 5 to 7; the seams for them are named below and nothing else about
//! the composition order changes when they arrive.
//!
//! Safety: the redaction boundary decides everything. `boundary_redactor`
//! must leave the workspace path, the tool definitions, and the system prompt
//! byte-identical, or the whole run degrades to a closed boundary: no
//! provider client, no bash tool, no runtime identity in the status line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use otto_core::agent::redactor::Redactor;
use otto_core::agent::{
    Agent, AgentError, CompactionResult, CompactionSettings, EventSink, Options,
};
use otto_core::config::resolve::{Overrides, Runtime, SessionDefaults};
use otto_core::config::{ConfigError, File};
use otto_core::model::{Message, ToolDefinition};
use otto_core::provider::{Provider, ProviderError, Request, RequestSizer, Response, StreamSink};
use otto_core::session::{
    CURRENT_VERSION, CompactionCheckpoint, CompactionMetadata, Header, MemorySession,
    RuntimeMetadata, Session, SessionError,
};
use otto_core::tool::ToolExecutor;
use tokio_util::sync::CancellationToken;

use crate::provider::openaicompat::Client;
use crate::sandbox::CommandExecutor;
use crate::session::Store;
use crate::tool::registry::Registry;
use crate::tool::workspace::Workspace;
use crate::tool::{Tool, bash, edit, find, grep, ls, read, write};

use super::boundary::{self, BoundaryInputs, FixedText};
use super::info::{SandboxInfo, SandboxMode, SandboxNetwork, SandboxReason};
use super::prompt::system_prompt_for;
use super::sandbox_runtime::canonical_directory;
use super::workspace_context::workspace_context_for;

/// Every failure here is already redacted text, so a `String` carries all a
/// caller may show. Port of Go's `redactedErrorMessage` discipline.
pub type BuildError = String;

/// Everything a frontend shows about the resolved runtime. Port of
/// `app.RuntimeInfo`; `internal/app` itself is phase 6.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub context_window: i64,
    pub sandbox: SandboxInfo,
}

/// The session operations a frontend needs beyond the transcript itself.
/// Port of the wider `session.Session` interface in Go.
pub trait SessionHandle: Session + Send + Sync {
    fn header(&self) -> Header;
    fn name(&self) -> String;
    fn path(&self) -> String;
    fn rename(&self, name: &str) -> Result<(), String>;
    fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String>;
    fn close(&self) -> Result<(), String>;
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

    fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String> {
        Store::update_runtime(self, runtime).map_err(|error| error.to_string())
    }

    fn close(&self) -> Result<(), String> {
        Store::close(self).map_err(|error| error.to_string())
    }
}

/// A transcript that is never written to disk. Port of `session.Memory`,
/// which `--no-session` selects; `otto_core::session::MemorySession` carries
/// the transcript and this wrapper carries the header and the name.
pub struct MemoryHandle {
    inner: MemorySession,
    state: Mutex<MemoryState>,
}

struct MemoryState {
    header: Header,
    name: String,
}

impl MemoryHandle {
    pub fn new(mut header: Header) -> Self {
        header.version = CURRENT_VERSION;
        Self {
            inner: MemorySession::new(),
            state: Mutex::new(MemoryState {
                header,
                name: String::new(),
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

    /// An in-memory session has no file, matching Go's empty `Path`.
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

    pub fn update_runtime(&self, runtime: &RuntimeMetadata) -> Result<(), String> {
        self.0.update_runtime(runtime)
    }

    pub fn close(&self) -> Result<(), String> {
        self.0.close()
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

/// The provider an agent calls, or the refusal used when the redaction
/// boundary is closed.
///
/// Go stores a nil `provider.Provider` there and the agent never reaches it,
/// because `Run` checks the redactor first. This enum keeps that shape
/// without an `Option`, so the agent's type parameter stays concrete.
pub enum ProviderClient {
    Compat(Arc<Client>),
    ChatGpt(Arc<crate::provider::chatgpt::Client>),
    Unavailable,
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
        }
    }
}

/// One composed agent, plus the two fixed strings a frontend may show.
pub struct Runner {
    agent: Agent<ProviderClient, Registry, SharedSession>,
    system_prompt: String,
    definitions: Vec<ToolDefinition>,
}

impl Runner {
    /// Runs one turn. Port of `app.Runner.Run`.
    pub async fn run(
        &self,
        user_text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.agent.run(user_text, emit, cancel).await
    }

    /// Compacts the transcript. Port of `app.Runner.Compact`.
    pub async fn compact(
        &self,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, AgentError> {
        self.agent.compact(focus, emit, cancel).await
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

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    /// Releases the agent's own resources. The session is closed separately,
    /// by whoever owns it.
    pub fn close(&self) {
        let _ = self.agent.close();
    }
}

/// The composition root. Port of Go's `runtimeBuilder`.
///
/// Seams left for later phases: memory (`memory_*` fields in Go), skills,
/// sub-agents, ChatGPT credentials, and the `OTTO_TRACE` writer. None of them
/// changes the order below; each only appends tools or wraps the client.
pub struct Builder {
    pub config_path: PathBuf,
    pub config: File,
    pub environment: HashMap<String, String>,
    /// Leaked for the process lifetime so the file tools, which borrow it,
    /// satisfy the registry's `'static` bound. See `leaked_workspace`.
    pub workspace: &'static Workspace,
    pub workspace_path: String,
    pub session_root: PathBuf,
    pub shell: String,
    pub no_session: bool,
    pub overrides: Overrides,
    pub command_executor: Option<Arc<dyn CommandExecutor>>,
    pub sandbox_environment: Option<Vec<String>>,
    pub sandbox_info: SandboxInfo,
    pub sandbox_secrets: Vec<String>,
    pub sandbox_secrets_complete: bool,
    /// Go's `runtimeBuilder.authPath`: the captured `~/.otto/auth/chatgpt.json`.
    pub auth_path: String,
    /// Go's `runtimeBuilder.authCredentials`, valid only when loaded is true.
    pub auth_credentials: crate::auth::Credentials,
    /// Go's `runtimeBuilder.authCredentialsLoaded`.
    pub auth_credentials_loaded: bool,
}

impl Builder {
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
    /// Port of `boundaryToolDefinitions`. The skill and sub-agent definitions
    /// Go also lists are phases 6 and 7; until those tools exist the set is
    /// the built-ins plus bash.
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
        if self.planned_bash_available() {
            definitions.push(bash::bash_definition());
        }
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

    /// Every secret form the run must hide. Port of `secretValues`.
    pub fn secret_values(&self, runtime: Option<&Runtime>) -> Vec<String> {
        boundary::boundary_secret_values(&self.boundary_inputs(), runtime).0
    }

    /// Whether a bash tool will actually be built. Port of `bashConfigured`.
    pub fn bash_configured(&self) -> bool {
        self.boundary_allows_dynamic(None) && self.planned_bash_available()
    }

    fn planned_bash_available(&self) -> bool {
        self.sandbox_info.bash_available
            && self.sandbox_environment.is_some()
            && self.command_executor.is_some()
    }

    /// Port of `plannedSandboxInfo`: a sandbox that reported bash available
    /// but left no executor behind is a runtime failure, not a usable one.
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

    /// Port of `effectiveSandboxInfo`: a closed boundary disables bash and
    /// says only that the environment was rejected.
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

    /// Port of `runtimeInfo`: a closed boundary reports no runtime identity,
    /// because provider, profile, and model are all model-visible text.
    pub fn runtime_info(&self, runtime: &Runtime) -> RuntimeInfo {
        let mut info = RuntimeInfo {
            provider: runtime.provider.clone(),
            profile: runtime.profile.clone(),
            model: runtime.model.clone(),
            context_window: runtime.compaction.context_window,
            sandbox: self.effective_sandbox_info(),
        };
        if !self.boundary_allows_dynamic(Some(runtime)) {
            info.provider = String::new();
            info.profile = String::new();
            info.model = String::new();
            info.context_window = 0;
        }
        info
    }

    fn builtin_file_tools(&self, max_output: usize) -> Vec<Box<dyn Tool + Send + Sync>> {
        vec![
            Box::new(read::ReadTool::new(self.workspace, max_output)),
            Box::new(grep::GrepTool::new(self.workspace, max_output)),
            Box::new(find::FindTool::new(self.workspace, max_output)),
            Box::new(ls::LsTool::new(self.workspace, max_output)),
            Box::new(write::WriteTool::new(self.workspace)),
            Box::new(edit::EditTool::new(self.workspace)),
        ]
    }

    /// Composes one runnable agent. Port of `buildRunner`.
    pub async fn build_runner(
        &self,
        session: &SharedSession,
        runtime: &Runtime,
    ) -> Result<Runner, BuildError> {
        let redaction_values = self.secret_values(Some(runtime));
        let max_output = output_cap(runtime.max_output_bytes);
        let mut tools = self.builtin_file_tools(max_output);
        if self.bash_configured() {
            let executor = self
                .command_executor
                .clone()
                .expect("bash_configured implies an executor");
            let tool = bash::BashTool::new(
                self.workspace,
                executor,
                &self.shell,
                self.sandbox_environment.clone().unwrap_or_default(),
                shell_timeout(runtime.shell_timeout),
                max_output,
                &redaction_values,
            )
            .map_err(|error| format!("create bash tool: {error}"))?;
            tools.push(Box::new(tool));
        }

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

        // The workspace context runs `git status` through the sandbox, so it
        // may only reach the executor when both the boundary is open and a
        // bash tool exists to hold it.
        let context_executor = if redactor.allows_dynamic_content() && self.bash_configured() {
            self.command_executor.as_ref()
        } else {
            None
        };
        let prompt_tail = redactor.redact_string(
            &workspace_context_for(
                &self.workspace_path,
                Utc::now(),
                context_executor,
                self.sandbox_environment.as_deref(),
                self.workspace,
            )
            .await,
        );

        let registry =
            Registry::new(tools).map_err(|error| format!("create tool registry: {error}"))?;
        let definitions = registry.definitions();
        let endpoint_host = boundary::endpoint_host_for(&runtime.base_url);
        let system_prompt = system_prompt_for(
            &definitions,
            self.effective_sandbox_info(),
            &runtime.provider,
            &endpoint_host,
            &runtime.model,
        ) + &prompt_tail;

        let request_sizer = match &client {
            ProviderClient::Compat(client) => {
                Some(client.clone() as Arc<dyn RequestSizer + Send + Sync>)
            }
            ProviderClient::ChatGpt(client) => {
                Some(client.clone() as Arc<dyn RequestSizer + Send + Sync>)
            }
            ProviderClient::Unavailable => None,
        };
        let options = Options {
            model: runtime.model.clone(),
            provider_name: runtime.provider.clone(),
            system_prompt: system_prompt.clone(),
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
            ..Options::default()
        };
        Ok(Runner {
            agent: Agent::with_redactor(client, registry, session.clone(), options, redactor),
            system_prompt,
            definitions,
        })
    }

    /// Port of `redactError`: the text a caller may print for `message`.
    ///
    /// A closed boundary yields the empty string, matching Go's
    /// `errRedactedRuntimeBoundary`, whose `Error()` is deliberately empty so
    /// no diagnostic escapes when the redaction set is incomplete.
    pub fn redact_error(&self, message: &str, runtime: Option<&Runtime>) -> String {
        let redactor = boundary::secret_redactor(&self.boundary_inputs(), runtime);
        if !redactor.allows_dynamic_content() {
            return String::new();
        }
        redactor.redact_string(message)
    }

    /// Port of `resolveSession`: resolves a replacement runtime from the
    /// provenance a session carries.
    pub fn resolve_session(&self, metadata: &RuntimeMetadata) -> Result<Runtime, BuildError> {
        resolve_initial_runtime(
            &self.config,
            &resume_environment(&self.environment),
            Some(metadata),
            &self.overrides,
        )
        .map_err(|error| self.redact_error(&error.to_string(), None))
    }

    /// Port of `buildProfileReplacement`'s resolve step: the named profile is
    /// an explicit override, so its own provider, model and base URL win.
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

    /// Port of `updateSessionRuntime`: records the resolved provenance on the
    /// session when it differs from the header's. A closed boundary writes
    /// nothing, because the values would be blanked anyway.
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

    /// Port of `newSession` in `cmd/otto/main.go`.
    ///
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

/// Port of `resumeEnvironment`: the environment a session replacement
/// resolves against. The four "pick a runtime" variables are dropped so a
/// replacement keeps the session's own provider, profile and model instead of
/// silently re-reading the process environment.
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

/// 16 random bytes from `/dev/urandom`, hex encoded. Port of `randomID`.
fn random_id() -> std::io::Result<String> {
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

/// Go passes `int`; a negative or oversized cap is clamped rather than
/// wrapped, and the tools reject zero themselves.
fn output_cap(max_output_bytes: i64) -> usize {
    usize::try_from(max_output_bytes).unwrap_or(0)
}

/// `BashTool::new` rejects a zero timeout; Go's `config.Resolve` never
/// produces one, and a negative `Duration` cannot exist in Rust.
fn shell_timeout(timeout: Duration) -> Duration {
    timeout
}

/// Port of `resolveInitialRuntime`: a resumed session's stored provider and
/// model win over its profile's, unless `--profile` was given explicitly.
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

/// Port of `configForSessionRuntime`: selects the stored profile and blanks
/// its provider and model so the session defaults are what fills them in.
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

/// Port of `validateSessionWorkspace`: a session may only be resumed from the
/// directory it was created in.
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
            sandbox_environment: None,
            sandbox_info: SandboxInfo::unavailable(SandboxReason::SeatbeltMissing),
            sandbox_secrets: Vec::new(),
            sandbox_secrets_complete: true,
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
    async fn builtin_file_tools_are_registered_in_the_go_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let builder = builder(dir.path());
        let session = SharedSession::memory(Header::default());
        let runner = builder
            .build_runner(&session, &runtime())
            .await
            .expect("runner");
        assert_eq!(
            tool_names(&runner),
            vec!["read", "grep", "find", "ls", "write", "edit"]
        );
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
        assert_eq!(tool_names(&runner).last().map(String::as_str), Some("bash"));
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
        assert!(prompt.contains("Usable tools: read, grep, find, ls, write, edit."));
        assert!(prompt.contains("<workspace-instructions"), "{prompt}");
        assert!(prompt.contains("house rules"), "{prompt}");
        assert!(!prompt.contains("sk-secret-value"), "{prompt}");
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
}
