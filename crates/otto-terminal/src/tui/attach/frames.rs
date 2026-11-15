//! Input, frame, status and action handling on the attached UI.

use super::*;

impl<'a> Ui<'a> {
    pub(super) fn on_input<B: TerminalBackend>(
        &mut self,
        terminal: &mut Terminal<B>,
        event: TuiEvent,
    ) -> Result<(), ReplError> {
        let entries = self.app.entries.len();
        if let Some(action) = super::super::apply_input(
            &mut self.app,
            terminal,
            event,
            self.cancel,
            &Backend::Attach(self.remote),
        )? {
            match action {
                Action::CancelTurn => self.cancel_followed(),
                _ => self.actions.push_back(action),
            }
        }
        if self.app.entries.len() != entries {
            self.transcript = self.transcript.wrapping_add(1);
        }
        Ok(())
    }

    /// Esc: cancels the followed turn. A `/compact` has no turn to cancel.
    fn cancel_followed(&mut self) {
        if self.transport.defer_cancel() {
            return;
        }
        let Some(turn) = self.followed.as_ref().map(|f| f.stream.turn_id.clone()) else {
            return;
        };
        self.cancel_turn(turn);
    }

    pub(super) fn cancel_turn(&mut self, turn: String) {
        let remote = self.remote;
        let session = remote.session();
        self.start_cancel(Box::pin(async move {
            within(remote.client.cancel_turn(&session, &turn)).await?;
            Ok(TransportResult::Cancel)
        }));
    }

    pub(super) fn on_frame(&mut self, result: Result<Option<WireEvent>, Error>) {
        self.transcript = self.transcript.wrapping_add(1);
        match result {
            Ok(Some(event)) if event.event_type == TURN_END => self.end_turn(&event),
            Ok(Some(event)) => {
                if self.app.apply_event(&event)
                    && let Some(followed) = &mut self.followed
                {
                    followed.error_rendered = true;
                }
            }
            Ok(None) | Err(_) => self.disconnect(),
        }
    }

    fn end_turn(&mut self, event: &WireEvent) {
        let Some(followed) = self.followed.take() else {
            return;
        };
        let id = followed.stream.turn_id;
        self.remote.finish(&id);
        self.last_done = Some(id);
        let completion = match event.status.as_str() {
            "error" => otto_platform::program_status::Completion::Failed,
            "canceled" => otto_platform::program_status::Completion::Canceled,
            _ => otto_platform::program_status::Completion::Succeeded,
        };
        self.app.finish_turn(completion);
        match event.status.as_str() {
            "error" if !followed.error_rendered => {
                self.app.push_system(if event.error.is_empty() {
                    "turn failed"
                } else {
                    &event.error
                })
            }
            "canceled" => self.app.push_system("turn canceled"),
            _ => {}
        }
        self.needs_sync = true;
        if self.app.queued_input.is_some()
            && let Some(action) = self
                .app
                .submit_queued_input(&Backend::Attach(self.remote), self.cancel)
        {
            self.actions.push_back(action);
        }
    }

    pub(super) fn on_status(&mut self, result: Result<Option<Vec<StatusRow>>, Error>) {
        match result {
            Ok(Some(rows)) => self.rows = rows,
            Ok(None) | Err(_) => self.disconnect(),
        }
    }

    pub(super) fn on_compact(&mut self, result: Result<Option<WireCompaction>, Error>) {
        self.compacting = None;
        let completion = match &result {
            Ok(Some(_)) => otto_platform::program_status::Completion::Succeeded,
            Ok(None) => otto_platform::program_status::Completion::Canceled,
            Err(_) => otto_platform::program_status::Completion::Failed,
        };
        self.app.finish_turn(completion);
        self.transcript = self.transcript.wrapping_add(1);
        match result {
            Ok(Some(compaction)) => self.app.push_system(compaction_line(&compaction)),
            Ok(None) => self.app.push_system("/compact: canceled"),
            Err(error) => return self.report("/compact", error),
        }
        self.needs_sync = true;
    }

    /// Runs the queued actions; `true` when one asked to exit.
    pub(super) fn run_actions(&mut self) -> bool {
        while let Some(action) = self.actions.pop_front() {
            if matches!(action, Action::Exit) {
                return true;
            }
            if self.app.busy() && matches!(action, Action::Compact(_)) {
                self.actions.push_front(action);
                break;
            }
            if !self.connection.is_connected() {
                self.app.push_system(DISCONNECTED);
                continue;
            }
            if self.transport.is_owned() {
                self.actions.push_front(action);
                break;
            }
            self.act(action);
        }
        false
    }

    fn act(&mut self, action: Action) {
        self.transcript = self.transcript.wrapping_add(1);
        let remote = self.remote;
        let session = remote.session();
        match action {
            Action::LocalPrompt { .. } => self.app.push_system("Local submission cannot execute in an attached session; reconnect to its original session and use /inputs"),
            Action::Prompt(line) => {
                if self.followed.is_none() && self.compacting.is_none() {
                    self.app.start_turn();
                }
                self.start_send(
                    Box::pin(async move {
                        let mut reply = None;
                        if let Some(control) =
                            within(remote.client.approval_message(&session, &line)).await?
                        {
                            reply = Some(control.text);
                            if !control.queued {
                                return Ok(TransportResult::Prompt {
                                    reply,
                                    stream: None,
                                });
                            }
                        }
                        let stream =
                            within(remote.client.start_turn(&session, &line, None)).await?;
                        Ok(TransportResult::Prompt {
                            reply,
                            stream: Some(stream),
                        })
                    }),
                );
            }
            Action::Compact(focus) => {
                if self.compacting.is_some() {
                    self.app.push_system("/compact: already running");
                    return;
                }
                self.app.start_turn();
                let client = &remote.client;
                self.compacting = Some(Box::pin(
                    async move { client.compact(&session, &focus).await },
                ));
            }
            Action::CancelTurn => self.cancel_followed(),
            Action::Exit => {}
            Action::Login(_) | Action::McpLogin(_) => {
                self.app.push_system("not available with --attach")
            }
        }
    }
}
