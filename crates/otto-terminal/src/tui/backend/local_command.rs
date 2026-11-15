//! Local-controller command handling and session or model switches.

use super::*;

pub(super) async fn local_command(c: Arc<Controller>, command: Command) -> Result<Reply, String> {
    if let Command::Fork(entry_id) = command {
        let (result, draft) = c.fork_session(Some(&entry_id)).await?;
        let (info, history) = blocking(c, move |c| Ok((c.info(), c.history()))).await?;
        return Ok(Reply::Forked {
            message: forked_message(&result.session_path),
            info,
            history,
            draft,
        });
    }
    if let Command::SteeringState = command {
        return c.steering_messages().map(Reply::Steering);
    }
    let mut history = false;
    let mut prompt = None;
    let message = match command {
        Command::MemoryInvocation { name, args } => {
            history = true;
            blocking(c.clone(), move |c| {
                if let Err(error) = c.memory_command(&name, &args)
                    && error.kind == otto::app::management::ManagementErrorKind::Internal
                {
                    return Err(error.message);
                }
                Ok(String::new())
            })
            .await?
        }
        Command::Fork(_) | Command::SteeringState => unreachable!(),
        Command::NewSession => {
            c.new_session().await?;
            history = true;
            format!("Session: {}", c.info().session_id)
        }
        Command::Resume(path) => {
            let result = c.resume_session(&path).await?;
            history = true;
            format!(
                "Resumed: {}{}",
                result.session_path,
                result
                    .warnings
                    .into_iter()
                    .map(|s| format!("\n{s}"))
                    .collect::<String>()
            )
        }
        Command::Delete(path) => {
            c.delete_session(&path).await?;
            history = true;
            "Session permanently deleted.".into()
        }
        Command::Archive(path) => {
            let result = c.archive_session(&path).await?;
            history = true;
            format!("Archived: {}", result.path)
        }
        Command::SwitchModel(model) => {
            let message = switch_model(c.clone(), model).await?;
            history = true;
            message
        }
        Command::SwitchModelThinking {
            model,
            thinking,
            save,
        } => {
            let mut message = switch_model(c.clone(), model).await?;
            history = true;
            let thinking = match apply_thinking(c.clone(), thinking, save).await {
                Ok(message) => message,
                Err(error) => format!("/thinking: {error}. The model switch was applied."),
            };
            message.push('\n');
            message.push_str(&thinking);
            message
        }
        Command::SetThinking { thinking, save } => {
            apply_thinking(c.clone(), thinking, save).await?
        }
        Command::Rename(name) => {
            blocking(c.clone(), move |c| {
                c.rename_session(&name)?;
                Ok(format!("Session name: {name}"))
            })
            .await?
        }
        Command::Permissions(args) => {
            blocking(c.clone(), move |c| c.permissions_command(&args)).await?
        }
        Command::SandboxReload => format!("Sandbox: {}", c.reload_sandbox().await?.summary()),
        Command::SandboxAllow(path) => {
            let path = blocking(c.clone(), move |c| c.resolve_sandbox_read_path(&path)).await?;
            let (message, followup) = amend_sandbox(&c, SandboxChange::AllowReadPath(path)).await?;
            prompt = followup;
            message
        }
        Command::SandboxNetwork(mode) => {
            let (message, followup) = amend_sandbox(&c, SandboxChange::Network(mode)).await?;
            prompt = followup;
            message
        }
        Command::SandboxExclude(entry) => {
            let change = SandboxChange::ExcludeCommand(entry);
            let (message, followup) = amend_sandbox(&c, change.clone()).await?;
            prompt = followup;
            format!(
                "{message}\n{}",
                change.exclude_saved_note(&c.config_path().display().to_string())
            )
        }
        Command::DecideApproval {
            id,
            allow,
            feedback,
            revision,
        } => {
            blocking(c.clone(), move |c| {
                decide(c, &id, allow, &feedback, revision)?;
                Ok(format!("Decision recorded for {id}."))
            })
            .await?
        }
        Command::Approve(id) => {
            let shared = blocking(c.clone(), {
                let id = id.clone();
                move |c| {
                    Ok(c.approval_service()
                        .get(&id, &c.info().workspace, Some(&c.info().session_id))
                        .is_ok())
                }
            })
            .await?;
            if shared {
                blocking(c.clone(), {
                    let id = id.clone();
                    move |c| decide(c, &id, true, "", None)
                })
                .await?;
            } else {
                prompt = Some(c.approve_tool(&id).await?);
            }
            format!("Approved {id}.")
        }
        Command::Deny(id) => {
            blocking(c.clone(), move |c| {
                if c.approval_service()
                    .get(&id, &c.info().workspace, Some(&c.info().session_id))
                    .is_ok()
                {
                    decide(c, &id, false, "", None)?;
                } else {
                    c.deny_tool(&id)?;
                }
                Ok(format!("Denied {id}."))
            })
            .await?
        }
        Command::ApproveAlways(id) => {
            prompt = Some(c.approve_bash_always(&id).await?);
            format!("Approved {id}.")
        }
        Command::AdmitLocalInput { text, input_id } => {
            let mut provenance =
                otto_core::model::InputProvenance::local(otto_core::model::InputSource::Tui);
            provenance.delivery = otto_core::model::InputDelivery::Steering;
            match c.admit_active_input_with_provenance(&text, Some(&input_id), Some(provenance)) {
                Ok(otto::app::ActiveInputAdmission::Steered) => {
                    c.update_input_submission(&input_id, "accepted", Some(provenance.delivery))?;
                    format!(
                        "[{input_id}] Tui / Steering / accepted; delivery at the next safe boundary"
                    )
                }
                Ok(otto::app::ActiveInputAdmission::Idle) => {
                    c.update_input_submission(
                        &input_id,
                        "accepted",
                        Some(otto_core::model::InputDelivery::Direct),
                    )?;
                    return Ok(Reply::LocalPrompt { text, input_id });
                }
                Ok(otto::app::ActiveInputAdmission::ApprovalPending) => {
                    c.update_input_submission(&input_id, "rejected: approval pending", None)?;
                    format!(
                        "[{input_id}] rejected: respond to approval first; /recover {input_id} restores the draft"
                    )
                }
                Err(error) => {
                    c.update_input_submission(&input_id, "failed admission", None)?;
                    format!("[{input_id}] admission failed: {error}; /recover {input_id}")
                }
            }
        }
        Command::AdmitInput(text) => match c.admit_active_input_with_provenance(
            &text,
            None,
            Some(otto_core::model::InputProvenance::local(
                otto_core::model::InputSource::Tui,
            )),
        )? {
            otto::app::ActiveInputAdmission::Steered => format!(
                "Guidance queued for the active task: {text}\nIt will be delivered at the next safe boundary."
            ),
            otto::app::ActiveInputAdmission::Idle => {
                prompt = Some(text);
                String::new()
            }
            otto::app::ActiveInputAdmission::ApprovalPending => {
                "The task is waiting for approval; respond to that approval before steering it."
                    .into()
            }
        },
        Command::MemoryReview { id, accept } => {
            blocking(c.clone(), move |c| {
                c.management(otto::app::management::Request::MemoryReview {
                    id,
                    decision: if accept {
                        ReviewDecision::Accept
                    } else {
                        ReviewDecision::Reject
                    },
                    transport: "tui",
                })
                .map(|reply| otto::app::management::format_management_reply(&reply))
            })
            .await?
        }
        Command::WithdrawSteer(id) => {
            c.withdraw_steering(id)?;
            "Queued input withdrawn.".into()
        }
        Command::InterruptStep => {
            c.interrupt_step().map_err(|refusal| refusal.message())?;
            String::new()
        }
        Command::Management(request) => {
            return blocking(c, move |c| c.management(request).map(Reply::Management)).await;
        }
        Command::Logout => {
            let auth_path = c.auth_path();
            if auth_path.is_empty() {
                return Err("sign-out is unavailable".into());
            }
            let removed = otto::auth::service::Service::new(auth_path.into())
                .logout(&CancellationToken::new())
                .await
                .map_err(|error| error.to_string())?;
            if removed {
                "Signed out."
            } else {
                "Not signed in."
            }
            .into()
        }
    };
    let (info, history) =
        blocking(c, move |c| Ok((c.info(), history.then(|| c.history())))).await?;
    Ok(Reply::Operation {
        message,
        info,
        history,
        prompt,
    })
}

async fn switch_model(c: Arc<Controller>, model: String) -> Result<String, String> {
    let result = c.switch_model_result(&model).await?;
    let saved = blocking(c.clone(), move |c| c.set_default_model(&model)).await;
    let info = c.info();
    let mut message = format!(
        "Switched to model {} (type {}).",
        info.model_selector, info.provider_type
    );
    match saved {
        Ok(()) => message.push_str(" Set as default model."),
        Err(error) => message.push_str(&format!(" The default model was not saved: {error}")),
    }
    for warning in result.warnings {
        message.push('\n');
        message.push_str(&warning);
    }
    Ok(message)
}

async fn apply_thinking(
    c: Arc<Controller>,
    thinking: String,
    save: bool,
) -> Result<String, String> {
    if !thinking.is_empty() {
        c.set_thinking(&thinking).await?;
    }
    let info = c.info();
    let display = if info.thinking.is_empty() {
        "default"
    } else {
        &info.thinking
    };
    let mut message = format!("Thinking: {display}");
    if save && !thinking.is_empty() {
        match blocking(c, move |c| c.save_model_thinking(&thinking)).await {
            Ok(()) => message.push_str(". Saved to model."),
            Err(error) => message.push_str(&format!(", but the model was not saved: {error}")),
        }
    }
    Ok(message)
}

async fn amend_sandbox(
    c: &Controller,
    change: SandboxChange,
) -> Result<(String, Option<String>), String> {
    let summary = change.summary();
    let info = c.amend_sandbox(change.clone()).await?;
    let prompt = change.followup_prompt(info.summary());
    Ok((format!("Sandbox: {} ({summary})", info.summary()), prompt))
}
