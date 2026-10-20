//! Prompt turns, sub-agent wake turns and interactive approval prompts.

use super::*;

impl<'a> Repl<'a> {
    /// One empty-text turn delivering the pending sub-agent notifications, and
    /// whether it ran. The leading newline keeps the output off the prompt
    /// marker.
    pub(super) async fn wake(&mut self, cancel: &CancellationToken) -> Result<bool, Error> {
        let Self {
            controller,
            stdout,
            stderr,
            program_status,
        } = self;
        let wake = controller.prepare_wake().map_err(|message| Error::Turn {
            fatal: false,
            message,
        })?;
        let Some(wake) = wake else { return Ok(false) };
        if let Some(reporter) = program_status {
            reporter.report(&mut **stdout, crate::program_status::State::Working);
        }
        let _ = writeln!(stdout);
        let turn = cancel.child_token();
        let mut error_rendered = false;
        let result = {
            let mut sink = |event: Event| {
                if let Some(reporter) = program_status.as_ref() {
                    reporter.observe(&mut **stdout, &event);
                }
                if render_event(&mut **stdout, &mut **stderr, &event) {
                    error_rendered = true;
                }
            };
            wake.run(&mut sink, &turn).await
        };
        if let Some(reporter) = program_status {
            let canceled = cancel.is_cancelled()
                || result.as_ref().err().is_some_and(AgentError::is_cancelled);
            let state = crate::program_status::State::finished(result.is_ok(), canceled);
            reporter.report(&mut **stdout, state);
        };
        if let Err(error) = &result
            && !error_rendered
        {
            let _ = writeln!(stderr, "{error}");
        }
        let _ = writeln!(stdout);
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match result {
            Ok(()) => Ok(true),
            Err(error) => Err(Error::Turn {
                fatal: is_fatal_persistence(&error),
                message: error.to_string(),
            }),
        }
    }

    /// Waits out every non-final task, then wakes until nothing is pending.
    /// Each wait and wake is bounded by `cancel`.
    pub(super) async fn drain_tasks(&mut self, cancel: &CancellationToken) -> Result<(), Error> {
        let Some(tasks) = self.controller.subagent_tasks() else {
            return Ok(());
        };
        loop {
            for task in tasks.list() {
                if task.is_final() {
                    continue;
                }
                match tasks.wait(&task.id, cancel).await {
                    Ok(_) => {}
                    Err(TaskError::Canceled) => return Err(Error::Cancelled),
                    Err(error) => {
                        return Err(Error::Turn {
                            fatal: false,
                            message: error.to_string(),
                        });
                    }
                }
            }
            if !self.wake(cancel).await? {
                return Ok(());
            }
        }
    }

    pub(super) async fn prompt(
        &mut self,
        line: &str,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let Self {
            controller,
            stdout,
            stderr,
            program_status,
        } = self;
        if let Some(reporter) = program_status {
            reporter.report(&mut **stdout, crate::program_status::State::Working);
        }
        let turn = cancel.child_token();
        let mut error_rendered = false;
        let result = {
            let mut sink = |event: Event| {
                if let Some(reporter) = program_status.as_ref() {
                    reporter.observe(&mut **stdout, &event);
                }
                if render_event(&mut **stdout, &mut **stderr, &event) {
                    error_rendered = true;
                }
            };
            controller.prompt(line, &mut sink, &turn).await
        };
        if let Some(reporter) = program_status {
            let canceled = cancel.is_cancelled()
                || result.as_ref().err().is_some_and(AgentError::is_cancelled);
            let state = crate::program_status::State::finished(result.is_ok(), canceled);
            reporter.report(&mut **stdout, state);
        }
        if let Err(error) = &result
            && !error_rendered
        {
            let _ = writeln!(stderr, "{error}");
        }
        let _ = writeln!(stdout);
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(Error::Turn {
                fatal: is_fatal_persistence(&error),
                message: error.to_string(),
            }),
        }
    }

    /// Keep reading the trusted terminal while a prepared call waits. Ordinary
    /// work stays queued; approving never sends a replacement model prompt.
    pub(super) async fn interactive_prompt(
        &mut self,
        line: &str,
        cancel: &CancellationToken,
        lines: &mut tokio::sync::mpsc::Receiver<Result<String, Error>>,
        queue: &mut std::collections::VecDeque<String>,
    ) -> Result<(), Error> {
        let controller = self.controller;
        if let Some(reporter) = &self.program_status {
            reporter.report(&mut *self.stdout, crate::program_status::State::Working);
        }
        let turn = cancel.child_token();
        let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = |event| {
            let _ = send.send(event);
        };
        let run = controller.prompt(line, &mut sink, &turn);
        tokio::pin!(run);
        let mut waiting = false;
        let mut input_closed = false;
        let mut input_error = None;
        let mut error_rendered = false;
        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                Some(event) = events.recv() => {
                    if let Event::ApprovalChanged { request } = &event {
                        waiting = request.blocking && request.state == otto_core::approval::State::Pending;
                    }
                    if let Some(reporter) = &self.program_status {
                        reporter.observe(&mut *self.stdout, &event);
                    }
                    error_rendered |= render_event(&mut *self.stdout, &mut *self.stderr, &event);
                    let _ = self.stdout.flush();
                }
                input = lines.recv(), if waiting && !input_closed => {
                    let Some(input) = input else {
                        input_closed = true;
                        turn.cancel();
                        continue;
                    };
                    let input = match input {
                        Ok(input) => input,
                        Err(error) => {
                            input_error = Some(error);
                            input_closed = true;
                            turn.cancel();
                            continue;
                        }
                    };
                    let trimmed = input.trim_end();
                    if let Some(args) = trimmed.strip_prefix("/auto").filter(|args|args.is_empty() || args.starts_with(char::is_whitespace)) {
                        let line = self.controller.auto_mode_command(args).unwrap_or_else(|e|e);
                        let _ = writeln!(self.stdout,"{line}");
                        continue;
                    }
                    if let Some(args) = trimmed.strip_prefix("/permissions").filter(|args|args.is_empty() || args.starts_with(char::is_whitespace)) {
                        let line = self.controller.permissions_command(args).unwrap_or_else(|e|e);
                        let _ = writeln!(self.stdout,"{line}");
                        continue;
                    }
                    if trimmed == "/exit" || trimmed == "/stop" {
                        turn.cancel();
                        if trimmed == "/exit" { queue.push_front(input); }
                        continue;
                    }
                    let mut words = trimmed.splitn(3, char::is_whitespace);
                    let command = words.next().unwrap_or("");
                    let id = words.next().unwrap_or("");
                    let feedback = words.next().unwrap_or("").trim();
                    if matches!(command, "/approve" | "/deny" | "/withdraw") {
                        let scope = controller.approval_scope();
                        let service = controller.approval_service();
                        let decision = service.get(id, &scope.workspace, Some(&scope.session_id))
                            .and_then(|request| {
                                if command == "/withdraw" {
                                    service.withdraw(id, &request.scope, request.revision)
                                } else {
                                    service.decide(id, &request.scope, request.revision,
                                        command == "/approve", feedback,
                                        &crate::approval::Actor::local("repl"))
                                }
                            });
                        match decision {
                            Ok(request) => {
                                let _ = writeln!(self.stdout, "Approval {id}: {:?}.", request.state);
                            }
                            Err(error) => { let _ = writeln!(self.stderr, "{error}"); }
                        }
                    } else if !trimmed.is_empty() && !trimmed.starts_with('/') {
                        match controller.approval_message(trimmed, &turn).await {
                            Ok(Some(reply)) => {
                                let _ = writeln!(self.stdout, "{}", reply.text);
                                if reply.queued { queue.push_back(input); }
                            }
                            Ok(None) => queue.push_back(input),
                            Err(error) => { let _ = writeln!(self.stderr, "{error}"); }
                        }
                    } else if !trimmed.is_empty() {
                        queue.push_back(input);
                    }
                }
            }
        };
        while let Ok(event) = events.try_recv() {
            if let Some(reporter) = &self.program_status {
                reporter.observe(&mut *self.stdout, &event);
            }
            error_rendered |= render_event(&mut *self.stdout, &mut *self.stderr, &event);
        }
        if let Some(reporter) = &self.program_status {
            let canceled = cancel.is_cancelled()
                || result.as_ref().err().is_some_and(AgentError::is_cancelled);
            let state = crate::program_status::State::finished(result.is_ok(), canceled);
            reporter.report(&mut *self.stdout, state);
        }
        if let Err(error) = &result
            && !error_rendered
        {
            let _ = writeln!(self.stderr, "{error}");
        }
        let _ = writeln!(self.stdout);
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Some(error) = input_error {
            return Err(error);
        }
        result.map_err(|error| Error::Turn {
            fatal: is_fatal_persistence(&error),
            message: error.to_string(),
        })
    }
}
