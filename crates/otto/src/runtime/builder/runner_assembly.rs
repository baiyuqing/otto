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
        let output_logs = Arc::new(crate::tool::bash_output::Logs::default());
        let schedule_origin = Arc::new(crate::schedule::tools::TurnOrigin::default());
        let presentation_origin = Arc::new(crate::schedule::tools::TurnOrigin::default());
        let skills = otto_core::config::resolve_skills(
            &self.config,
            &self.environment,
            &self.workspace_path,
        );
        if skills.enabled
            && let Some(roots) = crate::reflection::skill_roots(&self.home, &skills.roots)
        {
            self.reflector
                .attach_approvals(&self.approvals, &self.workspace_path, roots)?;
        }
        let mut tools = self.builtin_file_tools(max_output);
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
                self.workspace,
                executor,
                &self.shell,
                self.sandbox_environment.clone().unwrap_or_default(),
                shell_timeout(runtime.shell_timeout),
                runtime.resilience.deadlines.cancellation_grace,
                max_output,
                &redaction_values,
            )
            .map_err(|error| format!("create bash tool: {error}"))?;
            tool = tool.with_output_logs(Arc::clone(&output_logs));
            if self.sandbox_info.mode == SandboxMode::Seatbelt {
                tool = tool.with_private_home();
            }
            if let Some(approvals) = &self.bash_approvals {
                tool = tool.with_approvals(session.header().id, Arc::clone(approvals));
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
        let client = self.provider_client(runtime)?;
        let plugins =
            crate::plugin::Snapshot::load(Path::new(&self.home), &self.config, &self.environment);
        let extensions = self.plugin_runtime(&plugins, &redaction_values)?;
        let memory_client = self
            .runner_memory_client(&plugins, &extensions, runtime, warnings)
            .await?;
        let advisor = if self.memory_usable()
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
            let advice_agent = Agent::with_redactor(
                client.clone(),
                Registry::new(Vec::new()).map_err(|error| format!("advice registry: {error}"))?,
                SharedSession::memory(session.header()),
                Options {
                    diagnostics: self.logger.diagnostic_scope(
                        &session.header().id,
                        "",
                        self.boundary_redactor(Some(runtime)),
                    ),
                    model: runtime.model.clone(),
                    provider_name: runtime.provider.clone(),
                    thinking: runtime.thinking.clone(),
                    now: Box::new(Utc::now),
                    new_operation_id: Box::new(new_operation_id),
                    ..Options::default()
                },
                self.boundary_redactor(Some(runtime)),
            );
            let advisor = self.advisor(
                advice_agent,
                &runtime.model,
                self.usage_collector(session, runtime),
            );
            tools.extend(self.memory_tools(
                max_output,
                advisor.clone(),
                Some((
                    session.header().id.as_str(),
                    Arc::clone(&presentation_origin),
                )),
            ));
            advisor
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
            None
        };
        mark_build_trace(&mut trace, "runner/setup");
        let hook_guard = crate::plugin::hooks::session_guard(
            {
                let session = session.clone();
                Arc::new(move || session.lease())
            },
            PathBuf::from(&self.workspace_path),
            session.1.closed.clone(),
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
        let (mcp_tools, mcp_connected, mcp_servers) = if connect_mcp {
            self.connect_mcp_with_plugins(
                max_output,
                runtime.resilience.deadlines.cancellation_grace,
                warnings,
                &plugins,
            )
            .await
        } else {
            (Vec::new(), Vec::new(), self.connecting_mcp_servers())
        };
        tools.extend(mcp_tools);
        if let Some(guard) = crate::tool::project_guard::Guard::for_config(
            &self.config,
            &self.home,
            &self.workspace_path,
        ) && guard.check_trust
        {
            guard.watch_mcp(&mcp_servers);
        }
        mark_build_trace(&mut trace, "runner/mcp");

        match &client {
            ProviderClient::Compat { client, .. } => {
                tools.push(Box::new(models::ListModelsTool::new(
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
            self.workspace,
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
            Arc::clone(&output_logs),
        )?;
        child_tools.extend(super::super::wiring::mcp_child_tools(
            &mcp_connected,
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
        let mut workflow_binding = None;
        let mut subagents = self.build_subagents(
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
        if let Some(runner) = &subagents.runner {
            runner.bind_output_logs(Arc::clone(&output_logs));
            runner.bind_scratch_root(
                self.workspace
                    .root_fs()
                    .try_clone()
                    .map_err(|error| format!("pin child research workspace: {error}"))?,
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
        let reflection = Arc::new(crate::tool::reflect::Handle::default());
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
            // Parent-only, like the timer tools: a child has no session file
            // of its own to reflect on.
            if self.reflector.enabled() && !self.no_session {
                tools.push(Box::new(crate::tool::reflect::ReflectTool::new(
                    Arc::clone(&reflection),
                )));
            }
            if include_parent_tools {
                if subagents.inbox.is_none() {
                    let inbox = Arc::new(otto_core::agent::inbox::Inbox::default());
                    let source = session.clone();
                    let no_session = self.no_session;
                    crate::runtime::sidecar_mailbox::install(&inbox, ".inbox.json", move || {
                        let path = source.path();
                        (!no_session && !path.is_empty())
                            .then(|| Path::new(&path).with_extension("inbox.json"))
                    });
                    let path = session.path();
                    if !self.no_session
                        && !path.is_empty()
                        && let Some(warning) = crate::runtime::sidecar_mailbox::load(
                            &inbox,
                            &Path::new(&path).with_extension("inbox.json"),
                        )
                    {
                        let _ = writeln!(warnings, "warning: {warning}");
                    }
                    subagents.inbox = Some(inbox);
                }
                let controller = Box::pin(crate::runtime::workflow::shared_controller(
                    self, runtime, warnings,
                ))
                .await;
                if !self.no_session
                    && let (Ok(controller), Some(mailbox)) = (&controller, &subagents.inbox)
                {
                    workflow_binding = Some(crate::runtime::workflow_notifications::bind(
                        controller,
                        session.clone(),
                        mailbox,
                    ));
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
            &runtime.provider,
            &endpoint_host,
            &runtime.model,
        );
        let mut model_guidance = if subagents.runner.is_some() {
            redactor.redact_string(&subagent_model_guidance(&runtime.subagent_models))
        } else {
            String::new()
        };
        if subagents.runner.is_some()
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
        if !session.header().id.is_empty() {
            let review_runtime = match self.config.approval.reviewer_profile.as_deref() {
                Some(profile) => {
                    if !self.config.profiles.contains_key(profile) {
                        return Err("approval reviewer profile does not exist".into());
                    }
                    self.resolve_profile(profile)?
                }
                None => runtime.clone(),
            };
            let review_agent = Agent::with_redactor(
                match self.config.approval.reviewer_profile {
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
                    provider_name: review_runtime.provider.clone(),
                    thinking: review_runtime.thinking.clone(),
                    now: Box::new(Utc::now),
                    new_operation_id: Box::new(new_operation_id),
                    ..Options::default()
                },
                self.boundary_redactor(Some(&review_runtime)),
            );
            self.approvals
                .register_reviewer(
                    &self.workspace_path,
                    &session.header().id,
                    Arc::new(
                        crate::approval::reviewer::SessionReviewer::new(review_agent)
                            .with_usage(self.usage_collector(session, &review_runtime)),
                    ),
                )
                .map_err(|error| error.to_string())?;
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
            ProviderClient::ChatGpt { client, .. } => {
                Some(client.clone() as Arc<dyn RequestSizer + Send + Sync>)
            }
            ProviderClient::Unavailable => None,
            #[cfg(test)]
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
                extensions.bind(
                    session.header().id,
                    None,
                    String::new(),
                    if session.path().is_empty() {
                        "new"
                    } else {
                        "resume"
                    },
                    Some(hook_guard),
                )
            }),
            hook_state: session.1.clone(),
            model: runtime.model.clone(),
            provider_name: runtime.provider.clone(),
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
            tasks: subagents
                .tasks
                .clone()
                .map(|tasks| tasks as Arc<dyn otto_core::agent::tasks::TaskRegistry + Send + Sync>),
            mailbox: subagents
                .inbox
                .unwrap_or_else(|| Arc::new(otto_core::mailbox::InMemoryMailbox::default())),
            steering: task_control.steering(),
            version: env!("CARGO_PKG_VERSION").into(),
            ..Options::default()
        };
        let hook_port = options.hooks.clone();
        let hook_state = options.hook_state.clone();
        mark_build_trace(&mut trace, "runner/registry-and-options");
        if !plugins.packages.is_empty() {
            session
                .append_custom("otto.plugins", &plugins.provenance())
                .await
                .map_err(|e| e.to_string())?;
        }
        let trace_entries = trace.map(|trace| trace.entries).unwrap_or_default();
        let runner = Runner {
            workflow_binding,
            output_logs,
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
            agent: Agent::with_redactor(client, registry, session.clone(), options, redactor),
            turn_timeout: runtime.resilience.deadlines.turn_timeout,
            unattended_turn_timeout: runtime.resilience.deadlines.unattended_turn_timeout,
            headless: self.headless,
            system_prompt,
            definitions,
            usage: self.usage_collector(session, runtime),
            logger: self.logger.clone(),
            tasks: subagents.tasks,
            subagents: subagents.runner,
            reminders: subagents.reminders,
            skills: catalogs.skills.clone(),
            discovered_skills: catalogs.discovered_skills.clone(),
            agents: catalogs.agent_catalog.clone(),
            mcp: mcp_servers,
            task_control,
            advisor,
            memory_client,
            schedule_origin,
            presentation_origin,
            reflection,
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
                    runner.close_mcp().await;
                    return Err(message);
                }
            };
            if response.decision == Some(otto_core::agent::hooks::Decision::Deny) {
                runner.agent.end_hooks().await;
                runner.close();
                runner.close_mcp().await;
                return Err(response
                    .reason
                    .unwrap_or_else(|| "Hook denied session".into()));
            }
        }
        Ok((runner, trace_entries))
    }
}
