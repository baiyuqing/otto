//! Task view, steering, active-input admission, control messages and wake claims.

use super::*;

/// Why [`Controller::interrupt_step`] did not interrupt the running step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptRefusal {
    /// An approval is waiting; guidance is not delivered until it is decided.
    ApprovalPending,
    /// The active run has no queued guidance, or no run is active.
    NoGuidance,
    /// The controller is closed or has no runner.
    Unavailable(String),
}

impl InterruptRefusal {
    /// The stable machine-readable code the HTTP error carries.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ApprovalPending => "approval_pending",
            Self::NoGuidance => "no_queued_guidance",
            Self::Unavailable(_) => "task_control_unavailable",
        }
    }

    /// The user-facing reason.
    pub fn message(&self) -> String {
        match self {
            Self::ApprovalPending => {
                "The task is waiting for approval; Ctrl+C cancels the turn.".into()
            }
            Self::NoGuidance => "No queued guidance; Esc again cancels the turn.".into(),
            Self::Unavailable(reason) => reason.clone(),
        }
    }
}

impl std::fmt::Display for InterruptRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl Controller {
    /// The current runner's task registry, or `None` when it tracks none or the
    /// controller is closed.
    pub fn tasks(&self) -> Option<Arc<dyn TaskView>> {
        let state = self.lock();
        if state.is_closed() {
            return None;
        }
        tasks::task_view(state.current.as_ref()?.runner.as_ref())
    }

    /// Accepts guidance for the exact active run, at its next safe provider
    /// boundary. The acceptance is transient and never wakes a later run.
    pub fn steer(&self, text: &str) -> Result<otto_core::agent::steering::Message, String> {
        self.steer_input(text, None)
    }

    /// [`Self::steer`] with the client's `input_id`: a retry while the
    /// guidance is queued returns the same message, and one after it was
    /// delivered fails with [`task_control::DUPLICATE_INPUT`].
    pub fn steer_input(
        &self,
        text: &str,
        input_id: Option<&str>,
    ) -> Result<otto_core::agent::steering::Message, String> {
        self.steer_input_with_provenance(text, input_id, None)
    }

    pub fn steer_input_with_provenance(
        &self,
        text: &str,
        input_id: Option<&str>,
        provenance: Option<otto_core::model::InputProvenance>,
    ) -> Result<otto_core::agent::steering::Message, String> {
        let text = task_control::checked_text(text)?;
        if input_id.is_some_and(|id| self.has_input(id)) {
            return Err(task_control::DUPLICATE_INPUT.into());
        }
        if self.pending_approval().is_some() {
            return Err("the task is waiting for approval; guidance is not accepted yet".into());
        }
        let runner = self.runner()?;
        let Some(ticket) = runner.steering().ticket() else {
            return Err("there is no active task accepting guidance".to_string());
        };
        let session_id = runner.session().header().id.clone();
        runner
            .task_control()
            .steer_with_provenance(&session_id, ticket, text, input_id, provenance)
    }

    /// Lists the transient steering lifecycle for the current active run.
    pub fn steering_messages(&self) -> Result<Vec<otto_core::agent::steering::Message>, String> {
        Ok(self.runner()?.steering().messages())
    }

    /// Withdraws one queued steering message before its safe-point delivery.
    pub fn withdraw_steering(
        &self,
        id: u64,
    ) -> Result<otto_core::agent::steering::Message, String> {
        self.runner()?
            .steering()
            .withdraw(id)
            .ok_or_else(|| "steering message is not queued for the active task".to_string())
    }

    /// Stops the running provider call or tool call so the queued guidance is
    /// delivered inside the current turn. Returns the number of queued
    /// messages that will be delivered.
    pub fn interrupt_step(&self) -> Result<usize, InterruptRefusal> {
        if self.pending_approval().is_some() {
            return Err(InterruptRefusal::ApprovalPending);
        }
        let runner = self.runner().map_err(InterruptRefusal::Unavailable)?;
        runner
            .steering()
            .interrupt_step()
            .map_err(|_| InterruptRefusal::NoGuidance)
    }

    /// Deterministically admits ordinary human text while this session is
    /// active. It never queues a later turn: callers explicitly choose that
    /// separate operation after receiving [`ActiveInputAdmission::Idle`].
    pub fn admit_active_input(&self, text: &str) -> Result<ActiveInputAdmission, String> {
        self.admit_active_input_with(text, None)
    }

    /// [`Self::admit_active_input`] with the client's `input_id`; see
    /// [`Self::steer_input`].
    pub fn admit_active_input_with(
        &self,
        text: &str,
        input_id: Option<&str>,
    ) -> Result<ActiveInputAdmission, String> {
        self.admit_active_input_with_provenance(text, input_id, None)
    }

    pub fn admit_active_input_with_provenance(
        &self,
        text: &str,
        input_id: Option<&str>,
        provenance: Option<otto_core::model::InputProvenance>,
    ) -> Result<ActiveInputAdmission, String> {
        let text = task_control::checked_text(text)?;
        if input_id.is_some_and(|id| self.has_input(id)) {
            return Err(task_control::DUPLICATE_INPUT.into());
        }
        if self.pending_approval().is_some() {
            return Ok(ActiveInputAdmission::ApprovalPending);
        }
        let runner = self.runner()?;
        let Some(ticket) = runner.steering().ticket() else {
            return Ok(ActiveInputAdmission::Idle);
        };
        let session_id = runner.session().header().id.clone();
        match runner.task_control().steer_with_provenance(
            &session_id,
            ticket,
            text,
            input_id,
            provenance,
        ) {
            Ok(_) => {
                runner.queue_user_authorization(text)?;
                Ok(ActiveInputAdmission::Steered)
            }
            // The active run can close after observing its ticket. This text
            // was not accepted and must not be silently queued elsewhere.
            Err(error) if error.contains("active task") => Ok(ActiveInputAdmission::Idle),
            Err(error) => Err(error),
        }
    }

    /// Routes natural input during an active turn through a restricted control
    /// dialogue. `None` means the caller should handle it as a normal prompt.
    pub async fn control_message_with_channel(
        &self,
        text: &str,
        channel: Option<Channel>,
        cancel: &CancellationToken,
    ) -> Result<Option<approval_control::ControlReply>, String> {
        self.control_message_with_provenance(text, channel, None, None, cancel)
            .await
    }

    /// Restricted routing with the actual ingress identity, independent of generated replies.
    pub async fn control_message_with_provenance(
        &self,
        text: &str,
        channel: Option<Channel>,
        input_id: Option<&str>,
        provenance: Option<otto_core::model::InputProvenance>,
        cancel: &CancellationToken,
    ) -> Result<Option<approval_control::ControlReply>, String> {
        let input_id = match input_id {
            Some(id) if !otto_core::model::is_input_id(id) => return Err("invalid input_id".into()),
            Some(id) => id.to_owned(),
            None => crate::runtime::builder::random_id().map_err(|error| error.to_string())?,
        };
        if let Some(reply) = self
            .approval_message_with_provenance(text, channel, Some(&input_id), provenance, cancel)
            .await?
        {
            return Ok(Some(reply));
        }
        let runner = self.runner()?;
        let Some(ticket) = runner.steering().ticket() else {
            return Ok(None);
        };
        let _serial = tokio::select! {
            guard = self.approval_control.lock() => guard,
            () = cancel.cancelled() => return Err("context canceled".into()),
            () = self.close_cancel.cancelled() => return Err(CLOSED.into()),
        };
        if self.pending_approval().is_some() {
            drop(_serial);
            return self
                .approval_message_with_provenance(
                    text,
                    channel,
                    Some(&input_id),
                    provenance,
                    cancel,
                )
                .await;
        }
        let is_current = self
            .lock()
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.runner, &runner));
        if !is_current || runner.steering().ticket() != Some(ticket) {
            return Ok(None);
        }
        runner
            .record_user_authorization(text)
            .await
            .map_err(|error| error.to_string())?;
        task_control::control_message(
            self,
            &runner,
            task_control::ControlMessage {
                text,
                channel,
                ticket,
                input_id: Some(&input_id),
                provenance,
                cancel,
            },
        )
        .await
    }

    /// The current runner's timer registry, or `None` when the timer tools
    /// are not registered or the controller is closed. `/timers` lists it and
    /// [`Self::archive_current_session`] clears it.
    pub fn reminders(&self) -> Option<Arc<Reminders>> {
        let state = self.lock();
        if state.is_closed() {
            return None;
        }
        state.current.as_ref()?.runner.reminders().cloned()
    }

    /// Claims a turn only when the runner has pending inbox messages.
    pub fn prepare_wake(&self) -> Result<Option<WakeOperation<'_>>, String> {
        let generation = {
            let mut state = self.lock();
            if state.is_closed() {
                return Err(CLOSED.to_string());
            }
            if state.is_busy() {
                return Err(PROMPT_ACTIVE.to_string());
            }
            let Some(current) = state.current.as_ref() else {
                return Err(CLOSED.to_string());
            };
            if current.runner.mailbox().is_empty() {
                return Ok(None);
            }
            state.reserve_wake()?
        };
        let admission = Admission {
            controller: self,
            generation,
        };
        Ok(Some(WakeOperation::new(self, admission)))
    }
}
