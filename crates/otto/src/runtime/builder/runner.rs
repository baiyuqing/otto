//! Runner turn execution, accessors and shutdown.

use super::*;

impl Runner {
    /// A turn is unattended when this process is `otto --prompt` or the
    /// current user turn came from a chat.
    fn turn_deadline(&self) -> Deadline {
        let unattended = self.headless || self.schedule_origin.get().is_some_and(|o| o.is_chat());
        turn_limit(self.turn_timeout, self.unattended_turn_timeout, unattended)
            .map_or_else(Deadline::unlimited, Deadline::after)
    }

    pub(crate) async fn end_session_hooks(self: &Arc<Self>) {
        if self.agent.claim_end_hooks() {
            let runner = self.clone();
            tokio::spawn(async move {
                runner.agent.end_hooks().await;
            });
        }
        self.agent.wait_end_hooks().await;
    }
    /// Runs one turn.
    pub async fn run(
        &self,
        user_text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.run_with_system_context(user_text, "", emit, cancel)
            .await
    }

    /// Runs one turn with ephemeral system context that is not persisted.
    pub async fn run_with_system_context(
        &self,
        user_text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _output_lease = self.retain_temporary_outputs();
        let control = Control::new(self.turn_deadline());
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let mut send = |event| {
            let _ = events.send(event);
        };
        let mut collecting = self.collecting(&mut send);
        let run = self.agent.run_with_system_context_control(
            user_text,
            system_context,
            &mut collecting,
            &control,
        );
        let result = self
            .monitor_approvals(
                drive_with_control(run, cancel, &control),
                received,
                &mut *emit,
            )
            .await;
        self.report_lost_steering(&mut *emit).await;
        result
    }

    /// Runs one model-originated follow-up as display-only context.
    pub(crate) async fn run_followup_with_system_context(
        &self,
        text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _output_lease = self.retain_temporary_outputs();
        let control = Control::new(self.turn_deadline());
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let mut send = |event| {
            let _ = events.send(event);
        };
        let mut collecting = self.collecting(&mut send);
        let run = self.agent.run_followup_with_system_context_control(
            text,
            system_context,
            &mut collecting,
            &control,
        );
        let result = self
            .monitor_approvals(
                drive_with_control(run, cancel, &control),
                received,
                &mut *emit,
            )
            .await;
        self.report_lost_steering(&mut *emit).await;
        result
    }

    pub async fn run_with_image_system_context(
        &self,
        user_text: &str,
        image: Block,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _output_lease = self.retain_temporary_outputs();
        let control = Control::new(self.turn_deadline());
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let mut send = |event| {
            let _ = events.send(event);
        };
        let mut collecting = self.collecting(&mut send);
        let run = self.agent.run_with_image_system_context_control(
            user_text,
            Some(image),
            system_context,
            &mut collecting,
            &control,
        );
        let result = self
            .monitor_approvals(
                drive_with_control(run, cancel, &control),
                received,
                &mut *emit,
            )
            .await;
        self.report_lost_steering(&mut *emit).await;
        result
    }

    /// Reports the guidance a finished run accepted but never delivered.
    ///
    /// An interrupted turn — the user pressed Esc, the provider failed, the
    /// deadline expired — drops guidance it never reached a safe boundary
    /// for. The user typed those words and must learn what became of them, so
    /// each loss is persisted as a visible `agent_steering` context message
    /// and announced on the turn's own event stream. A failed append still
    /// announces the text, with the failure appended: losing the user's input
    /// silently is exactly what this prevents.
    async fn report_lost_steering(&self, emit: EventSink<'_>) {
        let lost = self.steering().take_discarded();
        if lost.is_empty() {
            return;
        }
        let mut text = lost_steering_notice(&lost);
        if let Err(error) = self
            .session()
            .append(Message {
                role: otto_core::model::Role::Context,
                blocks: vec![Block::text(&text)],
                created_at: Utc::now(),
                context_type: "agent_steering".into(),
                display: true,
                ..Message::default()
            })
            .await
        {
            text.push_str(&format!("\n\n(It could not be saved: {error})"));
        }
        emit(Event::Notification {
            input_id: String::new(),
            input_provenance: None,
            kind: Some(NotificationKind::TaskReport),
            task_id: "agent_steering".into(),
            text,
            usage: Usage::default(),
            present: false,
            cached_input_tokens_present: false,
        });
    }

    async fn monitor_approvals<T>(
        &self,
        future: impl Future<Output = T>,
        mut events: tokio::sync::mpsc::UnboundedReceiver<otto_core::agent::Event>,
        emit: EventSink<'_>,
    ) -> T {
        {
            let mut run = self.authorization_run.lock().expect("authorization run");
            if !run.starts_with("schedule:") {
                *run = self
                    .task_control()
                    .authorization_scope()
                    .unwrap_or_else(crate::runtime::sidecar_mailbox::new_input_id);
            }
        }
        tokio::pin!(future);
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut cursor = self
            .approvals
            .as_ref()
            .and_then(|(service, scope)| {
                service
                    .snapshot(&scope.workspace, Some(&scope.session_id))
                    .ok()
            })
            .map_or(0, |s| s.cursor);
        loop {
            tokio::select! {
                result=&mut future=>{
                    let _ = self.flush_user_authorization().await;
                    while let Ok(event)=events.try_recv(){emit(event);}
                    if let Some((service,scope))=&self.approvals {
                        while let Ok(changes)=service.events(&scope.workspace,Some(&scope.session_id),cursor) {
                            if changes.is_empty() {break;}
                            for change in changes {cursor=change.sequence;emit(otto_core::agent::Event::ApprovalChanged{request:Box::new(change.request)});}
                        }
                    }
                    return result
                },
                Some(event)=events.recv()=>emit(event),
                _=tick.tick(),if self.approvals.is_some()=>{
                    let _ = self.flush_user_authorization().await;
                    while let Ok(event)=events.try_recv(){emit(event);}
                    if let Some((service,scope))=&self.approvals && let Ok(changes)=service.events(&scope.workspace,Some(&scope.session_id),cursor) {
                        for change in changes {cursor=change.sequence;emit(otto_core::agent::Event::ApprovalChanged{request:Box::new(change.request)});}
                    }
                }
            }
        }
    }

    /// Compacts the transcript.
    pub async fn compact(
        &self,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, AgentError> {
        let control = Control::new(
            self.turn_timeout
                .map_or_else(Deadline::unlimited, Deadline::after),
        );
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let mut emit = self.collecting(emit);
        let compact = self.agent.compact_with_control(focus, &mut emit, &control);
        drive_with_control(compact, cancel, &control).await
    }

    /// Asks the model once with no tools, outside the turn loop, under the
    /// turn timeout and `cancel`. The provider usage is recorded against
    /// `task_id` so background cost is separable from turn cost.
    pub async fn complete_text(
        &self,
        request: &otto_core::agent::oneshot::TextRequest<'_>,
        task_id: &str,
        cancel: &CancellationToken,
    ) -> Result<otto_core::agent::oneshot::TextResponse, AgentError> {
        let control = Control::new(
            self.turn_timeout
                .map_or_else(Deadline::unlimited, Deadline::after),
        );
        if cancel.is_cancelled() {
            control.stop(OperationStopReason::UserCancellation);
        }
        let collector = self.usage.as_ref().map(|usage| usage.for_task(task_id));
        // The one-shot call emits its provider operation record, including
        // for a failed call, so operation and attempt counts include it.
        let mut emit = |event: otto_core::agent::Event| {
            if let Some(collector) = &collector {
                let _ = collector.record(&event);
            }
        };
        let call = self.agent.complete_text(request, &mut emit, &control);
        let response = drive_with_control(call, cancel, &control).await?;
        if let Some(collector) = &collector {
            let _ = collector.record(&otto_core::agent::Event::ProviderUsage {
                usage: response.usage,
                present: response.usage_present,
                cached_input_tokens_present: response.cached_input_tokens_present,
            });
        }
        Ok(response)
    }

    /// A bounded approval-only dialogue. It has an isolated memory transcript,
    /// the existing provider and usage collector, and no effectful task tools.
    pub(crate) async fn approval_dialogue(
        &self,
        text: &str,
        tools: Vec<Box<dyn crate::tool::Tool + Send + Sync>>,
        runtime: &Runtime,
        system_prompt: String,
        redaction_values: &[String],
        cancel: &CancellationToken,
    ) -> Result<String, AgentError> {
        let registry = Registry::new(tools).map_err(AgentError::Other)?;
        let options = Options {
            diagnostics: self.agent.diagnostics().cloned(),
            model: runtime.model.clone(),
            provider_name: runtime.provider_type.clone(),
            thinking: runtime.thinking.clone(),
            system_prompt,
            now: Box::new(Utc::now),
            new_operation_id: Box::new(new_operation_id),
            ..Options::default()
        };
        let agent = Agent::with_redactor(
            self.provider().clone(),
            registry,
            SharedSession::memory(self.session().header()),
            options,
            Redactor::new(redaction_values),
        );
        let control = Control::new(Deadline::after(Duration::from_secs(30)));
        let mut discard = |_| {};
        let mut sink = self.collecting(&mut discard);
        drive_with_control(
            agent.run_with_control(&self.redact_text(text), &mut sink, &control),
            cancel,
            &control,
        )
        .await?;
        Ok(agent
            .session()
            .messages()
            .iter()
            .rev()
            .find(|message| message.role == otto_core::model::Role::Assistant)
            .map_or_else(String::new, |message| self.redact_text(&message.text())))
    }

    /// Redacts transcript text exactly as a provider request would be.
    pub fn redact_text(&self, text: &str) -> String {
        self.agent.redact_text(text)
    }

    /// Whether transcript-derived text may be sent to the provider at all.
    pub fn allows_dynamic_content(&self) -> bool {
        self.agent.allows_dynamic_content()
    }

    pub(super) fn collecting<'a>(
        &'a self,
        emit: EventSink<'a>,
    ) -> impl FnMut(otto_core::agent::Event) + Send + use<'a> {
        move |event| {
            log_runtime_event(&self.logger, self.session(), &event);
            if let Some(usage) = &self.usage {
                let _ = usage.record(&event);
            }
            emit(event);
        }
    }

    pub fn session(&self) -> &SharedSession {
        self.agent.session()
    }

    pub fn provider(&self) -> &ProviderClient {
        self.agent.provider()
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Records the client's input id for the next run's user message.
    pub fn set_next_input_provenance(&self, provenance: Option<otto_core::model::InputProvenance>) {
        self.agent.set_next_input_provenance(provenance);
    }

    pub fn set_next_input_id(&self, input_id: Option<String>) {
        self.agent.set_next_input_id(input_id);
    }

    pub fn task_control(&self) -> Arc<crate::runtime::task_control::TaskControl> {
        Arc::clone(&self.task_control)
    }

    pub(crate) fn steering(&self) -> Arc<otto_core::agent::steering::Steering> {
        self.task_control.steering()
    }

    /// What the next provider request contains.
    pub fn context_report(&self) -> otto_core::agent::context_report::ContextReport {
        self.agent.context_report()
    }

    /// The session mailbox shared with notification producers.
    pub fn mailbox(&self) -> &Arc<dyn otto_core::mailbox::Mailbox> {
        self.agent.mailbox()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    pub fn skills(&self) -> &Catalog {
        &self.skills
    }

    pub fn agents(&self) -> &crate::subagent::Catalog {
        &self.agents
    }

    pub fn subagents(&self) -> Option<Arc<crate::subagent::runner::Runner>> {
        self.subagents.clone()
    }

    /// Replaces the skill catalogs, for a test that builds them apart from
    /// the runner.
    #[cfg(any(test, feature = "test-support"))]
    pub fn install_skill_catalogs(&mut self, skills: Catalog, discovered_skills: Catalog) {
        self.skills = skills;
        self.discovered_skills = discovered_skills;
    }

    /// Installs the sub-agent runner, for a test that spawns sub-agents
    /// through a scripted runner.
    #[cfg(any(test, feature = "test-support"))]
    pub fn install_subagents(&mut self, subagents: Arc<crate::subagent::runner::Runner>) {
        self.subagents = Some(subagents);
    }

    /// Releases what this runner owns: its approval reviewer registration
    /// and the agent's memory binding. The session's resources (sub-agent
    /// tasks, notifications, timers, MCP servers, workflow delivery) belong to
    /// the session and outlive the runner; they end through
    /// [`SharedSession::close`] or [`SharedSession::close_resources`].
    pub fn close(&self) {
        if let (Some((service, scope)), Some(provider)) = (&self.approvals, &self.review_provider) {
            let _ = service.unregister_reviewer(&scope.workspace, &scope.session_id, provider);
        }
        let _ = self.agent.close();
    }

    /// The session's sub-agent task registry, absent when sub-agents are off.
    /// `/tasks` and `/task` read it; it is the same value for every runner of
    /// the session.
    pub(crate) fn tasks(&self) -> Option<&Arc<crate::subagent::tasks::Tasks>> {
        self.session().resources()?.tasks()
    }

    /// The session's timer registry, absent when sub-agents are off. `/timers`
    /// lists it and archiving the session clears it.
    pub(crate) fn reminders(&self) -> Option<&Arc<crate::schedule::remind::Reminders>> {
        self.session().resources()?.reminders()
    }

    /// The MCP servers whose status `/mcp` shows: the session's connected
    /// servers, or the placeholder rows of a runner built without connecting.
    pub(crate) fn mcp(&self) -> Arc<crate::mcp::Servers> {
        if let Some(mcp) = self.session().resources().and_then(|r| r.mcp()) {
            return Arc::clone(&mcp.servers);
        }
        self.connecting_mcp.clone().unwrap_or_default()
    }

    /// Runs during a SIGTERM migration, after this session's turn was
    /// already cancelled: cancels every non-final sub-agent task, waits for
    /// each to reach a final status, and leaves one notification describing
    /// what moved (see [`crate::subagent::session_recovery::notify_moved`]).
    /// Returns no warnings and does nothing when sub-agents are off
    /// (`self.tasks()` is `None`) or the session has no persisted path
    /// (`--no-session`, so there is no children directory to scan).
    pub async fn migrate(&self) -> Vec<String> {
        let Some(tasks) = self.tasks() else {
            return Vec::new();
        };
        let ids = tasks.begin_migration();
        tasks.wait_final(&ids).await;

        let path = self.session().path();
        if path.is_empty() {
            return Vec::new();
        }
        let children_dir = Path::new(&path).with_extension("");
        let max_output_bytes = self
            .subagents
            .as_ref()
            .map(|subagents| subagents.max_output_bytes())
            .unwrap_or(0);
        crate::subagent::session_recovery::notify_moved(
            tasks.notifications().as_ref(),
            &children_dir,
            &ids,
            max_output_bytes,
            &self.session().messages(),
        )
    }

    /// A runner with no tools whose provider the test supplies, carrying the
    /// sub-agent registry the caller owns. See [`ProviderClient::Scripted`].
    ///
    /// `tasks` is wired to both [`Options::tasks`] and [`Options::inbox`], so
    /// an empty-text wake turn reaches the provider instead of being rejected
    /// as empty user text.
    #[cfg(any(test, feature = "test-support"))]
    pub fn scripted(
        session: SharedSession,
        provider: Arc<dyn Provider + Send + Sync>,
        tasks: Arc<crate::subagent::tasks::Tasks>,
    ) -> Self {
        Self::scripted_with_tools(session, provider, tasks, Vec::new())
    }

    /// [`Self::scripted`] with `tools` registered, for a test that needs the
    /// provider's tool call to reach a tool double.
    #[cfg(any(test, feature = "test-support"))]
    pub fn scripted_with_tools(
        session: SharedSession,
        provider: Arc<dyn Provider + Send + Sync>,
        tasks: Arc<crate::subagent::tasks::Tasks>,
        tools: Vec<Box<dyn crate::tool::Tool + Send + Sync>>,
    ) -> Self {
        let registry = Registry::new(tools).expect("test tool registry");
        let definitions = registry.definitions();
        let task_control = Arc::new(crate::runtime::task_control::TaskControl::default());
        let mailbox: Arc<dyn otto_core::mailbox::Mailbox> = tasks.notifications().clone();
        session.install_test_resources(
            super::super::session_resources::SessionResources::for_tests(
                &session.header().id,
                Some(Arc::clone(&tasks)),
            ),
        );
        Self {
            agent: Agent::new(
                ProviderClient::Scripted(provider),
                registry,
                session,
                Options {
                    model: "test-model".to_string(),
                    provider_name: "openai-compatible".to_string(),
                    request_sizer: Some(Arc::new(ScriptedSizer)),
                    now: Box::new(Utc::now),
                    mailbox: Arc::clone(&mailbox),
                    steering: task_control.steering(),
                    ..Options::default()
                },
            ),
            approvals: None,
            review_provider: None,
            authorization_run: Arc::default(),
            review_context: tokio::sync::Mutex::new(Default::default()),
            pending_authorization: Mutex::new(Vec::new()),
            turn_timeout: None,
            unattended_turn_timeout: None,
            headless: false,
            system_prompt: String::new(),
            definitions,
            usage: None,
            logger: Arc::new(crate::log::Logger::discard()),
            subagents: None,
            skills: Catalog::default(),
            discovered_skills: Catalog::default(),
            agents: crate::subagent::Catalog::default(),
            connecting_mcp: None,
            task_control,
            memory_client: None,
            memory_proposals: None,
            schedule_origin: Default::default(),
            presentation_origin: Default::default(),
        }
    }

    /// Gives the session of a [`Self::scripted`] runner the MCP servers a
    /// test injects, in place of none. Test-only: lets a close-ordering test
    /// use a [`crate::mcp::Servers`] with a fake connected client.
    #[cfg(test)]
    pub(crate) fn with_mcp(self, mcp: Arc<crate::mcp::Servers>) -> Self {
        let resources = self.session().resources().expect("scripted resources");
        resources.set_test_mcp(mcp);
        self
    }
}

/// The notice a frontend shows after an interrupted turn dropped guidance it
/// had accepted but never delivered.
///
/// The exact words matter: the user typed them and, with the turn gone, the
/// notice is the only place they can read them back. Each message is quoted,
/// bounded so a pathological queue cannot flood the transcript, and the
/// remainder is counted rather than hidden.
fn lost_steering_notice(lost: &[otto_core::agent::steering::Message]) -> String {
    const MAX_QUOTED: usize = 8;
    const MAX_QUOTED_CHARS: usize = 1_000;

    let mut notice = if lost.len() == 1 {
        "1 queued guidance message for the turn that just ended was not delivered: \
         the turn ended before the next safe boundary, so it was dropped. \
         Send it again if it is still needed."
            .to_string()
    } else {
        format!(
            "{} queued guidance messages for the turn that just ended were not \
             delivered: the turn ended before the next safe boundary, so they were \
             dropped. Send them again if they are still needed.",
            lost.len()
        )
    };
    for message in lost.iter().take(MAX_QUOTED) {
        notice.push_str("\n\n> ");
        let mut chars = message.text.chars();
        let quoted: String = chars.by_ref().take(MAX_QUOTED_CHARS).collect();
        notice.push_str(&quoted.split('\n').collect::<Vec<_>>().join("\n> "));
        if chars.next().is_some() {
            notice.push_str(" […]");
        }
    }
    if let Some(rest) = lost.len().checked_sub(MAX_QUOTED).filter(|rest| *rest > 0) {
        notice.push_str(&format!("\n\n({rest} more not shown)"));
    }
    notice
}

pub(super) fn log_runtime_event(
    logger: &crate::log::Logger,
    session: &SharedSession,
    event: &Event,
) {
    match event {
        Event::ProviderApiCall {
            operation_id,
            kind,
            provider,
            model,
            duration,
            attempts,
            status,
            outcome,
        } => logger.info(
            "provider_operation_finished",
            &[
                ("operation_id", operation_id.to_string()),
                ("kind", kind.name().into()),
                ("provider", provider.clone()),
                ("model", model.clone()),
                ("attempts", attempts.to_string()),
                ("status", status.name().into()),
                (
                    "disposition",
                    crate::usage::disposition_name(outcome.disposition).into(),
                ),
                (
                    "effect_certainty",
                    crate::usage::effect_certainty_name(outcome.effect_certainty).into(),
                ),
                (
                    "stop_reason",
                    outcome
                        .stop_reason
                        .map(crate::usage::stop_reason_name)
                        .unwrap_or_default()
                        .into(),
                ),
                ("duration_ms", duration.as_millis().to_string()),
            ],
        ),
        // A no-op result has an empty checkpoint id, which can equal the id of
        // a remote checkpoint the provider returned without one.
        Event::CompactionCompleted { compaction } if !compaction.noop => {
            let Some(checkpoint) = session
                .remote_compaction()
                .filter(|checkpoint| checkpoint.id == compaction.checkpoint_id)
            else {
                return;
            };
            logger.info(
                "remote_compaction_finished",
                &[
                    ("checkpoint_id", checkpoint.id),
                    ("provider", checkpoint.provider),
                    ("reason", compaction.reason.name().into()),
                    ("automatic", compaction.automatic.to_string()),
                    (
                        "tail_messages",
                        session.post_remote_compaction_messages().len().to_string(),
                    ),
                    (
                        "tokens_before",
                        compaction
                            .tokens_before
                            .map(|value| value.to_string())
                            .unwrap_or_default(),
                    ),
                    ("usage_present", compaction.usage_present.to_string()),
                    (
                        "cached_input_tokens_present",
                        compaction.cached_input_tokens_present.to_string(),
                    ),
                ],
            );
        }
        _ => {}
    }
}

/// The step deadline: for an unattended turn the earlier of the two limits,
/// where `None` means no limit; otherwise `turn_timeout`.
pub(super) fn turn_limit(
    turn_timeout: Option<Duration>,
    unattended_turn_timeout: Option<Duration>,
    unattended: bool,
) -> Option<Duration> {
    if !unattended {
        return turn_timeout;
    }
    match (turn_timeout, unattended_turn_timeout) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
