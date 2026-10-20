//! Connection prompt, steering, command and local prompt turns.

use super::*;

impl Connection {
    /// Runs one normal prompt to its stop reason. Local slash commands run
    /// here and never reach the task model.
    pub(super) async fn prompt(
        &self,
        session_id: &str,
        text: String,
        channel: Option<Channel>,
        origin: Option<Origin>,
        (request_id, input_id): (Option<&str>, Option<&str>),
        cancel: &CancellationToken,
    ) -> Reply {
        if let Some(command) = command::parse(&text) {
            return self.run_command(session_id, command, cancel).await;
        }
        // Keep the local agent turn and attach transport state machines on the
        // heap. Combining both large futures inline can overflow the default
        // ACP task stack as either backend evolves.
        match &self.backend {
            Backend::Local(local) => {
                let session = local.session(session_id).ok_or_else(unknown_session)?;
                if let Some(input_id) = input_id {
                    if session.controller.has_input(input_id) {
                        return Err(invalid_params(crate::app::task_control::DUPLICATE_INPUT));
                    }
                    session
                        .controller
                        .set_next_input_id(Some(input_id.to_string()));
                }
                let reply = Box::pin(self.local_prompt(
                    local,
                    &session,
                    session_id,
                    text,
                    (channel, origin),
                    cancel,
                ))
                .await;
                session.controller.set_next_input_id(None);
                reply
            }
            Backend::Attach(relay) => {
                Box::pin(relay.prompt(
                    self,
                    session_id,
                    &text,
                    (channel, origin.as_ref()),
                    (request_id, input_id),
                    cancel,
                ))
                .await
            }
        }
    }

    /// Runs a queued prompt as normal user text; slash commands inside its
    /// payload are not recursively interpreted as ACP control commands.
    pub(super) async fn queued_prompt(
        &self,
        session_id: &str,
        text: String,
        channel: Option<Channel>,
        origin: Option<Origin>,
        request_id: Option<&str>,
        cancel: &CancellationToken,
    ) -> Reply {
        // Match `prompt`: queued turns use the same backend boundary and must
        // not inline either large state machine into this dispatcher.
        match &self.backend {
            Backend::Local(local) => {
                let session = local.session(session_id).ok_or_else(unknown_session)?;
                Box::pin(self.local_prompt(
                    local,
                    &session,
                    session_id,
                    text,
                    (channel, origin),
                    cancel,
                ))
                .await
            }
            Backend::Attach(relay) => {
                Box::pin(relay.prompt(
                    self,
                    session_id,
                    &text,
                    (channel, origin.as_ref()),
                    (request_id, None),
                    cancel,
                ))
                .await
            }
        }
    }

    /// Deterministically admits an ordinary prompt to the exact active run.
    /// It intentionally does not use the model-backed control dialogue.
    pub(super) async fn admit_prompt(
        &self,
        session_id: &str,
        text: &str,
        input_id: Option<&str>,
    ) -> Result<ActiveInputAdmission, agent_client_protocol_schema::v1::Error> {
        match &self.backend {
            Backend::Local(local) => local
                .session(session_id)
                .ok_or_else(unknown_session)?
                .controller
                .admit_active_input_with(text, input_id)
                .map_err(|message| error(INTERNAL_ERROR, message)),
            Backend::Attach(relay) => relay
                .client
                .admit_input(session_id, text, input_id)
                .await
                .map_err(|failure| relay.task_control_error(failure)),
        }
    }

    pub(super) async fn steer_prompt(&self, session_id: &str, guidance: &str) -> Reply {
        let accepted = match &self.backend {
            Backend::Local(local) => match local.session(session_id) {
                Some(session) => session
                    .controller
                    .steer(guidance)
                    .map_err(|message| error(INTERNAL_ERROR, message)),
                None => return Err(unknown_session()),
            },
            Backend::Attach(relay) => relay
                .client
                .steer(session_id, guidance)
                .await
                .map_err(|failure| relay.task_control_error(failure))
                .and_then(|message| {
                    (message.status == otto_core::agent::steering::Status::Queued)
                        .then_some(message)
                        .ok_or_else(|| {
                            error(INTERNAL_ERROR, "the active task did not accept guidance")
                        })
                }),
        };
        match accepted {
            Ok(message) => {
                self.say(
                    session_id,
                    &format!(
                        "Guidance queued (#{}): {}\nIt will be delivered at the next safe boundary.",
                        message.id, message.text
                    ),
                );
                result(PromptResponse::new(StopReason::EndTurn))
            }
            Err(failure) => Err(failure),
        }
    }

    pub(super) async fn control_message(
        &self,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
    ) -> Reply {
        let reply = match &self.backend {
            Backend::Local(local) => match local.session(session_id) {
                Some(session) => {
                    let cancel = session.prompt.active_cancel();
                    session
                        .controller
                        .control_message_with_channel(text, channel, &cancel)
                        .await
                        .map_err(|message| error(INTERNAL_ERROR, message))?
                }
                None => return Err(unknown_session()),
            },
            Backend::Attach(relay) => relay
                .client
                .control_message_with_channel(session_id, text, channel)
                .await
                .map_err(|failure| relay.task_control_error(failure))?,
        };
        result(reply)
    }

    pub(super) async fn run_command(
        &self,
        session_id: &str,
        command: command::Command<'_>,
        cancel: &CancellationToken,
    ) -> Reply {
        let end_turn = || result(PromptResponse::new(StopReason::EndTurn));
        match (&self.backend, command) {
            (Backend::Local(local), command) => {
                let session = local.session(session_id).ok_or_else(unknown_session)?;
                match command {
                    command::Command::Permissions { args } => {
                        let text = session
                            .controller
                            .permissions_command(args)
                            .map_err(|e| invalid_params(&e))?;
                        self.say(session_id, &text);
                        end_turn()
                    }
                    command::Command::Auto { args } => {
                        let text = session
                            .controller
                            .auto_mode_command(args)
                            .map_err(|e| invalid_params(&e))?;
                        self.say(session_id, &text);
                        end_turn()
                    }
                    command::Command::Model { profile } => {
                        if profile.is_empty() {
                            let info = session.controller.info();
                            let available = session.controller.profiles();
                            self.say(
                                session_id,
                                &command::model_text(
                                    &info.profile,
                                    &info.provider,
                                    &info.model,
                                    &info.thinking,
                                    &available,
                                ),
                            );
                            return end_turn();
                        }
                        match session.controller.switch_profile_result(profile).await {
                            Ok(switched) => {
                                let info = session.controller.info();
                                self.say(
                                    session_id,
                                    &command::model_switched_text(
                                        &info.profile,
                                        &info.provider,
                                        &info.model,
                                        &info.thinking,
                                    ),
                                );
                                for warning in switched.warnings {
                                    self.say(session_id, &local.redact(&warning));
                                }
                                end_turn()
                            }
                            Err(message) => Err(invalid_params(local.redact(&message))),
                        }
                    }
                    command::Command::Context => {
                        let report = session.controller.context_report();
                        self.say(
                            session_id,
                            &report.map_or(command::CONTEXT_UNAVAILABLE.to_string(), |report| {
                                command::context_text(&report)
                            }),
                        );
                        end_turn()
                    }
                    command::Command::Compact { focus } => {
                        let outcome = session
                            .controller
                            .compact(
                                focus,
                                &mut |event| {
                                    if let Some(update) =
                                        update::event_update(&update::local_wire(&event))
                                    {
                                        self.update(session_id, update);
                                    }
                                },
                                cancel,
                            )
                            .await;
                        match outcome {
                            Ok(done) => {
                                self.say(
                                    session_id,
                                    &command::compaction_text(&to_wire_compaction(&done)),
                                );
                                end_turn()
                            }
                            Err(_) if cancel.is_cancelled() => {
                                result(PromptResponse::new(StopReason::Cancelled))
                            }
                            Err(failure) => {
                                Err(error(INTERNAL_ERROR, local.redact(&failure.to_string())))
                            }
                        }
                    }
                    command::Command::Reflect { focus } => {
                        if let Some(text) = crate::cli::repl_commands::reflect_runs_report(
                            &session.controller,
                            focus,
                        ) {
                            self.say(session_id, &local.redact(&text));
                            return end_turn();
                        }
                        match session.controller.reflect(focus, cancel).await {
                            Ok(report) => {
                                self.say(session_id, &report.summary());
                                end_turn()
                            }
                            Err(_) if cancel.is_cancelled() => {
                                result(PromptResponse::new(StopReason::Cancelled))
                            }
                            Err(failure) => {
                                Err(error(INTERNAL_ERROR, local.redact(&failure.to_string())))
                            }
                        }
                    }
                    command::Command::Skill { args } => {
                        let words: Vec<_> = args.split_whitespace().collect();
                        let text = match words.as_slice() {
                            ["generated"] => crate::cli::repl_commands::skill_report(
                                &session.controller,
                                "generated",
                            ),
                            ["revert", name] => crate::cli::repl_commands::skill_report(
                                &session.controller,
                                &format!("revert {name}"),
                            ),
                            _ => "Usage: /skill generated | /skill revert <name>".to_string(),
                        };
                        self.say(session_id, &text);
                        end_turn()
                    }
                    command::Command::Steer { guidance } => {
                        match session.controller.steer(guidance) {
                            Ok(message) => {
                                self.say(
                                    session_id,
                                    &format!(
                                        "Guidance queued (#{}): {}\nIt will be delivered at the next safe boundary.",
                                        message.id, message.text
                                    ),
                                );
                                end_turn()
                            }
                            Err(message) => Err(error(INTERNAL_ERROR, message)),
                        }
                    }
                    command::Command::Queue { .. } => Err(invalid_params(
                        "nested /queue is not supported; submit a top-level /queue prompt",
                    )),
                }
            }
            (Backend::Attach(relay), command) => {
                let text = relay.run_command(command, session_id, cancel).await?;
                match text {
                    Some(text) => {
                        self.say(session_id, &text);
                        end_turn()
                    }
                    None => result(PromptResponse::new(StopReason::Cancelled)),
                }
            }
        }
    }

    /// Runs one local prompt, including the approval retries.
    pub(super) async fn local_prompt(
        &self,
        local: &Local,
        session: &Session,
        session_id: &str,
        text: String,
        (channel, origin): (Option<Channel>, Option<Origin>),
        cancel: &CancellationToken,
    ) -> Reply {
        let outcome = session
            .controller
            .run_with_channel(
                Step::Text(&text),
                channel,
                origin,
                &mut |event| {
                    if let Some(update) = update::event_update(&update::local_wire(&event)) {
                        self.update(session_id, update);
                    }
                },
                cancel,
                |request| async move {
                    let scope = session.controller.approval_scope();
                    if let Ok(shared) = session.controller.approval_service().get(
                        &request.approval_id,
                        &scope.workspace,
                        Some(&scope.session_id),
                    ) {
                        let shared = match session
                            .controller
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
                        if shared.kind == otto_core::approval::Kind::Confirmation
                            || self
                                .client_capabilities
                                .lock()
                                .expect("ACP capabilities")
                                .pointer("/_meta/otto/approvals/version")
                                .and_then(serde_json::Value::as_u64)
                                == Some(1)
                        {
                            return self
                                .confirmation(&session.controller, &shared, cancel)
                                .await;
                        }
                        return approval::shared_request_permission(
                            self,
                            &session.controller,
                            &shared,
                            &request,
                            cancel,
                        )
                        .await;
                    }
                    request_permission(
                        self,
                        session_id,
                        &request.tool_call_id,
                        &request.command,
                        &request.read_path,
                        &request.justification,
                        cancel,
                    )
                    .await
                },
            )
            .await;
        match outcome {
            Ok(Stop::EndTurn) => result(PromptResponse::new(StopReason::EndTurn)),
            Ok(Stop::Cancelled) => result(PromptResponse::new(StopReason::Cancelled)),
            Err(failure) => Err(error(INTERNAL_ERROR, local.redact(&failure.to_string()))),
        }
    }
}
