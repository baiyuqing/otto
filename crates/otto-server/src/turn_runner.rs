//! Turn lifecycle on the server: wake loop, queued turns, approvals and turn start.

use super::*;

impl Server {
    /// Drains `session`'s pending sub-agent notifications for as long as it is
    /// open.
    ///
    /// It is the sole caller of [`Server::wake_turn`]: both a registry update
    /// signal and the end of any turn on `session` route through this one task,
    /// so every "is a notification pending and no turn active" check happens
    /// one at a time and no two wake turns can start for the same notification.
    /// On every update signal it also diffs the task list into the task
    /// metrics. It does nothing when the runner tracks no tasks.
    ///
    /// The turn is awaited here rather than started as its own task. A
    /// notification pushed while it runs bumps the registry's `watch` version,
    /// so the next `changed()` returns at once and the re-check happens anyway,
    /// with one fewer moving part.
    pub(super) fn start_wake_loop(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let Some(mailbox) = session.ctrl.notification_mailbox() else {
            return;
        };
        let mut updates = otto::runtime::workflow_notifications::updates(mailbox.as_ref());
        let tasks = session.ctrl.subagent_tasks();
        let server = Arc::clone(self);
        let session = Arc::clone(session);
        let handle = tokio::spawn(async move {
            let mut seen = std::collections::BTreeMap::new();
            let mut task_updates = tasks.as_ref().map(|tasks| tasks.updates());
            // Recheck pre-existing queued inputs on open.
            server.wake_turn(&session).await;
            loop {
                tokio::select! {
                    _ = async {
                        match task_updates.as_mut() {
                            Some(receiver) => {
                                if receiver.changed().await.is_err() {
                                    std::future::pending::<()>().await;
                                }
                            },
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        if let Some(view) = session.ctrl.tasks() { server.metrics.diff_tasks(&mut seen, &view.list()); }
                        server.status_changed.send_modify(|version| *version += 1);
                    }
                    changed = updates.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        if let Some(view) = session.ctrl.tasks() {
                            server.metrics.diff_tasks(&mut seen, &view.list());
                        }
                        server.status_changed.send_modify(|version| *version += 1);
                    }
                    () = session.turn_finished.notified() => {}
                    () = session.closed.cancelled() => return,
                    () = server.cancel.cancelled() => return,
                }
                server.wake_turn(&session).await;
            }
        });
        let mut loops = self
            .wake_loops
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // A deleted session's loop has already returned; this list does not
        // shrink on its own.
        loops.retain(|running| !running.is_finished());
        loops.push(handle);
    }

    pub(super) fn bump_status(&self) {
        self.status_changed.send_modify(|version| *version += 1);
    }

    /// Runs one task-triggered turn on `session` when a notification is pending
    /// and nothing else holds the session.
    ///
    /// The wake is prepared before the turn is published, so a no-op or busy
    /// admission cannot leave a phantom turn visible on `GET /v1/sessions`. A
    /// busy session, including one with queued turns, is left to the turn that
    /// holds it.
    pub(super) async fn wake_turn(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let prepared = {
            let mut state = session.lock();
            if state.busy_for_wake() {
                None
            } else {
                let trigger = if session.ctrl.resumes_interrupted_turn() {
                    TRIGGER_RECOVERY
                } else {
                    TRIGGER_TASK
                };
                match session.ctrl.prepare_wake() {
                    Ok(None) => None,
                    Ok(Some(wake)) => match new_id() {
                        Ok(id) => {
                            let turn = Arc::new(
                                Turn::new(id, trigger, self.cancel.child_token()).with_log(
                                    Arc::clone(&self.log),
                                    &session.ctrl.info().session_id,
                                    "",
                                ),
                            );
                            turn.start_running(session.ctrl.history().len());
                            state.turn = Some(Arc::clone(&turn));
                            Some(Ok((turn, wake, trigger)))
                        }
                        Err(error) => Some(Err(error)),
                    },
                    // A prompt or a close raced us; both are expected and
                    // ignored here.
                    Err(message) if message == app::PROMPT_ACTIVE || message == app::CLOSED => None,
                    Err(message) => Some(Err(message)),
                }
            }
        };
        let (turn, wake, trigger) = match prepared {
            // No wake turn to wait for: restored turns may run now.
            None => {
                self.release_restored(session);
                return;
            }
            Some(Err(message)) => {
                self.log.error("wake_turn_error", &[("error", message)]);
                return;
            }
            Some(Ok(prepared)) => prepared,
        };
        self.bump_status();

        self.metrics.turn_started();
        let mut fields = turn.log_ids();
        fields.push(("trigger", trigger.to_string()));
        if let Some(kind) = session.ctrl.subagent_tasks().and_then(|tasks| {
            tasks
                .notifications()
                .snapshot()
                .into_iter()
                .find_map(|item| item.kind)
        }) {
            fields.push(("inbox_kind", kind.as_str().to_string()));
        }
        self.log.info("turn_started", &fields);
        self.run_turn(session, &turn, Step::Wake(wake), trigger, None, None)
            .await;
        // A wake turn that ran for longer than the idle timeout restarts the
        // clock, so a reader can still replay it from the buffer.
        session.touch();
        // Restored turns, then queued turns that arrived during the wake
        // turn, start now.
        self.release_restored(session);
        self.start_next(session);
    }

    /// Rebuilds the turns saved in the session's turn queue before Otto
    /// stopped, keeping their turn ids. They wait in `restored` for the
    /// recovery wake turn the session's pending notice starts; with nothing
    /// pending they are released at once.
    pub(super) fn restore_queued_turns(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let prompts = session.ctrl.restorable_prompts();
        if prompts.is_empty() {
            return;
        }
        let session_id = session.ctrl.info().session_id;
        {
            let mut state = session.lock();
            for prompt in prompts {
                let id = if prompt.turn_id.is_empty() {
                    match new_id() {
                        Ok(id) => id,
                        Err(_) => continue,
                    }
                } else {
                    prompt.turn_id.clone()
                };
                let turn = Arc::new(
                    Turn::queued(id, TRIGGER_USER, self.cancel.child_token()).with_log(
                        Arc::clone(&self.log),
                        &session_id,
                        "",
                    ),
                );
                if state.inputs.len() >= MAX_REMEMBERED_INPUTS {
                    state.inputs.pop_front();
                }
                state
                    .inputs
                    .push_back((prompt.input_id.clone(), Arc::clone(&turn)));
                state.restored.push_back(QueuedTurn {
                    turn,
                    text: prompt.text,
                    image: None,
                    channel: prompt.channel,
                    origin: prompt.origin,
                    input_id: Some(prompt.input_id),
                });
            }
        }
        self.bump_status();
        let pending_wake = session
            .ctrl
            .notification_mailbox()
            .is_some_and(|mailbox| !mailbox.is_empty());
        if !pending_wake {
            self.release_restored(session);
        }
    }

    /// Moves the restored turns to the front of the queue, queues the notice
    /// the first of them delivers, and starts it when the session is free.
    pub(super) fn release_restored(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let released = {
            let mut state = session.lock();
            if state.restored.is_empty() {
                return;
            }
            let restored: Vec<QueuedTurn> = state.restored.drain(..).collect();
            let prompts: Vec<app::turn_queue::QueuedPrompt> = restored
                .iter()
                .map(|queued| app::turn_queue::QueuedPrompt {
                    input_id: queued.input_id.clone().unwrap_or_default(),
                    turn_id: queued.turn.id.clone(),
                    text: queued.text.clone(),
                    channel: queued.channel,
                    origin: queued.origin.clone(),
                    image_dropped: false,
                })
                .collect();
            for queued in restored.into_iter().rev() {
                state.queue.push_front(queued);
            }
            prompts
        };
        session.ctrl.announce_restored_prompts(&released);
        self.start_next(session);
    }

    /// Runs `first` and its approval retries as `turn`, then finishes it. The
    /// error text is redacted before it reaches the turn frame, the summary
    /// or the log.
    pub(super) fn run_turn<'a>(
        self: &'a Arc<Self>,
        session: &'a Arc<OpenSession>,
        turn: &'a Arc<Turn>,
        first: Step<'a>,
        trigger: &'static str,
        channel: Option<Channel>,
        origin: Option<otto::schedule::Origin>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        // Keep the full agent/review future out of the idle wake loop and
        // request-admission futures that only decide whether a turn can run.
        Box::pin(async move {
            let cancel = turn.cancel_token();
            let result = {
                let mut buffer = turn.emitter(&self.metrics);
                // The agent_error frame carries the same provider error that
                // turn_end carries redacted.
                let mut emit = |event: Event| match event {
                    Event::AgentError { message } => buffer(Event::AgentError {
                        message: session.ctrl.redact_error(&message),
                    }),
                    event @ Event::ApprovalChanged { .. } => {
                        buffer(event);
                        self.bump_status();
                    }
                    event => buffer(event),
                };
                let (server, open, running, token) = (&**self, &**session, &**turn, &cancel);
                session
                    .ctrl
                    .run_with_channel(first, channel, origin, &mut emit, &cancel, |request| {
                        server.wait_for_approval(open, running, token, request)
                    })
                    .await
            };
            let (error, canceled) = match result {
                Ok(Stop::EndTurn) => (None, false),
                Ok(Stop::Cancelled) => (Some(String::new()), true),
                Err(error) if error.is_cancelled() => (Some(String::new()), true),
                Err(error) => (Some(session.ctrl.redact_error(&error.to_string())), false),
            };
            turn.finish(error.clone(), canceled);
            self.bump_status();
            let summary = turn.summary();
            self.metrics.turn_finished(&summary.status, turn.elapsed());
            if let Some(message) = error.filter(|_| !canceled) {
                let mut fields = turn.log_ids();
                fields.extend([("trigger", trigger.to_string()), ("error", message)]);
                self.log.error("turn_error", &fields);
            }
            let mut fields = turn.log_ids();
            fields.extend([
                ("trigger", trigger.to_string()),
                ("status", summary.status.clone()),
                ("duration_ms", turn.elapsed().as_millis().to_string()),
            ]);
            fields.extend(turn.log_totals());
            self.log.info("turn_finished", &fields);
        })
    }

    /// Publishes the waiting approval, then waits for the decision route, the
    /// turn's cancellation, or [`APPROVAL_TIMEOUT`]. The turn stays `running`.
    /// Every outcome but cancellation is recorded as an `approval_decided`
    /// frame; a timeout is a deny.
    pub(super) async fn wait_for_approval(
        &self,
        session: &OpenSession,
        turn: &Turn,
        cancel: &CancellationToken,
        request: ApprovalRequest,
    ) -> ApprovalDecision {
        loop {
            let scope = session.ctrl.approval_scope();
            let mut actionable = None;
            if let Ok(shared) = session.ctrl.approval_service().get(
                &request.approval_id,
                &scope.workspace,
                Some(&scope.session_id),
            ) {
                let shared = match session
                    .ctrl
                    .approval_service()
                    .wait_actionable(&shared.id, &shared.scope, cancel)
                    .await
                {
                    Ok(shared) => shared,
                    Err(_) => return ApprovalDecision::Deny,
                };
                if !shared.is_human_pending() {
                    return match shared.state {
                        otto_core::approval::State::Approved => ApprovalDecision::Allow,
                        _ if cancel.is_cancelled() => ApprovalDecision::Cancelled,
                        _ => ApprovalDecision::Deny,
                    };
                }
                actionable = Some(shared);
            }
            let (decide, decided) = tokio::sync::oneshot::channel();
            let mut event = WireEvent::approval_requested(
                &request.approval_id,
                &request.tool_call_id,
                &request.command,
                &request.justification,
            );
            event.read_path = request.read_path.clone();
            event.approval = actionable.clone();
            session.lock().waiting = Some(WaitingApproval {
                id: request.approval_id.clone(),
                shared_revision: event.approval.as_ref().map(|request| request.revision),
                decide,
            });
            turn.push_event(event);
            self.bump_status();

            let (decision, label, refresh) = tokio::select! {
                biased;
                received = decided => match received {
                    Ok(ApprovalDecision::Allow) => (ApprovalDecision::Allow, "allow", false),
                    _ => (ApprovalDecision::Deny, "deny", false),
                },
                () = cancel.cancelled() => (ApprovalDecision::Cancelled, "", false),
                shared = async {
                    let scope = session.ctrl.approval_scope();
                    if let Ok(shared) = session.ctrl.approval_service().get(&request.approval_id,&scope.workspace,Some(&scope.session_id)) {
                        session.ctrl.approval_service().wait(&shared.id,&shared.scope,cancel).await.ok()
                    } else { std::future::pending().await }
                } => match shared {
                    Some(r) if r.state == otto_core::approval::State::Approved => (ApprovalDecision::Allow,"allow", false),
                    _ => (ApprovalDecision::Deny,"deny", false),
                },
                () = async {
                    let Some(binding) = &actionable else { return std::future::pending().await; };
                    let service = session.ctrl.approval_service();
                    let mut changes = service.changes();
                    let mut poll = tokio::time::interval(Duration::from_millis(200));
                    loop {
                        if let Ok(current) = service.get(&binding.id, &binding.scope.workspace, Some(&binding.scope.session_id))
                            && current.state == otto_core::approval::State::Pending
                            && (current.revision != binding.revision || current.subject != binding.subject) {
                            return;
                        }
                        tokio::select! { _ = changes.changed() => {}, _ = poll.tick() => {} }
                    }
                } => (ApprovalDecision::Deny, "superseded", true),
                () = session.ctrl.approval_withdrawn(&request.approval_id) => (ApprovalDecision::Deny, "deny", false),
                () = tokio::time::sleep(APPROVAL_TIMEOUT) => (ApprovalDecision::Deny, "timeout", false),
            };
            {
                let mut state = session.lock();
                if state
                    .waiting
                    .as_ref()
                    .is_some_and(|waiting| waiting.id == request.approval_id)
                {
                    state.waiting = None;
                }
                if !refresh {
                    state.remember_decided(&request.approval_id);
                }
            }
            if decision != ApprovalDecision::Cancelled {
                turn.push_event(WireEvent::approval_decided(&request.approval_id, label));
            }
            self.bump_status();
            if !refresh {
                return decision;
            }
        }
    }

    /// Starts a user turn on `session`, or queues it when `queue` is set and
    /// the session is busy. The `trigger == triggerTask` branch lives in
    /// [`Server::wake_turn`], because a wake turn needs no HTTP reply and is
    /// awaited by the wake loop that admitted it.
    /// Starts or queues a user turn. A turn whose `input_id` this session
    /// started recently is returned again instead of starting a second one;
    /// one whose `input_id` the transcript already holds fails with
    /// [`DUPLICATE_INPUT`].
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_turn(
        self: &Arc<Self>,
        session: &Arc<OpenSession>,
        text: String,
        image: Option<Block>,
        (channel, origin): (Option<Channel>, Option<otto::schedule::Origin>),
        queue: bool,
        request_id: &str,
        input_id: Option<String>,
    ) -> Result<Arc<Turn>, String> {
        let (turn, running) = {
            let mut state = session.lock();
            if let Some(input_id) = &input_id {
                if let Some((_, turn)) = state.inputs.iter().find(|(id, _)| id == input_id) {
                    return Ok(Arc::clone(turn));
                }
                if session.ctrl.has_input(input_id) {
                    return Err(DUPLICATE_INPUT.to_string());
                }
            }
            let busy = state.busy();
            if busy && !queue {
                return Err(TURN_ACTIVE.to_string());
            }
            if busy && state.queue.len() >= MAX_QUEUED_TURNS {
                return Err(QUEUE_FULL.to_string());
            }
            let id = new_id()?;
            let cancel = self.cancel.child_token();
            let session_id = session.ctrl.info().session_id;
            let log = Arc::clone(&self.log);
            let (turn, running) = if busy {
                // The saved copy needs an input id so a restore can tell
                // whether the turn already started.
                let queued_input_id = input_id.clone().unwrap_or_else(|| format!("queued:{id}"));
                if let Some(journal) = session.ctrl.turn_queue() {
                    journal
                        .save(&app::turn_queue::QueuedPrompt {
                            input_id: queued_input_id.clone(),
                            turn_id: id.clone(),
                            text: text.clone(),
                            channel,
                            origin: origin.clone(),
                            image_dropped: image.is_some(),
                        })
                        .map_err(|_| PROMPT_NOT_PERSISTED.to_string())?;
                }
                let turn = Arc::new(Turn::queued(id, TRIGGER_USER, cancel).with_log(
                    log,
                    &session_id,
                    request_id,
                ));
                state.queue.push_back(QueuedTurn {
                    turn: Arc::clone(&turn),
                    text,
                    image,
                    channel,
                    origin,
                    input_id: Some(queued_input_id),
                });
                (turn, None)
            } else {
                let turn = Arc::new(Turn::new(id, TRIGGER_USER, cancel).with_log(
                    log,
                    &session_id,
                    request_id,
                ));
                turn.start_running(session.ctrl.history().len());
                state.turn = Some(Arc::clone(&turn));
                (turn, Some((text, image, channel, origin, input_id.clone())))
            };
            if let Some(input_id) = input_id {
                if state.inputs.len() >= MAX_REMEMBERED_INPUTS {
                    state.inputs.pop_front();
                }
                state.inputs.push_back((input_id, Arc::clone(&turn)));
            }
            (turn, running)
        };
        session.touch();
        self.bump_status();
        if let Some((text, image, channel, origin, input_id)) = running {
            self.spawn_user_turn(
                session,
                Arc::clone(&turn),
                text,
                image,
                (channel, origin),
                input_id,
            );
        }
        Ok(turn)
    }

    /// Starts the oldest queued turn when nothing holds the session. While the
    /// server or the session is closing, ends every queued turn `canceled`
    /// instead. Called whenever a turn or a compaction ends.
    pub(super) fn start_next(self: &Arc<Self>, session: &Arc<OpenSession>) {
        let next = {
            let mut state = session.lock();
            if state.turn_running() || state.compacting.is_some() {
                return;
            }
            if self.cancel.is_cancelled() || session.closed.is_cancelled() {
                state.cancel_queue();
                None
            } else if let Some(next) = state.queue.pop_front() {
                next.turn.start_running(session.ctrl.history().len());
                state.turn = Some(Arc::clone(&next.turn));
                Some(next)
            } else {
                None
            }
        };
        self.bump_status();
        if let Some(next) = next {
            session.touch();
            self.spawn_user_turn(
                session,
                next.turn,
                next.text,
                next.image,
                (next.channel, next.origin),
                next.input_id,
            );
        }
    }

    /// Spawns the task that runs `turn`, which is already `state.turn`.
    pub(super) fn spawn_user_turn(
        self: &Arc<Self>,
        session: &Arc<OpenSession>,
        turn: Arc<Turn>,
        text: String,
        image: Option<Block>,
        (channel, origin): (Option<Channel>, Option<otto::schedule::Origin>),
        input_id: Option<String>,
    ) {
        turn.push_event(WireEvent::user_message(&text, image.is_some()));
        self.metrics.turn_started();
        let mut fields = turn.log_ids();
        fields.push(("trigger", TRIGGER_USER.to_string()));
        self.log.info("turn_started", &fields);
        let server = Arc::clone(self);
        let session = Arc::clone(session);
        tokio::spawn(async move {
            let first = match image {
                Some(image) => Step::Image(&text, image),
                None => Step::Text(&text),
            };
            session.ctrl.set_next_input_id(input_id.clone());
            server
                .run_turn(&session, &turn, first, TRIGGER_USER, channel, origin)
                .await;
            session.ctrl.set_next_input_id(None);
            // The turn ran: a saved copy of it is no longer waiting.
            if let (Some(input_id), Some(journal)) = (&input_id, session.ctrl.turn_queue()) {
                journal.forget(input_id);
            }
            // A turn that ran for longer than the idle timeout restarts the
            // clock, so a reader can still replay it from the buffer.
            session.touch();
            server.start_next(&session);
            // A notification that landed too late for this turn's own drain is
            // caught by the wake loop's end-of-turn check.
            session.turn_finished.notify_one();
        });
    }
}
