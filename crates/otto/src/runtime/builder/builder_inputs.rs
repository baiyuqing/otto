//! Builder usage queries, redaction boundary inputs and sandbox information.

use super::*;

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
                    provider: runtime.provider_name.clone(),
                    profile: runtime.model_selector.to_string(),
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
    pub(super) fn boundary_tool_definitions(
        &self,
        runtime: Option<&Runtime>,
    ) -> Vec<ToolDefinition> {
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
    pub(super) fn boundary_redactor(&self, runtime: Option<&Runtime>) -> Redactor {
        let definitions = self.boundary_tool_definitions(runtime);
        let (provider, model) = match runtime {
            Some(runtime) => (runtime.provider_type.as_str(), runtime.model.as_str()),
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

    pub(super) fn review_instruction_snapshot(
        &self,
        workspace_instructions: &str,
        redactor: &Redactor,
    ) -> Vec<String> {
        let mut instructions = vec![redactor.redact_string(workspace_instructions)];
        if !self.memory.external() && self.config.memory.enabled != Some(false) {
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
                ..Default::default()
            };
        }
        self.sandbox_info.clone()
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
                ..Default::default()
            };
        }
        self.planned_sandbox_info()
    }

    /// A closed boundary reports no runtime identity, because provider,
    /// selector, and model are all model-visible text.
    pub fn runtime_info(&self, runtime: &Runtime) -> RuntimeInfo {
        let mut info = RuntimeInfo {
            provider: runtime.provider_name.clone(),
            provider_type: runtime.provider_type.clone(),
            model_selector: runtime.model_selector.to_string(),
            model: runtime.model.clone(),
            thinking: runtime.thinking.clone(),
            context_window: runtime.compaction.context_window,
            sandbox: self.effective_sandbox_info(),
        };
        if !self.boundary_allows_dynamic(Some(runtime)) {
            info.provider = String::new();
            info.provider_type = String::new();
            info.model_selector = String::new();
            info.model = String::new();
            info.thinking = String::new();
            info.context_window = 0;
        }
        info
    }
}
