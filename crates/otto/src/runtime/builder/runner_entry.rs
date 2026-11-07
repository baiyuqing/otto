//! Builder tool sets and the `build_runner*` entry points.

use super::*;

impl Builder {
    pub(crate) fn plugin_runtime(
        &self,
        snapshot: &crate::plugin::Snapshot,
        secrets: &[String],
    ) -> Result<Arc<crate::plugin::hooks::Runtime>, String> {
        let mut package_environment = std::collections::BTreeMap::new();
        for installed in snapshot.packages.iter().filter(|p| p.trusted) {
            let bound = crate::plugin::runtime::package_environment(
                &self.config,
                &installed.name,
                &self.environment,
            )?;
            if !bound.is_empty() {
                package_environment.insert(installed.name.clone(), bound);
            }
        }
        crate::plugin::hooks::Runtime::new(
            snapshot,
            Path::new(&self.home),
            &self.config.hooks,
            crate::plugin::hooks::Execution {
                executor: self.command_executor.clone(),
                environment: self.sandbox_environment.clone().unwrap_or_default(),
                package_environment,
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
            Box::new(read::ReadTool::new(Arc::clone(&self.workspace), max_output)),
            Box::new(grep::GrepTool::new(Arc::clone(&self.workspace), max_output)),
            Box::new(find::FindTool::new(Arc::clone(&self.workspace), max_output)),
            Box::new(ls::LsTool::new(Arc::clone(&self.workspace), max_output)),
            Box::new(write::WriteTool::new(Arc::clone(&self.workspace))),
            Box::new(edit::EditTool::new(Arc::clone(&self.workspace))),
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
        runtime.is_some_and(|runtime| runtime.provider_type == otto_core::config::PROVIDER_CHATGPT)
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
    pub(super) fn provider_client(&self, runtime: &Runtime) -> Result<ProviderClient, BuildError> {
        #[cfg(any(test, feature = "test-support"))]
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
        } else if runtime.provider_type == otto_core::config::PROVIDER_CLAUDE {
            ProviderClient::Claude {
                client: {
                    let client = crate::provider::claude::Client::new_trusted(
                        runtime
                            .endpoint
                            .as_ref()
                            .ok_or_else(|| "missing trusted provider endpoint".to_string())?,
                        &runtime.api_key,
                        &runtime.api_version,
                        &runtime.workspace_id,
                    );
                    Arc::new(match missing_credential_message(runtime) {
                        Some(message) => client.with_missing_credential(&message),
                        None => client,
                    })
                },
                timeout: runtime.resilience.deadlines.provider_timeout,
                cancellation_grace: runtime.resilience.deadlines.cancellation_grace,
            }
        } else if runtime.provider_type == otto_core::config::PROVIDER_CHATGPT {
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
                client: {
                    let client = Client::new_trusted(
                        runtime
                            .endpoint
                            .as_ref()
                            .ok_or_else(|| "missing trusted provider endpoint".to_string())?,
                        &runtime.api_key,
                    );
                    Arc::new(match missing_credential_message(runtime) {
                        Some(message) => client.with_missing_credential(&message),
                        None => client,
                    })
                },
                timeout: runtime.resilience.deadlines.provider_timeout,
                cancellation_grace: runtime.resilience.deadlines.cancellation_grace,
            }
        })
    }
}

/// The request-time error for a provider with no API key bound, naming only the
/// variable (never a value) and both recovery paths.
pub(crate) fn missing_credential_message(runtime: &Runtime) -> Option<String> {
    if !runtime.api_key.is_empty() {
        return None;
    }
    let variable = if runtime.api_key_env.is_empty() {
        otto_core::config::providers::FALLBACK_API_KEY_ENV
    } else {
        runtime.api_key_env.as_str()
    };
    Some(format!(
        "missing api key for provider \"{}\": set {variable} and restart Otto, or switch to a model whose provider has credentials (for example with /model)",
        runtime.provider_name
    ))
}
