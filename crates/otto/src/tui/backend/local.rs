//! Local-controller request handling and fork-point choices.

use super::*;

pub(super) fn check_scope(info: &Info, ticket: &Ticket) -> Result<(), String> {
    if info.workspace == ticket.workspace && info.session_id == ticket.session {
        Ok(())
    } else {
        Err("session changed; operation was not applied".into())
    }
}

pub(super) async fn blocking<T: Send + 'static>(
    controller: Arc<Controller>,
    f: impl FnOnce(&Controller) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(move || f(&controller))
        .await
        .map_err(|error| format!("backend task failed; reconcile state before retrying: {error}"))?
}

pub(super) async fn local(
    controller: Arc<Controller>,
    ticket: &Ticket,
    request: Request,
) -> Result<Reply, String> {
    let observer_scope = ticket.clone();
    match request {
        Request::Command(command) => {
            let input_id = match &command {
                Command::AdmitLocalInput { input_id, .. } => Some(input_id.clone()),
                _ => None,
            };
            if let Err(error) = check_scope(&controller.info(), ticket) {
                return Err(input_id.map_or(error.clone(), |id| {
                    format!(
                        "[{id}] {error}; recover in original session {} with /inputs",
                        ticket.session
                    )
                }));
            }
            let result = local_command(controller, command).await;
            result.map_err(|error| {
                input_id.map_or(error.clone(), |id| {
                    format!("[{id}] {error}; /inputs and /recover {id}; admission may be uncertain")
                })
            })
        }
        Request::Management(request) => {
            blocking(controller, move |c| {
                c.management(request).map(Reply::Management)
            })
            .await
        }
        request => {
            blocking(controller, move |c| match request {
                Request::ForkPoints => c.fork_points().map(|points| {
                    Reply::Sessions(fork_point_choices(
                        points
                            .into_iter()
                            .map(|point| (point.entry_id, point.prompt)),
                    ))
                }),
                Request::Sessions => c.list_sessions(50).map(|result| {
                    Reply::Sessions(
                        result
                            .sessions
                            .into_iter()
                            .map(|row| SessionChoice {
                                label: format!(
                                    "{}{}",
                                    if row.name.is_empty() {
                                        &row.id
                                    } else {
                                        &row.name
                                    },
                                    if row.current { " (current)" } else { "" }
                                ),
                                value: row.path,
                            })
                            .collect(),
                    )
                }),
                Request::Models => Ok(Reply::Models(
                    c.model_summaries()
                        .into_iter()
                        .map(|mut row| {
                            row.thinking = c.model_effective_thinking(&row.selector);
                            row
                        })
                        .collect(),
                )),
                Request::Context => c
                    .context_report()
                    .map(Reply::Context)
                    .ok_or_else(|| "no session is open".into()),
                Request::SandboxPath(path) => {
                    c.resolve_sandbox_read_path(&path).map(Reply::SandboxPath)
                }
                Request::Agents(query) => c.builder().tasks_list(&query).map(Reply::Agents),
                Request::AgentDetail { parent, id } => {
                    let row = c
                        .builder()
                        .tasks_get(&parent, &id)?
                        .ok_or_else(|| "task not found".to_string())?;
                    let history = (!row.session_path.is_empty())
                        .then(|| crate::session::Store::read_transcript(&row.session_path).ok())
                        .flatten();
                    let missing = history.is_none();
                    Ok(Reply::AgentDetail {
                        row,
                        history: history.unwrap_or_default(),
                        missing,
                    })
                }
                Request::ObserverSnapshot => {
                    check_scope(&c.info(), &observer_scope)?;
                    c.observer_snapshot().map(Reply::ObserverSnapshot)
                }
                Request::Reviews => {
                    let snapshot = c.approval_snapshot().map_err(|error| error.to_string())?;
                    let crate::app::management::Reply::MemoryCandidates(rows) =
                        c.management(crate::app::management::Request::MemoryReviewList)?
                    else {
                        return Err("unexpected memory review reply".into());
                    };
                    Ok(Reply::Reviews(
                        rows.into_iter()
                            .filter(|candidate| {
                                crate::approval::memory::is_human_pending(
                                    candidate,
                                    &snapshot.requests,
                                )
                            })
                            .collect(),
                    ))
                }
                Request::ApprovalSnapshot => c
                    .approval_snapshot()
                    .map(Reply::ApprovalSnapshot)
                    .map_err(|error| error.to_string()),
                Request::ApprovalActivity => {
                    let scope = c.approval_scope();
                    c.approval_snapshot().map_err(|error| error.to_string())?;
                    c.approval_service()
                        .activity_snapshot(&scope.workspace, Some(&scope.session_id))
                        .map(Reply::ApprovalActivity)
                        .map_err(|error| error.to_string())
                }
                Request::Command(_) | Request::Management(_) => unreachable!(),
            })
            .await
        }
    }
}

pub(super) fn decide(
    c: &Controller,
    id: &str,
    allow: bool,
    feedback: &str,
    revision: Option<u64>,
) -> Result<(), String> {
    let scope = c.approval_scope();
    let request = c
        .approval_service()
        .get(id, &scope.workspace, Some(&scope.session_id))
        .map_err(|error| error.to_string())?;
    c.approval_service()
        .decide(
            id,
            &request.scope,
            revision.unwrap_or(request.revision),
            allow,
            feedback,
            &crate::approval::Actor::local("tui"),
        )
        .map_err(|error| error.to_string())?;
    c.approval_service()
        .apply_available(&scope.workspace)
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Picker rows for fork points given oldest first: newest first, labelled
/// with the prompt's first line.
pub(crate) fn fork_point_choices(
    points: impl DoubleEndedIterator<Item = (String, String)>,
) -> Vec<SessionChoice> {
    points
        .rev()
        .map(|(entry_id, prompt)| SessionChoice {
            label: prompt
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect(),
            value: entry_id,
        })
        .collect()
}

pub(super) fn forked_message(path: &str) -> String {
    format!("Forked: {path}\nThe prompt is in the editor; edit it or send it as is.")
}
