//! Dispatcher: request routing and prompt task start.

use super::*;

impl Dispatcher {
    pub(super) fn handle_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let frame: Value = match serde_json::from_str(line) {
            Ok(frame) => frame,
            Err(failure) => {
                return self
                    .connection
                    .reply(Value::Null, Err(error(PARSE_ERROR, failure.to_string())));
            }
        };
        let Some(object) = frame.as_object() else {
            return self.connection.reply(
                Value::Null,
                Err(error(INVALID_REQUEST, "frame is not a JSON object")),
            );
        };
        let method = object.get("method").and_then(Value::as_str);
        let id = object.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                self.request(method.to_string(), id, params);
            }
            (Some("session/cancel"), None) => {
                let session_id = object
                    .get("params")
                    .and_then(|params| params.get("sessionId"))
                    .and_then(Value::as_str);
                if let Some(slot) = session_id.and_then(|id| self.connection.slot(id)) {
                    let dropped = slot.cancel();
                    // Cancelled for good: a restart must not run them.
                    if let Some(journal) = session_id
                        .and_then(|id| self.connection.local_controller(id))
                        .and_then(|controller| controller.turn_queue())
                    {
                        journal.clear();
                    }
                    if dropped > 0 {
                        self.connection.say(
                            session_id.expect("slot requires session id"),
                            &format!("Canceled {dropped} queued follow-up prompt(s)."),
                        );
                    }
                }
            }
            (Some(_), None) => {}
            (None, Some(_)) if object.contains_key("result") || object.contains_key("error") => {
                self.connection.client_response(frame);
            }
            _ => self.connection.reply(
                object.get("id").cloned().unwrap_or(Value::Null),
                Err(error(
                    INVALID_REQUEST,
                    "not a request, notification or response",
                )),
            ),
        }
    }

    pub(super) fn request(&mut self, method: String, id: Value, params: Value) {
        let connection = Arc::clone(&self.connection);
        match method.as_str() {
            "initialize" => {
                let local = match &connection.backend {
                    Backend::Local(_) => true,
                    Backend::Attach(relay) => relay.server.is_some(),
                };
                let platforms = schedule::delivery_platforms(&params);
                if let Err(message) = schedule::delivery_chats(&params, &platforms) {
                    return connection.reply(id, Err(invalid_params(message)));
                }
                *connection
                    .client_capabilities
                    .lock()
                    .expect("ACP capabilities") = params.clone();
                if !self.scheduling
                    && let Some(worker) = schedule::worker(&connection, platforms)
                {
                    self.scheduling = true;
                    let stop = self.stop.clone();
                    self.tasks.spawn(worker.run(stop));
                }
                connection.reply(id, initialize_result(local));
            }
            "session/new" => {
                self.tasks.spawn(async move {
                    let reply = connection.new_session(params).await;
                    connection.reply(id, reply.clone());
                    connection.advertise_commands(&reply);
                    connection.notices_ready(&reply);
                });
            }
            "session/load" => {
                self.tasks.spawn(async move {
                    let reply = connection.load_session(params).await;
                    connection.reply(id, reply.clone());
                    connection.advertise_commands(&reply);
                    connection.notices_ready(&reply);
                });
            }
            "session/list" => {
                self.tasks.spawn(async move {
                    let reply = connection.list_sessions(params).await;
                    connection.reply(id, reply);
                });
            }
            "session/prompt" => self.start_prompt(id, &params),
            "_otto/tasks/steer" => {
                self.tasks.spawn(async move {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase", deny_unknown_fields)]
                    struct Input {
                        session_id: String,
                        text: String,
                    }
                    let reply = match serde_json::from_value::<Input>(params) {
                        Err(failure) => Err(invalid_params(failure.to_string())),
                        Ok(input) if input.text.trim().is_empty() => Err(invalid_params(
                            "text must contain non-whitespace characters",
                        )),
                        Ok(input) if input.text.len() > 32_000 => {
                            Err(invalid_params("text must be at most 32000 UTF-8 bytes"))
                        }
                        Ok(input) => match &connection.backend {
                            Backend::Local(local) => match local.session(&input.session_id) {
                                None => Err(unknown_session()),
                                Some(session) => session
                                    .controller
                                    .steer(&input.text)
                                    .map(|message| {
                                        serde_json::to_value(message)
                                            .expect("steering message serializes")
                                    })
                                    .map_err(|failure| error(INTERNAL_ERROR, failure)),
                            },
                            Backend::Attach(relay) => relay
                                .client
                                .steer(&input.session_id, &input.text)
                                .await
                                .map(|message| {
                                    serde_json::to_value(message)
                                        .expect("steering message serializes")
                                })
                                .map_err(|failure| relay.task_control_error(failure)),
                        },
                    };
                    connection.reply(id, reply);
                });
            }
            "_otto/tasks/control" => {
                self.tasks.spawn(async move {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase", deny_unknown_fields)]
                    struct Input {
                        session_id: String,
                        text: String,
                        #[serde(default)]
                        channel: Option<Channel>,
                    }
                    let reply = match serde_json::from_value::<Input>(params) {
                        Err(failure) => Err(invalid_params(failure.to_string())),
                        Ok(input) if input.text.trim().is_empty() => Err(invalid_params(
                            "text must contain non-whitespace characters",
                        )),
                        Ok(input) if input.text.len() > 32_000 => {
                            Err(invalid_params("text must be at most 32000 UTF-8 bytes"))
                        }
                        Ok(input) => {
                            connection
                                .control_message(&input.session_id, &input.text, input.channel)
                                .await
                        }
                    };
                    connection.reply(id, reply);
                });
            }
            "_otto/approvals/message" => {
                self.tasks.spawn(async move {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase", deny_unknown_fields)]
                    struct Input {
                        session_id: String,
                        text: String,
                        #[serde(default)]
                        channel: Option<Channel>,
                    }
                    let reply = match serde_json::from_value::<Input>(params) {
                        Err(failure) => Err(invalid_params(failure.to_string())),
                        Ok(input) => match &connection.backend {
                            Backend::Local(local) => match local.session(&input.session_id) {
                                None => Err(unknown_session()),
                                Some(session) => {
                                    let cancel = session.prompt.active_cancel();
                                    match session
                                        .controller
                                        .approval_message_with_channel(
                                            &input.text,
                                            input.channel,
                                            &cancel,
                                        )
                                        .await
                                    {
                                        Ok(reply) => result(reply),
                                        Err(failure) => Err(error(INTERNAL_ERROR, failure)),
                                    }
                                }
                            },
                            Backend::Attach(relay) => match relay
                                .client
                                .approval_message_with_channel(
                                    &input.session_id,
                                    &input.text,
                                    input.channel,
                                )
                                .await
                            {
                                Ok(reply) => result(reply),
                                Err(failure) => Err(error(INTERNAL_ERROR, failure.to_string())),
                            },
                        },
                    };
                    connection.reply(id, reply);
                });
            }
            "_otto/approval_policy/get" | "_otto/approval_policy/set" => {
                self.tasks.spawn(async move {
                    let reply = connection.approval_policy(&method, params).await;
                    connection.reply(id, reply);
                });
            }
            "_otto/approval_mode/get" | "_otto/approval_mode/set" => {
                self.tasks.spawn(async move {
                    let reply = connection.approval_mode(&method, params).await;
                    connection.reply(id, reply);
                });
            }
            "_otto/approvals/list"
            | "_otto/approvals/watch"
            | "_otto/approvals/get"
            | "_otto/approvals/decide"
            | "_otto/approvals/withdraw" => {
                self.tasks.spawn(async move {
                    let reply = connection.shared_approval(&method, params).await;
                    connection.reply(id, reply);
                });
            }
            memory::PENDING_METHOD | memory::REVIEW_METHOD => {
                self.tasks.spawn(async move {
                    let reply = connection.memory_request(&method, params);
                    connection.reply(id, reply);
                });
            }
            _ => connection.reply(
                id,
                Err(error(METHOD_NOT_FOUND, format!("unknown method {method}"))),
            ),
        }
    }

    /// Validates the request and installs the prompt token here, in the read
    /// loop, so a `session/cancel` read next already finds it.
    pub(super) fn start_prompt(&mut self, id: Value, params: &Value) {
        let connection = Arc::clone(&self.connection);
        let (session_id, text, channel) = match parse_prompt(params) {
            Ok(parsed) => parsed,
            Err(failure) => return connection.reply(id, Err(failure)),
        };
        let origin = parse_origin(params, channel);
        let request_id = parse_request_id(params);
        let input_id = match parse_input_id(params) {
            Ok(input_id) => input_id,
            Err(failure) => return connection.reply(id, Err(failure)),
        };
        let Some(slot) = connection.slot(&session_id) else {
            return connection.reply(id, Err(unknown_session()));
        };
        match command::parse(&text) {
            Some(command::Command::Steer { guidance }) => {
                let guidance = guidance.to_string();
                self.tasks.spawn(async move {
                    let reply = connection.steer_prompt(&session_id, &guidance).await;
                    connection.reply(id, reply);
                });
                return;
            }
            Some(command::Command::Queue { prompt }) => {
                if prompt.trim().is_empty() {
                    return connection.reply(
                        id,
                        Err(invalid_params("/queue requires a non-empty prompt")),
                    );
                }
                if prompt.len() > 32_000 {
                    return connection.reply(
                        id,
                        Err(invalid_params(
                            "/queue prompt must be at most 32000 UTF-8 bytes",
                        )),
                    );
                }
                let prompt = prompt.to_string();
                match &connection.backend {
                    Backend::Local(_) => {
                        // Saved before it is accepted, so a stop before it runs
                        // does not lose it.
                        let saved_id = input_id.clone().unwrap_or_else(|| {
                            format!("queued:{}", crate::runtime::sidecar_mailbox::new_input_id())
                        });
                        let journal = connection
                            .local_controller(&session_id)
                            .and_then(|controller| controller.turn_queue());
                        if let Some(journal) = &journal
                            && let Err(message) =
                                journal.save(&crate::app::turn_queue::QueuedPrompt {
                                    input_id: saved_id.clone(),
                                    turn_id: String::new(),
                                    text: prompt.clone(),
                                    channel,
                                    origin: origin.clone(),
                                    image_dropped: false,
                                })
                        {
                            return connection.reply(id, Err(error(INTERNAL_ERROR, message)));
                        }
                        match slot.enqueue(prompt, (channel, origin), Some(saved_id.clone())) {
                            Ok(start_worker) => {
                                connection.say(
                                &session_id,
                                "Follow-up accepted; its answer will arrive as a later session update.",
                            );
                                connection
                                    .reply(id, result(PromptResponse::new(StopReason::EndTurn)));
                                if start_worker {
                                    let worker_slot = Arc::clone(&slot);
                                    let worker_connection = Arc::clone(&connection);
                                    let worker_session = session_id.clone();
                                    self.tasks.spawn(async move {
                                        worker_slot
                                            .drain_queue(worker_connection, worker_session)
                                            .await;
                                    });
                                }
                            }
                            Err(message) => {
                                if let Some(journal) = &journal {
                                    journal.forget(&saved_id);
                                }
                                connection.reply(id, Err(error(INTERNAL_ERROR, message)))
                            }
                        }
                    }
                    Backend::Attach(_) => {
                        let Ok((followup_id, token)) = slot.begin_followup(&self.stop) else {
                            return connection.reply(
                                id,
                                Err(error(INTERNAL_ERROR, "queue is full (16 pending prompts)")),
                            );
                        };
                        self.tasks.spawn(async move {
                            let Backend::Attach(relay) = &connection.backend else {
                                unreachable!("backend remains attach for this request")
                            };
                            slot.wait_followup_admission(followup_id).await;
                            if token.is_cancelled() {
                                slot.finish_followup_admission(followup_id);
                                connection.say(&session_id, "Queued follow-up canceled before it started.");
                                connection.reply(
                                    id,
                                    result(PromptResponse::new(StopReason::EndTurn)),
                                );
                                slot.end_followup(followup_id);
                                return;
                            }
                            let started = relay
                                .start_queued_prompt(
                                    &session_id,
                                    &prompt,
                                    channel,
                                    origin.as_ref(),
                                    request_id.as_deref(),
                                )
                                .await;
                            match started {
                                Ok(stream) => {
                                    connection.say(
                                        &session_id,
                                        "Follow-up accepted; its answer will arrive as a later session update.",
                                    );
                                    connection.reply(
                                        id,
                                        result(PromptResponse::new(StopReason::EndTurn)),
                                    );
                                    slot.finish_followup_admission(followup_id);
                                    let reply = relay
                                        .stream_turn(&connection, &session_id, stream, &token)
                                        .await;
                                    if let Some(notice) = queued_outcome_notice(&reply) {
                                        connection.say(&session_id, &notice);
                                    }
                                }
                                Err(failure) => {
                                    connection.reply(id, Err(failure));
                                    slot.finish_followup_admission(followup_id);
                                }
                            }
                            slot.end_followup(followup_id);
                        });
                    }
                }
                return;
            }
            _ => {}
        }
        let stop = self.stop.clone();
        // Claimed here, before the task is spawned, so a `session/cancel`
        // read next finds the token. `None` when the slot is busy; the task
        // then claims it as before.
        let early_token = slot.begin(&stop);
        let holds_slot = early_token.is_some();
        self.tasks.spawn(async move {
            match connection
                .admit_prompt(&session_id, &text, input_id.as_deref())
                .await
            {
                Ok(ActiveInputAdmission::Steered) => {
                    if holds_slot {
                        slot.end();
                    }
                    connection.say(
                        &session_id,
                        "Guidance accepted for the active task; it will be delivered at the next safe boundary.",
                    );
                    connection.reply(id, result(PromptResponse::new(StopReason::EndTurn)));
                    return;
                }
                Ok(ActiveInputAdmission::ApprovalPending) => {
                    if holds_slot {
                        slot.end();
                    }
                    connection.say(
                        &session_id,
                        "The active task is waiting for approval; resolve that approval before sending task guidance.",
                    );
                    connection.reply(id, result(PromptResponse::new(StopReason::EndTurn)));
                    return;
                }
                Ok(ActiveInputAdmission::Idle) => {}
                Err(failure) => {
                    if holds_slot {
                        slot.end();
                    }
                    connection.reply(id, Err(failure));
                    return;
                }
            }

            let Some(token) = early_token.or_else(|| slot.begin(&stop)) else {
                // A wake turn is not a prompt: wait for it, then run.
                if let Some(token) = slot.wait_for_wake(&stop) {
                    if !slot.claim_after_wake(&token).await {
                        return connection
                            .reply(id, result(PromptResponse::new(StopReason::Cancelled)));
                    }
                    let reply = connection
                        .prompt(
                            &session_id,
                            text,
                            channel,
                            origin,
                            (request_id.as_deref(), input_id.as_deref()),
                            &token,
                        )
                        .await;
                    slot.end();
                    connection.reply(id, reply);
                    return;
                }
                return connection.reply(
                    id,
                    Err(error(INTERNAL_ERROR, "a prompt is already running")),
                );
            };
            let reply = connection
                .prompt(
                    &session_id,
                    text,
                    channel,
                    origin,
                    (request_id.as_deref(), input_id.as_deref()),
                    &token,
                )
                .await;
            slot.end();
            connection.reply(id, reply);
        });
    }
}
