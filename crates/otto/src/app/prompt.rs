//! Prompt turns, follow-ups, compaction entry points.

use super::*;

/// Why a follow-up was discarded when its turn ended while an approval waits.
const APPROVAL_SKIP_NOTICE: &str = "An agent-scheduled follow-up was not run because this terminal turn ended while approval is pending. Retry it after resolving the approval.";

/// Why a follow-up was discarded once a limit the root shares was used up.
pub(super) fn budget_skip_notice(exceeded: otto_core::agent::budget::Exceeded) -> String {
    let budget = match exceeded.kind {
        otto_core::agent::budget::BudgetKind::Duration => "active-time budget",
        _ => "token budget",
    };
    format!(
        "An agent-scheduled follow-up was not run because the {budget} of this turn is used up ({exceeded}). Send a new message to continue."
    )
}

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
            Some(otto_session::origin::Origin::terminal()),
            emit,
            cancel,
        )
        .await
    }

    /// Validates typed attachments before a draft is cleared. Images are foreground-only.
    pub fn validate_images(&self, images: &[otto_core::model::Block]) -> Result<(), AgentError> {
        let runner = self.runner().map_err(AgentError::Other)?;
        if !images.is_empty()
            && (runner.task_control().is_active() || self.pending_approval().is_some())
        {
            return Err(AgentError::Other("Images cannot be queued or used as steering; wait for the active turn and approvals to finish, then submit again.".into()));
        }
        runner.validate_images(images)
    }

    /// Runs an interactive foreground user turn, preserving attachment order.
    pub async fn prompt_with_images(
        &self,
        text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        images: Vec<otto_core::model::Block>,
    ) -> Result<(), AgentError> {
        self.validate_images(&images)?;
        self.prompt_with_images_system_context(
            text,
            "",
            Some(otto_session::origin::Origin::terminal()),
            emit,
            cancel,
            images,
        )
        .await
    }

    pub(crate) async fn prompt_with_system_context(
        &self,
        text: &str,
        system_context: &str,
        origin: Option<otto_session::origin::Origin>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        self.prompt_with_images_system_context(
            text,
            system_context,
            origin,
            emit,
            cancel,
            Vec::new(),
        )
        .await
    }

    async fn prompt_with_images_system_context(
        &self,
        text: &str,
        system_context: &str,
        origin: Option<otto_session::origin::Origin>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        images: Vec<otto_core::model::Block>,
    ) -> Result<(), AgentError> {
        self.validate_images(&images)?;
        let _admission = self.begin_operation().map_err(AgentError::Other)?;
        let runner = self.runner().map_err(AgentError::Other)?;
        // A user-origin turn is a root turn: it starts the budget that its
        // follow-ups, children and later wake turns share.
        let budget = runner.begin_root_turn();
        let _presentation = self.begin_user_turn(&runner, origin).await;
        runner.record_user_authorization(text).await?;
        let task_run = runner
            .task_control()
            .begin(&runner.session().header().id, &runner.redact_text(text))
            .map_err(AgentError::Other)?;
        self.record_unrun_followups(&runner, task_run.discarded(), &mut *emit)
            .await?;
        if images.is_empty() {
            self.run_prompt_step(&runner, text, system_context, &budget, &mut *emit, cancel)
                .await?;
        } else {
            runner
                .run_with_images_system_context(
                    text,
                    images,
                    system_context,
                    BudgetScope::Shared(budget.clone()),
                    &mut *emit,
                    cancel,
                )
                .await?;
        }
        self.run_followups_held(&runner, &task_run, system_context, &mut *emit, cancel)
            .await
    }

    /// One prompt step inside an already-admitted logical turn. Approval
    /// retries call this without opening a second follow-up scope, and with
    /// the same `budget`, so the turn keeps its token total and step count.
    pub(crate) async fn run_prompt_step(
        &self,
        runner: &Runner,
        text: &str,
        system_context: &str,
        budget: &TurnBudget,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let reporter = self.herdr_reporter();
        let session = match &reporter {
            Some(_) => self.herdr_session(),
            None => otto_platform::herdr::SessionRef {
                id: String::new(),
                path: String::new(),
            },
        };
        runner
            .run_with_system_context(
                text,
                system_context,
                BudgetScope::Shared(budget.clone()),
                &mut otto_platform::herdr::observing(emit, reporter.as_deref(), &session),
                cancel,
            )
            .await
    }

    pub(super) async fn run_followups_held(
        &self,
        runner: &Runner,
        task_run: &crate::runtime::task_control::Run,
        system_context: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        while let Some(followup) = task_run.next_followup() {
            if self.pending_approval().is_some() {
                task_run.clear_followups();
                self.record_skipped_followup(runner, APPROVAL_SKIP_NOTICE, emit)
                    .await?;
                return Ok(());
            }
            if otto_core::session::Session::has_input(runner.session(), &followup.input_id) {
                // Its step ran before an interruption kept it journaled.
                task_run.followup_done(&followup);
                continue;
            }
            // A follow-up counts its own steps and shares the root's tokens. With
            // the tokens used up it is not started; a follow-up lives only as
            // long as the turn that queued it, so it is discarded with a notice.
            let budget = match runner.followup_budget() {
                Ok(budget) => budget,
                Err(exceeded) => {
                    task_run.clear_followups();
                    self.record_skipped_followup(runner, &budget_skip_notice(exceeded), emit)
                        .await?;
                    return Ok(());
                }
            };
            task_run.set_current_task(&followup.text);
            runner.set_next_input_id(Some(followup.input_id.clone()));
            self.run_followup_step(
                runner,
                &followup.text,
                system_context,
                &budget,
                &mut *emit,
                cancel,
            )
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
            input_id: String::new(),
            input_provenance: None,
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
        origin: Option<otto_session::origin::Origin>,
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
        text: &str,
        emit: EventSink<'_>,
    ) -> Result<(), AgentError> {
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
            input_id: String::new(),
            input_provenance: None,
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
        budget: &TurnBudget,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        if cancel.is_cancelled() {
            return Ok(());
        }
        emit(otto_core::agent::Event::TextDelta {
            text: "\n\n".into(),
        });
        runner
            .run_followup_with_system_context(
                text,
                system_context,
                BudgetScope::Shared(budget.clone()),
                emit,
                cancel,
            )
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
        runner.compact(focus, emit, cancel).await
    }
}
