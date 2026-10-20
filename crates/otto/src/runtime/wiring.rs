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
mod selected_memory_tests;

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
    pub discovered_skills: skill::Catalog,
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

mod catalogs_and_subagents;
mod mcp_connect;
mod memory_binding;

/// Only the outcomes reachable without a real MCP server are covered: a
/// disabled server is never dialed, and an unspawnable command reports `Failed`
/// with one warning. A working stdio/HTTP connection would need a real server
/// process or socket, which the offline test rule forbids;
/// `crates/otto/tests/mcp_stdio.rs` covers the transport itself with a fake
/// server subprocess.
#[cfg(test)]
mod mcp_tests;

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
mod tests;

/// Each case is folded onto the composition seam it exercises:
/// [`Builder::build_catalogs`] for the prompt sections, the tool list and the
/// warnings, [`Builder::boundary_catalog_definitions`] for the redaction
/// boundary, and `resolve_sandbox_settings` for the Seatbelt read paths. A full
/// `skill` tool call/result round trip belongs to `skill::tool`'s own tests and
/// the section's XML position after `<environment>` to `system_prompt_for`, so
/// neither is repeated here.
#[cfg(test)]
mod catalog_tests;

#[cfg(test)]
mod workspace_identity_tests;
