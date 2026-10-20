//! Builder skill and agent catalogs, sub-agent runner and child tools.

use super::*;

impl Builder {
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
        // Inventory uses the same trust-filtered roots even when execution is off.
        // Only the active catalog below may feed tools, prompts, or sub-agents.
        let mut discovery_config = self.config.clone();
        discovery_config.skills.enabled = Some(true);
        let discovery = resolve_skills(&discovery_config, &self.environment, &self.workspace_path);
        let skill_roots = crate::config::layered::catalog_roots(
            &self.config,
            Path::new(&self.home),
            Path::new(&self.workspace_path),
            &discovery.roots,
            "skills",
        )?;
        let (mut discovered_catalog, mut warnings) = skill::Catalog::discover(&roots(&skill_roots));
        warnings.extend(plugins.diagnostics.clone());
        warnings.extend(plugins.extend_skills(&mut discovered_catalog));
        let catalog = if skills.enabled {
            discovered_catalog.without_names(&skills.disabled)
        } else {
            skill::Catalog::default()
        };

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
            discovered_skills: discovered_catalog,
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
        let session_for_fork = session_for_children.clone();
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
                new_operation_id: Arc::new(super::super::builder::new_operation_id),
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
            parent_fork: (!self.no_session).then(|| {
                let storage = Arc::clone(&self.storage);
                Arc::new(move |prompt: &str| {
                    let path = session_for_fork.path();
                    if path.is_empty() {
                        return Err("needs a saved session".to_owned());
                    }
                    storage.sessions()?.fork_context(&path, prompt)
                }) as subagent::runner::ParentFork
            }),
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
        super::super::sidecar_mailbox::install(&inbox, ".inbox.json", move || {
            let path = session_for_inbox.path();
            (!no_session && !path.is_empty()).then(|| Path::new(&path).with_extension("inbox.json"))
        });
        // A session with no persisted inbox path (`--no-session`, or nothing
        // written yet) never takes over a lease either, so it never has a
        // takeover to report; the recovery scan lives in this branch rather
        // than running unconditionally.
        if let Some(path) = inbox_persist {
            if let Some(warning) = super::super::sidecar_mailbox::load(&inbox, &path) {
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
        super::super::sidecar_mailbox::install(&journal, &format!(".{extension}"), move || {
            let path = for_path.path();
            (!path.is_empty()).then(|| Path::new(&path).with_extension(extension))
        });
        if let Some(path) = self.sidecar_path(session, extension)
            && let Some(warning) = super::super::sidecar_mailbox::load(&journal, &path)
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
                super::super::builder::shell_timeout(runtime.shell_timeout),
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
}

impl Builder {
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
            super::super::prompt::system_prompt_for(definitions, sandbox, &provider, &host, &model)
                + &tail
        })
    }
}
