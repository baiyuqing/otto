//! Per-session prompt slot: admission, wake claims and the bounded queue.

use super::*;

impl PromptSlot {
    pub(super) fn signal_released(&self) {
        self.released.send_modify(|count| *count += 1);
    }

    /// Claims the slot for a wake turn when nothing else holds or awaits it.
    pub(super) fn begin_wake(&self, parent: &CancellationToken) -> Option<CancellationToken> {
        let mut state = self.state.lock().expect("prompt slot");
        if state.active.is_some()
            || state.draining
            || !state.followups.is_empty()
            || state.waiting.is_some()
        {
            return None;
        }
        let token = parent.child_token();
        state.active = Some(token.clone());
        state.wake = true;
        Some(token)
    }

    /// Registers an ordinary prompt to run after the wake turn that holds the
    /// slot. The caller calls this after `begin` failed under a separate lock,
    /// so a slot that is free again (the wake turn ended in between) is also
    /// accepted. `None` when the slot is held by anything else or already
    /// awaited.
    pub(super) fn wait_for_wake(&self, parent: &CancellationToken) -> Option<CancellationToken> {
        let mut state = self.state.lock().expect("prompt slot");
        let free = state.active.is_none() && !state.draining && state.followups.is_empty();
        if !(state.wake || free) || state.waiting.is_some() {
            return None;
        }
        let token = parent.child_token();
        state.waiting = Some(token.clone());
        Some(token)
    }

    /// Waits until the slot is free and installs the waiting prompt's token as
    /// active. `false` when the wait was cancelled.
    pub(super) async fn claim_after_wake(&self, token: &CancellationToken) -> bool {
        let mut released = self.released.subscribe();
        loop {
            {
                let mut state = self.state.lock().expect("prompt slot");
                if token.is_cancelled() {
                    state.waiting = None;
                    return false;
                }
                if state.active.is_none() && !state.draining && state.followups.is_empty() {
                    state.waiting = None;
                    state.active = Some(token.clone());
                    return true;
                }
            }
            tokio::select! {
                biased;
                () = token.cancelled() => {}
                _ = released.changed() => {}
            }
        }
    }

    /// Installs a normal prompt only when no prompt or explicit queue is active.
    pub(super) fn begin(&self, parent: &CancellationToken) -> Option<CancellationToken> {
        let mut state = self.state.lock().expect("prompt slot");
        if state.active.is_some() || state.draining || !state.followups.is_empty() {
            return None;
        }
        let token = parent.child_token();
        state.active = Some(token.clone());
        Some(token)
    }

    pub(super) fn end(&self) {
        {
            let mut state = self.state.lock().expect("prompt slot");
            state.active = None;
            state.wake = false;
        }
        self.changed.notify_one();
        self.signal_released();
    }

    pub(super) fn cancel(&self) -> usize {
        let mut state = self.state.lock().expect("prompt slot");
        let dropped = state.queue.len();
        state.queue.clear();
        if let Some(token) = state.active.as_ref() {
            token.cancel();
        }
        if let Some(token) = state.waiting.as_ref() {
            token.cancel();
        }
        for token in state.followups.values() {
            token.cancel();
        }
        self.changed.notify_one();
        dropped
    }

    pub(super) fn active_cancel(&self) -> CancellationToken {
        self.state
            .lock()
            .expect("prompt slot")
            .active
            .clone()
            .unwrap_or_default()
    }

    pub(super) fn enqueue_with_provenance(
        &self,
        text: String,
        (channel, origin): (Option<Channel>, Option<Origin>),
        input_id: Option<String>,
        input_provenance: Option<otto_core::model::InputProvenance>,
    ) -> Result<bool, &'static str> {
        let mut state = self.state.lock().expect("prompt slot");
        if state.queue.len() >= ACP_QUEUE_LIMIT {
            return Err("queue is full (16 pending prompts)");
        }
        // A repeated restore of the same saved prompt (two `session/load`
        // requests) must not queue it twice. A copy already running is
        // skipped by the drain once its input id is in the session.
        if let Some(input_id) = &input_id
            && state
                .queue
                .iter()
                .any(|queued| queued.input_id.as_ref() == Some(input_id))
        {
            return Ok(false);
        }
        state.queue.push_back(QueuedPrompt {
            text,
            channel,
            origin,
            input_id,
            input_provenance,
        });
        let start_worker = !state.draining;
        state.draining = true;
        drop(state);
        self.changed.notify_one();
        Ok(start_worker)
    }

    pub(super) async fn drain_queue(
        self: Arc<Self>,
        connection: Arc<Connection>,
        session_id: String,
    ) {
        let stop = connection.task_stop.child_token();
        loop {
            let changed = self.changed.notified();
            let next = {
                let mut state = self.state.lock().expect("prompt slot");
                if stop.is_cancelled() {
                    state.queue.clear();
                    state.draining = false;
                    drop(state);
                    self.signal_released();
                    return;
                }
                if state.active.is_some() {
                    None
                } else if let Some(queued) = state.queue.pop_front() {
                    let token = stop.child_token();
                    state.active = Some(token.clone());
                    Some((queued, token))
                } else {
                    state.draining = false;
                    drop(state);
                    self.signal_released();
                    return;
                }
            };
            let Some((queued, token)) = next else {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => continue,
                    () = changed => continue,
                }
            };
            let controller = connection.local_controller(&session_id);
            // Same check as `Connection::prompt`: an input id already in the
            // transcript was delivered, so the saved copy is dropped unrun.
            if let (Some(controller), Some(input_id)) = (&controller, &queued.input_id)
                && controller.has_input(input_id)
            {
                if let Some(journal) = controller.turn_queue() {
                    journal.forget(input_id);
                }
                self.end();
                continue;
            }
            if let Some(controller) = &controller {
                controller.set_next_input_id(queued.input_id.clone());
                controller.set_next_input_provenance(queued.input_provenance);
            }
            let reply = connection
                .queued_prompt(
                    &session_id,
                    queued.text,
                    queued.channel,
                    queued.origin,
                    None,
                    &token,
                )
                .await;
            if let Some(controller) = &controller {
                controller.set_next_input_id(None);
                // The prompt ran: its saved copy is no longer waiting.
                if let (Some(input_id), Some(journal)) = (&queued.input_id, controller.turn_queue())
                {
                    journal.forget(input_id);
                }
            }
            if let Some(notice) = queued_outcome_notice(&reply) {
                connection.say(&session_id, &notice);
            }
            self.end();
        }
    }

    pub(super) fn begin_followup(
        &self,
        parent: &CancellationToken,
    ) -> Result<(u64, CancellationToken), &'static str> {
        let mut state = self.state.lock().expect("prompt slot");
        let limit = ACP_QUEUE_LIMIT + if state.active.is_none() { 1 } else { 0 };
        if state.followups.len() >= limit {
            return Err("queue is full (16 pending prompts)");
        }
        state.next_followup = state.next_followup.wrapping_add(1);
        if state.next_followup == 1 {
            state.next_followup_admission = 1;
        }
        let id = state.next_followup;
        let token = parent.child_token();
        state.followups.insert(id, token.clone());
        Ok((id, token))
    }

    pub(super) async fn wait_followup_admission(&self, id: u64) {
        loop {
            let mut changed = Box::pin(self.followup_changed.notified());
            changed.as_mut().enable();
            if self
                .state
                .lock()
                .expect("prompt slot")
                .next_followup_admission
                == id
            {
                return;
            }
            changed.await;
        }
    }

    pub(super) fn finish_followup_admission(&self, id: u64) {
        let mut state = self.state.lock().expect("prompt slot");
        if state.next_followup_admission == id {
            state.next_followup_admission = state.next_followup_admission.wrapping_add(1);
        }
        drop(state);
        self.followup_changed.notify_waiters();
    }

    pub(super) fn end_followup(&self, id: u64) {
        self.state
            .lock()
            .expect("prompt slot")
            .followups
            .remove(&id);
        self.signal_released();
    }
}
