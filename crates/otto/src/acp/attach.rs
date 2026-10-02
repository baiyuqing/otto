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
use crate::client::{Client, Error as ClientError};

const LOST: &str = "connection to otto serve lost";

/// A `session/request_permission` request waiting for the client's answer.
struct OpenApproval {
    approval_id: String,
    request_id: u64,
    reply: oneshot::Receiver<Value>,
}

pub(super) struct Relay {
    client: Client,
    /// Sessions this connection opened, with their prompt slots.
    sessions: Mutex<HashMap<String, Arc<PromptSlot>>>,
    lost: CancellationToken,
}

impl Relay {
    pub(super) fn slot(&self, id: &str) -> Option<Arc<PromptSlot>> {
        self.sessions.lock().expect("session map").get(id).cloned()
    }

    fn register(&self, id: &str) {
        self.sessions
            .lock()
            .expect("session map")
            .entry(id.to_string())
            .or_default();
    }

    /// The JSON-RPC error for a failed call. A connection failure also marks
    /// serve lost.
    fn fail(&self, failure: ClientError) -> agent_client_protocol_schema::v1::Error {
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

    /// Runs one prompt as a queued turn on serve, to its stop reason.
    pub(super) async fn prompt(
        &self,
        connection: &Connection,
        session_id: &str,
        text: &str,
        cancel: &CancellationToken,
    ) -> Reply {
        let mut stream = tokio::select! {
            biased;
            () = self.lost.cancelled() => return Err(error(INTERNAL_ERROR, LOST)),
            started = self.client.start_turn(session_id, text, None) => {
                started.map_err(|failure| self.fail(failure))?
            }
        };
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
