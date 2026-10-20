//! Local-controller command handling and session or profile switches.

use super::*;

pub(super) async fn local_command(c: Arc<Controller>, command: Command) -> Result<Reply, String> {
    if let Command::Image(path) = command {
        return load_image(path).await;
    }
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
        Command::Image(_) | Command::Fork(_) | Command::SteeringState => unreachable!(),
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
        Command::Archive(path) => {
            let result = c.archive_session(&path).await?;
            history = true;
            format!("Archived: {}", result.path)
        }
        Command::SwitchProfile(profile) => {
            let message = switch_profile(c.clone(), profile).await?;
            history = true;
            message
        }
        Command::SwitchProfileThinking {
            profile,
            thinking,
            save,
        } => {
            let mut message = switch_profile(c.clone(), profile).await?;
            history = true;
            let thinking = match apply_thinking(c.clone(), thinking, save).await {
                Ok(message) => message,
                Err(error) => format!("/thinking: {error}. The profile switch was applied."),
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
        Command::AutoMode(args) => blocking(c.clone(), move |c| c.auto_mode_command(&args)).await?,
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
            let summary = SandboxChange::ExcludeCommand(entry.clone()).summary();
            let (message, followup) =
                amend_sandbox(&c, SandboxChange::ExcludeCommand(entry)).await?;
            prompt = followup;
            format!(
                "{message}\nSaved {summary} in {} and reloaded immediately (existing entries are not duplicated). Matching simple commands run outside the sandbox; other approval requirements still apply.",
                c.config_path().display()
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
        Command::AdmitInput(text) => match c.admit_active_input(&text)? {
            crate::app::ActiveInputAdmission::Steered => format!(
                "Guidance queued for the active task: {text}\nIt will be delivered at the next safe boundary."
            ),
            crate::app::ActiveInputAdmission::Idle => {
                prompt = Some(text);
                String::new()
            }
            crate::app::ActiveInputAdmission::ApprovalPending => {
                "The task is waiting for approval; respond to that approval before steering it."
                    .into()
            }
        },
        Command::MemoryReview { id, accept } => {
            blocking(c.clone(), move |c| {
                let (service, user, workspace) = c
                    .memory_manager()
                    .ok_or_else(|| "Memory review is unavailable.".to_string())?;
                let reference = [user, workspace]
                    .into_iter()
                    .find_map(|scope| {
                        let reference = CandidateRef {
                            scope,
                            id: id.clone(),
                        };
                        service.get_candidate(&reference).ok().map(|_| reference)
                    })
                    .ok_or_else(|| "candidate not found".to_string())?;
                c.review_memory(
                    &ReviewRequest {
                        reference,
                        decision: if accept {
                            ReviewDecision::Accept
                        } else {
                            ReviewDecision::Reject
                        },
                        edited: None,
                        target_revision: None,
                    },
                    "tui",
                )
                .map_err(|error| error.to_string())?;
                Ok(format!(
                    "{} {id}",
                    if accept { "accepted" } else { "rejected" }
                ))
            })
            .await?
        }
        Command::WithdrawSteer(id) => {
            c.withdraw_steering(id)?;
            "Queued input withdrawn.".into()
        }
        Command::Steer(text) => {
            c.steer(&text)?;
            format!("Guidance queued: {text}")
        }
        Command::Management(request) => {
            return blocking(c, move |c| c.management(request).map(Reply::Management)).await;
        }
        Command::Logout => {
            blocking(c.clone(), |c| {
                if c.auth_path().is_empty() {
                    return Err("sign-out is unavailable".into());
                }
                let removed = crate::auth::service::Service::new(c.auth_path().into())
                    .logout(&CancellationToken::new())
                    .map_err(|error| error.to_string())?;
                Ok(if removed {
                    "Signed out."
                } else {
                    "Not signed in."
                }
                .into())
            })
            .await?
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

async fn switch_profile(c: Arc<Controller>, profile: String) -> Result<String, String> {
    let result = c.switch_profile_result(&profile).await?;
    let saved = blocking(c.clone(), move |c| c.set_default_profile(&profile)).await;
    let info = c.info();
    let mut message = format!(
        "Switched to profile {} (provider {}, model {}).",
        info.profile, info.provider, info.model
    );
    match saved {
        Ok(()) => message.push_str(" Set as default profile."),
        Err(error) => message.push_str(&format!(" The default profile was not saved: {error}")),
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
        match blocking(c, move |c| c.save_profile_thinking(&thinking)).await {
            Ok(()) => message.push_str(". Saved to profile."),
            Err(error) => message.push_str(&format!(", but the profile was not saved: {error}")),
        }
    }
    Ok(message)
}

async fn amend_sandbox(
    c: &Controller,
    change: SandboxChange,
) -> Result<(String, Option<String>), String> {
    let widens = match &change {
        SandboxChange::AllowReadPath(_) | SandboxChange::ExcludeCommand(_) => true,
        SandboxChange::Network(mode) => mode == "allow",
    };
    let summary = change.summary();
    let info = c.amend_sandbox(change).await?;
    let prompt = widens.then(|| format!("The user changed the sandbox configuration: {summary} is now allowed. Sandbox: {}. If a command failed because of the sandbox, retry it now.", info.summary()));
    Ok((format!("Sandbox: {} ({summary})", info.summary()), prompt))
}
