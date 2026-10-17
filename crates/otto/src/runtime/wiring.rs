//! Memory, skill and sub-agent wiring for one runner build.
//!
//! The substantive code lives here so the shared composition root only gains
//! call sites.
//!
//! Three properties of this wiring:
//!
//! - The Turso store is the only backend and never encrypts at rest, so
//!   `require_encryption` always fails startup.
//! - The memory binding is created last, after the provider client, so there
//!   is no failure left to unwind.
//! - Tools are boxed and cannot be cloned, so the sub-agent runner gets its
//!   own instances of the same tools, built the same way.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use futures_util::future::join_all;
use otto_core::agent::progress::ProgressPolicy;
use otto_core::agent::redactor::Redactor;
use otto_core::config::agents::AgentsRuntime;
use otto_core::config::memory::MemoryRuntime;
use otto_core::config::{McpAuth, McpTransport, Runtime, resolve_agents, resolve_skills};
use otto_core::model::ToolDefinition;
use otto_core::safetext::dynamic_redaction_marker;
use tokio_util::sync::CancellationToken;

use super::builder::{BuildError, Builder, ProviderClient, SharedSession};
use super::info::SandboxMode;
use crate::failover;
use crate::mcp;
use crate::memory::files::import::import_turso;
use crate::memory::files::{FileStore, has_identity, write_import};
use crate::memory::guard::{CompositeGuard, DefaultGuard, ExactGuard};
use crate::memory::repository::MemoryOptions as StoreOptions;
use crate::memory::scope::new_workspace_scope;
use crate::memory::{
    BindOptions, Binding, ErrorKind, MAX_EXACT_GUARD_VALUE_BYTES, Scope, Service, new_id,
};
use crate::skill;
use crate::subagent;
use crate::subagent::runner::{
    ChildSession, Config as RunnerConfig, OptionsTemplate, PromptFor, Runner as SubagentRunner,
    Transcript,
};
use crate::subagent::tasks::Tasks;
use crate::tool::Tool;
use crate::tool::mcp::{McpCatalogEntry, McpTool, router_tools, tools_for};
use crate::tool::memory::{ForgetTool, MemorySearchTool, RememberTool};
use crate::tool::remind::{self, Reminders};
use crate::tool::result::redact_exact_text;
use crate::tool::skill::SkillTool;

/// The message logged for [`Builder::connect_mcp`]'s redaction-limit guard
/// (Finding 5b) so the test asserting on it and the production call site
/// cannot drift apart.
const SECRETS_EXCEED_REDACTION_LIMITS: &str =
    "secret values exceed the redaction limits (64 values, 8 KiB each, 16 KiB total)";

/// The process-wide memory service and the parts of it that do not depend on
/// one workspace. Lives on `Shared`, so every `Builder` a process loads reads
/// the same service: opening a second workspace must not open a second
/// memory store. The workspace scope stays on `Builder` (`Builder::for_workspace`),
/// since it is derived from that workspace's own path.
///
/// The default is a null service reporting memory as disabled, which every
/// operation answers without touching a store.
#[derive(Clone)]
pub struct MemoryExtension {
    pub organization_id: Option<String>,
    pub service: Arc<dyn otto_core::memory::MemoryService>,
    pub user_scope: otto_core::memory::Scope,
    pub mode: otto_core::memory::AuthorizationMode,
    pub learning_grant: Option<otto_core::memory::LearningGrant>,
    /// Explicit provider shutdown ownership. Borrowed services remain open when Otto closes.
    pub owned: bool,
}

impl MemoryExtension {
    /// Borrows a shared provider in reviewed mode. Otto closes only its local client.
    pub fn new(
        service: Arc<dyn otto_core::memory::MemoryService>,
        user_scope: otto_core::memory::Scope,
    ) -> Self {
        Self {
            organization_id: None,
            service,
            user_scope,
            mode: otto_core::memory::AuthorizationMode::ReviewedChanges,
            learning_grant: None,
            owned: false,
        }
    }

    /// Transfers shutdown responsibility to one Otto host. Do not transfer the same provider to multiple hosts.
    pub fn owned(
        service: Arc<dyn otto_core::memory::MemoryService>,
        user_scope: otto_core::memory::Scope,
    ) -> Self {
        Self {
            owned: true,
            ..Self::new(service, user_scope)
        }
    }

    /// Selects a host-authoritative tenant without breaking existing embedding constructors.
    pub fn with_organization(mut self, organization_id: impl Into<String>) -> Self {
        self.organization_id = Some(organization_id.into());
        self
    }

    pub fn with_learning(mut self, grant: otto_core::memory::LearningGrant) -> Self {
        self.mode = otto_core::memory::AuthorizationMode::DelegatedLearning;
        self.learning_grant = Some(grant);
        self
    }

    pub fn validate(&self) -> otto_core::memory::Result<()> {
        self.user_scope.validate()?;
        if self
            .organization_id
            .as_deref()
            .is_some_and(|id| !otto_core::memory::valid_opaque_id(id, 128))
        {
            return Err(otto_core::memory::Error::new(
                otto_core::memory::ErrorKind::InvalidInput,
            ));
        }
        self.client().map(|_| ())
    }

    pub(crate) fn client(&self) -> otto_core::memory::Result<crate::memory::client::Client> {
        use otto_core::memory::{AuthorizationMode, Error, ErrorKind};
        let client = match (self.mode, &self.learning_grant) {
            (AuthorizationMode::ReviewedChanges, None) => {
                crate::memory::client::Client::new(self.service.clone())
            }
            (AuthorizationMode::DelegatedLearning, Some(grant)) => {
                crate::memory::client::Client::with_learning(self.service.clone(), grant.clone())
            }
            _ => Err(Error::new(ErrorKind::InvalidInput)),
        }?;
        Ok(client.with_provider_shutdown(self.owned))
    }
}

#[derive(Clone)]
pub struct MemoryWiring {
    pub extension: Option<MemoryExtension>,
    pub client: Option<Arc<crate::memory::client::Client>>,
    pub service: Arc<Service>,
    /// Whether the service is backed by a live store, and so worth exposing to
    /// the agent.
    pub usable: bool,
    pub user_scope: Scope,
    pub recall_limit: i64,
    pub recall_token_budget: i64,
    /// The append-only advice audit log, absent when it could not be opened.
    pub advice_audit: Option<Arc<dyn crate::memory::advice::audit::AuditStore>>,
    /// Uploads finished turns to the remote memory service; absent for every
    /// other backend.
    pub turns: Option<Arc<crate::memory::remote::upload::Uploader>>,
}

fn neutral_scope(scope: &Scope) -> otto_core::memory::Scope {
    otto_core::memory::Scope {
        namespace: scope.namespace.clone(),
        id: scope.id.clone(),
    }
}

struct ServiceBinding {
    client: Arc<crate::memory::client::Client>,
    context: otto_core::memory::MemoryContext,
    closed: AtomicBool,
}

#[cfg(test)]
mod selected_memory_tests {
    use super::*;
    use otto_core::{agent::memory::MemoryRecall, memory as port, operation::OperationControl};

    #[derive(Default)]
    struct RecallOnly {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl port::MemoryService for RecallOnly {
        fn describe(&self) -> port::ServiceDescription {
            let mut description = port::ServiceDescription::new("independent-service".into());
            description
                .capabilities
                .push(port::Capability::new(port::Operation::Recall));
            description
        }
        async fn recall(
            &self,
            request: &port::RecallRequest,
            _: &dyn OperationControl,
        ) -> port::Result<port::RecallResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(port::RecallResult {
                items: vec![port::RecallItem {
                    remote: None,
                    reference: Some(port::MemoryRef {
                        organization_id: None,
                        service: "independent-service".into(),
                        scope: request.context.scopes[0].clone(),
                        id: "opaque-item".into(),
                        revision: None,
                    }),
                    content: port::Content::Text("Useful context".into()),
                    sources: vec![],
                    effective_at: None,
                    metadata: [("standing".into(), "true".into())].into(),
                }],
                used_tokens: None,
                coverage: port::Coverage::Complete,
            })
        }
    }

    fn context() -> port::MemoryContext {
        let user = port::Scope {
            namespace: "user".into(),
            id: "host-user".into(),
        };
        let workspace = port::Scope {
            namespace: "workspace".into(),
            id: "host-workspace".into(),
        };
        port::MemoryContext {
            organization_id: None,
            principal: "host-user".into(),
            scopes: vec![user, workspace.clone()],
            write_scope: Some(workspace),
            workspace_id: Some("host-workspace".into()),
            session_id: Some("host-session".into()),
            task_id: None,
            origin: port::Origin::Model,
        }
    }

    #[tokio::test]
    async fn selected_readonly_service_recalls_without_granting_standing_authority() {
        let provider = Arc::new(RecallOnly::default());
        let client = Arc::new(crate::memory::client::Client::new(provider.clone()).unwrap());
        let binding = ServiceBinding {
            client: client.clone(),
            context: context(),
            closed: AtomicBool::new(false),
        };
        let result = binding
            .recall(
                &otto_core::agent::memory::RecallRequest {
                    query: "context".into(),
                    limit: 4,
                    token_budget: 1000,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.records.len(), 1);
        assert!(!result.records[0].standing);
        binding.close().unwrap();
        let request = otto_core::agent::memory::RecallRequest {
            query: "context".into(),
            limit: 4,
            token_budget: 1000,
        };
        let error = binding
            .recall(&request, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "memory service closed");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "closed binding must not contact the provider"
        );
        let other = ServiceBinding {
            client: client.clone(),
            context: context(),
            closed: AtomicBool::new(false),
        };
        assert_eq!(
            other
                .recall(&request, &CancellationToken::new())
                .await
                .unwrap()
                .records
                .len(),
            1
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert!(
            !client.is_closed(),
            "closing one session must preserve shared memory"
        );
        let tools = crate::tool::memory_service::tools(client, context(), 4096);
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.definition().name)
                .collect::<Vec<_>>(),
            ["memory_search"]
        );
    }

    #[tokio::test]
    async fn selected_recall_enforces_host_budget_even_without_provider_token_count() {
        let client =
            Arc::new(crate::memory::client::Client::new(Arc::new(RecallOnly::default())).unwrap());
        let binding = ServiceBinding {
            client,
            context: context(),
            closed: AtomicBool::new(false),
        };
        let result = binding
            .recall(
                &otto_core::agent::memory::RecallRequest {
                    query: "context".into(),
                    limit: 4,
                    token_budget: 1,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(result.records.is_empty());
        assert_eq!(result.used_tokens, 0);
    }

    #[test]
    fn extensions_require_explicit_matching_mode_and_grant() {
        let mut extension =
            MemoryExtension::new(Arc::new(RecallOnly::default()), context().scopes[0].clone());
        assert!(!extension.owned);
        assert!(extension.validate().is_ok());
        extension.mode = port::AuthorizationMode::DelegatedLearning;
        assert!(extension.validate().is_err());
    }
}

#[async_trait::async_trait]
impl otto_core::agent::memory::MemoryRecall for ServiceBinding {
    async fn recall(
        &self,
        request: &otto_core::agent::memory::RecallRequest,
        cancel: &CancellationToken,
    ) -> Result<otto_core::agent::memory::RecallResult, otto_core::agent::memory::MemoryError> {
        use otto_core::{agent::memory as agent, memory as port};
        if self.closed.load(Ordering::SeqCst) {
            return Err(agent::MemoryError(
                port::Error::new(port::ErrorKind::Closed).to_string(),
            ));
        }
        let result = self
            .client
            .recall(
                &port::RecallRequest {
                    scope_filter: None,
                    context: self.context.clone(),
                    query: request.query.clone(),
                    limit: usize::try_from(request.limit)
                        .unwrap_or(0)
                        .min(port::MAX_ITEMS),
                    token_budget: usize::try_from(request.token_budget)
                        .unwrap_or(0)
                        .min(port::MAX_TOKEN_BUDGET),
                },
                cancel,
            )
            .await
            .map_err(|error| agent::MemoryError(error.to_string()))?;
        let records: Vec<_> = result
            .items
            .into_iter()
            .map(|item| {
                let scope = item
                    .reference
                    .as_ref()
                    .map(|reference| reference.scope.clone())
                    .unwrap_or_else(|| self.context.write_scope.clone().expect("workspace scope"));
                agent::Record {
                    id: item
                        .reference
                        .map(|reference| reference.id)
                        .unwrap_or_default(),
                    scope: agent::Scope {
                        namespace: scope.namespace,
                        id: scope.id,
                    },
                    kind: "reference".into(),
                    key: String::new(),
                    text: match item.content {
                        port::Content::Text(text) => text,
                        port::Content::Resource { uri, media_type } => {
                            format!("Resource reference ({media_type}): {uri}")
                        }
                    },
                    remote: item.remote,
                    standing: false,
                }
            })
            .collect();
        // Provider token counts are advisory. Bound the complete rendered host
        // projection independently, using bytes as a conservative token bound.
        let remote = records.iter().any(|record| record.remote.is_some());
        let mut admitted = Vec::new();
        let budget = usize::try_from(request.token_budget)
            .unwrap_or(0)
            .min(port::MAX_TOKEN_BUDGET);
        for record in records {
            admitted.push(record);
            if agent::render_memory_context(&admitted).len() > budget {
                if remote {
                    return Err(agent::MemoryError(
                        "remote memory rendering exceeds budget".into(),
                    ));
                }
                admitted.pop();
            }
        }
        let used_tokens = agent::render_memory_context(&admitted).len() as i64;
        Ok(agent::RecallResult {
            records: admitted,
            used_tokens,
        })
    }

    fn close(&self) -> Result<(), otto_core::agent::memory::MemoryError> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

impl Default for MemoryWiring {
    fn default() -> Self {
        Self {
            extension: None,
            client: None,
            service: Arc::new(Service::null(None)),
            usable: false,
            user_scope: Scope::default(),
            recall_limit: 0,
            recall_token_budget: 0,
            advice_audit: None,
            turns: None,
        }
    }
}

/// Disabled config, or an open failure when `required` is false, degrades to a
/// null service with a stderr warning rather than failing startup.
pub fn open_memory_service(
    config: &MemoryRuntime,
    secret_values: &[String],
    stderr: &mut dyn Write,
) -> Result<(Arc<Service>, Scope, bool), String> {
    open_memory_service_with_storage(
        &super::storage::Storage::default(),
        config,
        secret_values,
        stderr,
    )
}

/// The line `open_memory_service_with_storage` writes for each file the store
/// skipped; `otto memory status` lists the same files from these lines.
pub const SKIPPED_FILE_PREFIX: &str = "warning: memory file skipped: ";

/// The Turso database that the previous release would open, when the store
/// has no `identity.json` yet.
fn import_source(
    storage: &super::storage::Storage,
    config: &MemoryRuntime,
) -> crate::memory::Result<PathBuf> {
    let default = Path::new(&config.turso_path);
    let endpoint = storage
        .memory_endpoint(default)
        .map_err(|error| crate::memory::Error::detailed(ErrorKind::Unavailable, error))?;
    if endpoint.starts_with("instance:")
        || !matches!(storage.memory_backend("sqlite"), "sqlite" | "turso")
    {
        return Err(crate::memory::Error::detailed(
            ErrorKind::Unavailable,
            "memory is routed to an external storage backend; transfer it back to local \
             storage with Otto 0.4.8 before upgrading",
        ));
    }
    Ok(PathBuf::from(endpoint))
}

/// Opens the file store at `config.dir`, importing the previous Turso database
/// first when the directory has no identity, and reports skipped files.
fn open_file_store(
    storage: &super::storage::Storage,
    config: &MemoryRuntime,
    options: StoreOptions,
    stderr: &mut dyn Write,
) -> crate::memory::Result<Arc<dyn crate::memory::MemoryStore>> {
    let root = Path::new(&config.dir);
    if !has_identity(root) {
        let source = import_source(storage, config)?;
        if source.exists() {
            let set = import_turso(&source)?;
            write_import(root, &set, options.busy_timeout)?;
        }
    }
    let store = FileStore::open(root, options)?;
    for file in store.invalid_files()? {
        let _ = writeln!(
            stderr,
            "{SKIPPED_FILE_PREFIX}{}: {}",
            file.path.display(),
            file.reason
        );
    }
    Ok(Arc::new(store))
}

pub fn open_memory_service_with_storage(
    storage: &super::storage::Storage,
    config: &MemoryRuntime,
    secret_values: &[String],
    stderr: &mut dyn Write,
) -> Result<(Arc<Service>, Scope, bool), String> {
    if !config.enabled {
        return Ok((
            Arc::new(Service::null(Some(ErrorKind::Disabled))),
            Scope::default(),
            false,
        ));
    }

    let guard_values: Vec<String> = secret_values
        .iter()
        .filter(|value| value.len() <= MAX_EXACT_GUARD_VALUE_BYTES)
        .cloned()
        .collect();
    let exact = ExactGuard::new(&guard_values)
        .map_err(|error| format!("build memory secret guard: {error}"))?;

    let options = StoreOptions {
        busy_timeout: if config.turso_busy_timeout.is_zero() {
            StoreOptions::default().busy_timeout
        } else {
            config.turso_busy_timeout
        },
        new_id: Box::new(new_id),
        guard: Box::new(CompositeGuard::new(vec![
            Box::new(DefaultGuard),
            Box::new(exact),
        ])),
    };
    let store = match open_file_store(storage, config, options, stderr) {
        Ok(store) => store,
        Err(error) => {
            if config.required {
                return Err(format!("open memory store: {error}"));
            }
            let _ = writeln!(
                stderr,
                "warning: memory store unavailable, continuing without memory: {error}"
            );
            return Ok((
                Arc::new(Service::null(Some(error.kind))),
                Scope::default(),
                false,
            ));
        }
    };

    // require_encryption is a security requirement, not an availability
    // preference: reject adapters that cannot make that guarantee.
    if config.require_encryption && !store.encryption_at_rest() {
        let _ = store.close();
        return Err(format!(
            "memory backend {:?} does not support encryption at rest, but require_encryption is set",
            config.backend
        ));
    }

    let identity = match store.identity() {
        Ok(identity) => identity,
        Err(error) => {
            let _ = store.close();
            if config.required {
                return Err(format!("read memory store identity: {error}"));
            }
            let _ = writeln!(
                stderr,
                "warning: memory store unavailable, continuing without memory: {error}"
            );
            return Ok((
                Arc::new(Service::null(Some(error.kind))),
                Scope::default(),
                false,
            ));
        }
    };
    let service = Service::from_store(store, crate::memory::decide_default_policy);
    Ok((Arc::new(service), identity.user_scope, true))
}

/// A configured stable id keeps a moved workspace's records reachable.
pub fn workspace_memory_scope(
    config: &MemoryRuntime,
    canonical_path: &str,
) -> Result<Scope, String> {
    let stable = config
        .workspace_ids
        .get(canonical_path)
        .map(String::as_str)
        .unwrap_or_default();
    let identity =
        crate::workspace::read(Path::new(canonical_path)).map_err(|error| error.to_string())?;
    if let Some(identity) = identity {
        if !stable.is_empty() && stable != identity.memory_scope {
            return Err("workspace identity conflicts with memory.workspace_ids".into());
        }
        return new_workspace_scope(canonical_path, &identity.memory_scope)
            .map_err(|error| error.to_string());
    }
    new_workspace_scope(canonical_path, stable).map_err(|error| error.to_string())
}

/// Configured roots are strings; discovery takes paths.
fn roots(paths: &[String]) -> Vec<std::path::PathBuf> {
    paths.iter().map(std::path::PathBuf::from).collect()
}

/// The skills and agents resolved for one build, plus their prompt sections.
pub struct CatalogWiring {
    pub skills: skill::Catalog,
    pub skill_section: String,
    pub agents: AgentsRuntime,
    pub agent_catalog: subagent::Catalog,
    pub agent_section: String,
}

/// What the sub-agent runner contributes to the parent's agent options.
#[derive(Default)]
pub struct SubagentWiring {
    pub runner: Option<Arc<SubagentRunner>>,
    pub tasks: Option<Arc<Tasks>>,
    pub inbox: Option<Arc<dyn otto_core::mailbox::Mailbox>>,
    /// The session's timer registry, absent when the timer tools are not
    /// registered. `/timers` and archiving read the active runner's.
    pub reminders: Option<Arc<Reminders>>,
}

/// `"stdio"` or `"http"`, for [`mcp::ServerStatus::transport`].
fn transport_label(transport: &McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio { .. } => "stdio",
        McpTransport::Http { .. } => "http",
    }
}

/// One MCP server `connect_mcp` connected: its client, the secrets to redact
/// from its tool results (its configured secrets plus, for an OAuth server,
/// its bearer's tokens), the bearer itself (so [`mcp_child_tools`] can build
/// the same dynamically-redacting tools `connect_mcp` did), and the
/// server-prefixed names of the tools that survived cross-server
/// deduplication.
pub struct ConnectedServer {
    server: Arc<dyn mcp::ToolServer>,
    tool_infos: Vec<mcp::ToolInfo>,
    secrets: Vec<String>,
    bearer: Option<Arc<dyn mcp::BearerSource>>,
    tool_names: Vec<String>,
}

/// A second set of tool adapters for the connected MCP clients, for the
/// sub-agent registry. Mirrors [`Builder::child_tools`]: `mcp::tool::McpTool`
/// cannot be cloned, so the children get their own adapters over the same
/// `Arc<client::Client>` connections `connect_mcp` already opened. Re-runs
/// [`tools_for`] per server (it deterministically reproduces the same
/// within-server-deduped candidate list from the same `client.tools()`) and
/// keeps only the names `connect_mcp` recorded as registered, so the child
/// set exactly matches the parent's without re-running the cross-server
/// warnings a second time.
pub fn mcp_child_tools(
    connected: &[ConnectedServer],
    max_output: usize,
) -> Vec<Box<dyn Tool + Send + Sync>> {
    mcp_router_tools(connected, max_output)
}

fn mcp_router_tools(
    connected: &[ConnectedServer],
    max_output: usize,
) -> Vec<Box<dyn Tool + Send + Sync>> {
    let mut entries = Vec::new();
    for server in connected {
        let kept: HashSet<&str> = server.tool_names.iter().map(String::as_str).collect();
        for info in &server.tool_infos {
            let Some(prefixed_name) =
                crate::tool::mcp::prefixed_name(server.server.name(), &info.name)
            else {
                continue;
            };
            if !kept.contains(prefixed_name.as_str()) {
                continue;
            }
            entries.push(McpCatalogEntry {
                server: Arc::clone(&server.server),
                info: info.clone(),
                prefixed_name,
                max_output_bytes: max_output,
                secrets: server.secrets.clone(),
                bearer: server.bearer.clone(),
            });
        }
    }
    router_tools(entries, max_output)
}

/// Cross-server tool-name collision guard (Finding 2). Server names may
/// contain `_`, so [`tools_for`]'s per-server dedup cannot see a collision
/// between, say, server `a` tool `b__c` and server `a__b` tool `c` — both
/// sanitize to `mcp__a__b__c`, and `Registry::new` would otherwise reject the
/// second one as a hard startup error. Called once per connected server, in
/// configuration order, against the names already claimed by earlier
/// servers; extends `registered` with every name this server keeps. A pure
/// function over already-built tools so it is unit-testable without a live
/// MCP connection.
fn dedup_cross_server(
    server_name: &str,
    tools: &[McpTool],
    registered: &mut HashSet<String>,
) -> (Vec<usize>, Vec<String>) {
    let mut keep = Vec::with_capacity(tools.len());
    let mut warnings = Vec::new();
    for (index, tool) in tools.iter().enumerate() {
        let name = tool.definition().name;
        if registered.insert(name.clone()) {
            keep.push(index);
        } else {
            warnings.push(format!(
                "mcp {server_name}: skipping tool {:?}: name {name:?} is already provided by another server",
                tool.remote_name()
            ));
        }
    }
    (keep, warnings)
}

/// A connect failure's message, redacted against the server's configured
/// secrets before it is stored in [`mcp::ServerState::Failed`] or written to
/// the warning stream. `error`'s text is server-controlled (an RPC error
/// message, or a stdio child's stderr tail) and can echo a secret just like
/// a call result can. `marker` comes from `dynamic_redaction_marker`,
/// already confirmed non-`None` for `secrets` by the caller's redaction-limit
/// guard. A pure function over an already-produced error so it is
/// unit-testable without a live MCP connection.
fn failure_message(error: &mcp::CallError, secrets: &[String], marker: &str) -> String {
    redact_exact_text(&error.to_string(), secrets, marker)
}

struct McpConnectOutcome {
    server: otto_core::config::McpServerRuntime,
    transport: &'static str,
    result: McpConnectResult,
}

enum McpConnectResult {
    Disabled,
    RedactionLimit,
    Connected(mcp::client::Client, Option<Arc<dyn mcp::BearerSource>>),
    NeedsLogin,
    Failed(String),
}

impl Builder {
    pub fn memory_context(
        &self,
        origin: otto_core::memory::Origin,
        session_id: Option<String>,
    ) -> otto_core::memory::MemoryContext {
        let user = self
            .memory
            .extension
            .as_ref()
            .map(|extension| extension.user_scope.clone())
            .unwrap_or_else(|| neutral_scope(&self.memory.user_scope));
        let workspace = neutral_scope(&self.workspace_scope);
        let organization_id = self
            .memory
            .extension
            .as_ref()
            .and_then(|extension| extension.organization_id.clone());
        let mut scopes = vec![user.clone(), workspace.clone()];
        if let Some(id) = &organization_id {
            scopes.push(otto_core::memory::Scope {
                namespace: "organization".into(),
                id: id.clone(),
            });
        }
        otto_core::memory::MemoryContext {
            organization_id,
            principal: user.id.clone(),
            scopes,
            write_scope: Some(workspace.clone()),
            workspace_id: Some(workspace.id),
            session_id,
            task_id: None,
            origin,
        }
    }

    /// The memory tools, in the order they are registered.
    pub fn memory_tools(
        &self,
        max_output: usize,
        advisor: Option<Arc<crate::memory::advice::Advisor>>,
        turn: Option<(&str, Arc<crate::schedule::tools::TurnOrigin>)>,
    ) -> Vec<Box<dyn Tool + Send + Sync>> {
        if self.memory.extension.is_some() {
            return self
                .memory
                .client
                .as_ref()
                .map(|client| {
                    crate::tool::memory_service::tools(
                        client.clone(),
                        self.memory_context(otto_core::memory::Origin::Model, None),
                        max_output,
                    )
                })
                .unwrap_or_default();
        }
        let scopes = vec![self.memory.user_scope.clone(), self.workspace_scope.clone()];
        let mut remember = RememberTool::new(
            Arc::clone(&self.memory.service),
            self.memory.user_scope.clone(),
            self.workspace_scope.clone(),
        );
        let mut forget = ForgetTool::new(Arc::clone(&self.memory.service), scopes.clone());
        if let Some((session_id, origin)) = turn {
            remember = remember.with_turn_context(session_id, Arc::clone(&origin));
            forget = forget.with_turn_context(session_id, origin);
        }
        vec![
            Box::new(MemorySearchTool::new(
                Arc::clone(&self.memory.service),
                scopes,
                max_output,
            )),
            Box::new(remember.with_advisor(advisor)),
            Box::new(forget),
        ]
    }

    /// The advisor that adds review hints to memory candidates: the auxiliary
    /// Jev model when `TYPESAFE_API_KEY` is non-empty, otherwise the session
    /// model through `agent`, a tool-less agent over the runner's provider.
    /// `None` when memory is unusable.
    pub fn advisor(
        &self,
        agent: super::builder::AdviceAgent,
        model: &str,
        usage: Option<crate::usage::Collector>,
    ) -> Option<Arc<crate::memory::advice::Advisor>> {
        if !self.memory.usable || self.memory.extension.is_some() {
            return None;
        }
        let key = self
            .environment
            .get("TYPESAFE_API_KEY")
            .map_or("", String::as_str);
        let backend: Box<dyn crate::decision::Backend> = if key.is_empty() {
            Box::new(super::advice::SessionBackend::new(
                agent,
                model.to_owned(),
                usage,
            ))
        } else {
            Box::new(crate::decision::jev::Jev::new(
                crate::typesafe::BASE_URL,
                key,
            ))
        };
        Some(Arc::new(crate::memory::advice::Advisor::new_with_audit(
            backend,
            self.memory.advice_audit.clone(),
        )))
    }

    /// Whether memory tools belong in this build. The boundary half of the
    /// decision is checked by the caller that already knows it.
    pub fn memory_usable(&self) -> bool {
        self.memory.usable
    }

    /// Discovers skills and agent definitions, prints their warnings, and
    /// appends the skill tool to `tools`.
    pub fn build_catalogs(
        &self,
        tools: &mut Vec<Box<dyn Tool + Send + Sync>>,
        max_output: usize,
        stderr: &mut dyn Write,
    ) -> Result<CatalogWiring, BuildError> {
        let plugins =
            crate::plugin::Snapshot::load(Path::new(&self.home), &self.config, &self.environment);
        self.build_catalogs_with_plugins(tools, max_output, stderr, &plugins)
    }

    pub(crate) fn build_catalogs_with_plugins(
        &self,
        tools: &mut Vec<Box<dyn Tool + Send + Sync>>,
        max_output: usize,
        stderr: &mut dyn Write,
        plugins: &crate::plugin::Snapshot,
    ) -> Result<CatalogWiring, BuildError> {
        let skills = resolve_skills(&self.config, &self.environment, &self.workspace_path);
        let skill_roots = crate::config::layered::catalog_roots(
            &self.config,
            Path::new(&self.home),
            Path::new(&self.workspace_path),
            &skills.roots,
            "skills",
        )?;
        let (mut discovered_catalog, mut warnings) = skill::Catalog::discover(&roots(&skill_roots));
        warnings.extend(plugins.diagnostics.clone());
        warnings.extend(plugins.extend_skills(&mut discovered_catalog));
        let catalog = discovered_catalog.without_names(&skills.disabled);

        let agents = resolve_agents(&self.config, &self.environment, &self.workspace_path)
            .map_err(|error| error.to_string())?;
        let (mut agent_catalog, mut agent_warnings) = if agents.enabled {
            let agent_roots = crate::config::layered::catalog_roots(
                &self.config,
                Path::new(&self.home),
                Path::new(&self.workspace_path),
                &agents.roots,
                "agents",
            )?;
            subagent::Catalog::discover(&roots(&agent_roots))
        } else {
            (subagent::Catalog::default(), Vec::new())
        };
        // Only a skill that actually became a definition may be marked
        // agent-callable, and only while sub-agents are enabled at all:
        // pointing the model at an `agent` name nothing registered is worse
        // than not routing it. So the agent catalog is built first and its
        // answer drives the skills listing.
        let agent_callable = if agents.enabled {
            let (skill_warnings, registered) = agent_catalog.extend_from_skills(&catalog);
            agent_warnings.extend(skill_warnings);
            registered
        } else {
            std::collections::BTreeSet::new()
        };

        let (skill_section, section_warnings) = skill::prompt_section(&catalog, &agent_callable);
        warnings.extend(section_warnings);
        for warning in &warnings {
            let _ = writeln!(stderr, "warning: {warning}");
        }
        if !catalog.is_empty() {
            tools.push(Box::new(SkillTool::new(catalog.clone(), max_output)));
        }

        let (agent_section, section_warnings) = subagent::prompt_section(&agent_catalog);
        agent_warnings.extend(section_warnings);
        for warning in &agent_warnings {
            let _ = writeln!(stderr, "warning: {warning}");
        }

        Ok(CatalogWiring {
            skills: catalog,
            skill_section,
            agents,
            agent_catalog,
            agent_section,
        })
    }

    /// Builds the sub-agent runner and appends the agent tools and `remind`.
    #[allow(clippy::too_many_arguments)]
    pub fn build_subagents(
        &self,
        tools: &mut Vec<Box<dyn Tool + Send + Sync>>,
        catalogs: &CatalogWiring,
        client: &ProviderClient,
        redaction_values: &[String],
        redactor: &Redactor,
        runtime: &Runtime,
        session: &SharedSession,
        prompt_for: PromptFor,
        child_tools: Vec<Box<dyn Tool + Send + Sync>>,
        extensions: Option<Arc<crate::plugin::hooks::Runtime>>,
        hook_guard: Option<Arc<crate::plugin::hooks::SessionGuard>>,
        stderr: &mut dyn Write,
    ) -> Result<SubagentWiring, BuildError> {
        let provider: Arc<dyn otto_core::provider::Provider + Send + Sync> = match client {
            ProviderClient::Compat { client, .. } => Arc::clone(client) as _,
            ProviderClient::ChatGpt { client, .. } => Arc::clone(client) as _,
            ProviderClient::Unavailable => return Ok(SubagentWiring::default()),
            #[cfg(test)]
            ProviderClient::Scripted(_) => return Ok(SubagentWiring::default()),
        };
        if !catalogs.agents.enabled {
            return Ok(SubagentWiring::default());
        }

        let persist = self.sidecar_path(session, "reminders.json");
        // A task id must stay unique within the session across resumes, so
        // the counter picks up after the highest `t<k>` already used by a
        // child transcript file, rather than always starting at `t1`.
        let children_dir = PathBuf::from(session.path()).with_extension("");
        let starting_counter = subagent::tasks::highest_task_counter(&children_dir);
        let inbox_persist = self.sidecar_path(session, "inbox.json");
        let tasks = Arc::new(match &self.task_recorder {
            Some(store) => {
                let (pid, process_started_at) = subagent::record::current_process();
                let parent_session = if self.no_session {
                    format!("memory:{pid}:{process_started_at}")
                } else {
                    session.header().id
                };
                Tasks::with_recorder_from(
                    Arc::clone(store) as Arc<dyn subagent::record::Recorder>,
                    subagent::record::TaskContext {
                        parent_session,
                        parent_session_path: session.path(),
                        workspace: self.workspace_path.clone(),
                        pid,
                        process_started_at,
                    },
                    starting_counter,
                )
            }
            None => Tasks::new_from(starting_counter),
        });
        let usage = self.usage_collector(session, runtime);
        let session_for_children = session.clone();
        let session_for_resume = session_for_children.clone();
        let session_for_lease = session_for_children.clone();
        let session_for_recovery = session_for_children.clone();
        let session_for_inbox = session_for_children.clone();
        let session = session.clone();
        let (runner, warnings) = SubagentRunner::new(RunnerConfig {
            approval: Some((
                self.approvals.clone(),
                otto_core::approval::Scope {
                    workspace: self.workspace_path.clone(),
                    session_id: session.header().id,
                    task_id: String::new(),
                    run_id: String::new(),
                },
            )),
            provider,
            tools: child_tools,
            redaction_values: redaction_values.to_vec(),
            redaction_complete: redactor.allows_dynamic_content(),
            template: OptionsTemplate {
                diagnostics: self.logger.diagnostic_scope(
                    &session.header().id,
                    "",
                    Redactor::with_completeness(
                        redaction_values,
                        redactor.allows_dynamic_content(),
                    ),
                ),
                plugins: extensions,
                hook_guard,
                model: runtime.model.clone(),
                subagent_models: runtime.subagent_models.clone(),
                provider_name: runtime.provider.clone(),
                thinking: runtime.thinking.clone(),
                compaction: otto_core::agent::CompactionSettings {
                    auto: runtime.compaction.auto,
                    hard_input_window: runtime.compaction.hard_input_window,
                    working_window: runtime.compaction.working_window,
                    reserve_tokens: runtime.compaction.reserve_tokens,
                    keep_recent_tokens: runtime.compaction.keep_recent_tokens,
                },
                new_operation_id: Arc::new(super::builder::new_operation_id),
                progress: ProgressPolicy {
                    repeated_calls: runtime.progress.repeated_calls,
                    consecutive_failures: runtime.progress.consecutive_failures,
                },
                provider_timeout: runtime.resilience.deadlines.provider_timeout,
                cancellation_grace: runtime.resilience.deadlines.cancellation_grace,
                task_timeout: runtime.resilience.deadlines.subagent_timeout,
                ..OptionsTemplate::default()
            },
            prompt_for,
            tasks: Arc::clone(&tasks),
            catalog: catalogs.agent_catalog.clone(),
            parent_session: Some(Arc::new(move || {
                otto_core::session::Session::messages(&session)
            })),
            max_parallel: catalogs.agents.max_parallel.max(0) as usize,
            max_output_bytes: runtime.max_output_bytes.max(0) as usize,
            usage,
            child_session: Some(child_sessions(
                session_for_children,
                Arc::clone(&self.storage),
            )),
            child_mailbox: (!self.no_session).then(child_mailbox),
            checker: self.skill_checker.clone(),
            lease: Some(Arc::new(move || session_for_lease.lease())),
            workspace_path: PathBuf::from(&self.workspace_path),
            children_dir: Some(Arc::new(move || {
                let path = session_for_resume.path();
                if path.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(path).with_extension(""))
                }
            })),
        })
        .map_err(|error| format!("create sub-agent runner: {error}"))?;
        for warning in &warnings {
            let _ = writeln!(stderr, "warning: {warning}");
        }

        let runner = Arc::new(runner);
        let inbox = Arc::clone(tasks.notifications_inbox());
        // The persist hook resolves the sidecar path at write time rather than
        // capturing `inbox_persist` here: a lazy session store's `path()` is
        // still empty at this point (nothing has been written to it yet), so
        // gating the hook on `inbox_persist` would permanently skip
        // persistence for a session that gets its first write only after this
        // function returns (see `Store::path`).
        let no_session = self.no_session;
        super::sidecar_mailbox::install(&inbox, ".inbox.json", move || {
            let path = session_for_inbox.path();
            (!no_session && !path.is_empty()).then(|| Path::new(&path).with_extension("inbox.json"))
        });
        // A session with no persisted inbox path (`--no-session`, or nothing
        // written yet) never takes over a lease either, so it never has a
        // takeover to report; the recovery scan lives in this branch rather
        // than running unconditionally.
        if let Some(path) = inbox_persist {
            if let Some(warning) = super::sidecar_mailbox::load(&inbox, &path) {
                let _ = writeln!(stderr, "warning: {warning}");
            }
            let recovery_warnings = failover::recovery::notify(
                inbox.as_ref(),
                session_for_recovery.take_recovery(),
                &children_dir,
                runtime.max_output_bytes.max(0) as usize,
                &otto_core::session::Session::messages(&session_for_recovery),
            );
            for warning in &recovery_warnings {
                let _ = writeln!(stderr, "warning: {warning}");
            }
        }
        tools.extend(subagent::tools::tools(&runner));
        // Parent-only; the child registry drops the timer tools by name.
        let reminders = Arc::new(match persist {
            Some(path) => Reminders::with_persist(inbox.clone(), path),
            None => Reminders::new(inbox.clone()),
        });
        tools.extend(remind::tools(&reminders));
        Ok(SubagentWiring {
            runner: Some(runner),
            tasks: Some(tasks),
            inbox: Some(inbox),
            reminders: Some(reminders),
        })
    }

    /// A durable journal beside the session file, `<id>.<extension>`, for
    /// inputs a turn accepted and has not consumed yet: `inputs.json` for
    /// steering, `followups.json` for model follow-ups. `None` for
    /// `--no-session`. The path is resolved at write time, as for the inbox,
    /// because a new session has no file until its first write.
    pub(crate) fn session_journal(
        &self,
        session: &SharedSession,
        extension: &'static str,
        stderr: &mut dyn Write,
    ) -> Option<Arc<dyn otto_core::mailbox::Mailbox>> {
        if self.no_session {
            return None;
        }
        let journal = Arc::new(otto_core::mailbox::InMemoryMailbox::new(None));
        let for_path = session.clone();
        super::sidecar_mailbox::install(&journal, &format!(".{extension}"), move || {
            let path = for_path.path();
            (!path.is_empty()).then(|| Path::new(&path).with_extension(extension))
        });
        if let Some(path) = self.sidecar_path(session, extension)
            && let Some(warning) = super::sidecar_mailbox::load(&journal, &path)
        {
            let _ = writeln!(stderr, "warning: {warning}");
        }
        Some(journal)
    }

    /// Where a top-level session sidecar (`reminders.json`, `inbox.json`)
    /// lives: beside the session file, also when that file is in `archive/`
    /// and was resumed by path. Archiving removes the sidecars from there, and
    /// `remind::session_file_exists` checks for the session file there. `None`
    /// for `--no-session`. Child inboxes are never persisted: only the
    /// registry built for the top-level session installs the hook.
    fn sidecar_path(&self, session: &SharedSession, extension: &str) -> Option<PathBuf> {
        let path = session.path();
        if self.no_session || path.is_empty() {
            return None;
        }
        Some(Path::new(&path).with_extension(extension))
    }

    /// A second set of the parent's non-memory tools, for the children. Boxed
    /// tools cannot be cloned, so the children get their own.
    pub fn child_tools(
        &self,
        runtime: &Runtime,
        max_output: usize,
        redaction_values: &[String],
        skills: &skill::Catalog,
    ) -> Result<Vec<Box<dyn Tool + Send + Sync>>, BuildError> {
        self.child_tools_with_output_logs(
            runtime,
            max_output,
            redaction_values,
            skills,
            Arc::default(),
        )
    }

    pub(crate) fn child_tools_with_output_logs(
        &self,
        runtime: &Runtime,
        max_output: usize,
        redaction_values: &[String],
        skills: &skill::Catalog,
        output_logs: Arc<crate::tool::bash_output::Logs>,
    ) -> Result<Vec<Box<dyn Tool + Send + Sync>>, BuildError> {
        let mut tools = self.builtin_file_tools(max_output);
        tools.extend(self.web_tools(max_output, redaction_values, Some(runtime)));
        if self.bash_configured() {
            let executor = self
                .command_executor
                .clone()
                .expect("bash_configured implies an executor");
            let mut tool = crate::tool::bash::BashTool::new_with_grace(
                self.workspace,
                executor,
                &self.shell,
                self.sandbox_environment.clone().unwrap_or_default(),
                super::builder::shell_timeout(runtime.shell_timeout),
                runtime.resilience.deadlines.cancellation_grace,
                max_output,
                redaction_values,
            )
            .map_err(|error| format!("create bash tool: {error}"))?;
            tool = tool.with_output_logs(output_logs);
            if self.sandbox_info.mode == SandboxMode::Seatbelt {
                tool = tool.with_private_home();
            }
            if let Some(approvals) = &self.bash_approvals {
                tool = tool.with_approvals(String::new(), approvals.clone());
            }
            if let Some(control) = &self.grant_control {
                tool = tool.with_grant_control(self.config_path.clone(), control.clone());
            }
            if let Some(excluded) = &self.excluded_commands {
                tool = tool.with_excluded_commands(Arc::clone(excluded));
            }
            tools.push(Box::new(tool));
        }
        if !skills.is_empty() {
            tools.push(Box::new(SkillTool::new(skills.clone(), max_output)));
        }
        Ok(tools)
    }

    pub fn connecting_mcp_servers(&self) -> Arc<mcp::Servers> {
        let servers = Arc::new(mcp::Servers::default());
        if !self.mcp.enabled {
            return servers;
        }
        let plugins =
            crate::plugin::Snapshot::load(Path::new(&self.home), &self.config, &self.environment);
        for server in self.mcp.servers.iter().chain(plugins.servers.iter()) {
            servers.push(
                mcp::ServerStatus {
                    name: plugins
                        .guards
                        .get(&server.name)
                        .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                    transport: transport_label(&server.transport),
                    era: None,
                    state: if server.enabled {
                        mcp::ServerState::Connecting
                    } else {
                        mcp::ServerState::Disabled
                    },
                },
                None,
            );
        }
        servers
    }

    /// Connects enabled MCP servers concurrently, then processes every outcome
    /// in configuration order so `warnings`, `/mcp` status, and cross-server
    /// tool-name deduplication stay stable and reproducible. Each server's
    /// outcome is pushed to the returned [`mcp::Servers`] handle for
    /// `/mcp`; a disabled or failed server never stops the others, and never
    /// fails the build. Restarting an exited stdio server is out of scope; a
    /// server that later exits stays `Connected` until the process restarts.
    /// A server whose configured secrets already exceed
    /// [`dynamic_redaction_marker`]'s limits is reported `Failed` without
    /// attempting to connect, so an oversized secret never falls back to
    /// blanking every tool result with an empty marker (Finding 5b).
    ///
    /// Returns the parent's tools, the connected servers (for
    /// [`mcp_child_tools`], since [`mcp::client::Client`] tools cannot be
    /// cloned into a second registry), and the status handle.
    pub async fn connect_mcp(
        &self,
        max_output: usize,
        warnings: &mut (dyn Write + Send),
    ) -> (
        Vec<Box<dyn Tool + Send + Sync>>,
        Vec<ConnectedServer>,
        Arc<mcp::Servers>,
    ) {
        self.connect_mcp_with_grace(max_output, mcp::DEFAULT_CANCELLATION_GRACE, warnings)
            .await
    }

    /// Connects MCP servers using the runtime cooperative cancellation grace.
    pub async fn connect_mcp_with_grace(
        &self,
        max_output: usize,
        cancellation_grace: Duration,
        warnings: &mut (dyn Write + Send),
    ) -> (
        Vec<Box<dyn Tool + Send + Sync>>,
        Vec<ConnectedServer>,
        Arc<mcp::Servers>,
    ) {
        let plugins =
            crate::plugin::Snapshot::load(Path::new(&self.home), &self.config, &self.environment);
        self.connect_mcp_with_plugins(max_output, cancellation_grace, warnings, &plugins)
            .await
    }

    pub(crate) async fn connect_mcp_with_plugins(
        &self,
        max_output: usize,
        cancellation_grace: Duration,
        warnings: &mut (dyn Write + Send),
        plugins: &crate::plugin::Snapshot,
    ) -> (
        Vec<Box<dyn Tool + Send + Sync>>,
        Vec<ConnectedServer>,
        Arc<mcp::Servers>,
    ) {
        let servers = Arc::new(mcp::Servers::default());
        let mut tools: Vec<Box<dyn Tool + Send + Sync>> = Vec::new();
        let mut connected: Vec<ConnectedServer> = Vec::new();
        if !self.mcp.enabled {
            return (tools, connected, servers);
        }

        let cancel = CancellationToken::new();
        let connect_timeout = Duration::from_secs(self.mcp.connect_timeout_secs);
        let call_timeout = Duration::from_secs(self.mcp.call_timeout_secs);
        let outcomes = join_all(
            self.mcp
                .servers
                .iter()
                .chain(plugins.servers.iter())
                .cloned()
                .map(|server| {
                    let cancel = cancel.clone();
                    async move {
                        let transport = transport_label(&server.transport);
                        if !server.enabled {
                            return McpConnectOutcome {
                                server,
                                transport,
                                result: McpConnectResult::Disabled,
                            };
                        }
                        let Some(marker) = dynamic_redaction_marker(&server.secrets) else {
                            return McpConnectOutcome {
                                server,
                                transport,
                                result: McpConnectResult::RedactionLimit,
                            };
                        };
                        let result = if plugins.guards.get(&server.name).is_some_and(|g| !g.valid())
                        {
                            Err(mcp::CallError::Transport(
                                "plugin authorization changed before connection".into(),
                            ))
                        } else {
                            self.connect_one(
                                &server,
                                connect_timeout,
                                call_timeout,
                                cancellation_grace,
                                &cancel,
                            )
                            .await
                        };
                        let result = match result {
                            Ok((client, bearer)) => McpConnectResult::Connected(client, bearer),
                            Err(mcp::CallError::NeedsLogin) => McpConnectResult::NeedsLogin,
                            Err(error) => McpConnectResult::Failed(failure_message(
                                &error,
                                &server.secrets,
                                &marker,
                            )),
                        };
                        McpConnectOutcome {
                            server,
                            transport,
                            result,
                        }
                    }
                }),
        )
        .await;

        let mut registered_names: HashSet<String> = HashSet::new();
        for outcome in outcomes {
            let server = outcome.server;
            let transport_kind = outcome.transport;
            match outcome.result {
                McpConnectResult::Disabled => {
                    servers.push(
                        mcp::ServerStatus {
                            name: plugins
                                .guards
                                .get(&server.name)
                                .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                            transport: transport_kind,
                            era: None,
                            state: mcp::ServerState::Disabled,
                        },
                        None,
                    );
                }
                McpConnectResult::RedactionLimit => {
                    servers.push(
                        mcp::ServerStatus {
                            name: plugins
                                .guards
                                .get(&server.name)
                                .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                            transport: transport_kind,
                            era: None,
                            state: mcp::ServerState::Failed(SECRETS_EXCEED_REDACTION_LIMITS.into()),
                        },
                        None,
                    );
                    self.log_mcp_failed(
                        &server.name,
                        transport_kind,
                        SECRETS_EXCEED_REDACTION_LIMITS,
                    );
                    let _ = writeln!(
                        warnings,
                        "warning: mcp server {:?} failed to connect: {SECRETS_EXCEED_REDACTION_LIMITS}",
                        server.name
                    );
                }
                McpConnectResult::Connected(client, bearer) => {
                    let client = Arc::new(client);
                    let tool_server: Arc<dyn mcp::ToolServer> =
                        if let Some(authorization) = plugins.guards.get(&server.name) {
                            if !authorization.valid() {
                                client.close().await;
                                let _ = writeln!(
                                    warnings,
                                    "warning: plugin {} authorization changed during connection",
                                    authorization.logical_name
                                );
                                servers.push(
                                    mcp::ServerStatus {
                                        name: authorization.logical_name.clone(),
                                        transport: transport_kind,
                                        era: None,
                                        state: mcp::ServerState::Failed(
                                            "plugin authorization changed".into(),
                                        ),
                                    },
                                    None,
                                );
                                continue;
                            }
                            authorization.watch(&client, &servers);
                            Arc::new(crate::plugin::GuardedServer::new(
                                Arc::clone(&client),
                                authorization.clone(),
                            ))
                        } else {
                            Arc::clone(&client) as Arc<dyn mcp::ToolServer>
                        };
                    let mut secrets = server.secrets.clone();
                    if let Some(bearer) = &bearer {
                        for secret in bearer.secrets() {
                            if !secret.is_empty() && !secrets.contains(&secret) {
                                secrets.push(secret);
                            }
                        }
                    }
                    let (server_tools, tool_warnings) = tools_for(
                        Arc::clone(&tool_server),
                        client.tools(),
                        max_output,
                        secrets.clone(),
                        bearer.clone(),
                    );
                    for warning in &tool_warnings {
                        let _ = writeln!(warnings, "warning: {warning}");
                    }
                    let (keep, dedup_warnings) =
                        dedup_cross_server(&server.name, &server_tools, &mut registered_names);
                    for warning in &dedup_warnings {
                        let _ = writeln!(warnings, "warning: {warning}");
                    }
                    let keep: HashSet<usize> = keep.into_iter().collect();
                    let tool_names: Vec<String> = server_tools
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| keep.contains(index))
                        .map(|(_, tool)| tool.definition().name)
                        .collect();
                    self.logger.info(
                        "mcp_server_connected",
                        &[
                            ("server", server.name.clone()),
                            ("transport", transport_kind.to_string()),
                            ("tools", tool_names.len().to_string()),
                        ],
                    );
                    servers.push(
                        mcp::ServerStatus {
                            name: plugins
                                .guards
                                .get(&server.name)
                                .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                            transport: transport_kind,
                            era: Some(client.era().clone()),
                            state: mcp::ServerState::Connected {
                                tools: tool_names.len(),
                            },
                        },
                        Some(Arc::clone(&client)),
                    );
                    connected.push(ConnectedServer {
                        server: tool_server,
                        tool_infos: client
                            .tools()
                            .iter()
                            .cloned()
                            .map(|mut info| {
                                if let Some(g) = plugins.guards.get(&server.name) {
                                    info.description = Some(format!(
                                        "[{}] {}",
                                        g.logical_name,
                                        info.description.as_deref().unwrap_or(&info.name)
                                    ));
                                }
                                info
                            })
                            .collect(),
                        secrets,
                        bearer,
                        tool_names,
                    });
                }
                McpConnectResult::NeedsLogin => {
                    servers.push(
                        mcp::ServerStatus {
                            name: plugins
                                .guards
                                .get(&server.name)
                                .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                            transport: transport_kind,
                            era: None,
                            state: mcp::ServerState::NeedsLogin,
                        },
                        None,
                    );
                    let _ = writeln!(
                        warnings,
                        "warning: mcp server {:?} needs login: run 'otto mcp login {}'",
                        server.name, server.name
                    );
                }
                McpConnectResult::Failed(message) => {
                    self.log_mcp_failed(&server.name, transport_kind, &message);
                    servers.push(
                        mcp::ServerStatus {
                            name: plugins
                                .guards
                                .get(&server.name)
                                .map_or_else(|| server.name.clone(), |g| g.logical_name.clone()),
                            transport: transport_kind,
                            era: None,
                            state: mcp::ServerState::Failed(message.clone()),
                        },
                        None,
                    );
                    let _ = writeln!(
                        warnings,
                        "warning: mcp server {:?} failed to connect: {message}",
                        server.name
                    );
                }
            }
        }
        tools.extend(mcp_router_tools(&connected, max_output));
        (tools, connected, servers)
    }

    /// `message` is the redacted text `/mcp` shows for the failure.
    fn log_mcp_failed(&self, server: &str, transport: &str, message: &str) {
        self.logger.warn(
            "mcp_server_failed",
            &[
                ("server", server.to_string()),
                ("transport", transport.to_string()),
                ("error", message.to_string()),
            ],
        );
    }

    /// Builds the transport for one configured server and connects it.
    /// Returns the OAuth bearer alongside the client, when the server uses
    /// one, so `connect_mcp` can redact its current tokens from tool results
    /// (Finding 6) without `crate::mcp::oauth` gaining a caller outside tests.
    async fn connect_one(
        &self,
        server: &otto_core::config::McpServerRuntime,
        connect_timeout: Duration,
        call_timeout: Duration,
        cancellation_grace: Duration,
        cancel: &CancellationToken,
    ) -> Result<(mcp::client::Client, Option<Arc<dyn mcp::BearerSource>>), mcp::CallError> {
        if crate::tool::project_guard::Guard::for_config(
            &self.config,
            &self.home,
            &self.workspace_path,
        )
        .is_some_and(|g| !g.valid())
        {
            return Err(mcp::CallError::Transport(
                "project executable trust was revoked before connection".into(),
            ));
        }
        match &server.transport {
            McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                if self
                    .config
                    .policy
                    .as_ref()
                    .and_then(|p| p.allowed_mcp_commands.as_ref())
                    .is_some()
                {
                    // Policies name immutable administrator-controlled binaries, never a
                    // PATH lookup or a workspace-owned file at an allowlisted spelling.
                    let path = Path::new(command);
                    for ancestor in path.ancestors() {
                        use std::os::unix::fs::MetadataExt;
                        let info = std::fs::symlink_metadata(ancestor).map_err(|_| {
                            mcp::CallError::Transport("cannot inspect managed MCP command".into())
                        })?;
                        if info.file_type().is_symlink()
                            || info.uid() != 0
                            || info.mode() & 0o022 != 0
                        {
                            return Err(mcp::CallError::Transport("managed MCP command must be administrator-owned and not user-writable".into()));
                        }
                    }
                }
                let transport = mcp::stdio::StdioTransport::spawn_with_grace(
                    command,
                    args,
                    env,
                    Path::new(cwd),
                    cancellation_grace,
                )
                .await?;
                let client = mcp::client::Client::connect_with_grace(
                    server.name.clone(),
                    Box::new(transport),
                    connect_timeout,
                    call_timeout,
                    cancellation_grace,
                    cancel,
                )
                .await?;
                Ok((client, None))
            }
            McpTransport::Http {
                url, headers, auth, ..
            } => {
                let bearer: Option<Arc<dyn mcp::BearerSource>> = match auth {
                    McpAuth::OAuth => Some(Arc::new(mcp::oauth::TokenStore::new(
                        server.name.clone(),
                        mcp::oauth::token_path(Path::new(&self.home), &server.name),
                    ))),
                    McpAuth::None => None,
                };
                let transport = mcp::http::HttpTransport::new(
                    url.clone(),
                    headers.clone(),
                    bearer.clone(),
                    call_timeout,
                )?;
                let client = mcp::client::Client::connect_with_grace(
                    server.name.clone(),
                    Box::new(transport),
                    connect_timeout,
                    call_timeout,
                    cancellation_grace,
                    cancel,
                )
                .await?;
                Ok((client, bearer))
            }
        }
    }

    /// Renders a child's static system prompt: the parent's prompt for the
    /// child's tool set, plus the shared redacted tail. The Agents section is
    /// parent-only, because children never have the agent tool.
    pub fn child_prompt_for(
        &self,
        runtime: &Runtime,
        endpoint_host: &str,
        prompt_tail: &str,
    ) -> PromptFor {
        let sandbox = self.effective_sandbox_info();
        let provider = runtime.provider.clone();
        let host = endpoint_host.to_string();
        let model = runtime.model.clone();
        let tail = prompt_tail.to_string();
        Arc::new(move |definitions: &[ToolDefinition]| {
            super::prompt::system_prompt_for(definitions, sandbox, &provider, &host, &model) + &tail
        })
    }

    /// Binds the memory service for one session.
    pub fn bind_memory(&self) -> Result<Binding, BuildError> {
        self.memory
            .service
            .bind(BindOptions {
                scopes: vec![self.memory.user_scope.clone(), self.workspace_scope.clone()],
                default_write_scope: self.workspace_scope.clone(),
                ..BindOptions::default()
            })
            .map_err(|error| format!("bind memory: {error}"))
    }

    pub fn bind_selected_memory(
        &self,
        session_id: String,
    ) -> Result<Arc<dyn otto_core::agent::memory::MemoryRecall + Send + Sync>, BuildError> {
        if self.memory.extension.is_some() {
            let client = self
                .memory
                .client
                .clone()
                .ok_or("memory service unavailable")?;
            Ok(Arc::new(ServiceBinding {
                client,
                context: self.memory_context(otto_core::memory::Origin::Model, Some(session_id)),
                closed: AtomicBool::new(false),
            }))
        } else {
            Ok(Arc::new(self.bind_memory()?))
        }
    }

    /// The memory, skill and sub-agent halves of `boundaryToolDefinitions`. The
    /// caller splices them around the bash definition.
    pub fn boundary_memory_definitions(&self, max_output: usize) -> Vec<ToolDefinition> {
        if !self.memory.usable {
            return Vec::new();
        }
        self.memory_tools(max_output, None, None)
            .iter()
            .map(|tool| tool.definition())
            .collect()
    }

    /// The skill and sub-agent definitions the boundary check must see.
    /// `dynamic` is the redaction-boundary gate on the sub-agent tools.
    pub fn boundary_catalog_definitions(
        &self,
        max_output: usize,
        dynamic: bool,
    ) -> Vec<ToolDefinition> {
        let mut definitions = Vec::new();
        if resolve_skills(&self.config, &self.environment, &self.workspace_path).enabled {
            definitions.push(SkillTool::new(skill::Catalog::default(), max_output).definition());
        }
        // Erring toward including the agent tools keeps the check
        // conservative; `build_catalogs` is the gate that surfaces the error.
        if dynamic
            && resolve_agents(&self.config, &self.environment, &self.workspace_path)
                .map(|agents| agents.enabled)
                .unwrap_or(true)
        {
            definitions.extend(subagent::tools::tool_definitions());
            definitions.extend(remind::definitions());
        }
        definitions
    }
}

/// Only the outcomes reachable without a real MCP server are covered: a
/// disabled server is never dialed, and an unspawnable command reports `Failed`
/// with one warning. A working stdio/HTTP connection would need a real server
/// process or socket, which the offline test rule forbids;
/// `crates/otto/tests/mcp_stdio.rs` covers the transport itself with a fake
/// server subprocess.
#[cfg(test)]
mod mcp_tests {
    use super::*;
    use otto_core::config::{McpRuntime, McpServerRuntime};

    fn builder_with_servers(root: &std::path::Path, servers: Vec<McpServerRuntime>) -> Builder {
        let mut builder = crate::cli::testutil::builder(root, &root.join("sessions"));
        builder.mcp = McpRuntime {
            enabled: true,
            call_timeout_secs: 5,
            connect_timeout_secs: 1,
            servers,
        };
        builder
    }

    fn stdio_server(
        name: &str,
        enabled: bool,
        command: &str,
        cwd: &std::path::Path,
    ) -> McpServerRuntime {
        McpServerRuntime {
            name: name.to_string(),
            enabled,
            transport: McpTransport::Stdio {
                command: command.to_string(),
                args: Vec::new(),
                env: Vec::new(),
                cwd: cwd.to_string_lossy().into_owned(),
            },
            secrets: Vec::new(),
        }
    }

    #[test]
    fn connecting_servers_report_configuration_order_without_dialing() {
        let directory = tempfile::tempdir().expect("directory");
        let builder = builder_with_servers(
            directory.path(),
            vec![
                stdio_server(
                    "slow",
                    true,
                    "otto-mcp-test-nonexistent-command",
                    directory.path(),
                ),
                stdio_server(
                    "off",
                    false,
                    "otto-mcp-test-nonexistent-command",
                    directory.path(),
                ),
            ],
        );

        let status = builder.connecting_mcp_servers().status();
        assert_eq!(status.len(), 2);
        assert_eq!(status[0].name, "slow");
        assert_eq!(status[0].transport, "stdio");
        assert_eq!(status[0].state, mcp::ServerState::Connecting);
        assert_eq!(status[1].name, "off");
        assert_eq!(status[1].state, mcp::ServerState::Disabled);
    }

    #[tokio::test]
    async fn a_disabled_server_is_reported_without_connecting() {
        let directory = tempfile::tempdir().expect("directory");
        let builder = builder_with_servers(
            directory.path(),
            vec![stdio_server(
                "example",
                false,
                "otto-mcp-test-nonexistent-command",
                directory.path(),
            )],
        );

        let mut warnings = Vec::new();
        let (tools, connected, servers) = builder.connect_mcp(65536, &mut warnings).await;
        assert!(tools.is_empty());
        assert!(connected.is_empty());
        let status = servers.status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].name, "example");
        assert_eq!(status[0].transport, "stdio");
        assert_eq!(status[0].state, mcp::ServerState::Disabled);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[tokio::test]
    async fn a_command_that_cannot_be_spawned_is_reported_as_failed_and_does_not_stop_the_runner() {
        let directory = tempfile::tempdir().expect("directory");
        let builder = builder_with_servers(
            directory.path(),
            vec![stdio_server(
                "broken",
                true,
                "otto-mcp-test-nonexistent-command",
                directory.path(),
            )],
        );

        let mut warnings = Vec::new();
        let (tools, connected, servers) = builder.connect_mcp(65536, &mut warnings).await;
        assert!(tools.is_empty());
        assert!(connected.is_empty());
        let status = servers.status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].name, "broken");
        assert!(
            matches!(status[0].state, mcp::ServerState::Failed(_)),
            "{:?}",
            status[0].state
        );
        let text = String::from_utf8(warnings).expect("utf-8 warnings");
        assert!(text.contains("broken"), "{text:?}");
        assert!(text.starts_with("warning: "), "{text:?}");
    }

    struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A stdio server in `/bin/sh` that answers the legacy handshake and lists
    /// one tool. It uses only shell builtins, because the child's environment
    /// has no `PATH`.
    fn shell_mcp_server(name: &str, cwd: &std::path::Path) -> McpServerRuntime {
        let script = format!(
            r#"while IFS= read -r line; do
  id=${{line#*'"id":'}}; id=${{id%%[,\}}]*}}
  case "$line" in
    *'"method":"server/discover"'*) printf '{{"jsonrpc":"2.0","id":%s,"error":{{"code":-32601,"message":"no"}}}}\n' "$id";;
    *'"method":"initialize"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"{}"}}}}\n' "$id";;
    *'"method":"tools/list"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}}}\n' "$id";;
  esac
done"#,
            mcp::LEGACY_VERSION
        );
        McpServerRuntime {
            name: name.to_string(),
            enabled: true,
            transport: McpTransport::Stdio {
                command: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), script],
                env: Vec::new(),
                cwd: cwd.to_string_lossy().into_owned(),
            },
            secrets: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_connected_and_a_failed_server_each_write_their_event() {
        let directory = tempfile::tempdir().expect("directory");
        let mut builder = builder_with_servers(
            directory.path(),
            vec![
                shell_mcp_server("good", directory.path()),
                stdio_server(
                    "broken",
                    true,
                    "otto-mcp-test-nonexistent-command",
                    directory.path(),
                ),
            ],
        );
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        builder.shared_mut().logger = Arc::new(crate::log::Logger::new(Box::new(CapturedLog(
            Arc::clone(&bytes),
        ))));

        let mut warnings = Vec::new();
        let (_tools, _connected, servers) = builder.connect_mcp(65536, &mut warnings).await;
        let status = servers.status();
        let shown = match &status[1].state {
            mcp::ServerState::Failed(message) => message.clone(),
            other => panic!("{other:?}"),
        };
        let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let connected = text
            .lines()
            .find(|line| line.contains("msg=mcp_server_connected"))
            .unwrap_or_else(|| panic!("no connected event in:\n{text}"));
        for field in ["level=INFO", "server=good", "transport=stdio", "tools=1"] {
            assert!(connected.contains(field), "{field} missing in {connected}");
        }
        let failed = text
            .lines()
            .find(|line| line.contains("msg=mcp_server_failed"))
            .unwrap_or_else(|| panic!("no failed event in:\n{text}"));
        for field in ["level=WARN", "server=broken", "transport=stdio"] {
            assert!(failed.contains(field), "{field} missing in {failed}");
        }
        let quoted = serde_json::to_string(&shown).unwrap();
        assert!(
            failed.contains(&format!("error={quoted}"))
                || failed.contains(&format!("error={shown}")),
            "the logged error must equal the /mcp text {shown:?}: {failed}"
        );
        servers.close().await;
    }

    #[tokio::test]
    async fn a_server_whose_secrets_exceed_redaction_limits_fails_without_connecting() {
        let directory = tempfile::tempdir().expect("directory");
        let mut server = stdio_server(
            "oversized",
            true,
            "otto-mcp-test-nonexistent-command",
            directory.path(),
        );
        server.secrets = vec!["x".repeat(9 * 1024)];
        let builder = builder_with_servers(directory.path(), vec![server]);

        let mut warnings = Vec::new();
        let (tools, connected, servers) = builder.connect_mcp(65536, &mut warnings).await;
        assert!(tools.is_empty());
        assert!(connected.is_empty());
        let status = servers.status();
        assert_eq!(status.len(), 1);
        assert_eq!(
            status[0].state,
            mcp::ServerState::Failed(SECRETS_EXCEED_REDACTION_LIMITS.to_string())
        );
        let text = String::from_utf8(warnings).expect("utf-8 warnings");
        assert!(text.contains(SECRETS_EXCEED_REDACTION_LIMITS), "{text:?}");
    }

    #[tokio::test]
    async fn connection_results_are_reported_in_configuration_order() {
        let directory = tempfile::tempdir().expect("directory");
        let builder = builder_with_servers(
            directory.path(),
            vec![
                stdio_server(
                    "first",
                    true,
                    "otto-mcp-test-first-missing",
                    directory.path(),
                ),
                stdio_server(
                    "second",
                    true,
                    "otto-mcp-test-second-missing",
                    directory.path(),
                ),
            ],
        );

        let mut warnings = Vec::new();
        let (tools, connected, servers) = builder.connect_mcp(65536, &mut warnings).await;
        assert!(tools.is_empty());
        assert!(connected.is_empty());
        let status = servers.status();
        assert_eq!(status.len(), 2);
        assert_eq!(status[0].name, "first");
        assert_eq!(status[1].name, "second");
        let text = String::from_utf8(warnings).expect("utf-8 warnings");
        let first = text.find("first").expect("first warning");
        let second = text.find("second").expect("second warning");
        assert!(first < second, "{text}");
    }

    #[test]
    fn a_secret_in_a_connect_failure_message_is_redacted() {
        // `connect_mcp` cannot be driven end to end with a real connect
        // failure that echoes a secret (a spawn error's text is OS-owned,
        // not server-controlled); this drives the extracted pure
        // `failure_message` helper `connect_mcp` calls in its `Err(error)`
        // arm instead.
        let secrets = vec!["SECRET123".to_owned()];
        let marker = dynamic_redaction_marker(&secrets).expect("marker");
        let error = mcp::CallError::Transport("token=SECRET123".to_owned());
        let message = failure_message(&error, &secrets, &marker);
        assert!(!message.contains("SECRET123"), "{message:?}");
        assert!(message.contains("token="), "{message:?}");
    }

    #[test]
    fn mcp_child_tools_is_empty_for_no_connected_servers() {
        assert!(mcp_child_tools(&[], 65536).is_empty());
    }

    #[test]
    fn lazy_mcp_exposes_router_tools_instead_of_remote_tool_schemas() {
        let server: Arc<dyn mcp::ToolServer> = Arc::new(FakeToolServer {
            name: "notion".into(),
        });
        let connected = vec![ConnectedServer {
            server,
            tool_infos: vec![tool_info("notion-fetch"), tool_info("notion-ai-search")],
            secrets: Vec::new(),
            bearer: None,
            tool_names: vec![
                "mcp__notion__notion-fetch".to_string(),
                "mcp__notion__notion-ai-search".to_string(),
            ],
        }];

        let names = mcp_child_tools(&connected, 65536)
            .into_iter()
            .map(|tool| tool.definition().name)
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["mcp_search_tools", "mcp_call_tool"]);
        assert!(!names.iter().any(|name| name.starts_with("mcp__notion__")));
    }

    // --- cross-server tool-name collisions (Finding 2) ---
    //
    // `connect_mcp` itself is not exercised end to end here: producing a
    // real collision needs two servers that actually answer `tools/list`,
    // which needs a live connection the offline test rule forbids. Instead
    // this drives `tools_for` with two fake `ToolServer`s the way
    // `connect_mcp` does, then feeds the results through the extracted pure
    // `dedup_cross_server` helper `connect_mcp` also calls.

    struct FakeToolServer {
        name: String,
    }

    #[async_trait::async_trait]
    impl mcp::ToolServer for FakeToolServer {
        fn name(&self) -> &str {
            &self.name
        }

        async fn call(
            &self,
            _tool: &str,
            _arguments: serde_json::Value,
            _cancel: &CancellationToken,
        ) -> Result<mcp::CallOutcome, mcp::CallError> {
            Ok(mcp::CallOutcome::default())
        }
    }

    fn tool_info(name: &str) -> mcp::ToolInfo {
        mcp::ToolInfo {
            name: name.to_owned(),
            title: None,
            description: None,
            input_schema: None,
        }
    }

    #[test]
    fn a_cross_server_collision_is_skipped_with_a_warning_and_the_child_set_matches() {
        // Server "a" tool "b__c" and server "a__b" tool "c" both sanitize to
        // "mcp__a__b__c": `tools_for`'s per-server dedup cannot see this, so
        // it builds both tools without a warning.
        let server_a: Arc<dyn mcp::ToolServer> = Arc::new(FakeToolServer { name: "a".into() });
        let server_a_b: Arc<dyn mcp::ToolServer> = Arc::new(FakeToolServer {
            name: "a__b".into(),
        });
        let (tools_a, warnings_a) = tools_for(
            Arc::clone(&server_a),
            &[tool_info("b__c")],
            65536,
            Vec::new(),
            None,
        );
        assert!(warnings_a.is_empty(), "{warnings_a:?}");
        let (tools_b, warnings_b) = tools_for(
            Arc::clone(&server_a_b),
            &[tool_info("c")],
            65536,
            Vec::new(),
            None,
        );
        assert!(warnings_b.is_empty(), "{warnings_b:?}");
        assert_eq!(tools_a[0].definition().name, "mcp__a__b__c");
        assert_eq!(tools_b[0].definition().name, "mcp__a__b__c");

        let mut registered = HashSet::new();
        let (keep_a, dedup_warnings_a) = dedup_cross_server("a", &tools_a, &mut registered);
        let (keep_b, dedup_warnings_b) = dedup_cross_server("a__b", &tools_b, &mut registered);

        assert_eq!(keep_a, vec![0], "the first server's tool is registered");
        assert!(dedup_warnings_a.is_empty(), "{dedup_warnings_a:?}");
        assert!(
            keep_b.is_empty(),
            "the second server's colliding tool is skipped"
        );
        assert_eq!(dedup_warnings_b.len(), 1);
        assert_eq!(
            dedup_warnings_b[0],
            "mcp a__b: skipping tool \"c\": name \"mcp__a__b__c\" is already provided by another server"
        );
    }
}

/// Persists each sub-agent transcript beside the parent session file, as
/// `<parent without .jsonl>/<task id>-<child id>.jsonl`. A parent with no file
/// (`--no-session`, or nothing written yet) keeps its children in memory.
fn child_sessions(parent: SharedSession, storage: Arc<super::storage::Storage>) -> ChildSession {
    Arc::new(move |task_id: &str| {
        let parent_path = parent.path();
        if parent_path.is_empty() {
            return Ok(None);
        }
        let mut header = parent.header();
        header.id = super::builder::random_id()
            .map_err(|error| format!("create sub-agent session id: {error}"))?;
        header.created_at = chrono::Utc::now();
        let name = format!("{task_id}-{}", header.id);
        let (store, path) =
            storage
                .sessions()?
                .create_child_lazy(&parent_path, &name, header, parent.lease())?;
        let transcript: Transcript = store;
        Ok(Some((transcript, path)))
    })
}

/// Builds a child's durable mailbox, `<child transcript>.inbox.json`, and
/// restores the parent messages it had not read. An unreadable sidecar is
/// moved aside and the child is told so in its first notification.
pub(crate) fn child_mailbox() -> crate::subagent::runner::ChildMailbox {
    Arc::new(|transcript: &str| {
        let mailbox = Arc::new(otto_core::mailbox::InMemoryMailbox::new(None));
        let sidecar = Path::new(transcript).with_extension("inbox.json");
        let warning = super::sidecar_mailbox::load(&mailbox, &sidecar);
        super::sidecar_mailbox::install(&mailbox, ".inbox.json", move || Some(sidecar.clone()));
        if let Some(warning) = warning {
            mailbox.push(otto_core::agent::inbox::Notification {
                kind: Some(otto_core::agent::inbox::NotificationKind::Message),
                text: format!("[parent-message] {warning}"),
                ..Default::default()
            });
        }
        mailbox as Arc<dyn otto_core::mailbox::Mailbox>
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{NAMESPACE_USER, NAMESPACE_WORKSPACE, RememberRequest, SearchRequest};
    use otto_core::agent::inbox::Inbox;

    #[test]
    fn children_of_an_in_memory_parent_stay_in_memory() {
        let build = child_sessions(
            SharedSession::memory(otto_core::session::Header::default()),
            Arc::new(Default::default()),
        );
        assert!(build("t1").expect("build").is_none());
    }

    #[tokio::test]
    async fn a_child_store_refuses_to_append_after_the_parent_lease_is_lost() {
        let root = tempfile::tempdir().expect("root");
        let workspace = tempfile::tempdir().expect("workspace");
        let lease = crate::failover::lease::Lease::for_test(workspace.path());
        let store = crate::session::Store::create(
            root.path(),
            otto_core::session::Header {
                id: "parent".into(),
                workspace: workspace.path().to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: chrono::Utc::now(),
                ..otto_core::session::Header::default()
            },
        )
        .expect("parent");
        store.set_lease_check(Arc::clone(&lease) as Arc<dyn crate::session::WriterOwnership>);
        let build = child_sessions(
            SharedSession::new(Arc::new(store)),
            Arc::new(Default::default()),
        );

        let (child, _path) = build("t1").expect("build").expect("persisted");

        lease.mark_lost_for_test("test fence");

        let message = otto_core::model::Message {
            role: otto_core::model::Role::User,
            blocks: vec![otto_core::model::Block::text("hello")],
            ..otto_core::model::Message::default()
        };
        let error = child
            .append(message)
            .await
            .expect_err("append must be rejected once the parent lease is lost");
        assert!(error.to_string().contains("session lease lost"), "{error}");
    }

    #[tokio::test]
    async fn children_of_a_session_file_are_written_beside_it() {
        let root = tempfile::tempdir().expect("root");
        let workspace = tempfile::tempdir().expect("workspace");
        let store = crate::session::Store::create(
            root.path(),
            otto_core::session::Header {
                id: "parent".into(),
                workspace: workspace.path().to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: chrono::Utc::now(),
                ..otto_core::session::Header::default()
            },
        )
        .expect("parent");
        let parent_path = store.path();
        let build = child_sessions(
            SharedSession::new(Arc::new(store)),
            Arc::new(Default::default()),
        );

        let (_, path) = build("t1").expect("build").expect("persisted");

        let prefix = std::path::Path::new(&parent_path)
            .with_extension("")
            .join("t1-");
        assert!(path.starts_with(&*prefix.to_string_lossy()), "{path}");
        assert!(path.ends_with(".jsonl"), "{path}");
    }

    #[test]
    fn a_reopened_inbox_loads_its_sidecar_and_the_file_is_updated_after_delivery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("s1.inbox.json");
        std::fs::write(
            &path,
            r#"[
                {"seq":0,"task_id":"t1","kind":"task_finished","text":"first","usage":null},
                {"seq":1,"task_id":"t2","kind":"task_finished","text":"second","usage":null}
            ]"#,
        )
        .expect("write sidecar");

        let inbox = Inbox::new(None);
        assert_eq!(super::super::sidecar_mailbox::load(&inbox, &path), None);
        let hook_path = path.clone();
        super::super::sidecar_mailbox::install(&inbox, ".inbox.json", move || {
            Some(hook_path.clone())
        });

        let queued = inbox.queued();
        assert_eq!(
            queued
                .iter()
                .map(|entry| entry.notification.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );

        // Delivery removes each loaded item by sequence number, exactly like
        // `deliver_notifications`, so every one is handled exactly once.
        let mut delivered = Vec::new();
        for entry in inbox.queued() {
            delivered.push(entry.notification.text.clone());
            inbox.remove_seq(entry.seq);
        }
        assert_eq!(delivered, ["first", "second"]);
        assert!(inbox.is_empty());
        assert!(
            !path.exists(),
            "the sidecar is removed once every loaded notification has been delivered"
        );
    }

    /// A database path whose parent is a regular file, so every open fails
    /// deterministically.
    fn unopenable_path(directory: &std::path::Path) -> String {
        let blocker = directory.join("blocker-file");
        std::fs::write(&blocker, b"not a directory").expect("write blocker");
        blocker.join("memory").to_string_lossy().into_owned()
    }

    /// A store directory below the temporary directory.
    fn store_path(directory: &std::path::Path) -> String {
        directory.join("memory").to_string_lossy().into_owned()
    }

    fn enabled(path: String) -> MemoryRuntime {
        MemoryRuntime {
            enabled: true,
            backend: "files".into(),
            dir: path,
            ..MemoryRuntime::default()
        }
    }

    #[test]
    fn disabled_memory_returns_an_unusable_null_service() {
        let mut stderr = Vec::new();
        let (service, scope, usable) =
            open_memory_service(&MemoryRuntime::default(), &[], &mut stderr).expect("open");
        assert!(!usable);
        assert_eq!(scope, Scope::default());
        assert_eq!(
            service
                .search(&SearchRequest::default())
                .expect_err("disabled")
                .kind,
            ErrorKind::Disabled
        );
        assert!(stderr.is_empty(), "{stderr:?}");
        service.close().expect("close");
    }

    #[test]
    fn an_open_failure_degrades_to_a_null_service_unless_required() {
        let directory = tempfile::tempdir().expect("directory");
        let mut config = enabled(unopenable_path(directory.path()));

        let mut stderr = Vec::new();
        let (service, scope, usable) =
            open_memory_service(&config, &[], &mut stderr).expect("degrade");
        assert!(!usable);
        assert_eq!(scope, Scope::default());
        assert_eq!(
            service
                .search(&SearchRequest::default())
                .expect_err("unavailable")
                .kind,
            ErrorKind::Unavailable
        );
        assert!(
            String::from_utf8_lossy(&stderr).contains("warning"),
            "{stderr:?}"
        );
        service.close().expect("close");

        config.required = true;
        assert!(open_memory_service(&config, &[], &mut Vec::new()).is_err());
    }

    #[test]
    fn an_invalid_record_file_is_reported_once_and_the_service_stays_usable() {
        let directory = tempfile::tempdir().expect("directory");
        let path = store_path(directory.path());
        let config = enabled(path.clone());
        let (service, _, usable) =
            open_memory_service(&config, &[], &mut Vec::new()).expect("open");
        assert!(usable);
        service.close().expect("close");
        std::fs::write(
            std::path::Path::new(&path).join("records").join("bad.md"),
            "not a record\n",
        )
        .expect("write bad record");

        let mut stderr = Vec::new();
        let (service, _, usable) = open_memory_service(&config, &[], &mut stderr).expect("reopen");
        assert!(usable);
        let stderr = String::from_utf8_lossy(&stderr);
        let lines: Vec<&str> = stderr
            .lines()
            .filter(|line| line.starts_with(SKIPPED_FILE_PREFIX))
            .collect();
        assert_eq!(lines.len(), 1, "{stderr:?}");
        assert!(lines[0].contains("bad.md"), "{}", lines[0]);
        assert!(!stderr.contains("not a record"), "{stderr:?}");
        service.close().expect("close");
    }

    #[test]
    fn a_successful_open_returns_a_stable_user_scope() {
        let directory = tempfile::tempdir().expect("directory");
        let config = enabled(store_path(directory.path()));

        let (service, scope, usable) =
            open_memory_service(&config, &[], &mut Vec::new()).expect("open");
        assert!(usable);
        assert_eq!(scope.namespace, NAMESPACE_USER);
        assert!(!scope.id.is_empty());
        service.close().expect("close");

        let (reopened, again, usable) =
            open_memory_service(&config, &[], &mut Vec::new()).expect("reopen");
        assert!(usable);
        assert_eq!(again, scope, "the installation id must survive a reopen");
        reopened.close().expect("close");
    }

    #[test]
    fn require_encryption_fails_because_turso_never_encrypts_at_rest() {
        let directory = tempfile::tempdir().expect("directory");
        let config = MemoryRuntime {
            require_encryption: true,
            ..enabled(store_path(directory.path()))
        };
        let error = match open_memory_service(&config, &[], &mut Vec::new()) {
            Err(error) => error,
            Ok(_) => panic!("require_encryption must fail startup"),
        };
        assert!(error.contains("encrypt"), "{error}");
    }

    #[test]
    fn configured_secret_values_are_rejected_in_remembered_text() {
        let directory = tempfile::tempdir().expect("directory");
        let config = enabled(store_path(directory.path()));
        let secrets = ["sk-configured-secret".to_string()];

        let (service, scope, usable) =
            open_memory_service(&config, &secrets, &mut Vec::new()).expect("open");
        assert!(usable);
        let error = service
            .remember(&RememberRequest {
                scope,
                kind: "preference".into(),
                key: "credentials".into(),
                text: "the api key is sk-configured-secret".into(),
                ..RememberRequest::default()
            })
            .expect_err("sensitive");
        assert_eq!(error.kind, ErrorKind::SensitiveMemory);
        service.close().expect("close");
    }

    #[test]
    fn an_invalid_secret_value_fails_the_open() {
        let directory = tempfile::tempdir().expect("directory");
        let config = enabled(store_path(directory.path()));
        assert!(
            open_memory_service(&config, &[String::new()], &mut Vec::new()).is_err(),
            "an empty guard value must be rejected"
        );
    }

    #[test]
    fn the_workspace_scope_uses_a_configured_override() {
        let config = MemoryRuntime {
            workspace_ids: std::collections::HashMap::from([(
                "/work/otto".to_string(),
                "custom-id".to_string(),
            )]),
            ..MemoryRuntime::default()
        };
        let scope = workspace_memory_scope(&config, "/work/otto").expect("scope");
        assert_eq!(scope.namespace, NAMESPACE_WORKSPACE);
        assert_eq!(scope.id, "custom-id");
    }

    #[test]
    fn the_workspace_scope_is_derived_from_the_path_without_an_override() {
        let directory = tempfile::tempdir().expect("directory");
        let scope = workspace_memory_scope(
            &MemoryRuntime::default(),
            &directory.path().to_string_lossy(),
        )
        .expect("scope");
        assert_eq!(scope.namespace, NAMESPACE_WORKSPACE);
        assert!(scope.id.starts_with("sha256:"), "{}", scope.id);
    }

    // -----------------------------------------------------------------
    // R12: build_subagents reports a takeover through the recovery push.
    //
    // A takeover is simulated the same way `session::tests` does: a lease
    // directory is written by hand (no live `Lease` of this process's own),
    // with `lease_seconds` = 6. `Prepared::prepare`/`activate` then performs
    // a real takeover, populating the returned store's `take_recovery()` and
    // handing this process a live `Keeper::global` lease. `L` must stay at
    // or above 6: the watchdog fences a live global-keeper lease at 5L/6
    // staleness, and the renewal thread ticks every 1s, so L < 6 leaves no
    // margin and calls `libc::_exit(75)` mid-test. `Prepared::prepare`'s
    // 7L/6 takeover wait is therefore about 7s per test.
    // -----------------------------------------------------------------

    fn recovery_test_header(workspace: &Path) -> otto_core::session::Header {
        otto_core::session::Header {
            id: "parent".into(),
            workspace: workspace.to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: chrono::Utc::now(),
            ..otto_core::session::Header::default()
        }
    }

    fn recovery_dangling_call(id: &str, name: &str) -> otto_core::model::Message {
        otto_core::model::Message {
            role: otto_core::model::Role::Assistant,
            blocks: vec![otto_core::model::Block {
                block_type: otto_core::model::BlockType::ToolCall,
                tool_call_id: id.into(),
                tool_name: name.into(),
                arguments: Some(
                    serde_json::value::RawValue::from_string("{}".into()).expect("valid JSON"),
                ),
                ..otto_core::model::Block::default()
            }],
            created_at: chrono::Utc::now(),
            finish_reason: Some(otto_core::model::FinishReason::ToolCalls),
            ..otto_core::model::Message::default()
        }
    }

    fn recovery_assistant_text(text: &str) -> otto_core::model::Message {
        otto_core::model::Message {
            role: otto_core::model::Role::Assistant,
            blocks: vec![otto_core::model::Block::text(text)],
            created_at: chrono::Utc::now(),
            finish_reason: Some(otto_core::model::FinishReason::Stop),
            ..otto_core::model::Message::default()
        }
    }

    /// The `lease.json` + `epoch-<epoch>` + `heartbeat` layout
    /// `failover::lease` writes, reproduced by hand so no live `Lease` of
    /// this process's own is ever created; see the section header above and
    /// `session::tests`'s helper of the same shape.
    fn recovery_write_lease_dir_by_hand(session_path: &Path, host: &str, pid: u32) {
        let dir = session_path.with_extension("lease");
        std::fs::create_dir_all(&dir).expect("create lease dir by hand");
        std::fs::write(
            dir.join("lease.json"),
            serde_json::json!({ "lease_seconds": 6u64 }).to_string(),
        )
        .expect("write lease.json by hand");
        std::fs::write(dir.join("epoch-1"), b"{}").expect("write epoch marker by hand");
        let json = serde_json::json!({
            "epoch": 1u64,
            "host": host,
            "pid": pid,
            "released": false,
            "seq": 1u64,
        })
        .to_string();
        let mut bytes = vec![b' '; 512];
        bytes[..json.len()].copy_from_slice(json.as_bytes());
        bytes[511] = b'\n';
        std::fs::write(dir.join("heartbeat"), bytes).expect("write heartbeat by hand");
    }

    /// Builds a `Builder` and its `build_subagents` call args over a fresh
    /// workspace/session root, with sub-agents unconditionally enabled.
    fn recovery_test_builder(root: &Path) -> (Builder, Runtime, ProviderClient, CatalogWiring) {
        let session_root = root.join("sessions");
        let builder = crate::cli::testutil::builder(root, &session_root);
        let runtime = Runtime {
            provider: "openai-compatible".to_string(),
            base_url: "http://127.0.0.1:0".to_string(),
            model: "test-model".to_string(),
            max_output_bytes: 4096,
            ..Runtime::default()
        };
        let client = ProviderClient::Compat {
            client: Arc::new(crate::provider::openaicompat::Client::new(
                &runtime.base_url,
                "test-key",
            )),
            timeout: runtime.resilience.deadlines.provider_timeout,
            cancellation_grace: runtime.resilience.deadlines.cancellation_grace,
        };
        let catalogs = CatalogWiring {
            skills: skill::Catalog::default(),
            skill_section: String::new(),
            agents: AgentsRuntime {
                enabled: true,
                roots: Vec::new(),
                max_parallel: 4,
            },
            agent_catalog: subagent::Catalog::default(),
            agent_section: String::new(),
        };
        (builder, runtime, client, catalogs)
    }

    #[allow(clippy::too_many_arguments)]
    fn recovery_call_build_subagents(
        builder: &Builder,
        runtime: &Runtime,
        client: &ProviderClient,
        catalogs: &CatalogWiring,
        session: &SharedSession,
    ) -> SubagentWiring {
        let mut tools = Vec::new();
        let mut stderr = Vec::new();
        let redactor = Redactor::with_completeness(&[], true);
        let prompt_for = builder.child_prompt_for(runtime, "host", "");
        builder
            .build_subagents(
                &mut tools,
                catalogs,
                client,
                &[],
                &redactor,
                runtime,
                session,
                prompt_for,
                Vec::new(),
                None,
                None,
                &mut stderr,
            )
            .expect("build_subagents")
    }

    #[test]
    fn a_takeover_with_a_dangling_call_and_an_interrupted_child_pushes_one_recovery_notification() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let session_root = root.path().join("sessions");
        std::fs::create_dir_all(&session_root).expect("create session root");

        let parent = crate::session::Store::create(&session_root, recovery_test_header(&workspace))
            .expect("create parent");
        parent
            .append_message(&recovery_dangling_call("call-1", "read"))
            .expect("append dangling call");
        let parent_path = parent.path();
        parent.close().expect("close before simulating a takeover");

        // A pre-existing inbox sidecar: build_subagents must load this and
        // keep it queued alongside the recovery push, not replace it.
        let inbox_path = Path::new(&parent_path).with_extension("inbox.json");
        std::fs::write(
            &inbox_path,
            serde_json::to_vec_pretty(&[otto_core::agent::inbox::Entry {
                seq: 0,
                notification: otto_core::agent::inbox::Notification {
                    task_id: "t0".to_string(),
                    kind: Some(otto_core::agent::inbox::NotificationKind::TaskFinished),
                    text: "already queued".to_string(),
                    usage: None,
                    ..Default::default()
                },
            }])
            .expect("encode inbox"),
        )
        .expect("write inbox sidecar");

        // An interrupted child transcript: `otto.task_spec` with no
        // `otto.task_result`.
        let child = crate::session::Store::create_child_lazy(
            &parent_path,
            "t1-child",
            otto_core::session::Header {
                id: "child".into(),
                workspace: workspace.to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: chrono::Utc::now(),
                ..otto_core::session::Header::default()
            },
        )
        .expect("create child");
        child
            .append_custom_entry(
                crate::subagent::runner::TASK_SPEC_CUSTOM_TYPE,
                &serde_json::json!({
                    "id": "t1",
                    "name": "worker",
                    "description": "",
                    "model": "test-model",
                    "context": "fresh",
                    "prompt": "do it",
                    "definition": null,
                })
                .to_string(),
            )
            .expect("append task_spec");
        child.close().expect("close child");
        let children_dir = Path::new(&parent_path).with_extension("");

        recovery_write_lease_dir_by_hand(Path::new(&parent_path), "stopped-host", 4242);

        let (activated, _warnings) = crate::failover::lease::without_acquire_wait(|| {
            crate::session::Prepared::prepare(Path::new(&parent_path), None)
        })
        .expect("prepare takes over")
        .activate()
        .expect("activate");
        let session = SharedSession::new(Arc::new(activated));

        let (builder, runtime, client, catalogs) = recovery_test_builder(root.path());
        let wiring =
            recovery_call_build_subagents(&builder, &runtime, &client, &catalogs, &session);

        let inbox = wiring.inbox.expect("inbox");
        let queued = inbox.queued();
        assert_eq!(queued.len(), 2, "{queued:?}");
        assert_eq!(queued[0].notification.text, "already queued");
        assert!(
            queued[1].notification.text.contains("stopped-host"),
            "{}",
            queued[1].notification.text
        );
        assert!(
            queued[1].notification.text.contains("read"),
            "{}",
            queued[1].notification.text
        );
        assert!(
            queued[1].notification.text.contains("t1"),
            "{}",
            queued[1].notification.text
        );

        let on_disk: Vec<otto_core::agent::inbox::Entry> =
            serde_json::from_str(&std::fs::read_to_string(&inbox_path).expect("read inbox file"))
                .expect("decode inbox file");
        assert_eq!(on_disk.len(), 2, "{on_disk:?}");

        let records = crate::subagent::interrupted::scan(&children_dir);
        let record = records
            .iter()
            .find(|record| record.task_id == "t1")
            .expect("the child transcript was scanned");
        assert_eq!(
            record.final_status,
            Some(crate::subagent::interrupted::INTERRUPTED_STATUS.to_string()),
            "notify's mark_interrupted call must have run after the push"
        );
    }

    #[test]
    fn a_plain_reopen_after_process_loss_notifies_and_marks_children_interrupted() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let session_root = root.path().join("sessions");
        std::fs::create_dir_all(&session_root).expect("create session root");

        let parent = crate::session::Store::create(&session_root, recovery_test_header(&workspace))
            .expect("create parent");
        parent
            .append_custom_entry(
                otto_core::session::TURN_CUSTOM_TYPE,
                &otto_core::session::encode_turn_record(&otto_core::session::TurnRecord::started(
                    "op_lost",
                    otto_core::session::TurnTrigger::User,
                    1,
                    None,
                )),
            )
            .expect("turn start");
        parent
            .append_message(&recovery_dangling_call("call-1", "bash"))
            .expect("append dangling call");
        let parent_path = parent.path();
        let child = crate::session::Store::create_child_lazy(
            &parent_path,
            "t1-child",
            otto_core::session::Header {
                id: "child".into(),
                workspace: workspace.to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: chrono::Utc::now(),
                ..otto_core::session::Header::default()
            },
        )
        .expect("create child");
        child
            .append_custom_entry(
                crate::subagent::runner::TASK_SPEC_CUSTOM_TYPE,
                &serde_json::json!({
                    "id": "t1",
                    "name": "worker",
                    "description": "",
                    "model": "test-model",
                    "context": "fresh",
                    "prompt": "do it",
                    "definition": null,
                })
                .to_string(),
            )
            .expect("append task_spec");
        child.close().expect("close child");
        // The process stops: the parent is closed without ending its turn,
        // and no lease directory exists.
        parent.close().expect("close without ending the turn");
        let children_dir = Path::new(&parent_path).with_extension("");

        let (activated, _warnings) =
            crate::session::Store::open(Path::new(&parent_path)).expect("reopen");
        let session = SharedSession::new(Arc::new(activated));
        let (builder, runtime, client, catalogs) = recovery_test_builder(root.path());
        let wiring =
            recovery_call_build_subagents(&builder, &runtime, &client, &catalogs, &session);

        let queued = wiring.inbox.expect("inbox").queued();
        assert_eq!(queued.len(), 1, "{queued:?}");
        let text = &queued[0].notification.text;
        assert!(text.contains("previous Otto process"), "{text}");
        assert!(text.contains("Continue the interrupted turn"), "{text}");
        assert!(text.contains("bash"), "{text}");
        assert!(text.contains("t1"), "{text}");
        let record = crate::subagent::interrupted::scan(&children_dir)
            .into_iter()
            .find(|record| record.task_id == "t1")
            .expect("the child transcript was scanned");
        assert_eq!(
            record.final_status,
            Some(crate::subagent::interrupted::INTERRUPTED_STATUS.to_string()),
        );
    }

    #[test]
    fn a_takeover_with_no_dangling_state_and_no_children_pushes_nothing() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let session_root = root.path().join("sessions");
        std::fs::create_dir_all(&session_root).expect("create session root");

        let parent = crate::session::Store::create(&session_root, recovery_test_header(&workspace))
            .expect("create parent");
        parent
            .append_message(&recovery_assistant_text("all done"))
            .expect("append finished turn");
        let parent_path = parent.path();
        parent.close().expect("close before simulating a takeover");

        recovery_write_lease_dir_by_hand(Path::new(&parent_path), "stopped-host", 4242);

        let (activated, _warnings) = crate::failover::lease::without_acquire_wait(|| {
            crate::session::Prepared::prepare(Path::new(&parent_path), None)
        })
        .expect("prepare takes over")
        .activate()
        .expect("activate");
        assert!(
            activated.take_recovery().is_some(),
            "sanity: the parent must actually have been taken over"
        );
        // Re-open to get an un-taken `take_recovery()` back for the build,
        // since the sanity check above just consumed it.
        activated.close().expect("close");
        let (activated, _warnings) = crate::session::Store::open(Path::new(&parent_path))
            .expect("reopen without a lease dir race: no other holder exists");

        let session = SharedSession::new(Arc::new(activated));
        let (builder, runtime, client, catalogs) = recovery_test_builder(root.path());
        let wiring =
            recovery_call_build_subagents(&builder, &runtime, &client, &catalogs, &session);

        let inbox = wiring.inbox.expect("inbox");
        assert!(inbox.is_empty(), "{:?}", inbox.queued());
    }

    #[test]
    fn a_second_build_subagents_for_the_same_store_pushes_nothing_more() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let session_root = root.path().join("sessions");
        std::fs::create_dir_all(&session_root).expect("create session root");

        let parent = crate::session::Store::create(&session_root, recovery_test_header(&workspace))
            .expect("create parent");
        parent
            .append_message(&recovery_dangling_call("call-1", "read"))
            .expect("append dangling call");
        let parent_path = parent.path();
        parent.close().expect("close before simulating a takeover");

        recovery_write_lease_dir_by_hand(Path::new(&parent_path), "stopped-host", 4242);

        let (activated, _warnings) = crate::failover::lease::without_acquire_wait(|| {
            crate::session::Prepared::prepare(Path::new(&parent_path), None)
        })
        .expect("prepare takes over")
        .activate()
        .expect("activate");
        let session = SharedSession::new(Arc::new(activated));

        let (builder, runtime, client, catalogs) = recovery_test_builder(root.path());
        let first = recovery_call_build_subagents(&builder, &runtime, &client, &catalogs, &session);
        assert_eq!(first.inbox.expect("inbox").len(), 1);

        // The second call builds a fresh `Tasks`/`Inbox` that reloads the
        // sidecar the first call persisted, so it still shows that one
        // entry; the assertion is that the count does not grow to 2, which
        // it would if `notify` pushed again from a takeover that
        // `take_recovery()` had already consumed on the first call.
        let second =
            recovery_call_build_subagents(&builder, &runtime, &client, &catalogs, &session);
        assert_eq!(
            second.inbox.expect("inbox").len(),
            1,
            "take_recovery() already consumed the record on the first call, so the second call must not push again"
        );
    }
}

/// Each case is folded onto the composition seam it exercises:
/// [`Builder::build_catalogs`] for the prompt sections, the tool list and the
/// warnings, [`Builder::boundary_catalog_definitions`] for the redaction
/// boundary, and `resolve_sandbox_settings` for the Seatbelt read paths. A full
/// `skill` tool call/result round trip belongs to `skill::tool`'s own tests and
/// the section's XML position after `<environment>` to `system_prompt_for`, so
/// neither is repeated here.
#[cfg(test)]
mod catalog_tests {
    use super::*;
    use crate::cli::testutil;
    use std::path::{Path, PathBuf};

    /// Writes `<root>/<directory>/<file>` with the standard frontmatter shape
    /// and returns the definition directory.
    fn write_definition(
        root: &Path,
        file: &str,
        directory: &str,
        name: &str,
        description: &str,
        body: &str,
    ) -> PathBuf {
        let directory = root.join(directory);
        std::fs::create_dir_all(&directory).expect("definition directory");
        let content = format!("---\nname: {name}\ndescription: {description}\n---\n{body}");
        std::fs::write(directory.join(file), content).expect("definition file");
        directory
    }

    /// A builder over two canonical temporary directories, with `HOME` set so
    /// the default `~/.otto/...` roots resolve under the first one.
    struct Fixture {
        _home: tempfile::TempDir,
        _workspace: tempfile::TempDir,
        home: PathBuf,
        workspace: PathBuf,
        builder: Builder,
    }

    fn fixture() -> Fixture {
        let home_dir = tempfile::tempdir().expect("home");
        let workspace_dir = tempfile::tempdir().expect("workspace");
        let home = std::fs::canonicalize(home_dir.path()).expect("canonical home");
        let workspace = std::fs::canonicalize(workspace_dir.path()).expect("canonical workspace");
        let mut builder = testutil::builder(&workspace, workspace_dir.path());
        builder
            .shared_mut()
            .environment
            .insert("HOME".to_string(), home.to_string_lossy().into_owned());
        Fixture {
            _home: home_dir,
            _workspace: workspace_dir,
            home,
            workspace,
            builder,
        }
    }

    /// `build_catalogs` with its two output channels made inspectable.
    fn catalogs(builder: &Builder) -> (CatalogWiring, Vec<String>, String) {
        let mut tools: Vec<Box<dyn Tool + Send + Sync>> = Vec::new();
        let mut stderr = Vec::new();
        let wiring = builder
            .build_catalogs(&mut tools, 65536, &mut stderr)
            .expect("build_catalogs");
        let names = tools
            .iter()
            .map(|tool| tool.definition().name.clone())
            .collect();
        (
            wiring,
            names,
            String::from_utf8(stderr).expect("utf-8 warnings"),
        )
    }

    fn definition_names(definitions: &[ToolDefinition]) -> Vec<String> {
        definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect()
    }

    #[test]
    fn a_user_level_skill_is_listed_and_registers_the_skill_tool() {
        let fixture = fixture();
        let directory = write_definition(
            &fixture.home.join(".otto/skills"),
            "SKILL.md",
            "pdf",
            "pdf",
            "Extract text and tables from PDF files.",
            "# PDF handling\nExtract pdfs from files.\n",
        );

        let (wiring, tools, warnings) = catalogs(&fixture.builder);
        assert!(
            wiring.skill_section.contains("\n<skills>\n"),
            "{:?}",
            wiring.skill_section
        );
        assert!(
            wiring.skill_section.contains(&format!(
                "<skill name=\"pdf\" location=\"{}\"",
                directory.display()
            )),
            "{:?}",
            wiring.skill_section
        );
        assert_eq!(tools, vec!["skill".to_string()]);
        assert_eq!(warnings, "");
    }

    #[test]
    fn a_workspace_skill_overrides_a_user_level_skill() {
        let fixture = fixture();
        let user = write_definition(
            &fixture.home.join(".otto/skills"),
            "SKILL.md",
            "repo-notes",
            "repo-notes",
            "User-level notes skill.",
            "user body\n",
        );
        let workspace = write_definition(
            &fixture.workspace.join(".otto/skills"),
            "SKILL.md",
            "repo-notes",
            "repo-notes",
            "Workspace-level notes skill.",
            "workspace body\n",
        );

        let (wiring, _, _) = catalogs(&fixture.builder);
        assert!(
            wiring.skill_section.contains(&format!(
                "<skill name=\"repo-notes\" location=\"{}\"",
                workspace.display()
            )),
            "{:?}",
            wiring.skill_section
        );
        assert!(
            !wiring.skill_section.contains(&user.display().to_string()),
            "the overridden user skill must not be listed: {:?}",
            wiring.skill_section
        );
    }

    #[test]
    fn no_skills_omit_the_section_and_the_tool() {
        let fixture = fixture();
        let (wiring, tools, warnings) = catalogs(&fixture.builder);
        assert_eq!(wiring.skill_section, "");
        assert!(tools.is_empty(), "{tools:?}");
        assert_eq!(warnings, "");
    }

    #[test]
    fn disabled_skills_omit_the_section_the_tool_and_the_boundary_definition() {
        let mut fixture = fixture();
        write_definition(
            &fixture.home.join(".otto/skills"),
            "SKILL.md",
            "pdf",
            "pdf",
            "Extract pdfs.",
            "body\n",
        );
        fixture.builder.shared_mut().config.skills.enabled = Some(false);

        let (wiring, tools, _) = catalogs(&fixture.builder);
        assert_eq!(wiring.skill_section, "");
        assert!(!tools.contains(&"skill".to_string()), "{tools:?}");
        let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, true));
        assert!(!names.contains(&"skill".to_string()), "{names:?}");
    }

    #[test]
    fn an_invalid_skill_warns_and_is_skipped_while_the_valid_one_is_listed() {
        let fixture = fixture();
        let root = fixture.home.join(".otto/skills");
        let pdf = write_definition(&root, "SKILL.md", "pdf", "pdf", "Extract pdfs.", "body\n");
        write_definition(
            &root,
            "SKILL.md",
            "bad",
            "other",
            "Mismatched name.",
            "body\n",
        );

        let (wiring, _, warnings) = catalogs(&fixture.builder);
        assert!(warnings.starts_with("warning: skill "), "{warnings:?}");
        assert!(
            warnings.contains("does not match directory \"bad\""),
            "{warnings:?}"
        );
        assert!(
            wiring.skill_section.contains(&format!(
                "<skill name=\"pdf\" location=\"{}\"",
                pdf.display()
            )),
            "{:?}",
            wiring.skill_section
        );
    }

    #[test]
    fn a_workspace_agent_is_listed_in_the_agents_section() {
        let fixture = fixture();
        write_definition(
            &fixture.workspace.join(".otto/agents"),
            "AGENT.md",
            "reviewer",
            "reviewer",
            "Reviews code for style and correctness.",
            "# Reviewer\nFocus on style.\n",
        );

        let (wiring, _, warnings) = catalogs(&fixture.builder);
        assert!(wiring.agents.enabled);
        assert!(
            wiring.agent_section.contains("\n<agents>\n"),
            "{:?}",
            wiring.agent_section
        );
        assert!(
            wiring.agent_section.contains("<agent name=\"reviewer\">"),
            "{:?}",
            wiring.agent_section
        );
        assert_eq!(warnings, "");
        let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, true));
        for want in [
            "agent",
            "agent_status",
            "agent_send",
            "agent_cancel",
            "remind",
            "remind_status",
            "remind_cancel",
        ] {
            assert!(
                names.contains(&want.to_string()),
                "{names:?} is missing {want}"
            );
        }
    }

    #[test]
    fn disabled_agents_omit_the_section_and_the_boundary_definitions() {
        let mut fixture = fixture();
        write_definition(
            &fixture.workspace.join(".otto/agents"),
            "AGENT.md",
            "reviewer",
            "reviewer",
            "Reviews code for style and correctness.",
            "body\n",
        );
        fixture.builder.shared_mut().config.agents.enabled = Some(false);

        let (wiring, _, _) = catalogs(&fixture.builder);
        assert!(!wiring.agents.enabled);
        assert_eq!(wiring.agent_section, "");
        let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, true));
        for unwanted in [
            "agent",
            "agent_status",
            "agent_send",
            "agent_cancel",
            "remind",
            "remind_status",
            "remind_cancel",
        ] {
            assert!(
                !names.contains(&unwanted.to_string()),
                "{names:?} still lists {unwanted}"
            );
        }
    }

    /// The caller passes the dynamic-content decision directly rather than
    /// reaching it through a provider secret that exhausts the redaction
    /// marker.
    #[test]
    fn a_closed_redaction_boundary_drops_the_agent_definitions() {
        let fixture = fixture();
        let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, false));
        assert_eq!(names, vec!["skill".to_string()]);
    }

    #[test]
    fn an_out_of_range_max_parallel_fails_the_build() {
        let mut fixture = fixture();
        fixture.builder.shared_mut().config.agents.max_parallel = Some(17);
        let mut tools: Vec<Box<dyn Tool + Send + Sync>> = Vec::new();
        let error = match fixture
            .builder
            .build_catalogs(&mut tools, 65536, &mut Vec::new())
        {
            Err(error) => error,
            Ok(_) => panic!("max_parallel = 17 must fail the build"),
        };
        assert_eq!(
            error,
            "[agents].max_parallel must be between 1 and 16, got 17"
        );
    }

    /// Skill and agent sandbox read paths, folded into one table because both
    /// roots travel the same code path.
    #[test]
    fn sandbox_read_paths_pick_up_only_existing_enabled_roots() {
        for (name, make_directories, disabled, want) in [
            ("both roots exist", true, false, true),
            ("roots absent", false, false, false),
            ("disabled with roots present", true, true, false),
        ] {
            let fixture = fixture();
            let skills = fixture.workspace.join(".otto/skills");
            let agents = fixture.workspace.join(".otto/agents");
            if make_directories {
                std::fs::create_dir_all(&skills).expect("skills root");
                std::fs::create_dir_all(&agents).expect("agents root");
            }
            let mut config = fixture.builder.config.clone();
            if disabled {
                config.skills.enabled = Some(false);
                config.agents.enabled = Some(false);
            }
            let settings = crate::runtime::startup::resolve_sandbox_settings(
                &config,
                &fixture.builder.environment,
                &fixture.workspace.to_string_lossy(),
                None,
            )
            .expect("sandbox settings");
            for root in [&skills, &agents] {
                let root = root.to_string_lossy().into_owned();
                assert_eq!(
                    settings.read_paths.contains(&root),
                    want,
                    "{name}: {:?} against {root}",
                    settings.read_paths
                );
            }
        }
    }
}

#[cfg(test)]
mod workspace_identity_tests {
    use super::*;
    use crate::memory::files::testsupport::{open_temp, sample_record};
    use crate::memory::{RecordKey, UpsertRequest};

    #[test]
    fn legacy_memory_remains_reachable_after_a_move_and_is_separate_after_fork() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("workspace");
        std::fs::create_dir(&original).unwrap();
        let original = std::fs::canonicalize(original).unwrap();
        let config = MemoryRuntime::default();
        let scope = workspace_memory_scope(&config, original.to_str().unwrap()).unwrap();
        let (_store_dir, store) = open_temp();
        let record = store
            .upsert(&UpsertRequest {
                record: sample_record("legacy-memory", &scope, "style", "concise"),
                expected_revision: None,
            })
            .unwrap();
        crate::workspace::initialize(&original, "").unwrap();
        let moved = dir.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();
        crate::workspace::initialize(&moved, "").unwrap();
        let moved_scope = workspace_memory_scope(&config, moved.to_str().unwrap()).unwrap();
        assert_eq!(moved_scope, scope);
        assert_eq!(
            store
                .get_by_key(&RecordKey {
                    scope: moved_scope,
                    kind: "preference".into(),
                    key: "style".into()
                })
                .unwrap(),
            record
        );
        crate::runtime::builder::validate_session_workspace(
            original.to_str().unwrap(),
            moved.to_str().unwrap(),
        )
        .unwrap();
        crate::workspace::fork(&moved).unwrap();
        let fork_scope = workspace_memory_scope(&config, moved.to_str().unwrap()).unwrap();
        assert_ne!(fork_scope, scope);
        assert!(
            store
                .get_by_key(&RecordKey {
                    scope: fork_scope,
                    kind: "preference".into(),
                    key: "style".into()
                })
                .is_err()
        );
        assert!(
            crate::runtime::builder::validate_session_workspace(
                original.to_str().unwrap(),
                moved.to_str().unwrap()
            )
            .is_err()
        );
    }
}
