//! Owned asynchronous work for the terminal UI. Completions retain their source scope.
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use otto_core::agent::context_report::ContextReport;
use otto_core::model::{Block, Message};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::attach::Remote;
use crate::app::{Controller, Info, ProfileSummary};
use crate::memory::{
    Candidate, CandidateListRequest, CandidateRef, CandidateState, ReviewDecision, ReviewRequest,
};
use crate::runtime::sandbox_config::SandboxChange;
use crate::subagent::record::{ListQuery, ListResult, TaskRow};

#[derive(Clone)]
pub(crate) enum Connection {
    Local(Arc<Controller>),
    Attach(Arc<Remote>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Ticket {
    pub workspace: String,
    pub session: String,
    pub surface: u64,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum Request {
    Sessions,
    /// The current session's fork points, newest first, as session choices
    /// whose value is the prompt's entry id.
    ForkPoints,
    Profiles,
    Context,
    /// The current session's span tree.
    Trace,
    /// The text of one span of the trace.
    TraceSpan(String),
    SandboxPath(String),
    Agents(ListQuery),
    AgentDetail {
        parent: String,
        id: String,
    },
    Reviews,
    ApprovalSnapshot,
    ApprovalActivity,
    Management(crate::app::management::Request),
    Command(Command),
}

#[derive(Debug, Clone)]
pub(crate) enum Command {
    Image(String),
    NewSession,
    Resume(String),
    Archive(String),
    /// Fork the current session before the prompt with this entry id and
    /// switch to the fork.
    Fork(String),
    SwitchProfile(String),
    SwitchProfileThinking {
        profile: String,
        thinking: String,
        save: bool,
    },
    SetThinking {
        thinking: String,
        save: bool,
    },
    Rename(String),
    AutoMode(String),
    Permissions(String),
    SandboxReload,
    SandboxAllow(String),
    SandboxNetwork(String),
    SandboxExclude(String),
    DecideApproval {
        id: String,
        allow: bool,
        feedback: String,
        revision: Option<u64>,
    },
    Approve(String),
    Deny(String),
    ApproveAlways(String),
    MemoryReview {
        id: String,
        accept: bool,
    },
    /// Admit ordinary input to the active turn rather than creating a queued turn.
    AdmitInput(String),
    /// Explicit guidance for the active task, never a start of a new turn.
    Steer(String),
    /// The active run's guidance items; the caller repaints the queued one
    /// from this read rather than from what it sent.
    SteeringState,
    /// Withdraw one guidance item that is still queued.
    WithdrawSteer(u64),
    Management(crate::app::management::Request),
    Logout,
}

#[derive(Debug)]
pub(crate) struct SessionChoice {
    pub value: String,
    pub label: String,
}

#[derive(Debug)]
pub(crate) enum Reply {
    Image {
        path: String,
        image: Block,
    },
    Sessions(Vec<SessionChoice>),
    Profiles(Vec<ProfileSummary>),
    Context(ContextReport),
    Trace(otto_core::session::trace::Trace),
    TraceSpan(otto_core::session::trace::SpanContent),
    SandboxPath(String),
    Agents(ListResult),
    AgentDetail {
        row: TaskRow,
        history: Vec<Message>,
        missing: bool,
    },
    Reviews(Vec<Candidate>),
    ApprovalSnapshot(otto_core::approval::Snapshot),
    ApprovalActivity(crate::approval::ApprovalActivity),
    Steering(Vec<otto_core::agent::steering::Message>),
    Management(crate::app::management::Reply),
    Operation {
        message: String,
        info: Info,
        history: Option<Vec<Message>>,
        prompt: Option<String>,
    },
    /// The switch to a fork; `draft` is the prompt the fork stops before,
    /// placed in the editor rather than sent.
    Forked {
        message: String,
        info: Info,
        history: Vec<Message>,
        draft: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) struct Completion {
    pub mutation: bool,
    pub ticket: Ticket,
    pub result: Result<Reply, String>,
}

pub(crate) struct Driver {
    connection: Connection,
    tx: mpsc::UnboundedSender<Completion>,
    rx: mpsc::UnboundedReceiver<Completion>,
    tasks: Vec<(bool, JoinHandle<()>)>,
    reads: Arc<Mutex<HashSet<Ticket>>>,
    writes: Arc<tokio::sync::Mutex<()>>,
}

impl Driver {
    pub fn new(connection: Connection) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            connection,
            tx,
            rx,
            tasks: Vec::new(),
            reads: Arc::new(Mutex::new(HashSet::new())),
            writes: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn submit(&mut self, ticket: Ticket, request: Request) {
        let read = !matches!(request, Request::Command(_));
        if read
            && !self
                .reads
                .lock()
                .expect("backend reads")
                .insert(ticket.clone())
        {
            return;
        }
        self.tasks.retain(|(_, task)| !task.is_finished());
        let connection = self.connection.clone();
        let tx = self.tx.clone();
        let reads = self.reads.clone();
        let writes = self.writes.clone();
        self.tasks.push((
            !read,
            tokio::spawn(async move {
                let _write = if read {
                    None
                } else {
                    Some(writes.lock().await)
                };
                let result = match connection {
                    Connection::Local(controller) => local(controller, &ticket, request).await,
                    Connection::Attach(remote) => remote_request(remote, &ticket, request).await,
                };
                if read {
                    reads.lock().expect("backend reads").remove(&ticket);
                }
                let _ = tx.send(Completion {
                    mutation: !read,
                    ticket,
                    result,
                });
            }),
        ));
    }

    pub async fn next(&mut self) -> Completion {
        self.rx
            .recv()
            .await
            .expect("driver keeps completion sender")
    }

    pub fn try_next(&mut self) -> Option<Completion> {
        self.rx.try_recv().ok()
    }

    /// Call during clean shutdown; effects already admitted are never replayed.
    pub async fn drain_pending_writes(&mut self) -> Vec<Completion> {
        for (mutation, task) in &mut self.tasks {
            if *mutation {
                let _ = task.await;
            }
        }
        self.tasks.retain(|(_, task)| !task.is_finished());
        let mut completions = Vec::new();
        while let Some(completion) = self.try_next() {
            completions.push(completion);
        }
        completions
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        for (mutation, task) in &self.tasks {
            if !mutation {
                task.abort();
            }
        }
    }
}

async fn load_image(path: String) -> Result<Reply, String> {
    tokio::task::spawn_blocking(move || {
        super::image_block_from_path(&path).map(|image| Reply::Image { path, image })
    })
    .await
    .map_err(|_| "image reader failed".to_string())?
}

fn check_scope(info: &Info, ticket: &Ticket) -> Result<(), String> {
    if info.workspace == ticket.workspace && info.session_id == ticket.session {
        Ok(())
    } else {
        Err("session changed; operation was not applied".into())
    }
}

async fn blocking<T: Send + 'static>(
    controller: Arc<Controller>,
    f: impl FnOnce(&Controller) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(move || f(&controller))
        .await
        .map_err(|error| format!("backend task failed; reconcile state before retrying: {error}"))?
}

async fn local(
    controller: Arc<Controller>,
    ticket: &Ticket,
    request: Request,
) -> Result<Reply, String> {
    match request {
        Request::Command(command) => {
            check_scope(&controller.info(), ticket)?;
            local_command(controller, command).await
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
                Request::Profiles => Ok(Reply::Profiles(
                    c.profile_summaries()
                        .into_iter()
                        .map(|mut row| {
                            row.thinking = c.profile_effective_thinking(&row.name);
                            row
                        })
                        .collect(),
                )),
                Request::Context => c
                    .context_report()
                    .map(Reply::Context)
                    .ok_or_else(|| "no session is open".into()),
                Request::Trace => c.trace().map(Reply::Trace),
                Request::TraceSpan(span_id) => c
                    .trace_span(&span_id)?
                    .map(Reply::TraceSpan)
                    .ok_or_else(|| "span not found".to_string()),
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
                Request::Reviews => {
                    let snapshot = c.approval_snapshot().map_err(|error| error.to_string())?;
                    let (service, user, workspace) = c
                        .memory_manager()
                        .ok_or_else(|| "Memory review is unavailable.".to_string())?;
                    service
                        .list_candidates(&CandidateListRequest {
                            scopes: vec![user, workspace],
                            states: vec![CandidateState::Pending],
                            limit: 20,
                            cursor: String::new(),
                        })
                        .map(|page| {
                            Reply::Reviews(
                                page.candidates
                                    .into_iter()
                                    .filter(|candidate| {
                                        crate::approval::memory::is_human_pending(
                                            candidate,
                                            &snapshot.requests,
                                        )
                                    })
                                    .collect(),
                            )
                        })
                        .map_err(|error| error.to_string())
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

fn decide(
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

fn forked_message(path: &str) -> String {
    format!("Forked: {path}\nThe prompt is in the editor; edit it or send it as is.")
}

async fn local_command(c: Arc<Controller>, command: Command) -> Result<Reply, String> {
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

async fn remote_request(
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
                generated: None,
            }
        }
        Request::SkillSet { name, enabled } => ManagementReply::SkillSet(
            remote
                .client
                .set_skill_enabled(session, &name, enabled)
                .await
                .map_err(mutation_error)?,
        ),
        Request::ReflectList => ManagementReply::ReflectList(
            remote
                .client
                .reflection_runs(session)
                .await
                .map_err(|e| e.to_string())?
                .runs,
        ),
        Request::ReflectShow(id) => ManagementReply::ReflectShow {
            log: remote
                .client
                .reflection_run(session, &id)
                .await
                .map_err(|e| e.to_string())?
                .map(|log| (log.path, log.content)),
            id,
        },
        Request::SkillRevert(name) => ManagementReply::Notice(
            remote
                .client
                .revert_generated_skill(session, &name)
                .await
                .map_err(mutation_error)?,
        ),
        _ => return Err("not available with --attach".into()),
    };
    Ok(Reply::Management(reply))
}

#[cfg(test)]
mod tests;
