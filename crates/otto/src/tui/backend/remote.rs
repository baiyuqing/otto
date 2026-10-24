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
    if let Request::Command(Command::Image(path)) = request {
        return load_image(path).await;
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
        Request::Trace => client
            .trace(session)
            .await
            .map(Reply::Trace)
            .map_err(|e| e.to_string()),
        Request::TraceSpan(span_id) => client
            .trace_span(session, &span_id)
            .await
            .map_err(|e| e.to_string())?
            .map(Reply::TraceSpan)
            .ok_or_else(|| "span not found".to_string()),
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
        Request::Command(command) => {
            if let Command::Image(path) = command {
                return load_image(path).await;
            }

            let mut history = false;
            let mut prompt = None;
            let message = match command {
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
                Command::Rename(name) => {
                    client
                        .rename_session(session, &name)
                        .await
                        .map_err(mutation_error)?;
                    format!("Session name: {name}")
                }
                Command::AutoMode(args) => {
                    client.auto_mode_command(session, &args).await.map_err(mutation_error)?
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
                Command::Steer(text) => {
                    client
                        .steer(session, &text)
                        .await
                        .map_err(mutation_error)?;
                    format!("Guidance queued: {text}")
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
        _ => return Err("not available with --attach".into()),
    };
    Ok(Reply::Management(reply))
}
