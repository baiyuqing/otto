//! Task view, steering, active-input admission, control messages and wake claims.

use super::*;

impl Controller {
    /// The current runner's task registry, or `None` when it tracks none or the
    /// controller is closed.
    pub fn tasks(&self) -> Option<Arc<dyn TaskView>> {
        let state = self.lock();
        if state.closed {
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
            .steer(&session_id, ticket, text, input_id)
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
        match runner
            .task_control()
            .steer(&session_id, ticket, text, input_id)
        {
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
        if let Some(reply) = self
            .approval_message_with_channel(text, channel, cancel)
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
            () = self.auto_cancel.cancelled() => return Err(CLOSED.into()),
        };
        if self.pending_approval().is_some() {
            drop(_serial);
            return self
                .approval_message_with_channel(text, channel, cancel)
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
        task_control::control_message(self, &runner, text, channel, ticket, cancel).await
    }

    /// The current runner's timer registry, or `None` when the timer tools
    /// are not registered or the controller is closed. `/timers` lists it and
    /// [`Self::archive_current_session`] clears it.
    pub fn reminders(&self) -> Option<Arc<Reminders>> {
        let state = self.lock();
        if state.closed {
            return None;
        }
        state.current.as_ref()?.runner.reminders.clone()
    }

    /// Claims a turn only when the runner has pending inbox messages.
    pub fn prepare_wake(&self) -> Result<Option<WakeOperation<'_>>, String> {
        let pending = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            if state.busy {
                return Err(PROMPT_ACTIVE.to_string());
            }
            let Some(current) = state.current.as_ref() else {
                return Err(CLOSED.to_string());
            };
            current.runner.mailbox().len()
        };
        if pending == 0 {
            return Ok(None);
        }
        let admission = self.begin_operation()?;
        self.lock().wake_unstarted = true;
        Ok(Some(WakeOperation::new(self, admission)))
    }
}
