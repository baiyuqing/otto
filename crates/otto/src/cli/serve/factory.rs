//! The per-session `Factory` over the workspace registry.

use super::*;

// ---- the session factory ----

/// Builds one [`Controller`] per server-side session on top of the same
/// replacement plumbing the CLI's `/new` and `/resume` use.
pub(super) struct ServeFactory {
    pub(super) workspaces: Arc<Workspaces>,
    /// Optional read-only delivery status owner; it does not grant any delivery
    /// effect. Test/frontends without delivery return no delivery status.
    pub(super) delivery: Option<Arc<DeliveryManager>>,
    pub(super) runtime: Runtime,
    /// Cancels a workspace load in progress when the server shuts down;
    /// otherwise never fired during normal operation.
    pub(super) cancel: CancellationToken,
}

/// The sessions for one workspace's builder, with a missing session root
/// reported as no sessions.
pub(super) fn listed_for(builder: &Builder) -> Result<ListResult, String> {
    if !builder.session_root.exists() {
        return Ok(ListResult::default());
    }
    sessionfs::list(
        &builder.session_root,
        &builder.workspace_path,
        "",
        MAX_LIST_SESSIONS,
    )
    .map_err(|error| builder.redact_error(&error.to_string(), None))
}

impl ServeFactory {
    pub(super) fn builder(&self) -> &Arc<Builder> {
        &self.workspaces.startup_host().builder
    }

    fn wire(&self, controller: Controller, host: &WorkspaceHost) -> Controller {
        match &host.reloader {
            Some(control) => {
                controller.with_sandbox_control(Arc::clone(control) as Arc<dyn SandboxControl>)
            }
            None => controller,
        }
    }

    /// The hosts `open`/`list` search: just `workspace`'s host when named,
    /// every loaded host otherwise. `None` when a named workspace is not
    /// loaded.
    async fn hosts_for(&self, workspace: Option<&str>) -> Option<Vec<Arc<WorkspaceHost>>> {
        match workspace {
            Some(path) => Some(vec![self.workspaces.host(path).await?]),
            None => Some(
                self.workspaces
                    .list()
                    .await
                    .into_iter()
                    .map(|(_, host)| host)
                    .collect(),
            ),
        }
    }
}

#[async_trait::async_trait]
impl Factory for ServeFactory {
    fn delivery_status(
        &self,
        source_session: &str,
        approval_id: &str,
    ) -> Result<Option<crate::server::delivery::Status>, String> {
        let Some(delivery) = &self.delivery else {
            return Ok(None);
        };
        delivery.status(source_session, approval_id).map(|status| {
            status.map(|status| crate::server::delivery::Status {
                approval_id: status.approval_id,
                state: match status.state {
                    crate::delivery::TaskState::Running => "running",
                    crate::delivery::TaskState::Succeeded => "succeeded",
                    crate::delivery::TaskState::Failed => "failed",
                    crate::delivery::TaskState::Canceled => "canceled",
                    crate::delivery::TaskState::Unknown => "unknown",
                }
                .into(),
                result: status.result,
                error: status.error,
            })
        })
    }

    fn approval_service(&self) -> Option<Arc<crate::approval::Service>> {
        Some(self.workspaces.startup_host().builder.approvals.clone())
    }
    async fn create(&self, workspace: Option<&str>) -> Result<Controller, String> {
        let host = match workspace {
            Some(path) => self
                .workspaces
                .host(path)
                .await
                .ok_or_else(|| format!("workspace not loaded: {path}"))?,
            None => Arc::clone(self.workspaces.startup_host()),
        };
        let runtime = crate::runtime::builder::resolve_initial_runtime(
            &host.builder.config,
            &host.builder.environment,
            None,
            &host.builder.overrides,
        )
        .map_err(|error| error.to_string())?;
        let controller = Controller::create(Arc::clone(&host.builder), &runtime).await?;
        Ok(self.wire(controller, &host))
    }

    async fn create_with_receipt(
        &self,
        workspace: Option<&str>,
        id: String,
        request_id: &str,
    ) -> Result<Controller, String> {
        let host = match workspace {
            Some(path) => self
                .workspaces
                .host(path)
                .await
                .ok_or_else(|| format!("workspace not loaded: {path}"))?,
            None => Arc::clone(self.workspaces.startup_host()),
        };
        let runtime = crate::runtime::builder::resolve_initial_runtime(
            &host.builder.config,
            &host.builder.environment,
            None,
            &host.builder.overrides,
        )
        .map_err(|error| error.to_string())?;
        let controller =
            Controller::create_with_receipt(Arc::clone(&host.builder), &runtime, id, request_id)
                .await?;
        Ok(self.wire(controller, &host))
    }

    async fn open(&self, id: &str, workspace: Option<&str>) -> Result<Controller, String> {
        let hosts = self
            .hosts_for(workspace)
            .await
            .ok_or_else(|| SESSION_NOT_FOUND.to_string())?;
        if !sessionfs::is_session_id(id) {
            return Err(SESSION_NOT_FOUND.to_string());
        }
        for host in &hosts {
            let path = sessionfs::session_directory(
                &host.builder.session_root,
                &host.builder.workspace_path,
            )
            .map_err(|error| host.builder.redact_error(&error.to_string(), None))?
            .join(format!("{id}.jsonl"));
            if path.is_file() {
                // The repair warnings the CLI prints have no channel here;
                // the web UI reads the repaired history like any other.
                let (controller, _warnings) =
                    Controller::open(Arc::clone(&host.builder), &path).await?;
                return Ok(self.wire(controller, host));
            }
        }
        Err(SESSION_NOT_FOUND.to_string())
    }

    async fn list(&self, workspace: Option<&str>) -> Option<Result<ListResult, String>> {
        let hosts = self.hosts_for(workspace).await?;
        let mut sessions = Vec::new();
        let mut skipped = 0i64;
        for host in &hosts {
            match listed_for(&host.builder) {
                Ok(result) => {
                    sessions.extend(result.sessions);
                    skipped += result.skipped;
                }
                Err(error) => return Some(Err(error)),
            }
        }
        if workspace.is_none() {
            // Each host's own list is already newest-first and capped; the
            // merge needs the same cap re-applied across all of them.
            sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
            sessions.truncate(MAX_LIST_SESSIONS);
        }
        Some(Ok(ListResult { sessions, skipped }))
    }

    fn sandbox_reload_available(&self) -> bool {
        self.workspaces.startup_host().reloader.is_some()
    }

    async fn reload_sandbox(&self) -> Option<Result<SandboxInfo, String>> {
        let host = self.workspaces.startup_host();
        let control = host.reloader.as_ref()?;
        Some(SandboxControl::reload(control.as_ref()).await)
    }

    async fn reload_other_sandboxes(&self) -> Vec<(String, Result<SandboxInfo, String>)> {
        let mut results = Vec::new();
        for (path, host) in self.workspaces.list().await {
            if path == self.workspaces.startup {
                continue;
            }
            if let Some(control) = &host.reloader {
                let result = SandboxControl::reload(control.as_ref())
                    .await
                    .map_err(|error| self.builder().redact_error(&error, Some(&self.runtime)));
                results.push((path, result));
            }
        }
        results
    }

    async fn diff_runner(
        &self,
        workspace: &str,
    ) -> Option<(Arc<dyn crate::sandbox::CommandExecutor>, Vec<String>)> {
        let host = self.workspaces.host(workspace).await?;
        let executor = host.builder.command_executor.clone()?;
        let environment = host.builder.sandbox_environment.clone()?;
        Some((executor, environment))
    }

    fn usage_summary(&self, session_id: Option<&str>) -> Result<crate::usage::Summary, String> {
        self.builder().usage_summary(session_id)
    }

    fn usage_analysis(
        &self,
        days: u16,
        session_id: Option<&str>,
    ) -> Result<crate::usage::Analysis, String> {
        self.builder().usage_analysis(days, session_id)
    }

    fn tasks_list(
        &self,
        query: &crate::subagent::record::ListQuery,
    ) -> Result<crate::subagent::record::ListResult, String> {
        self.builder().tasks_list(query)
    }

    fn tasks_get(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Option<crate::subagent::record::TaskRow>, String> {
        self.builder().tasks_get(parent_session, task_id)
    }

    async fn workspaces(&self) -> server::WorkspaceList {
        let loaded = self.workspaces.list().await;
        server::WorkspaceList {
            startup: self.workspaces.startup.clone(),
            roots: self
                .workspaces
                .roots
                .iter()
                .map(|root| root.to_string_lossy().into_owned())
                .collect(),
            loaded: loaded
                .into_iter()
                .map(|(path, host)| server::WorkspaceInfo {
                    path,
                    workflows: host.workflows.is_some(),
                })
                .collect(),
        }
    }

    async fn trust_workspace(&self, path: &str) -> Result<(), server::WorkspaceLoadError> {
        // Relative paths are refused for the same reason `admit_workspace`
        // refuses them: they would resolve against serve's working directory.
        if !Path::new(path).is_absolute() || canonical_directory(Path::new(path)).is_err() {
            return Err(server::WorkspaceLoadError::Invalid(format!(
                "{path}: not an existing directory"
            )));
        }
        crate::cli::trust::trust_directory(&self.workspaces.config_path, Path::new(path))
            .map(|_| ())
            .map_err(server::WorkspaceLoadError::Failed)
    }

    fn browse_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = canonical_directory(Path::new(&self.builder().shared.home))
            .into_iter()
            .collect();
        for root in &self.workspaces.roots {
            if !roots.contains(root) {
                roots.push(root.clone());
            }
        }
        roots
    }

    async fn load_workspace(
        &self,
        path: &str,
    ) -> Result<(server::WorkspaceInfo, bool), server::WorkspaceLoadError> {
        let canonical = self
            .workspaces
            .admit(path)
            .map_err(|admission| match admission {
                Admission::Invalid => server::WorkspaceLoadError::Invalid(format!(
                    "{path}: not an existing directory"
                )),
                Admission::NotAdmitted => server::WorkspaceLoadError::NotAdmitted(format!(
                    "{path}: outside the startup workspace and configured roots"
                )),
            })?;
        let shared = Arc::clone(&self.builder().shared);
        // ponytail: no writer is threaded through `Factory::load_workspace`,
        // so a workflow-lock warning goes straight to the process stderr,
        // matching `runtime/builder.rs`'s own `build_runner` warnings.
        let mut stderr = std::io::stderr();
        let (host, newly_loaded) = self
            .workspaces
            .load(
                &canonical,
                &shared,
                &self.runtime,
                &self.cancel,
                &mut stderr,
                true,
            )
            .await
            .map_err(|error| {
                server::WorkspaceLoadError::Failed(
                    self.builder().redact_error(&error, Some(&self.runtime)),
                )
            })?;
        Ok((
            server::WorkspaceInfo {
                path: canonical.to_string_lossy().into_owned(),
                workflows: host.workflows.is_some(),
            },
            newly_loaded,
        ))
    }

    async fn remove_workspace(&self, path: &str) -> Result<(), server::WorkspaceRemoveError> {
        let canonical = canonical_or_literal(path);
        let home = self.builder().shared.home.clone();
        // ponytail: no writer is threaded through `Factory::remove_workspace`,
        // matching `load_workspace`'s own stderr fallback.
        let mut stderr = std::io::stderr();
        self.workspaces.remove(&canonical, &home, &mut stderr).await
    }

    async fn workflow_controller(
        &self,
        workspace: Option<&str>,
    ) -> Option<Arc<crate::workflow::Controller>> {
        let host = match workspace {
            Some(path) => self.workspaces.host(path).await?,
            None => Arc::clone(self.workspaces.startup_host()),
        };
        host.workflows.clone()
    }

    async fn workflow_controllers(&self) -> Vec<Arc<crate::workflow::Controller>> {
        self.workspaces
            .list()
            .await
            .into_iter()
            .filter_map(|(_, host)| host.workflows.clone())
            .collect()
    }
}
