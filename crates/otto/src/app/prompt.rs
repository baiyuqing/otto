//! Prompt turns, follow-ups, compaction and reflection entry points.

use super::*;

impl Controller {
    pub async fn prompt(
        &self,
        text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.prompt_with_system_context(
            text,
            "",
            Some(crate::schedule::Origin::terminal()),
            emit,
            cancel,
        )
        .await
    }

    pub(crate) async fn prompt_with_system_context(
        &self,
        text: &str,
        system_context: &str,
        origin: Option<crate::schedule::Origin>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _admission = self.begin_operation().map_err(AgentError::Other)?;
        let runner = self.runner().map_err(AgentError::Other)?;
        let _presentation = self.begin_user_turn(&runner, origin).await;
        runner.record_user_authorization(text).await?;
        let task_run = runner
            .task_control()
            .begin(&runner.session().header().id, &runner.redact_text(text))
            .map_err(AgentError::Other)?;
        self.record_unrun_followups(&runner, task_run.discarded(), &mut *emit)
            .await?;
        self.run_prompt_step(&runner, text, system_context, &mut *emit, cancel)
            .await?;
        self.run_followups_held(&runner, &task_run, system_context, &mut *emit, cancel)
            .await
    }

    /// One prompt step inside an already-admitted logical turn. Approval
    /// retries call this without opening a second follow-up scope.
    pub(crate) async fn run_prompt_step(
        &self,
        runner: &Runner,
        text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let observation = auto_reflection::RunObservation::default();
        let reporter = self.herdr_reporter();
        let session = match &reporter {
            Some(_) => self.herdr_session(),
            None => crate::herdr::SessionRef {
                id: String::new(),
                path: String::new(),
            },
        };
        let mut observed = auto_reflection::observing_prompt(&mut *emit, &observation);
        self.start_turn_recording(runner);
        let result = runner
            .run_with_system_context(
                text,
                system_context,
                &mut crate::herdr::observing(&mut observed, reporter.as_deref(), &session),
                cancel,
            )
            .await;
        self.upload_turn(runner, result.is_ok(), cancel);
        self.after_prompt(&observation, result.is_ok() && !cancel.is_cancelled());
        result
    }

    pub(super) async fn run_followups_held(
        &self,
        runner: &Runner,
        task_run: &task_control::Run,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        while let Some(followup) = task_run.next_followup() {
            if self.pending_approval().is_some() {
                task_run.clear_followups();
                self.record_skipped_followup(runner, emit).await?;
                return Ok(());
            }
            if otto_core::session::Session::has_input(runner.session(), &followup.input_id) {
                // Its step ran before an interruption kept it journaled.
                task_run.followup_done(&followup);
                continue;
            }
            task_run.set_current_task(&followup.text);
            runner.set_next_input_id(Some(followup.input_id.clone()));
            self.run_followup_step(runner, &followup.text, system_context, &mut *emit, cancel)
                .await?;
            task_run.followup_done(&followup);
        }
        Ok(())
    }

    /// Records, as a persisted and visible `agent_followup` notice, that
    /// `count` follow-ups of an earlier task were discarded because Otto
    /// stopped before they started and that task is not being resumed.
    pub(crate) async fn record_unrun_followups(
        &self,
        runner: &Runner,
        count: usize,
        emit: EventSink<'_>,
    ) -> Result<(), AgentError> {
        if count == 0 {
            return Ok(());
        }
        let text = format!(
            "{count} agent-scheduled follow-up(s) from an earlier task were not run because Otto stopped before they started. Schedule them again if they are still needed."
        );
        runner
            .session()
            .append(Message {
                role: otto_core::model::Role::Context,
                blocks: vec![Block::text(&text)],
                created_at: chrono::Utc::now(),
                context_type: "agent_followup".into(),
                display: true,
                ..Message::default()
            })
            .await
            .map_err(|error| AgentError::Other(error.to_string()))?;
        emit(otto_core::agent::Event::Notification {
            kind: Some(NotificationKind::TaskReport),
            task_id: "agent_followup".into(),
            text,
            usage: Usage::default(),
            present: false,
            cached_input_tokens_present: false,
        });
        Ok(())
    }

    /// Runs the journaled follow-ups of the task a wake turn just resumed,
    /// in order, under the same rules as any follow-up.
    pub(crate) async fn run_resumed_followups(
        &self,
        runner: &Runner,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let task_run = runner
            .task_control()
            .begin_resuming(&runner.session().header().id, "", true)
            .map_err(AgentError::Other)?;
        self.run_followups_held(runner, &task_run, "", emit, cancel)
            .await
    }

    /// Whether the session's last turn was interrupted by process loss and
    /// still has its automatic resume: the next wake turn resumes it and
    /// adopts its journaled follow-ups.
    pub(crate) fn resumable_interrupted_turn(&self) -> bool {
        let state = self.lock();
        state.current.as_ref().is_some_and(|current| {
            otto_core::session::Session::interrupted_turn(&current.session)
                .is_some_and(|turn| !turn.resume_exhausted())
        })
    }

    /// Sets the turn origin the schedule tools read and appends the scheduled
    /// deliveries for that origin that no session has recorded yet, before the
    /// user message. Called once per logical user turn, not for follow-ups or
    /// wake turns.
    pub(crate) async fn begin_user_turn(
        &self,
        runner: &Runner,
        origin: Option<crate::schedule::Origin>,
    ) -> PresentationOriginGuard {
        runner.schedule_origin.set(origin.clone());
        runner.presentation_origin.set(origin.clone());
        let presentation = PresentationOriginGuard(Arc::clone(&runner.presentation_origin));

        let (Some(origin), Some(store)) = (origin, self.builder.schedule.as_ref()) else {
            return presentation;
        };
        let session_id = runner.session().header().id.clone();
        // A store or append error ends the write-back; the next user turn
        // retries, because the row stays unmarked.
        let Ok(deliveries) = store.pending_writebacks(&origin, &session_id, 10) else {
            return presentation;
        };
        for delivery in deliveries {
            let appended = runner
                .session()
                .append(Message {
                    role: otto_core::model::Role::Context,
                    blocks: vec![Block::text(runner.redact_text(&delivery.context_text()))],
                    created_at: chrono::Utc::now(),
                    context_type: "scheduled_delivery".into(),
                    display: true,
                    ..Message::default()
                })
                .await;
            if appended.is_err()
                || store
                    .mark_appended(delivery.item_id, delivery.due_at)
                    .is_err()
            {
                return presentation;
            }
        }
        presentation
    }

    pub(super) async fn record_skipped_followup(
        &self,
        runner: &Runner,
        emit: EventSink<'_>,
    ) -> Result<(), AgentError> {
        let text = "An agent-scheduled follow-up was not run because this terminal turn ended while approval is pending. Retry it after resolving the approval.";
        runner
            .session()
            .append(Message {
                role: otto_core::model::Role::Context,
                blocks: vec![Block::text(text)],
                created_at: chrono::Utc::now(),
                context_type: "agent_followup".into(),
                display: true,
                ..Message::default()
            })
            .await
            .map_err(|error| AgentError::Other(error.to_string()))?;
        emit(otto_core::agent::Event::Notification {
            kind: Some(NotificationKind::TaskReport),
            task_id: "agent_followup".into(),
            text: text.into(),
            usage: Usage::default(),
            present: false,
            cached_input_tokens_present: false,
        });
        Ok(())
    }

    pub(super) async fn run_followup_step(
        &self,
        runner: &Runner,
        text: &str,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        if cancel.is_cancelled() {
            return Ok(());
        }
        emit(otto_core::agent::Event::TextDelta {
            text: "\n\n".into(),
        });
        let observation = auto_reflection::RunObservation::default();
        self.start_turn_recording(runner);
        let result = runner
            .run_followup_with_system_context(
                text,
                system_context,
                &mut auto_reflection::observing_prompt(&mut *emit, &observation),
                cancel,
            )
            .await;
        self.upload_turn(runner, result.is_ok(), cancel);
        self.after_prompt(&observation, result.is_ok() && !cancel.is_cancelled());
        result
    }

    pub async fn prompt_with_image(
        &self,
        text: &str,
        image: Block,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.prompt_with_image_and_system_context(
            text,
            image,
            "",
            Some(crate::schedule::Origin::terminal()),
            emit,
            cancel,
        )
        .await
    }

    pub(crate) async fn prompt_with_image_and_system_context(
        &self,
        text: &str,
        image: Block,
        system_context: &str,
        origin: Option<crate::schedule::Origin>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _admission = self.begin_operation().map_err(AgentError::Other)?;
        let runner = self.runner().map_err(AgentError::Other)?;
        let _presentation = self.begin_user_turn(&runner, origin).await;
        runner.record_user_authorization(text).await?;
        let task_run = runner
            .task_control()
            .begin(&runner.session().header().id, &runner.redact_text(text))
            .map_err(AgentError::Other)?;
        self.record_unrun_followups(&runner, task_run.discarded(), &mut *emit)
            .await?;
        let observation = auto_reflection::RunObservation::default();
        self.start_turn_recording(&runner);
        let result = runner
            .run_with_image_system_context(
                text,
                image,
                system_context,
                &mut auto_reflection::observing_prompt(&mut *emit, &observation),
                cancel,
            )
            .await;
        self.upload_turn(&runner, result.is_ok(), cancel);
        self.after_prompt(&observation, result.is_ok() && !cancel.is_cancelled());
        result?;
        self.run_followups_held(&runner, &task_run, system_context, &mut *emit, cancel)
            .await
    }

    pub async fn compact(
        &self,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, AgentError> {
        let _admission = self.begin_operation().map_err(AgentError::Other)?;
        let runner = self.runner().map_err(AgentError::Other)?;
        let compacted = std::sync::atomic::AtomicBool::new(false);
        let result = runner
            .compact(
                focus,
                &mut auto_reflection::observing(emit, &compacted),
                cancel,
            )
            .await;
        self.after_operation(&compacted);
        result
    }

    /// Starts background reflection when the operation just ended compacted
    /// the transcript.
    pub(crate) fn after_operation(&self, compacted: &std::sync::atomic::AtomicBool) {
        if compacted.load(std::sync::atomic::Ordering::SeqCst) {
            self.start_auto_reflection(crate::reflection::Trigger::OnCompaction);
        }
    }

    /// Starts collecting the messages of one prompt step for the remote
    /// memory service. Does nothing for other memory backends.
    pub(super) fn start_turn_recording(&self, runner: &Runner) {
        if self.builder.memory.turns.is_some() && self.builder.memory.usable {
            runner.session().start_recording();
        }
    }

    /// Hands the messages recorded since [`Self::start_turn_recording`] to
    /// the upload worker.
    pub(super) fn upload_turn(&self, runner: &Runner, succeeded: bool, cancel: &CancellationToken) {
        let Some(turns) = &self.builder.memory.turns else {
            return;
        };
        let messages = runner.session().finish_recording();
        let status = if cancel.is_cancelled() {
            crate::memory::remote::wire::TurnStatus::Cancelled
        } else if succeeded {
            crate::memory::remote::wire::TurnStatus::Success
        } else {
            crate::memory::remote::wire::TurnStatus::Error
        };
        let context = self.builder.memory_context(
            otto_core::memory::Origin::Model,
            Some(runner.session().header().id),
        );
        turns.submit(&context, status, messages);
    }

    /// Runs compaction reflection as before, then considers a task-end run
    /// only for a successful completed prompt with enough successful tools.
    pub(super) fn after_prompt(
        &self,
        observation: &auto_reflection::RunObservation,
        completed: bool,
    ) {
        if observation.compacted() {
            self.start_auto_reflection(crate::reflection::Trigger::OnCompaction);
        }
        if observation.eligible_work(completed) {
            self.start_auto_reflection(crate::reflection::Trigger::OnWork);
        }
    }

    /// Reflects on the part of the current session no earlier run covered and
    /// queues what it finds as memory candidates for human review.
    ///
    /// Takes the same admission as a turn or a compaction, so it never runs
    /// beside either. Appends nothing to the session.
    pub async fn reflect(
        &self,
        focus: &str,
        cancel: &CancellationToken,
    ) -> Result<crate::reflection::Report, crate::reflection::Error> {
        self.reflect_as(crate::reflection::Trigger::Manual, focus, cancel)
            .await
    }

    /// The newest reflection run logs, newest first. Reads files only, so it
    /// needs no admission and may run beside a turn.
    pub fn reflection_runs(
        &self,
    ) -> Result<Vec<crate::reflection::runlog::Summary>, crate::reflection::Error> {
        self.builder
            .reflector
            .runs(crate::reflection::runlog::MAXIMUM_LISTED)
    }

    /// One reflection run log by the id [`Self::reflection_runs`] shows.
    pub fn reflection_run(
        &self,
        id: &str,
    ) -> Result<Option<(PathBuf, String)>, crate::reflection::Error> {
        self.builder.reflector.run_log(id)
    }

    /// Where reflection writes and looks up skills, or `None` when skills are
    /// disabled in `[skills]` or there is no home directory.
    pub fn reflection_skill_roots(&self) -> Option<crate::reflection::skillwrite::Roots> {
        let skills = otto_core::config::resolve_skills(
            &self.builder.config,
            &self.builder.environment,
            &self.builder.workspace_path,
        );
        skills
            .enabled
            .then(|| crate::reflection::skill_roots(&self.builder.home, &skills.roots))
            .flatten()
    }

    // ---- profiles ----
}
