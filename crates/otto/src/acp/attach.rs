//! `otto acp --attach`: one ACP connection forwarded to `otto serve`.
//!
//! The relay opens no session, runs no model or tool and reads no provider
//! credentials; every session operation is an HTTP request on serve's Unix
//! socket (see `crate::client`). It shares the transport, dispatcher, request
//! parsing and update mapping with local `otto acp`; only [`Relay`] differs.
//!
//! Prompts: `session/prompt` posts a queued turn and reads that turn's SSE
//! stream until `turn_end`. `session/cancel` reaches the prompt through the
//! token the read loop installs, so it works before the turn id is known: the
//! prompt task posts the turn cancel as soon as it holds the id, whether the
//! turn is queued or running.
//!
//! Approvals: `approval_requested` becomes a `session/request_permission`
//! request while the stream keeps being read. The client's answer posts the
//! decision (`allow` only for `allow_once`). If serve reports the decision
//! first (`approval_decided`, from another client or a timeout) or the turn
//! ends, the relay sends `$/cancel_request` for the open request and ignores
//! the client's later answer.
//!
//! Loss: a connection error, or a stream that ends without `turn_end`, sets
//! the `lost` token. Every open prompt answers `connection to otto serve
//! lost`, the connection stops and [`run`] returns 1.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::v1::{PromptResponse, SessionInfo, StopReason};
use otto_core::model::Message;
use otto_core::wire::events::WireEvent;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::approval::{decide, permission_params};
use super::{
    Backend, Connection, INTERNAL_ERROR, PromptSlot, Reply, TITLE_CHARS, drive, error, result,
    unknown_session, update,
};
use crate::app::ApprovalDecision;
use crate::app::Channel;
use crate::client::{Client, Error as ClientError};
use crate::schedule::Origin;

const LOST: &str = "connection to otto serve lost";

/// A `session/request_permission` request waiting for the client's answer.
struct OpenApproval {
    approval_id: String,
    request_id: u64,
    reply: oneshot::Receiver<Value>,
}

pub(super) struct Relay {
    pub(super) client: Client,
    pub(super) server: Option<Arc<crate::server::Server>>,
    /// The store this connection delivers scheduled items from.
    pub(super) schedule: Option<Arc<crate::schedule::Store>>,
    /// Sessions this connection opened, with their prompt slots.
    sessions: Mutex<HashMap<String, Arc<PromptSlot>>>,
    lost: CancellationToken,
}

impl Relay {
    /// A relay whose server is `router`, for tests of what the relay sends.
    #[cfg(test)]
    pub(super) fn with_router(router: axum::Router) -> Self {
        Self {
            client: Client::in_process(router),
            server: None,
            schedule: None,
            sessions: Mutex::new(HashMap::new()),
            lost: CancellationToken::new(),
        }
    }

    /// A relay to no server, for tests of connection-level behavior.
    #[cfg(test)]
    pub(super) fn unreachable(schedule: Option<Arc<crate::schedule::Store>>) -> Self {
        Self {
            client: Client::new(Path::new("/nonexistent/otto.sock")).expect("client"),
            server: None,
            schedule,
            sessions: Mutex::new(HashMap::new()),
            lost: CancellationToken::new(),
        }
    }

    pub(super) fn slot(&self, id: &str) -> Option<Arc<PromptSlot>> {
        self.sessions.lock().expect("session map").get(id).cloned()
    }

    pub(super) fn register(&self, id: &str) {
        self.sessions
            .lock()
            .expect("session map")
            .entry(id.to_string())
            .or_default();
    }

    /// The JSON-RPC error for a failed call. A connection failure also marks
    /// serve lost.
    pub(super) fn fail(&self, failure: ClientError) -> agent_client_protocol_schema::v1::Error {
        match failure {
            ClientError::Unreachable(_) => {
                self.lost.cancel();
                error(INTERNAL_ERROR, LOST)
            }
            ClientError::Http {
                message, status, ..
            } if message.is_empty() => {
                error(INTERNAL_ERROR, format!("otto serve answered HTTP {status}"))
            }
            ClientError::Http { message, .. } => error(INTERNAL_ERROR, message),
        }
    }

    pub(super) fn task_control_error(
        &self,
        failure: ClientError,
    ) -> agent_client_protocol_schema::v1::Error {
        match &failure {
            ClientError::Http {
                status: 404, code, ..
            } if code.is_empty() => super::task_control_error(failure),
            _ => self.fail(failure),
        }
    }

    pub(super) async fn new_session(&self, workspace: &Path) -> Reply {
        let id = self
            .client
            .open_session(Some(&workspace.to_string_lossy()), None)
            .await
            .map_err(|failure| self.fail(failure))?;
        self.register(&id);
        result(agent_client_protocol_schema::v1::NewSessionResponse::new(
            id,
        ))
    }

    /// Resumes the session on serve and returns its history. A 404 is the
    /// same `unknown sessionId` error local `otto acp` returns.
    pub(super) async fn load_session(
        &self,
        id: &str,
        workspace: &Path,
    ) -> Result<Vec<Message>, agent_client_protocol_schema::v1::Error> {
        let resumed = self
            .client
            .open_session(Some(&workspace.to_string_lossy()), Some(id))
            .await;
        match resumed {
            Ok(_) => {}
            Err(ClientError::Http { status: 404, .. }) => return Err(unknown_session()),
            Err(failure) => return Err(self.fail(failure)),
        }
        let history = self
            .client
            .history(id, None)
            .await
            .map_err(|failure| self.fail(failure))?;
        self.register(id);
        Ok(history)
    }

    pub(super) async fn list_sessions(
        &self,
        workspace: &Path,
    ) -> Result<Vec<SessionInfo>, agent_client_protocol_schema::v1::Error> {
        let rows = self
            .client
            .list_sessions(&workspace.to_string_lossy())
            .await
            .map_err(|failure| self.fail(failure))?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let title = if row.name.is_empty() {
                    &row.last_user_text
                } else {
                    &row.name
                };
                let cwd = if row.workspace.is_empty() {
                    workspace.to_path_buf()
                } else {
                    PathBuf::from(&row.workspace)
                };
                let info = SessionInfo::new(row.id.clone(), cwd)
                    .title(title.chars().take(TITLE_CHARS).collect::<String>());
                if row.modified.is_empty() {
                    info
                } else {
                    info.updated_at(row.modified.clone())
                }
            })
            .collect())
    }

    /// Runs `/compact` or `/context` on serve and returns the reply text;
    /// `None` when serve cancelled the compaction at shutdown.
    pub(super) async fn run_command(
        &self,
        command: super::command::Command<'_>,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, agent_client_protocol_schema::v1::Error> {
        use super::command::{Command, compaction_text, context_text};
        let command = match command {
            Command::Reflect { focus } => {
                return self.reflect_command(session_id, focus, cancel).await;
            }
            Command::Skill { args } => return self.skill_command(session_id, args).await,
            Command::Steer { guidance } => {
                let accepted = self
                    .client
                    .steer(session_id, guidance)
                    .await
                    .map_err(|failure| self.task_control_error(failure))?;
                return Ok(Some(if accepted {
                    "Guidance accepted for the active task; it will be delivered at the next safe boundary."
                        .to_string()
                } else {
                    "The active task did not accept guidance.".to_string()
                }));
            }
            Command::Queue { .. } => {
                return Ok(Some(
                    "Nested /queue is not supported; submit a top-level /queue prompt".to_string(),
                ));
            }
            command => command,
        };
        let call = async {
            match command {
                Command::Context => self
                    .client
                    .context(session_id)
                    .await
                    .map(|report| Some(context_text(&report))),
                Command::Compact { focus } => self
                    .client
                    .compact(session_id, focus)
                    .await
                    .map(|done| done.map(|done| compaction_text(&done))),
                Command::Reflect { .. }
                | Command::Skill { .. }
                | Command::Steer { .. }
                | Command::Queue { .. } => unreachable!(),
            }
        };
        tokio::select! {
            biased;
            () = self.lost.cancelled() => Err(error(INTERNAL_ERROR, LOST)),
            () = cancel.cancelled() => Ok(None),
            answered = call => answered.map_err(|failure| self.fail(failure)),
        }
    }

    async fn reflect_command(
        &self,
        session_id: &str,
        focus: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, agent_client_protocol_schema::v1::Error> {
        let request_id = crate::memory::new_id().map_err(|failure| {
            error(
                INTERNAL_ERROR,
                format!("cannot create reflection request ID: {failure}"),
            )
        })?;
        let mut reflect = Box::pin(self.client.reflect(session_id, focus, &request_id));
        tokio::select! {
            biased;
            () = self.lost.cancelled() => Err(error(INTERNAL_ERROR, LOST)),
            result = &mut reflect => result
                .map(|report| report.map(|report| report.line))
                .map_err(|failure| self.fail(failure)),
            () = cancel.cancelled() => {
                self.client
                    .cancel_reflection(session_id, &request_id)
                    .await
                    .map_err(|failure| self.fail(failure))?;
                reflect.await
                    .map(|report| report.map(|report| report.line))
                    .map_err(|failure| self.fail(failure))
            }
        }
    }

    async fn skill_command(
        &self,
        session_id: &str,
        args: &str,
    ) -> Result<Option<String>, agent_client_protocol_schema::v1::Error> {
        match args.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["generated"] => {
                let list = self
                    .client
                    .generated_skills(session_id)
                    .await
                    .map_err(|failure| self.fail(failure))?;
                let text = if !list.enabled {
                    "skill generated: reflection is disabled".to_string()
                } else if list.skills.is_empty() {
                    "No skills generated by reflection.".to_string()
                } else {
                    let mut text = "Skills generated by reflection:".to_string();
                    for skill in list.skills {
                        let ownership = if skill.owned {
                            ""
                        } else {
                            " [edited or missing: reflection no longer owns it]"
                        };
                        text.push_str(&format!(
                            "\n- {}{ownership}: run {} (session {}) on {}: {}",
                            skill.name,
                            skill.run_id,
                            skill.session_id,
                            skill.updated_at,
                            skill.reason
                        ));
                    }
                    text
                };
                Ok(Some(text))
            }
            ["revert", name] => {
                let result = self
                    .client
                    .revert_generated_skill(session_id, name)
                    .await
                    .map_err(|failure| self.fail(failure))?;
                Ok(Some(match result.as_str() {
                    "restored" => format!(
                        "Reverted skill {name} to its previous version; the change applies to new sessions"
                    ),
                    "removed" => format!(
                        "Removed skill {name}, which reflection created; the change applies to new sessions"
                    ),
                    _ => format!("skill revert: unexpected result {result}"),
                }))
            }
            _ => Ok(Some(
                "Usage: /skill generated | /skill revert <name>".to_string(),
            )),
        }
    }

    /// Runs one prompt as a queued turn on serve, to its stop reason.
    pub(super) async fn prompt(
        &self,
        connection: &Connection,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
        origin: Option<&Origin>,
        cancel: &CancellationToken,
    ) -> Reply {
        let stream = tokio::select! {
            biased;
            () = self.lost.cancelled() => return Err(error(INTERNAL_ERROR, LOST)),
            started = self.client.start_turn_with_channel(session_id, text, None, channel, origin) => {
                started.map_err(|failure| self.fail(failure))?
            }
        };
        self.stream_turn(connection, session_id, stream, cancel)
            .await
    }

    /// Starts one separately admitted ACP `/queue` follow-up on serve.
    pub(super) async fn start_queued_prompt(
        &self,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
        origin: Option<&Origin>,
    ) -> Result<crate::client::TurnStream, agent_client_protocol_schema::v1::Error> {
        if self.lost.is_cancelled() {
            return Err(error(INTERNAL_ERROR, LOST));
        }
        let stream = self
            .client
            .start_turn_with_channel(session_id, text, None, channel, origin)
            .await
            .map_err(|failure| self.fail(failure))?;
        Ok(stream)
    }

    /// Forwards one already-started server turn as ACP session updates.
    pub(super) async fn stream_turn(
        &self,
        connection: &Connection,
        session_id: &str,
        mut stream: crate::client::TurnStream,
        cancel: &CancellationToken,
    ) -> Reply {
        let mut cancel_sent = false;
        let mut approval: Option<OpenApproval> = None;
        loop {
            let answer = async {
                match approval.as_mut() {
                    Some(open) => (&mut open.reply).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = self.lost.cancelled() => return Err(error(INTERNAL_ERROR, LOST)),
                () = cancel.cancelled(), if !cancel_sent => {
                    cancel_sent = true;
                    match self.client.cancel_turn(session_id, &stream.turn_id).await {
                        Err(failure @ ClientError::Unreachable(_)) => return Err(self.fail(failure)),
                        // The turn already ended (404): its `turn_end` follows.
                        Ok(()) | Err(ClientError::Http { .. }) => {}
                    }
                }
                reply = answer => {
                    let open = approval.take().expect("answer exists only for an open request");
                    let allow = reply.is_ok_and(|frame| decide(&frame) == ApprovalDecision::Allow);
                    match self.client.decide_approval(session_id, &open.approval_id, allow).await {
                        Err(failure @ ClientError::Unreachable(_)) => return Err(self.fail(failure)),
                        // 409 `approval_decided`: another client or the
                        // timeout decided first.
                        Ok(()) | Err(ClientError::Http { .. }) => {}
                    }
                }
                frame = stream.next() => {
                    let event = match frame {
                        Ok(Some(event)) => event,
                        Ok(None) | Err(_) => {
                            self.lost.cancel();
                            return Err(error(INTERNAL_ERROR, LOST));
                        }
                    };
                    match event.event_type.as_str() {
                        "approval_requested" => {
                            let params = permission_params(
                                session_id,
                                &event.tool_call_id,
                                &event.command,
                                &event.read_path,
                                &event.justification,
                            );
                            let (request_id, reply) =
                                connection.send_request("session/request_permission", params);
                            approval = Some(OpenApproval {
                                approval_id: event.approval_id,
                                request_id,
                                reply,
                            });
                        }
                        "approval_decided" => {
                            if approval
                                .as_ref()
                                .is_some_and(|open| open.approval_id == event.approval_id)
                            {
                                withdraw(connection, approval.take().expect("checked above"));
                            }
                        }
                        "turn_end" => {
                            if let Some(open) = approval.take() {
                                withdraw(connection, open);
                            }
                            return turn_end_reply(&event);
                        }
                        _ => {
                            if let Some(update) = update::event_update(&event) {
                                connection.update(session_id, update);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Withdraws an open permission request: the client gets `$/cancel_request`
/// and its later answer finds no pending entry.
fn withdraw(connection: &Connection, open: OpenApproval) {
    connection.forget_request(open.request_id);
    connection.send(json!({
        "jsonrpc": "2.0",
        "method": "$/cancel_request",
        "params": {"requestId": open.request_id},
    }));
}

fn turn_end_reply(event: &WireEvent) -> Reply {
    match event.status.as_str() {
        "ok" => result(PromptResponse::new(StopReason::EndTurn)),
        "canceled" => result(PromptResponse::new(StopReason::Cancelled)),
        "error" => Err(error(INTERNAL_ERROR, event.error.clone())),
        other => Err(error(
            INTERNAL_ERROR,
            format!("unexpected turn status {other:?}"),
        )),
    }
}

/// Serves ACP on `stdin`/`stdout` as a relay to the `otto serve` on `socket`
/// and returns the process exit code: 1 when serve is not reachable at start
/// or was lost later, else 0.
pub async fn run(
    socket: &Path,
    workspace: PathBuf,
    schedule: Option<Arc<crate::schedule::Store>>,
    stdin: Box<dyn BufRead + Send + 'static>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> i32 {
    let unreachable = |stderr: &mut (dyn Write + Send), failure: ClientError| {
        let _ = writeln!(
            stderr,
            "otto serve is not reachable at {}: {failure}",
            socket.display()
        );
        1
    };
    let client = match Client::new(socket) {
        Ok(client) => client,
        Err(failure) => return unreachable(stderr, failure),
    };
    if let Err(failure) = client.healthz().await {
        return unreachable(stderr, failure);
    }
    let lost = CancellationToken::new();
    let relay = Relay {
        client,
        server: None,
        schedule,
        sessions: Mutex::new(HashMap::new()),
        lost: lost.clone(),
    };
    drive(
        workspace,
        Backend::Attach(relay),
        stdin,
        stdout,
        &lost,
        cancel,
        |_| (),
    )
    .await;
    i32::from(lost.is_cancelled())
}

/// ACP served by the HTTP server process, sharing its router and controllers.
pub(super) async fn serve_in_process(
    server: Arc<crate::server::Server>,
    workspace: PathBuf,
    input: Box<dyn BufRead + Send>,
    output: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) {
    let lost = CancellationToken::new();
    let relay = Relay {
        client: Client::in_process(server.socket_router()),
        schedule: server.schedule_store(),
        server: Some(server),
        sessions: Mutex::new(HashMap::new()),
        lost: lost.clone(),
    };
    drive(
        workspace,
        Backend::Attach(relay),
        input,
        output,
        &lost,
        cancel,
        |_| (),
    )
    .await;
}
