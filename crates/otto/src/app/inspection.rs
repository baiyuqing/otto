//! Read-only views of the controller: runtime info, history, skills, MCP and tools.

use super::*;

impl Controller {
    /// The captured ChatGPT credential path, empty when none was captured.
    pub fn auth_path(&self) -> &str {
        &self.builder.auth_path
    }

    /// The process-wide memory service, its user scope, and the recall
    /// limits. `/memory` and `/remember` read them through
    /// `Controller::memory_manager`.
    pub(crate) fn memory_wiring(&self) -> &crate::runtime::wiring::MemoryWiring {
        &self.builder.memory
    }

    pub fn memory_service(&self) -> Option<Arc<crate::memory::client::Client>> {
        self.dynamic_content_available()
            .then(|| {
                self.builder.memory.client.clone().or_else(|| {
                    self.current_runner()
                        .and_then(|runner| runner.memory_client.clone())
                })
            })
            .flatten()
            .filter(|client| !client.is_closed())
    }

    pub fn memory_context(
        &self,
        origin: otto_core::memory::Origin,
    ) -> otto_core::memory::MemoryContext {
        self.builder
            .memory_context(origin, Some(self.info().session_id))
    }

    /// This session's memory workspace scope, unlike `memory_wiring`'s
    /// process-wide service and user scope.
    pub(crate) fn workspace_memory_scope(&self) -> &crate::memory::Scope {
        &self.builder.workspace_scope
    }

    /// The runner currently in force. `/tasks` and `/task` read its sub-agent
    /// task registry.
    pub fn current_runner(&self) -> Option<Arc<Runner>> {
        self.runner().ok()
    }

    pub fn config_path(&self) -> &PathBuf {
        &self.builder.config_path
    }

    /// The sandbox state now in effect: the live process sandbox when one is
    /// wired, otherwise what the builder resolved at startup.
    pub fn sandbox_info(&self) -> SandboxInfo {
        match &self.sandbox {
            Some(control) => control.info(),
            None => self.builder.effective_sandbox_info(),
        }
    }

    pub fn dynamic_content(&self) -> bool {
        self.dynamic_content
    }

    pub fn dynamic_content_available(&self) -> bool {
        !self.lock().closed && self.dynamic_content
    }

    pub fn workspace(&self) -> &str {
        &self.builder.workspace_path
    }

    /// A closed boundary reports the sandbox only.
    pub fn info(&self) -> Info {
        let sandbox = self.sandbox_info();
        if !self.dynamic_content {
            return Info {
                sandbox,
                ..Info::default()
            };
        }
        let state = self.lock();
        let Some(current) = state.current.as_ref() else {
            return Info {
                sandbox,
                ..Info::default()
            };
        };
        let header = current.session.header();
        let snapshot = current.session.snapshot();
        let approval_settings = self
            .builder
            .approvals
            .settings(&otto_core::approval::Scope {
                workspace: header.workspace.clone(),
                session_id: header.id.clone(),
                task_id: String::new(),
                run_id: String::new(),
            })
            .unwrap_or_default();
        Info {
            local_commands: current.session.handle().local_commands(),
            session_id: header.id,
            session_name: current.session.name(),
            session_path: current.path(),
            workspace: header.workspace,
            provider: current.info.provider.clone(),
            provider_type: current.info.provider_type.clone(),
            model_selector: current.info.model_selector.clone(),
            model: current.info.model.clone(),
            thinking: current.info.thinking.clone(),
            usage: snapshot.aggregate_usage,
            usage_present: snapshot.aggregate_usage_present,
            context_window: current.info.context_window,
            context_input_tokens: snapshot.context_input_tokens,
            context_input_tokens_present: snapshot.context_input_tokens_present,
            context_input_tokens_pending: snapshot.context_input_tokens_pending,
            sandbox,
            approval_mode: approval_settings.mode(),
            approval_settings,
            forked_from: current.session.forked_from().map(ForkedFrom::from),
            plan: otto_core::session::Session::plan(&current.session),
        }
    }

    /// Whether the current session's transcript holds a message delivering
    /// input `input_id` (a client retry of an input already delivered).
    pub fn has_input(&self, input_id: &str) -> bool {
        let state = self.lock();
        state.current.as_ref().is_some_and(|current| {
            otto_core::session::Session::has_input(&current.session, input_id)
        })
    }

    /// The session's durable turn queue; `None` under `--no-session`.
    pub fn turn_queue(&self) -> Option<Arc<turn_queue::TurnQueue>> {
        self.current_runner()?.task_control().turn_queue()
    }

    /// The prompts saved in the turn queue that have not started, oldest
    /// first. A saved prompt whose user message is already in the transcript
    /// started before Otto stopped: its entry is removed, and the
    /// interrupted turn's recovery covers it.
    pub fn restorable_prompts(&self) -> Vec<turn_queue::QueuedPrompt> {
        let Some(queue) = self.turn_queue() else {
            return Vec::new();
        };
        queue
            .pending()
            .into_iter()
            .filter(|prompt| {
                let started = self.has_input(&prompt.input_id);
                if started {
                    queue.forget(&prompt.input_id);
                }
                !started
            })
            .collect()
    }

    /// Queues the context notice that `prompts` were restored, for the
    /// first of them to deliver before its user message. Its id is stable,
    /// so a second restore of the same prompts does not repeat it.
    pub fn announce_restored_prompts(&self, prompts: &[turn_queue::QueuedPrompt]) {
        let (Some(first), Some(tasks)) = (prompts.first(), self.subagent_tasks()) else {
            return;
        };
        let _ = tasks
            .notifications()
            .enqueue(otto_core::agent::inbox::Notification {
                id: format!("restored:{}", first.input_id),
                text: turn_queue::restored_notice(prompts),
                ..otto_core::agent::inbox::Notification::default()
            });
    }

    /// Records the client's input id for the next turn's user message; see
    /// `Agent::set_next_input_id`.
    pub fn set_next_input_id(&self, input_id: Option<String>) {
        if let Some(runner) = self.current_runner() {
            runner.set_next_input_id(input_id);
        }
    }

    /// Whether the session's last turn was interrupted by process loss, so
    /// the next wake turn resumes it (see `otto_core::session::turn`).
    pub fn resumes_interrupted_turn(&self) -> bool {
        let state = self.lock();
        state.current.as_ref().is_some_and(|current| {
            otto_core::session::Session::interrupted_turn(&current.session).is_some()
        })
    }

    pub fn history(&self) -> Vec<Message> {
        if !self.dynamic_content {
            return Vec::new();
        }
        let state = self.lock();
        match state.current.as_ref() {
            Some(current) => {
                let mut messages = current.session.messages();
                project_invocation_identity(&mut messages, &current.session.operation_ledger());
                messages
            }
            None => Vec::new(),
        }
    }

    /// What the next provider request contains, or `None` after a close or
    /// when the redaction boundary withholds dynamic content.
    pub fn context_report(&self) -> Option<otto_core::agent::context_report::ContextReport> {
        if !self.dynamic_content {
            return None;
        }
        let runner = self.current_runner()?;
        Some(runner.context_report())
    }

    /// The session currently in force, or `None` after a close.
    pub fn current_session_opt(&self) -> Option<SharedSession> {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.session.clone())
    }

    /// The session currently in force. Panics only if called after a close,
    /// which no frontend does; `current_session_opt` is the checked form.
    pub fn current_session(&self) -> SharedSession {
        self.current_session_opt()
            .expect("controller has no current session")
    }

    /// The system prompt the current runner was built with.
    pub fn system_prompt(&self) -> String {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.runner.system_prompt().to_string())
            .unwrap_or_default()
    }

    /// The skill catalog fixed for the current runner.
    pub fn skills(&self) -> crate::skill::Catalog {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.runner.skills().clone())
            .unwrap_or_default()
    }

    /// All discovered skills, including those excluded from the active runner.
    pub(crate) fn discovered_skills(&self) -> crate::skill::Catalog {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.runner.discovered_skills.clone())
            .unwrap_or_default()
    }

    pub fn managed_skills(&self) -> Result<management::SkillManager, String> {
        let config = crate::config::load(&self.builder().config_path)
            .map_err(|e| format!("cannot read skill configuration: {e}"))?;
        let active = self.skills();
        Ok(management::SkillManager {
            global_enabled: config.skills.enabled.unwrap_or(true),
            skills: self
                .discovered_skills()
                .skills()
                .iter()
                .map(|skill| management::ManagedSkill {
                    skill: skill.clone(),
                    enabled: !config.skills.disabled.contains(&skill.name),
                    loaded: active.lookup(&skill.name).is_some(),
                })
                .collect(),
        })
    }

    /// The automatic skill contract checker, when the feature is enabled and
    /// the current runner has sub-agents built.
    pub fn skill_checker(&self) -> Option<Arc<crate::skill::check::Checker>> {
        self.lock()
            .current
            .as_ref()
            .and_then(|current| current.runner.subagents())
            .and_then(|subagents| subagents.checker().cloned())
    }

    /// The MCP server status rows for `/mcp`, in configuration order. Empty
    /// for a closed controller.
    pub fn mcp(&self) -> Vec<crate::mcp::ServerStatus> {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.runner.mcp().status())
            .unwrap_or_default()
    }

    /// The names of the tools the current runner offers the model.
    #[cfg(test)]
    pub(crate) fn tool_names(&self) -> Vec<String> {
        self.runner()
            .map(|runner| {
                runner
                    .definitions()
                    .iter()
                    .map(|d| d.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}
