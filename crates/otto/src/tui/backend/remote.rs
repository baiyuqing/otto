//! Requests and commands forwarded to an attached server.

use super::*;

pub(super) async fn remote_request(
    remote: Arc<Remote>,
    ticket: &Ticket,
    request: Request,
) -> Result<Reply, String> {
    let command = matches!(request, Request::Command(_));
    if command {
        check_scope(&remote.info(), ticket)?;
    }
    tokio::time::timeout(
        Duration::from_secs(5),
        remote_call(&remote, ticket, request),
    )
    .await
    .map_err(|_| {
        if command {
            "request timed out; outcome unknown, reconcile state before retrying".to_string()
        } else {
            "request timed out".to_string()
        }
    })?
}

async fn remote_call(remote: &Remote, ticket: &Ticket, request: Request) -> Result<Reply, String> {
    let client = &remote.client;
    let session = &ticket.session;
    match request {
        Request::Sessions => Ok(Reply::Sessions(
            client
                .list_sessions(&ticket.workspace)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .take(50)
                .map(|row| SessionChoice {
                    label: format!(
                        "{}{}",
                        if row.name.is_empty() {
                            &row.id
                        } else {
                            &row.name
                        },
                        if row.id == *session { " (current)" } else { "" }
                    ),
                    value: row.id,
                })
                .collect(),
        )),
        Request::ForkPoints => Ok(Reply::Sessions(fork_point_choices(
            client
                .fork_points(session)
                .await
                .map_err(|e| e.to_string())?
                .into_iter(),
        ))),
        Request::Context => client
            .context(session)
            .await
            .map(Reply::Context)
            .map_err(|e| e.to_string()),
        Request::ObserverSnapshot => client
            .observer_snapshot(session)
            .await
            .map(Reply::ObserverSnapshot)
            .map_err(|error| error.to_string()),
        Request::Agents(query) => client
            .tasks(&query)
            .await
            .map(Reply::Agents)
            .map_err(|e| e.to_string()),
        Request::AgentDetail { parent, id } => {
            let detail = client
                .task_detail(&parent, &id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "task not found".to_string())?;
            Ok(Reply::AgentDetail {
                row: detail.task,
                history: detail.history,
                missing: detail.transcript_missing,
            })
        }
        Request::ApprovalActivity => client
            .approval_activity(session)
            .await
            .map(Reply::ApprovalActivity)
            .map_err(|e| e.to_string()),
        Request::ApprovalSnapshot => {
            let value = client
                .shared_approval("_otto/approvals/list", session, "", 0, "", "", 0)
                .await
                .map_err(|e| e.to_string())?;
            serde_json::from_value(value)
                .map(Reply::ApprovalSnapshot)
                .map_err(|_| "invalid approval snapshot".into())
        }
        Request::Management(request) => remote_management(remote, session, request).await,
        Request::Models => client
            .model_summaries()
            .await
            .map(Reply::Models)
            .map_err(|error| error.to_string()),
        Request::Command(command) => {
            let mut history = false;
            let mut prompt = None;
            let message = match command {
                Command::MemoryInvocation { name, args } => {
                    history = true;
                    client.memory_command(session, &name, &args).await.map_err(mutation_error)?;
                    String::new()
                }
                Command::NewSession => {
                    remote
                        .open(Some(remote.workspace()), None)
                        .await
                        .map_err(mutation_error)?;
                    history = true;
                    format!("Session: {}", remote.session())
                }
                Command::Resume(id) => {
                    remote.open(None, Some(&id)).await.map_err(mutation_error)?;
                    history = true;
                    format!("Session: {}", remote.session())
                }
                Command::Fork(entry_id) => {
                    let (fork, draft) = client
                        .fork_session(session, Some(&entry_id))
                        .await
                        .map_err(mutation_error)?;
                    remote.open(None, Some(&fork)).await.map_err(mutation_error)?;
                    remote.refresh_info().await.map_err(|e| {
                        format!("operation completed but session refresh failed: {e}")
                    })?;
                    return Ok(Reply::Forked {
                        message: forked_message(&fork),
                        info: remote.info(),
                        history: remote.history(),
                        draft,
                    });
                }
                Command::SwitchModel(model) => {
                    let info = client
                        .switch_model(session, &model)
                        .await
                        .map_err(mutation_error)?;
                    history = true;
                    switched_model_message(&info)
                }
                Command::SetThinking { thinking, save } => {
                    attached_thinking_message(client, session, thinking, save).await?
                }
                Command::SwitchModelThinking {
                    model,
                    thinking,
                    save,
                } => {
                    let info = client
                        .switch_model(session, &model)
                        .await
                        .map_err(mutation_error)?;
                    history = true;
                    let mut message = switched_model_message(&info);
                    let thinking = match attached_thinking_message(client, session, thinking, save).await {
                        Ok(message) => message,
                        Err(error) => format!("/thinking: {error}. The model switch was applied."),
                    };
                    message.push('\n');
                    message.push_str(&thinking);
                    message
                }
                Command::Archive(target) => {
                    let archived_path = client
                        .archive_session(&target)
                        .await
                        .map_err(mutation_error)?;
                    let mut message = format!("Archived: {archived_path}");
                    history = retired_current_session(remote, &target, &mut message).await?;
                    message
                }
                Command::Delete(target) => {
                    client
                        .delete_session_permanently(&target)
                        .await
                        .map_err(mutation_error)?;
                    let mut message = "Session permanently deleted.".to_string();
                    history = retired_current_session(remote, &target, &mut message).await?;
                    message
                }
                Command::Rename(name) => {
                    client
                        .rename_session(session, &name)
                        .await
                        .map_err(mutation_error)?;
                    format!("Session name: {name}")
                }
                Command::Permissions(args) => {
                    client.permissions_command(session, &args).await.map_err(mutation_error)?
                }
                Command::SandboxReload => format!(
                    "Sandbox: {}",
                    client.reload_sandbox().await.map_err(mutation_error)?
                ),
                Command::DecideApproval {
                    id,
                    allow,
                    feedback,
                    revision,
                } => {
                    let revision = match revision {
                        Some(value) => value,
                        None => client
                            .shared_approval("_otto/approvals/get", session, &id, 0, "", "", 0)
                            .await
                            .map_err(mutation_error)?["revision"]
                            .as_u64()
                            .ok_or_else(|| "invalid approval revision".to_string())?,
                    };
                    client
                        .shared_approval(
                            "_otto/approvals/decide",
                            session,
                            &id,
                            revision,
                            if allow { "approve" } else { "reject" },
                            &feedback,
                            0,
                        )
                        .await
                        .map_err(mutation_error)?;
                    format!("Decision recorded for {id}.")
                }
                Command::Approve(id) => {
                    client
                        .decide_approval(session, &id, true)
                        .await
                        .map_err(mutation_error)?;
                    format!("Approved {id}.")
                }
                Command::Deny(id) => {
                    client
                        .decide_approval(session, &id, false)
                        .await
                        .map_err(mutation_error)?;
                    format!("Denied {id}.")
                }
                Command::Management(request) => {
                    return remote_management(remote, session, request).await;
                }
                Command::AdmitLocalInput { .. } => return Err("local input command is unavailable on attached sessions".into()),
                Command::AdmitInput(text) => match client
                    .admit_input(session, &text, None)
                    .await
                    .map_err(mutation_error)?
                {
                    crate::app::ActiveInputAdmission::Steered => format!(
                        "Guidance queued for the active task: {text}\nIt will be delivered at the next safe boundary."
                    ),
                    crate::app::ActiveInputAdmission::Idle => {
                        prompt = Some(text);
                        String::new()
                    }
                    crate::app::ActiveInputAdmission::ApprovalPending => {
                        "The task is waiting for approval; respond to that approval before steering it.".into()
                    }
                },
                Command::WithdrawSteer(id) => {
                    client
                        .withdraw_steering(session, id)
                        .await
                        .map_err(mutation_error)?;
                    "Queued input withdrawn.".into()
                }
                Command::InterruptStep => {
                    client
                        .interrupt_steering(session)
                        .await
                        .map_err(mutation_error)?;
                    String::new()
                }
                // Reads rather than reports: the caller paints the queued
                // guidance from this list. An unreachable serve changed
                // nothing here, so the error carries no unknown outcome.
                Command::SteeringState => {
                    return client
                        .steering_messages(session)
                        .await
                        .map(Reply::Steering)
                        .map_err(|error| error.to_string());
                }
                _ => return Err("not available with --attach".into()),
            };
            remote
                .refresh_info()
                .await
                .map_err(|e| format!("operation completed but session refresh failed: {e}"))?;
            Ok(Reply::Operation {
                message,
                info: remote.info(),
                history: history.then(|| remote.history()),
                prompt,
            })
        }
        _ => Err("not available with --attach".into()),
    }
}

/// After `/archive` or `/delete` of `target`: when it is the attached session,
/// opens a new session in the workspace, as `/new` does, and appends its id to
/// `message`. Returns whether the history was replaced.
async fn retired_current_session(
    remote: &Remote,
    target: &str,
    message: &mut String,
) -> Result<bool, String> {
    if target != remote.session() {
        return Ok(false);
    }
    remote
        .open(Some(remote.workspace()), None)
        .await
        .map_err(|error| {
            format!(
                "session retired but opening a new session failed: {}",
                mutation_error(error)
            )
        })?;
    message.push_str(&format!("\nSession: {}", remote.session()));
    Ok(true)
}

fn switched_model_message(info: &crate::app::Info) -> String {
    format!(
        "Switched to model {} (type {}).",
        info.model_selector, info.provider_type
    )
}

/// Applies a non-empty thinking level to the attached session and reports the
/// level in force. An attached session never writes the configuration file,
/// so `save` only adds a notice that nothing was saved.
async fn attached_thinking_message(
    client: &crate::client::Client,
    session: &str,
    thinking: String,
    save: bool,
) -> Result<String, String> {
    let info = if thinking.is_empty() {
        client.session(session).await
    } else {
        client.set_thinking(session, &thinking).await
    }
    .map_err(mutation_error)?;
    let display = if info.thinking.is_empty() {
        "default"
    } else {
        &info.thinking
    };
    let mut message = format!("Thinking: {display}");
    if save && !thinking.is_empty() {
        message.push_str(". Not saved: attached sessions change the session only.");
    }
    Ok(message)
}

fn mutation_error(error: crate::client::Error) -> String {
    match error {
        crate::client::Error::Unreachable(_) => {
            format!("{error}; outcome unknown, reconcile state before retrying")
        }
        _ => error.to_string(),
    }
}

async fn remote_management(
    remote: &Remote,
    session: &str,
    request: crate::app::management::Request,
) -> Result<Reply, String> {
    use crate::app::management::{Reply as ManagementReply, Request};
    let reply = match request {
        Request::SkillManager => ManagementReply::SkillManager(
            remote
                .client
                .skill_manager(session)
                .await
                .map_err(|e| e.to_string())?,
        ),
        Request::SkillInfo(name) => ManagementReply::SkillInfo(
            remote
                .client
                .skill_info(session, &name)
                .await
                .map_err(|e| e.to_string())?,
        ),
        Request::SkillDetail(name) => {
            let info = remote
                .client
                .skill_info(session, &name)
                .await
                .map_err(|e| e.to_string())?;
            ManagementReply::SkillDetail {
                skill: info.skill,
                body: info.body,
                contract_check: info.contract_check,
            }
        }
        Request::SkillSet { name, enabled } => ManagementReply::SkillSet(
            remote
                .client
                .set_skill_enabled(session, &name, enabled)
                .await
                .map_err(mutation_error)?,
        ),
        Request::Tasks => ManagementReply::Tasks(
            remote
                .client
                .session_task_records(session)
                .await
                .map_err(|e| e.to_string())?,
        ),
        Request::Task(id) => ManagementReply::Task(
            remote
                .client
                .session_task_record(session, &id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "task not found".to_string())?,
        ),
        Request::TaskCancel(id) => {
            remote
                .client
                .cancel_task(session, &id)
                .await
                .map_err(mutation_error)?;
            ManagementReply::TaskCancelled(id)
        }
        Request::Timers => ManagementReply::Timers(
            remote
                .client
                .timers(session)
                .await
                .map_err(|e| e.to_string())?,
        ),
        Request::TimerCancel(id) => ManagementReply::TimerCancelled(
            remote
                .client
                .cancel_timer(session, &id)
                .await
                .map_err(mutation_error)?,
        ),
        Request::McpStatus => ManagementReply::McpStatus(
            remote
                .client
                .mcp_servers(session)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(crate::mcp::ServerStatus::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        _ => return Err("not available with --attach".into()),
    };
    Ok(Reply::Management(reply))
}
