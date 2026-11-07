//! Builder composition of one runner from resolved configuration.

use super::*;

impl Builder {
    pub(super) async fn build_runner_inner(
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
        let session_id = session.header().id;
        let client = self.provider_client(runtime)?;
        // Disabling conversational delegation must not disable named workflow
        // execution: the internal workflow runner always has sub-agents.
        let agents_enabled = !include_parent_tools
            || otto_core::config::resolve_agents(
                &self.config,
                &self.environment,
                &self.workspace_path,
            )
            .map_err(|error| error.to_string())?
            .enabled;
        // Resource creation depends on configuration only, not on whether this
        // runner's provider is available: the first runner of a session may
        // have a closed redaction boundary while a later model does not.
        let resources = self.session_resources(
            session,
            agents_enabled,
            runtime.max_output_bytes.max(0) as usize,
            warnings,
        );
        let output_logs = Arc::clone(resources.output_logs());
        let schedule_origin = Arc::new(crate::schedule::tools::TurnOrigin::default());
        let presentation_origin = Arc::new(crate::schedule::tools::TurnOrigin::default());
        let mut tools = self.builtin_file_tools(max_output);
        tools.push(Box::new(crate::tool::session_info::SessionInfo {
            session_id: session.header().id,
        }));
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
                &self.workspace,
                executor,
                &self.shell,
                self.sandbox_environment.clone().unwrap_or_default(),
                shell_timeout(runtime.shell_timeout),
                runtime.resilience.deadlines.cancellation_grace,
                max_output,
                &redaction_values,
                Arc::clone(&output_logs),
            )
            .map_err(|error| format!("create bash tool: {error}"))?;
            if let Some(approvals) = &self.bash_approvals {
                tool = tool.with_approvals(session_id.clone(), Arc::clone(approvals));
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
        let plugins =
            crate::plugin::Snapshot::load(Path::new(&self.home), &self.config, &self.environment);
        let extensions = self.plugin_runtime(&plugins, &redaction_values)?;
        let memory_client = self
            .runner_memory_client(&plugins, &extensions, runtime, warnings)
            .await?;
        let mut memory_proposals = None;
        if self.memory_usable()
            && !self.memory.external()
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
            let new_advice_agent = || -> Result<_, String> {
                Ok(Agent::with_redactor(
                    client.clone(),
                    Registry::new(Vec::new())
                        .map_err(|error| format!("advice registry: {error}"))?,
                    SharedSession::memory(session.header()),
                    Options {
                        diagnostics: self.logger.diagnostic_scope(
                            &session.header().id,
                            "",
                            self.boundary_redactor(Some(runtime)),
                        ),
                        model: runtime.model.clone(),
                        provider_name: runtime.provider_type.clone(),
                        thinking: runtime.thinking.clone(),
                        now: Box::new(Utc::now),
                        new_operation_id: Box::new(new_operation_id),
                        ..Options::default()
                    },
                    self.boundary_redactor(Some(runtime)),
                ))
            };
            let advisor = self.advisor(
                new_advice_agent()?,
                &runtime.model,
                self.usage_collector(session, runtime),
            );
            memory_proposals = self.proposal_generator(
                new_advice_agent()?,
                &runtime.model,
                self.usage_collector(session, runtime),
            );
            tools.extend(self.memory_tools(
                max_output,
                advisor,
                Some((
                    session.header().id.as_str(),
                    Arc::clone(&presentation_origin),
                )),
            ));
        } else {
            if self.memory.external()
                && self.memory_usable()
                && self.boundary_allows_dynamic(Some(runtime))
                && let Some(client) = &memory_client
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
        }
        mark_build_trace(&mut trace, "runner/setup");
        let hook_guard = crate::plugin::hooks::session_guard(
            {
                let session = session.clone();
                Arc::new(move || session.lease())
            },
            PathBuf::from(&self.workspace_path),
            session.hooks().closed.clone(),
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
        let (mcp_connected, connecting_mcp): (&[crate::runtime::wiring::ConnectedServer], _) =
            if connect_mcp {
                let session_mcp = self
                    .session_mcp(
                        &resources,
                        max_output,
                        runtime.resilience.deadlines.cancellation_grace,
                        warnings,
                        &plugins,
                    )
                    .await;
                (session_mcp.connected.as_slice(), None)
            } else {
                // A build that does not connect shows the configured servers as
                // connecting until a later build connects them.
                let servers = self.connecting_mcp_servers();
                if let Some(guard) = crate::tool::project_guard::Guard::for_config(
                    &self.config,
                    &self.home,
                    &self.workspace_path,
                ) && guard.check_trust
                {
                    guard.watch_mcp(&servers);
                }
                (&[][..], Some(servers))
            };
        tools.extend(super::super::wiring::mcp_adapter_tools(
            mcp_connected,
            max_output,
        ));
        mark_build_trace(&mut trace, "runner/mcp");

        match &client {
            ProviderClient::Compat { client, .. } => {
                tools.push(Box::new(models::ListModelsTool::new(
                    Arc::clone(client),
                    max_output,
                )));
            }
            ProviderClient::Claude { client, .. } => {
                tools.push(Box::new(models::ListModelsTool::claude(
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
            &self.workspace,
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
            &session_id,
            Arc::clone(&output_logs),
        )?;
        child_tools.extend(super::super::wiring::mcp_adapter_tools(
            mcp_connected,
            max_output,
        ));
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
            Some(extensions.clone()),
            Some(hook_guard.clone()),
            warnings,
        )?;
        if let Some(runner) = &subagents {
            runner.bind_output_logs(Arc::clone(&output_logs));
            runner.bind_scratch_root(
                self.workspace
                    .root_fs()
                    .try_clone()
                    .map_err(|error| format!("pin child research workspace: {error}"))?,
                &session_id,
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
            if include_parent_tools {
                let controller = Box::pin(crate::runtime::workflow::shared_controller(
                    self, runtime, warnings,
                ))
                .await;
                if !self.no_session
                    && let Ok(controller) = &controller
                {
                    crate::runtime::workflow_notifications::bind(
                        controller,
                        session.clone(),
                        &(Arc::clone(resources.notifications())
                            as Arc<dyn otto_core::mailbox::Mailbox>),
                    );
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
            &runtime.provider_type,
            &endpoint_host,
            &runtime.model,
        );
        let mut model_guidance = if subagents.is_some() {
            redactor.redact_string(&subagent_model_guidance(&runtime.subagent_models))
        } else {
            String::new()
        };
        if subagents.is_some()
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
        let mut review_provider: Option<Arc<dyn crate::approval::reviewer::ReviewProvider>> = None;
        if !session.header().id.is_empty() {
            let review_runtime = match self.config.approval.reviewer_model.as_deref() {
                Some(selector) => self
                    .resolve_model(selector)
                    .map_err(|_| "approval reviewer model does not exist".to_string())?,
                None => runtime.clone(),
            };
            let review_agent = Agent::with_redactor(
                match self.config.approval.reviewer_model {
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
                    provider_name: review_runtime.provider_type.clone(),
                    thinking: review_runtime.thinking.clone(),
                    now: Box::new(Utc::now),
                    new_operation_id: Box::new(new_operation_id),
                    ..Options::default()
                },
                self.boundary_redactor(Some(&review_runtime)),
            );
            let provider: Arc<dyn crate::approval::reviewer::ReviewProvider> = Arc::new(
                crate::approval::reviewer::SessionReviewer::new(review_agent)
                    .with_usage(self.usage_collector(session, &review_runtime)),
            );
            self.approvals
                .register_reviewer(&self.workspace_path, &session.header().id, provider.clone())
                .map_err(|error| error.to_string())?;
            review_provider = Some(provider);
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
            ProviderClient::Claude { client, .. } => {
                Some(client.clone() as Arc<dyn RequestSizer + Send + Sync>)
            }
            ProviderClient::ChatGpt { client, .. } => {
                Some(client.clone() as Arc<dyn RequestSizer + Send + Sync>)
            }
            ProviderClient::Unavailable => None,
            #[cfg(any(test, feature = "test-support"))]
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
                let history_handle = session.handle().clone();
                extensions.bind(
                    crate::plugin::hooks::SessionBinding {
                        session_id: session.header().id,
                        parent_session_id: None,
                        task_id: String::new(),
                        mode: if session.path().is_empty() {
                            "new"
                        } else {
                            "resume"
                        }
                        .into(),
                        headless: self.headless,
                        history_path: Arc::new(move || history_handle.path()),
                    },
                    Some(hook_guard),
                )
            }),
            hook_state: session.hooks().clone(),
            model: runtime.model.clone(),
            provider_name: runtime.provider_type.clone(),
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
                true => self.bind_selected_memory(session.header().id, memory_client.clone())?,
                false => None,
            },
            memory_recall_limit: self.memory.recall_limit,
            memory_recall_token_budget: self.memory.recall_token_budget,
            mailbox: Arc::clone(resources.notifications()) as Arc<dyn otto_core::mailbox::Mailbox>,
            steering: task_control.steering(),
            version: env!("CARGO_PKG_VERSION").into(),
            ..Options::default()
        };
        let hook_port = options.hooks.clone();
        let hook_state = options.hook_state.clone();
        mark_build_trace(&mut trace, "runner/registry-and-options");
        if !plugins.packages.is_empty() {
            session
                .append_custom_when_created("otto.plugins", &plugins.provenance())
                .await
                .map_err(|e| e.to_string())?;
        }
        let trace_entries = trace.map(|trace| trace.entries).unwrap_or_default();
        let runner = Runner {
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
            review_provider,
            agent: Agent::with_redactor(client, registry, session.clone(), options, redactor),
            turn_timeout: runtime.resilience.deadlines.turn_timeout,
            unattended_turn_timeout: runtime.resilience.deadlines.unattended_turn_timeout,
            headless: self.headless,
            system_prompt,
            definitions,
            usage: self.usage_collector(session, runtime),
            logger: self.logger.clone(),
            subagents,
            skills: catalogs.skills.clone(),
            discovered_skills: catalogs.discovered_skills.clone(),
            agents: catalogs.agent_catalog.clone(),
            connecting_mcp,
            task_control,
            memory_client,
            memory_proposals,
            schedule_origin,
            presentation_origin,
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
                    session.close_resources().await;
                    return Err(message);
                }
            };
            if response.decision == Some(otto_core::agent::hooks::Decision::Deny) {
                runner.agent.end_hooks().await;
                runner.close();
                session.close_resources().await;
                return Err(response
                    .reason
                    .unwrap_or_else(|| "Hook denied session".into()));
            }
        }
        Ok((runner, trace_entries))
    }
}
