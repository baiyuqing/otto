//! Process info, the composition-root factory trait and workspace registration types.

use super::*;

// ---- process-level info ----

/// Process-level static info, unrelated to any session.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Info {
    pub workspace: String,
    pub provider: String,
    pub provider_type: String,
    pub model: String,
    pub thinking: String,
    pub sandbox: String,
    pub models: Vec<String>,
}

/// What a [`Server`] needs from its composition root.
///
/// `list` and `reload_sandbox` return `None` when the composition root wires no
/// such capability.
#[async_trait::async_trait]
pub trait Factory: Send + Sync {
    fn approval_service(&self) -> Option<Arc<crate::approval::Service>> {
        None
    }
    /// A brand new session. `workspace` is the canonical path of an already
    /// admitted-and-loaded workspace; `None` means the startup workspace.
    async fn create(&self, workspace: Option<&str>) -> Result<Controller, String>;
    /// Create a session with a durable command receipt before acknowledging it.
    async fn create_with_receipt(
        &self,
        _workspace: Option<&str>,
        _id: String,
        _request_id: &str,
    ) -> Result<Controller, String> {
        Err("idempotent session creation is unavailable".into())
    }
    /// An existing session by id. [`SESSION_NOT_FOUND`] means 404.
    /// `workspace` searches just that workspace; `None` searches every
    /// loaded workspace.
    async fn open(&self, id: &str, workspace: Option<&str>) -> Result<Controller, String>;
    /// The sessions on disk for `workspace` (`None`: every loaded workspace,
    /// merged newest-first and capped at `MAX_LIST_SESSIONS`). `None` at the
    /// outer `Option` disables the disk half of `GET /v1/sessions`.
    async fn list(&self, _workspace: Option<&str>) -> Option<Result<ListResult, String>> {
        None
    }
    /// Whether `POST /v1/sandbox/reload` is wired at all. Checked before the
    /// turn-active guard.
    fn sandbox_reload_available(&self) -> bool {
        false
    }
    /// Re-reads the sandbox configuration. `None` disables `POST
    /// /v1/sandbox/reload`, which then answers 501.
    async fn reload_sandbox(&self) -> Option<Result<SandboxInfo, String>> {
        None
    }
    /// Reloads every other loaded workspace's own sandbox (the startup
    /// workspace's reload is [`Factory::reload_sandbox`]), by path. A
    /// workspace with no reloader of its own is left out.
    async fn reload_other_sandboxes(&self) -> Vec<(String, Result<SandboxInfo, String>)> {
        Vec::new()
    }
    /// Persisted provider token usage, optionally scoped to one session.
    fn usage_summary(&self, _session_id: Option<&str>) -> Result<crate::usage::Summary, String> {
        Ok(crate::usage::Summary::default())
    }
    fn usage_analysis(
        &self,
        days: u16,
        _session_id: Option<&str>,
    ) -> Result<crate::usage::Analysis, String> {
        crate::usage::Analysis::empty(days).map_err(|error| error.to_string())
    }
    /// Sub-agent task rows from `tasks.db`, across every otto process on the
    /// machine. An empty result when no recorder is wired.
    fn tasks_list(
        &self,
        _query: &crate::subagent::record::ListQuery,
    ) -> Result<crate::subagent::record::ListResult, String> {
        Ok(crate::subagent::record::ListResult::default())
    }
    /// One task row from `tasks.db` by its primary key. `Ok(None)` when no
    /// recorder is wired.
    fn tasks_get(
        &self,
        _parent_session: &str,
        _task_id: &str,
    ) -> Result<Option<crate::subagent::record::TaskRow>, String> {
        Ok(None)
    }
    /// A delivery status scoped to its source session. The status is a narrow
    /// read-only projection and deliberately does not reuse ordinary tasks.
    fn delivery_status(
        &self,
        _source_session: &str,
        _approval_id: &str,
    ) -> Result<Option<delivery::Status>, String> {
        Ok(None)
    }
    /// Every loaded workspace, startup first then by path, for `GET
    /// /v1/workspaces`.
    async fn workspaces(&self) -> WorkspaceList;
    /// Admits and loads `path`, or returns the already-loaded workspace.
    /// `(_, true)` when this call built a new host, `(_, false)` when it was
    /// already loaded.
    async fn load_workspace(&self, path: &str)
    -> Result<(WorkspaceInfo, bool), WorkspaceLoadError>;
    /// Records `path` as trusted (the same config write as `otto trust`), so
    /// a following [`Factory::load_workspace`] admits it. Only `POST
    /// /v1/workspaces` with `trust: true` calls this, after a load answered
    /// [`WorkspaceLoadError::NotAdmitted`]. The default refuses, for a
    /// `Factory` with no config file to write.
    async fn trust_workspace(&self, path: &str) -> Result<(), WorkspaceLoadError> {
        Err(WorkspaceLoadError::Failed(format!(
            "{path}: trusting a directory is not available"
        )))
    }
    /// The canonical directories `GET /v1/fs/dirs` may list, with their
    /// descendants: the user's home directory, then the configured workspace
    /// roots. The first entry is where browsing starts. Empty (the default)
    /// makes the route answer 404.
    fn browse_roots(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
    /// Unloads `path` and removes it from the persisted workspace list.
    /// `path` is matched literally when it does not resolve (a deleted
    /// directory's persisted entry). The caller has already checked for open
    /// sessions, which `Factory` cannot see; this checks active workflow
    /// runs.
    async fn remove_workspace(&self, path: &str) -> Result<(), WorkspaceRemoveError>;
    /// The loaded workspace's command executor and sandbox environment, for
    /// `GET /v1/workspaces/diff`. `None` when the workspace has no usable
    /// sandbox (also the default for a `Factory` that never wires one, which
    /// answers `501 diff_unavailable`).
    async fn diff_runner(
        &self,
        _workspace: &str,
    ) -> Option<(Arc<dyn crate::sandbox::CommandExecutor>, Vec<String>)> {
        None
    }
    /// The workflow controller for `workspace` (`None`: the startup
    /// workspace). `None` when that workspace has no controller (workflows
    /// disabled there).
    async fn workflow_controller(
        &self,
        workspace: Option<&str>,
    ) -> Option<Arc<crate::workflow::Controller>>;
    /// Every loaded workspace's workflow controller, startup first then by
    /// path. A workspace with workflows disabled is left out.
    async fn workflow_controllers(&self) -> Vec<Arc<crate::workflow::Controller>>;
}

/// One workspace's registration state, independent of any open session.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceInfo {
    pub path: String,
    pub workflows: bool,
}

/// [`Factory::workspaces`]'s result.
pub struct WorkspaceList {
    pub startup: String,
    pub roots: Vec<String>,
    /// Startup first, then by path.
    pub loaded: Vec<WorkspaceInfo>,
}

/// Why [`Factory::load_workspace`] refused or failed a path.
#[derive(Debug)]
pub enum WorkspaceLoadError {
    /// Not an absolute, existing directory.
    Invalid(String),
    /// A real directory, but neither the startup workspace nor a descendant
    /// of a configured root.
    NotAdmitted(String),
    /// Admitted, but opening it failed (sandbox, MCP config, ...). Nothing
    /// was registered.
    Failed(String),
}

/// Why [`Factory::remove_workspace`] refused a path.
#[derive(Debug)]
pub enum WorkspaceRemoveError {
    /// Neither loaded nor in the persisted list.
    NotFound,
    /// The startup workspace, which is never removable.
    IsStartup,
    /// A session is open in that workspace or its workflow controller has an
    /// active run. The message names which.
    InUse(String),
}
