//! `otto acp --attach`: one ACP connection forwarded to `otto serve`.
//!
//! The relay opens no session, runs no model or tool and reads no provider
//! credentials; every session operation is an HTTP request on serve's Unix
//! socket (see `otto::client`). It shares the transport, dispatcher, request
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
use otto::app::ApprovalDecision;
use otto::app::Channel;
use otto::client::{Client, Error as ClientError};
use otto::schedule::Origin;

const LOST: &str = "connection to otto serve lost";

/// A `session/request_permission` request waiting for the client's answer.
struct OpenApproval {
    confirmation: bool,
    revision: Option<u64>,
    approval_id: String,
    request_id: u64,
    reply: oneshot::Receiver<Value>,
}

pub(super) struct Relay {
    pub(super) client: Client,
    pub(super) server: Option<Arc<otto_server::Server>>,
    /// The store this connection delivers scheduled items from.
    pub(super) schedule: Option<Arc<dyn otto::schedule::ScheduleStore>>,
    /// The process log, for the connection's schedule worker.
    pub(super) logger: Arc<otto::log::Logger>,
    /// Sessions this connection opened, with their prompt slots.
    sessions: Mutex<HashMap<String, Arc<PromptSlot>>>,
    lost: CancellationToken,
}

impl Drop for Relay {
    /// Releases the sessions this connection pinned, so the server's idle
    /// sweeper can close them once nothing else holds them.
    fn drop(&mut self) {
        let Some(server) = &self.server else {
            return;
        };
        let sessions = self.sessions.lock().expect("session map");
        for id in sessions.keys() {
            server.unpin_session(id);
        }
    }
}

impl Relay {
    /// A relay whose server is `router`, for tests of what the relay sends.
    #[cfg(test)]
    pub(super) fn with_router(router: axum::Router) -> Self {
        Self {
            client: Client::in_process(router),
            server: None,
            schedule: None,
            logger: Arc::new(otto::log::Logger::discard()),
            sessions: Mutex::new(HashMap::new()),
            lost: CancellationToken::new(),
        }
    }

    /// A relay to no server, for tests of connection-level behavior.
    #[cfg(test)]
    pub(super) fn unreachable(schedule: Option<Arc<dyn otto::schedule::ScheduleStore>>) -> Self {
        Self {
            client: Client::new(Path::new("/nonexistent/otto.sock")).expect("client"),
            server: None,
            schedule,
            logger: Arc::new(otto::log::Logger::discard()),
            sessions: Mutex::new(HashMap::new()),
            lost: CancellationToken::new(),
        }
    }

    pub(super) fn slot(&self, id: &str) -> Option<Arc<PromptSlot>> {
        self.sessions.lock().expect("session map").get(id).cloned()
    }

    pub(super) fn register(&self, id: &str) {
        let newly = {
            let mut sessions = self.sessions.lock().expect("session map");
            let newly = !sessions.contains_key(id);
            sessions.entry(id.to_string()).or_default();
            newly
        };
        // A connection that holds a session keeps it open: the server's idle
        // sweeper must not close a session this frontend is still using.
        if newly && let Some(server) = &self.server {
            server.pin_session(id);
        }
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
                status: 400 | 403,
                code,
                message,
            } if matches!(
                code.as_str(),
                "INVALID_WORKSPACE" | "WORKSPACE_NOT_ADMITTED"
            ) =>
            {
                super::invalid_params(message)
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
    ) -> Result<
        (Vec<Message>, Vec<otto_core::session::PlanStep>),
        agent_client_protocol_schema::v1::Error,
    > {
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
        let plan = self
            .client
            .session(id)
            .await
            .map_err(|failure| self.fail(failure))?
            .plan;
        self.register(id);
        Ok((history, plan))
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
        use super::command::{
            Command, bare_model_hint, compaction_text, context_text, model_switched_text,
            model_text,
        };
        let command = match command {
            Command::Steer { guidance } => {
                let accepted = self
                    .client
                    .steer(session_id, guidance)
                    .await
                    .map_err(|failure| self.task_control_error(failure))?;
                return Ok(Some(format!(
                    "Guidance queued (#{}) for the active task: {}\nIt will be delivered at the next safe boundary.",
                    accepted.id, accepted.text
                )));
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
                Command::Memory { name, args } => self
                    .client
                    .memory_command(session_id, name, args)
                    .await
                    .map(Some),
                Command::Permissions { args } => self
                    .client
                    .permissions_command(session_id, args)
                    .await
                    .map(Some),
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
                Command::Model { selector } => {
                    if selector.is_empty() {
                        let available = self.client.models().await?;
                        return self.client.session(session_id).await.map(|info| {
                            Some(model_text(
                                &info.model_selector,
                                &info.provider_type,
                                &info.thinking,
                                &available,
                            ))
                        });
                    }
                    if !selector.contains('/') {
                        return Err(ClientError::Http {
                            status: 400,
                            code: "invalid_model".to_string(),
                            message: bare_model_hint(selector),
                        });
                    }
                    self.client
                        .switch_model(session_id, selector)
                        .await
                        .map(|info| {
                            Some(model_switched_text(
                                &info.model_selector,
                                &info.provider_type,
                            ))
                        })
                }
                Command::Steer { .. } | Command::Queue { .. } => {
                    unreachable!()
                }
            }
        };
        tokio::select! {
            biased;
            () = self.lost.cancelled() => Err(error(INTERNAL_ERROR, LOST)),
            () = cancel.cancelled() => Ok(None),
            answered = call => answered.map_err(|failure| self.fail(failure)),
        }
    }

    /// Runs one prompt as a queued turn on serve, to its stop reason.
    pub(super) async fn prompt(
        &self,
        connection: &Connection,
        session_id: &str,
        text: &str,
        (channel, origin): (Option<Channel>, Option<&Origin>),
        (request_id, input_id): (Option<&str>, Option<&str>),
        cancel: &CancellationToken,
    ) -> Reply {
        let input_client = connection.input_client(&self.client);
        let stream = tokio::select! {
            biased;
            () = self.lost.cancelled() => return Err(error(INTERNAL_ERROR, LOST)),
            started = input_client.start_turn_with_channel(session_id, text, None, channel, origin, request_id, input_id) => {
                started.map_err(|failure| self.fail(failure))?
            }
        };
        self.stream_turn(connection, session_id, stream, cancel)
            .await
    }

    /// Starts one separately admitted ACP `/queue` follow-up on serve.
    pub(super) async fn start_queued_prompt(
        &self,
        connection: &Connection,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
        origin: Option<&Origin>,
        (request_id, input_id): (Option<&str>, Option<&str>),
    ) -> Result<otto::client::TurnStream, agent_client_protocol_schema::v1::Error> {
        if self.lost.is_cancelled() {
            return Err(error(INTERNAL_ERROR, LOST));
        }
        let stream = connection
            .input_client(&self.client)
            .with_queued_input()
            .start_turn_with_channel(
                session_id, text, None, channel, origin, request_id, input_id,
            )
            .await
            .map_err(|failure| self.fail(failure))?;
        Ok(stream)
    }

    /// Forwards one already-started server turn as ACP session updates.
    pub(super) async fn stream_turn(
        &self,
        connection: &Connection,
        session_id: &str,
        mut stream: otto::client::TurnStream,
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
                    let frame=reply.ok();
                    let action=frame.as_ref().and_then(|frame|frame.pointer("/result/action")).and_then(Value::as_str);
                    if open.confirmation && !matches!(action,Some("accept"|"decline")) {continue}
                    let allow=if open.confirmation {action==Some("accept") && frame.as_ref().and_then(|f|f.pointer("/result/content/approved")).and_then(Value::as_bool)==Some(true)}
                        else {frame.as_ref().is_some_and(|frame|decide(frame)==ApprovalDecision::Allow)};
                    let feedback=if open.confirmation {frame.as_ref().and_then(|f|f.pointer("/result/content/feedback")).and_then(Value::as_str).unwrap_or("")}else{""};
                    let decision=if let Some(revision)=open.revision {
                        self.client.shared_approval("_otto/approvals/decide",session_id,&open.approval_id,revision,if allow {"approve"}else{"reject"},feedback,0).await.map(|_|())
                    } else {self.client.decide_approval(session_id,&open.approval_id,allow).await};
                    match decision {
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
                            let shared=self.client.shared_approval("_otto/approvals/get",session_id,&event.approval_id,0,"","",0).await.ok()
                                .and_then(|value|serde_json::from_value::<otto_core::approval::Request>(value).ok());
                            if shared.as_ref().is_some_and(|request| !request.is_human_pending()) { continue; }
                            let caps=connection.client_capabilities.lock().expect("ACP capabilities").clone();
                            if shared.is_some() && caps.pointer("/_meta/otto/approvals").is_some() {continue}
                            let confirmation=shared.as_ref().is_some_and(|r|r.kind==otto_core::approval::Kind::Confirmation);
                            let (method,mut params)=if confirmation {
                                if caps.pointer("/clientCapabilities/elicitation/form").is_none_or(Value::is_null) {
                                    connection.send(json!({"jsonrpc":"2.0",
                                        "method":"session/update",
                                        "params":{"sessionId":session_id,
                                        "update":{"sessionUpdate":"agent_message_chunk",
                                        "content":{"type":"text",
                                        "text":format!("Approval {} is pending. This client cannot display confirmation; use \
                                        Otto's approval UI.",event.approval_id)}}}}));
                                    continue;
                                }
                                let request=shared.as_ref().expect("confirmation has request");
                                ("elicitation/create",json!({"sessionId":session_id,
                                    "mode":"form",
                                    "message":format!("{}\n{}\n{}",request.title,request.subject.details,request.reason),
                                    "requestedSchema":{"type":"object",
                                    "properties":{"approved":{"type":"boolean"},
                                    "feedback":{"type":"string"}},
                                    "required":["approved"]}}))
                            } else {("session/request_permission",permission_params(session_id,&event.tool_call_id,&event.command,&event.read_path,&event.justification))};
                            if let Some(request) = &shared {
                                params["_meta"] = json!({"otto":{"approvalId":request.id,"revision":request.revision,"subjectVersion":request.subject.version}});
                            }
                            let (request_id,reply)=connection.send_request(method,params);
                            if let Some(previous)=approval.take(){withdraw(connection,previous);}
                            approval=Some(OpenApproval {approval_id:event.approval_id,request_id,reply,confirmation,revision:shared.map(|r|r.revision)});
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
#[allow(clippy::too_many_arguments)]
pub async fn run(
    socket: &Path,
    workspace: PathBuf,
    schedule: Option<Arc<dyn otto::schedule::ScheduleStore>>,
    logger: Arc<otto::log::Logger>,
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
    let client = client.with_input_source(otto_core::model::InputSource::Acp);
    if let Err(failure) = client.healthz().await {
        return unreachable(stderr, failure);
    }
    let lost = CancellationToken::new();
    let relay = Relay {
        client,
        server: None,
        schedule,
        logger,
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
    server: Arc<otto_server::Server>,
    workspace: PathBuf,
    input: Box<dyn BufRead + Send>,
    output: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) {
    let lost = CancellationToken::new();
    let relay = Relay {
        client: Client::in_process(server.socket_router())
            .with_input_source(otto_core::model::InputSource::Acp),
        schedule: server.schedule_store(),
        logger: Arc::clone(server.logger()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_rejections_remain_invalid_params_without_losing_the_connection() {
        let relay = Relay::with_router(axum::Router::new());
        for (status, code) in [(400, "INVALID_WORKSPACE"), (403, "WORKSPACE_NOT_ADMITTED")] {
            let response = serde_json::to_value(relay.fail(ClientError::Http {
                status,
                code: code.into(),
                message: "workspace rejected".into(),
            }))
            .unwrap();
            assert_eq!(response["code"], super::super::INVALID_PARAMS);
            assert_eq!(response["message"], "workspace rejected");
            assert!(!relay.lost.is_cancelled());
        }
        let response = serde_json::to_value(relay.fail(ClientError::Http {
            status: 500,
            code: "internal".into(),
            message: "server failed".into(),
        }))
        .unwrap();
        assert_eq!(response["code"], INTERNAL_ERROR);
    }
}
