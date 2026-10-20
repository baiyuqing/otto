//! An HTTP client of `otto serve` over its Unix socket.
//!
//! Used by `otto acp --attach` and by the TUI's `--attach` mode; the calls
//! are the ones those two frontends need. Contract:
//!
//! - One [`Client`] holds one connection pool to one socket path. The URL
//!   host is ignored by the transport.
//! - Every call returns [`Error::Unreachable`] when the socket cannot be
//!   reached, the connection breaks or a body does not decode, and
//!   [`Error::Http`] when serve answered with a non-2xx status. The latter
//!   keeps the status and the `code` and `message` of serve's
//!   `{"error":{"code","message"}}` body (empty when the body has another
//!   shape). The former carries the failure and its causes, so a missing
//!   socket reads as `... (http://otto/healthz): client error (Connect): No
//!   such file or directory (os error 2)` instead of a bare `reqwest`
//!   request error.
//! - [`Client::start_turn`] returns once the response headers arrive, which
//!   serve sends before a queued turn starts. [`TurnStream::next`] then
//!   yields the turn's frames in order and `Ok(None)` when serve closes the
//!   stream. A turn's last frame is `turn_end`; a stream that closes without
//!   it was cut off, and the caller must treat that as a lost connection.
//! - [`Client::turn_events`] replays and follows one turn's stream and
//!   [`Client::status`] opens `GET /v1/status`. [`StatusStream::next`] yields
//!   one full snapshot per `status` frame (the first on connect) and
//!   `Ok(None)` when serve closes the stream. Both streams share the SSE
//!   reader of [`TurnStream`] and are cancel safe in the same way.
//! - No call retries and no call has a timeout: a turn stream stays open for
//!   as long as the turn runs.

use std::collections::VecDeque;
use std::fmt;
use std::path::Path;

use otto_core::agent::context_report::ContextReport;
use otto_core::model::{Block, Message, Usage};
use otto_core::wire::events::{WireCompaction, WireEvent};
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::{ActiveInputAdmission, Channel};
use crate::mcp::sse::SseParser;
use crate::runtime::info::{SandboxMode, SandboxNetwork};
use crate::schedule::Origin;
use crate::subagent::record::{ListQuery, ListResult, TaskRow};

const BASE: &str = "http://otto";
/// The largest SSE frame accepted from serve: a tool result frame carries at
/// most the tool output limit, so 64 MiB is above anything serve writes.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const TURN_ID_HEADER: &str = "otto-turn-id";

#[derive(Debug)]
pub enum Error {
    /// Serve could not be reached, the connection broke, or a response did
    /// not decode.
    Unreachable(String),
    /// Serve answered with an error status.
    Http {
        status: u16,
        code: String,
        message: String,
    },
}

/// The server-side detail for one recorded sub-agent task.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TaskDetail {
    pub task: TaskRow,
    #[serde(default)]
    pub history: Vec<Message>,
    #[serde(default)]
    pub transcript_missing: bool,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(message) => f.write_str(message),
            Self::Http {
                status,
                code,
                message,
            } => write!(f, "HTTP {status} {code}: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// [`Error::Unreachable`] for a failure. `reqwest`'s `Display` names only the
/// request, so an absent socket or a refused connection is named only by the
/// chain, and `otto acp --attach` has to print it to explain the failure.
fn unreachable_error(error: impl std::error::Error + 'static) -> Error {
    Error::Unreachable(error_chain(&error))
}

/// [`unreachable_error`] for a literal or already-formatted message.
fn unreachable_text(error: impl fmt::Display) -> Error {
    Error::Unreachable(error.to_string())
}

/// Joins an error with its causes, so a transport failure names the reason
/// instead of hiding it behind `reqwest`'s generic "error sending request".
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// A row of `GET /v1/sessions`; absent fields are empty.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionRow {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub last_user_text: String,
    /// RFC 3339; empty for a session that has no file yet.
    #[serde(default)]
    pub modified: String,
}

#[derive(Debug, Deserialize)]
pub struct ReflectionResult {
    pub line: String,
    /// Empty from a serve that has none, or predates the field.
    #[serde(default)]
    pub retrospective: String,
}

#[derive(Debug, Deserialize)]
pub struct GeneratedSkill {
    pub name: String,
    pub run_id: String,
    pub session_id: String,
    pub reason: String,
    pub updated_at: String,
    pub owned: bool,
}

#[derive(Debug, Deserialize)]
pub struct GeneratedSkillList {
    pub enabled: bool,
    pub skills: Vec<GeneratedSkill>,
}

#[derive(Debug, Deserialize)]
pub struct ReflectionRunList {
    pub enabled: bool,
    pub runs: Vec<crate::reflection::runlog::Summary>,
}

#[derive(Debug, Deserialize)]
pub struct ReflectionRunLog {
    pub path: std::path::PathBuf,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct ReflectionNotice {
    pub id: u64,
    pub text: String,
    #[serde(default)]
    pub presentation_origin: String,
}

#[derive(Debug, Deserialize)]
pub struct ReflectionNotices {
    pub notices: Vec<ReflectionNotice>,
    pub last: u64,
}

pub struct Client {
    http: reqwest::Client,
    router: Option<axum::Router>,
}

impl Client {
    #[cfg(unix)]
    pub fn new(socket: &Path) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .unix_socket(socket)
            .no_proxy()
            .build()
            .map_err(unreachable_error)?;
        Ok(Self { http, router: None })
    }

    /// Dispatch against the same serve router without a socket or extra process.
    pub(crate) fn in_process(router: axum::Router) -> Self {
        Self {
            http: reqwest::Client::new(),
            router: Some(router),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn shared_approval(
        &self,
        method: &str,
        session: &str,
        id: &str,
        revision: u64,
        option: &str,
        feedback: &str,
        after: i64,
    ) -> Result<Value, Error> {
        let response = match method {
            "_otto/approvals/list" => self.send(self.http.get(format!("{BASE}/v1/approvals")).query(&[("session_id",session)])).await?,
            "_otto/approvals/get" => self.send(self.http.get(format!("{BASE}/v1/approvals/{id}")).query(&[("session_id",session)])).await?,
            "_otto/approvals/watch" => self.send(self.http.get(format!("{BASE}/v1/sessions/{session}/approval-events")).query(&[("session_id",session),("after",&after.to_string())])).await?,
            _ => self.send(self.post(&format!("{BASE}/v1/approvals/{id}/{}",if method.ends_with("withdraw") {"withdraw"} else {"decision"}),
                &json!({"session_id":session,"revision":revision,"option_id":option,"feedback":feedback}))).await?,
        };
        serde_json::from_slice(&response.bytes().await.map_err(unreachable_error)?)
            .map_err(unreachable_error)
    }

    /// Reads the session's approval mode, or sets it when `mode` is given.
    /// `None` keeps the older server's boolean reading: a response without
    /// `mode` means AUTO when `automatic` is true and MANUAL otherwise.
    pub async fn approval_mode(
        &self,
        session: &str,
        mode: Option<otto_core::approval::ApprovalMode>,
    ) -> Result<otto_core::approval::ApprovalMode, Error> {
        self.approval_mode_response(session, mode)
            .await
            .map(|(mode, _)| mode)
    }

    pub async fn auto_mode_command(&self, session: &str, args: &str) -> Result<String, Error> {
        let selected = crate::app::parse_auto_mode(args).map_err(Error::Unreachable)?;
        let (mode, policy) = self.approval_mode_response(session, selected).await?;
        Ok(format!(
            "Approval mode: {}",
            if policy == Some(otto_core::approval::ApprovalPolicy::Never) {
                "NEVER (reviewer disabled)"
            } else {
                mode.label()
            }
        ))
    }

    async fn approval_mode_response(
        &self,
        session: &str,
        mode: Option<otto_core::approval::ApprovalMode>,
    ) -> Result<
        (
            otto_core::approval::ApprovalMode,
            Option<otto_core::approval::ApprovalPolicy>,
        ),
        Error,
    > {
        let url = format!("{BASE}/v1/sessions/{session}/approval-mode");
        let request = match mode {
            Some(mode) => self
                .http
                .put(url)
                .header(CONTENT_TYPE, "application/json")
                .body(json!({"mode":mode.as_str()}).to_string()),
            None => self.http.get(url),
        };
        #[derive(Deserialize)]
        struct Mode {
            #[serde(default)]
            automatic: bool,
            #[serde(default)]
            mode: Option<otto_core::approval::ApprovalMode>,
            #[serde(default)]
            policy: Option<otto_core::approval::ApprovalPolicy>,
        }
        let response = self.send(request).await?;
        let body: Mode =
            serde_json::from_slice(&response.bytes().await.map_err(unreachable_error)?)
                .map_err(unreachable_error)?;
        Ok((
            body.mode.unwrap_or(if body.automatic {
                otto_core::approval::ApprovalMode::Auto
            } else {
                otto_core::approval::ApprovalMode::Manual
            }),
            body.policy,
        ))
    }

    pub async fn approval_policy(
        &self,
        session: &str,
        update: Option<(
            u64,
            otto_core::approval::ApprovalPolicy,
            otto_core::approval::ApprovalReviewer,
        )>,
    ) -> Result<otto_core::approval::ApprovalSettings, Error> {
        let url = format!("{BASE}/v1/sessions/{session}/approval-policy");
        let request = match update {
            Some((revision, policy, reviewer)) => self
                .http
                .put(url)
                .header(CONTENT_TYPE, "application/json")
                .body(
                    json!({"expected_revision":revision,"policy":policy,"reviewer":reviewer})
                        .to_string(),
                ),
            None => self.http.get(url),
        };
        #[derive(Deserialize)]
        struct Body {
            policy: otto_core::approval::ApprovalPolicy,
            reviewer: otto_core::approval::ApprovalReviewer,
            revision: u64,
        }
        let response = self.send(request).await?;
        let body: Body =
            serde_json::from_slice(&response.bytes().await.map_err(unreachable_error)?)
                .map_err(unreachable_error)?;
        Ok(otto_core::approval::ApprovalSettings {
            policy: body.policy,
            reviewer: body.reviewer,
            revision: body.revision,
        })
    }

    pub async fn approval_activity(
        &self,
        session: &str,
    ) -> Result<crate::approval::ApprovalActivity, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/approvals"))
                    .query(&[("session_id", session)]),
            )
            .await?;
        #[derive(Deserialize)]
        struct Body {
            requests: Vec<otto_core::approval::Request>,
            cursor: i64,
            #[serde(default)]
            reviewing: Vec<otto_core::approval::Request>,
            #[serde(default)]
            recent_decisions: Vec<otto_core::approval::Request>,
            #[serde(default)]
            counts: Option<crate::approval::ApprovalCounts>,
        }
        let body: Body = decode(response).await?;
        let requests: Vec<_> = body
            .requests
            .into_iter()
            .filter(otto_core::approval::Request::is_human_pending)
            .collect();
        let counts = body.counts.unwrap_or(crate::approval::ApprovalCounts {
            human_pending: requests.len(),
            reviewing: body.reviewing.len(),
            application_failed: 0,
        });
        Ok(crate::approval::ApprovalActivity {
            requests,
            cursor: body.cursor,
            reviewing: body.reviewing,
            recent_decisions: body.recent_decisions,
            counts,
        })
    }

    pub async fn permissions_command(&self, session: &str, args: &str) -> Result<String, Error> {
        let current = self.approval_policy(session, None).await?;
        let settings =
            match crate::app::permissions_update(args, current).map_err(Error::Unreachable)? {
                Some((policy, reviewer)) => {
                    self.approval_policy(session, Some((current.revision, policy, reviewer)))
                        .await?
                }
                None => current,
            };
        let activity = self.approval_activity(session).await?;
        Ok(format!(
            "{}\n{} human pending · {} reviewing · {} application failed",
            crate::app::format_permissions(settings),
            activity.counts.human_pending,
            activity.counts.reviewing,
            activity.counts.application_failed
        ))
    }

    pub async fn approval_message(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<Option<crate::app::approval_control::ControlReply>, Error> {
        self.approval_message_with_channel(session_id, text, None)
            .await
    }

    pub async fn approval_message_with_channel(
        &self,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
    ) -> Result<Option<crate::app::approval_control::ControlReply>, Error> {
        let mut body = json!({"text": text});
        if let Some(channel) = channel {
            body["channel"] = serde_json::to_value(channel).expect("Channel serializes");
        }
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/approvals/message"),
                &body,
            ))
            .await?;
        serde_json::from_slice(&response.bytes().await.map_err(unreachable_error)?)
            .map_err(unreachable_error)
    }

    /// `POST /v1/sessions/{id}/inputs`. Deterministically steers an active
    /// task, reports idle for an explicit new turn, and never queues silently.
    pub async fn admit_input(
        &self,
        session_id: &str,
        text: &str,
        input_id: Option<&str>,
    ) -> Result<ActiveInputAdmission, Error> {
        let mut body = json!({"text": text});
        if let Some(input_id) = input_id {
            body["input_id"] = json!(input_id);
        }
        let response = self
            .send(self.post(&format!("{BASE}/v1/sessions/{session_id}/inputs"), &body))
            .await?;
        decode(response).await
    }

    /// `POST /v1/sessions/{id}/tasks/steer`. Accepts guidance only for the
    /// active task in this session. This explicit compatibility endpoint is
    /// intentionally stricter than [`Self::admit_input`].
    pub async fn steer(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<otto_core::agent::steering::Message, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/tasks/steer"),
                &json!({"text": text}),
            ))
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/tasks/steer`: transient statuses for guidance
    /// accepted by the active run. These records disappear after the run ends.
    pub async fn steering_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<otto_core::agent::steering::Message>, Error> {
        decode(
            self.send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/tasks/steer")),
            )
            .await?,
        )
        .await
    }

    /// Withdraw one queued steer before delivery at the next safe boundary.
    pub async fn withdraw_steering(
        &self,
        session_id: &str,
        message_id: u64,
    ) -> Result<otto_core::agent::steering::Message, Error> {
        decode(
            self.send(self.http.delete(format!(
                "{BASE}/v1/sessions/{session_id}/tasks/steer/{message_id}"
            )))
            .await?,
        )
        .await
    }

    /// `POST /v1/sessions/{id}/tasks/control`. `None` means the caller should
    /// route the message as an ordinary prompt.
    pub async fn control_message_with_channel(
        &self,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
    ) -> Result<Option<crate::app::approval_control::ControlReply>, Error> {
        let mut body = json!({"text": text});
        if let Some(channel) = channel {
            body["channel"] = serde_json::to_value(channel).expect("Channel serializes");
        }
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/tasks/control"),
                &body,
            ))
            .await?;
        serde_json::from_slice(&response.bytes().await.map_err(unreachable_error)?)
            .map_err(unreachable_error)
    }

    /// `GET /healthz`.
    pub async fn healthz(&self) -> Result<(), Error> {
        self.send(self.http.get(format!("{BASE}/healthz")))
            .await
            .map(drop)
    }

    /// `POST /v1/sessions`: a new session in `workspace`, or with `resume`
    /// the stored session of that id. Without `workspace`, serve uses its
    /// startup workspace for a new session and searches every loaded
    /// workspace for a resumed one. Returns the session id.
    pub async fn open_session(
        &self,
        workspace: Option<&str>,
        resume: Option<&str>,
    ) -> Result<String, Error> {
        let mut body = json!({});
        if let Some(workspace) = workspace {
            body["workspace"] = json!(workspace);
        }
        if let Some(id) = resume {
            body["resume"] = json!(id);
        }
        let response = self
            .send(self.post(&format!("{BASE}/v1/sessions"), &body))
            .await?;
        #[derive(Deserialize)]
        struct Opened {
            id: String,
        }
        Ok(decode::<Opened>(response).await?.id)
    }

    /// `GET /v1/sessions?workspace=...`.
    pub async fn list_sessions(&self, workspace: &str) -> Result<Vec<SessionRow>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions"))
                    .query(&[("workspace", workspace)]),
            )
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            sessions: Vec<SessionRow>,
        }
        Ok(decode::<Listed>(response).await?.sessions)
    }

    /// `GET /v1/sessions/{id}/history`; with `before_turn`, only the
    /// messages that existed when that turn started.
    pub async fn history(
        &self,
        session_id: &str,
        before_turn: Option<&str>,
    ) -> Result<Vec<Message>, Error> {
        let mut request = self
            .http
            .get(format!("{BASE}/v1/sessions/{session_id}/history"));
        if let Some(turn_id) = before_turn {
            request = request.query(&[("before_turn", turn_id)]);
        }
        decode(self.send(request).await?).await
    }

    /// `POST /v1/sessions/{id}/turns` with `queue: true` and origin
    /// `terminal`, for the attach TUI. `image` is an image block; its data
    /// and MIME type go in the `image` field.
    pub async fn start_turn(
        &self,
        session_id: &str,
        text: &str,
        image: Option<&Block>,
    ) -> Result<TurnStream, Error> {
        self.start_turn_with_channel(
            session_id,
            text,
            image,
            None,
            Some(&Origin::terminal()),
            None,
            None,
        )
        .await
    }

    /// Starts a queued turn with optional per-turn chat channel metadata and
    /// origin. Without an origin the server records the turn as `web`.
    /// `request_id` is sent as `X-Request-Id` for log correlation;
    /// `input_id` is the body's idempotency key, so a retry is not run twice.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_turn_with_channel(
        &self,
        session_id: &str,
        text: &str,
        image: Option<&Block>,
        channel: Option<Channel>,
        origin: Option<&Origin>,
        request_id: Option<&str>,
        input_id: Option<&str>,
    ) -> Result<TurnStream, Error> {
        let mut body = json!({ "text": text, "queue": true });
        if let Some(input_id) = input_id {
            body["input_id"] = json!(input_id);
        }
        if let Some(channel) = channel {
            body["channel"] = serde_json::to_value(channel).expect("Channel serializes");
        }
        if let Some(origin) = origin {
            body["origin"] = json!(origin.as_str());
        }
        if let Some(image) = image {
            body["image"] = json!({ "data": image.data, "mime_type": image.mime_type });
        }
        let mut request = self.post(&format!("{BASE}/v1/sessions/{session_id}/turns"), &body);
        if let Some(request_id) = request_id {
            request = request.header("x-request-id", request_id);
        }
        let response = self.send(request).await?;
        let turn_id = response
            .headers()
            .get(TURN_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| unreachable_text("response has no Otto-Turn-Id header"))?;
        Ok(TurnStream::new(turn_id, response))
    }

    /// `GET /v1/sessions/{id}/turns/{turn_id}/events`: the events with an
    /// index above `after`, or from the first event when `None`, then the
    /// live ones.
    pub async fn turn_events(
        &self,
        session_id: &str,
        turn_id: &str,
        after: Option<usize>,
    ) -> Result<TurnStream, Error> {
        let mut request = self.http.get(format!(
            "{BASE}/v1/sessions/{session_id}/turns/{turn_id}/events"
        ));
        if let Some(after) = after {
            request = request.query(&[("after", after)]);
        }
        let response = self.send(request).await?;
        Ok(TurnStream::new(turn_id.to_string(), response))
    }

    /// `GET /v1/status`: snapshots of every open session in serve.
    pub async fn status(&self) -> Result<StatusStream, Error> {
        let response = self
            .send(self.http.get(format!("{BASE}/v1/status")))
            .await?;
        Ok(StatusStream {
            body: SseBody::new(response),
        })
    }

    /// `GET /v1/sessions/{id}`. `session_path` is empty, `reason` is the
    /// default and `pending` is false: the wire carries none of them.
    pub async fn session(&self, session_id: &str) -> Result<crate::app::Info, Error> {
        let response = self
            .send(self.http.get(format!("{BASE}/v1/sessions/{session_id}")))
            .await?;
        let wire: SessionWire = decode(response).await?;
        Ok(wire.into_info())
    }

    /// `POST /v1/sessions/{id}/profile`: run the session on another configured
    /// profile and return the session's new info. The configuration file and
    /// every other session are unchanged.
    pub async fn switch_profile(
        &self,
        session_id: &str,
        profile: &str,
    ) -> Result<crate::app::Info, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/profile"),
                &json!({ "profile": profile }),
            ))
            .await?;
        let wire: SessionWire = decode(response).await?;
        Ok(wire.into_info())
    }

    /// `GET /v1/config/profiles`: the configured profile names, sorted.
    pub async fn profiles(&self) -> Result<Vec<String>, Error> {
        #[derive(Deserialize)]
        struct Row {
            name: String,
        }
        #[derive(Deserialize)]
        struct Reply {
            profiles: Vec<Row>,
        }
        let response = self
            .send(self.http.get(format!("{BASE}/v1/config/profiles")))
            .await?;
        let mut names: Vec<String> = decode::<Reply>(response)
            .await?
            .profiles
            .into_iter()
            .map(|row| row.name)
            .collect();
        names.sort();
        Ok(names)
    }

    /// `GET /v1/sessions/{id}/fork-points`, oldest first, as (entry id,
    /// prompt) pairs.
    pub async fn fork_points(&self, session_id: &str) -> Result<Vec<(String, String)>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/fork-points")),
            )
            .await?;
        #[derive(Deserialize)]
        struct Point {
            entry_id: String,
            prompt: String,
        }
        #[derive(Deserialize)]
        struct Points {
            fork_points: Vec<Point>,
        }
        Ok(decode::<Points>(response)
            .await?
            .fork_points
            .into_iter()
            .map(|point| (point.entry_id, point.prompt))
            .collect())
    }

    /// `POST /v1/sessions/{id}/fork`: the fork's session id and the prompt it
    /// stops before.
    pub async fn fork_session(
        &self,
        session_id: &str,
        before: Option<&str>,
    ) -> Result<(String, Option<String>), Error> {
        let body = match before {
            Some(before) => json!({ "before": before }),
            None => json!({}),
        };
        let response = self
            .send(self.post(&format!("{BASE}/v1/sessions/{session_id}/fork"), &body))
            .await?;
        #[derive(Deserialize)]
        struct Forked {
            id: String,
            #[serde(default)]
            prompt: Option<String>,
        }
        let forked = decode::<Forked>(response).await?;
        Ok((forked.id, forked.prompt))
    }

    /// `PATCH /v1/sessions/{id}`.
    pub async fn rename_session(&self, session_id: &str, name: &str) -> Result<(), Error> {
        self.send(
            self.http
                .patch(format!("{BASE}/v1/sessions/{session_id}"))
                .header(CONTENT_TYPE, "application/json")
                .body(json!({ "name": name }).to_string()),
        )
        .await
        .map(drop)
    }

    /// `POST /v1/sessions/{id}/compact`. `None` when serve answered 200 with
    /// an empty body, which it does when shutdown cancelled the compaction.
    pub async fn compact(
        &self,
        session_id: &str,
        focus: &str,
    ) -> Result<Option<WireCompaction>, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/compact"),
                &json!({ "focus": focus }),
            ))
            .await?;
        let body = response.bytes().await.map_err(unreachable_error)?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| unreachable_text(format!("decode response: {error}")))
    }

    /// `POST /v1/sessions/{id}/reflect`. The request id binds a later cancel
    /// to this reflection only.
    pub async fn reflect(
        &self,
        session_id: &str,
        focus: &str,
        request_id: &str,
    ) -> Result<Option<ReflectionResult>, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/reflect"),
                &json!({ "focus": focus, "request_id": request_id }),
            ))
            .await?;
        let body = response.bytes().await.map_err(unreachable_error)?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| unreachable_text(format!("decode response: {error}")))
    }

    /// Cancels only the reflection with this request id. A missing or stale
    /// id has no effect on another operation in the session.
    pub async fn cancel_reflection(&self, session_id: &str, request_id: &str) -> Result<(), Error> {
        self.send(self.post(
            &format!("{BASE}/v1/sessions/{session_id}/reflect/cancel"),
            &json!({ "request_id": request_id }),
        ))
        .await
        .map(drop)
    }

    /// `GET /v1/sessions/{id}/reflection/skills`.
    pub async fn generated_skills(&self, session_id: &str) -> Result<GeneratedSkillList, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/reflection/skills")),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/reflection/runs`.
    pub async fn reflection_runs(&self, session_id: &str) -> Result<ReflectionRunList, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/reflection/runs")),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/reflection/runs/{run}`; `None` when serve has
    /// no such run log.
    pub async fn reflection_run(
        &self,
        session_id: &str,
        run: &str,
    ) -> Result<Option<ReflectionRunLog>, Error> {
        let mut url = reqwest::Url::parse(BASE).expect("static Otto base URL");
        url.path_segments_mut()
            .expect("base URL supports path segments")
            .extend(["v1", "sessions", session_id, "reflection", "runs", run]);
        match self.send(self.http.get(url.as_str())).await {
            Ok(response) => decode(response).await.map(Some),
            Err(Error::Http { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn skill_manager(
        &self,
        session: &str,
    ) -> Result<crate::app::management::SkillManager, Error> {
        let mut url = reqwest::Url::parse(BASE).expect("static base");
        url.path_segments_mut().expect("base URL").extend([
            "v1",
            "sessions",
            session,
            "skill-manager",
        ]);
        decode(self.send(self.http.get(url.as_str())).await?).await
    }

    pub(crate) async fn skill_info(
        &self,
        session: &str,
        name: &str,
    ) -> Result<crate::app::management::SkillInfo, Error> {
        let mut url = reqwest::Url::parse(BASE).expect("static base");
        url.path_segments_mut().expect("base URL").extend([
            "v1",
            "sessions",
            session,
            "skill-manager",
            name,
        ]);
        decode(self.send(self.http.get(url.as_str())).await?).await
    }

    pub(crate) async fn set_skill_enabled(
        &self,
        session: &str,
        name: &str,
        enabled: bool,
    ) -> Result<String, Error> {
        let mut url = reqwest::Url::parse(BASE).expect("static base");
        url.path_segments_mut().expect("base URL").extend([
            "v1",
            "sessions",
            session,
            "skills",
            name,
            if enabled { "enable" } else { "disable" },
        ]);
        let response = self.send(self.post(url.as_str(), &json!({}))).await?;
        let _: serde_json::Value = decode(response).await?;
        Ok(format!(
            "skill {name} preference is {}; restart Otto to apply the active catalog (activation also requires [skills].enabled = true)",
            if enabled { "enabled" } else { "disabled" }
        ))
    }

    /// `POST /v1/sessions/{id}/reflection/skills/{name}/revert`.
    pub async fn revert_generated_skill(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<String, Error> {
        let mut url = reqwest::Url::parse(BASE).expect("static Otto base URL");
        url.path_segments_mut()
            .expect("base URL supports path segments")
            .extend([
                "v1",
                "sessions",
                session_id,
                "reflection",
                "skills",
                name,
                "revert",
            ]);
        let response = self.send(self.post(url.as_str(), &json!({}))).await?;
        #[derive(Deserialize)]
        struct Reverted {
            result: String,
        }
        Ok(decode::<Reverted>(response).await?.result)
    }

    /// `GET /v1/sessions/{id}/notices?after=...`.
    pub async fn reflection_notices(
        &self,
        session_id: &str,
        after: u64,
    ) -> Result<ReflectionNotices, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/notices"))
                    .query(&[("after", after)]),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/wake-replies?after=...`.
    pub async fn wake_replies(
        &self,
        session_id: &str,
        after: u64,
    ) -> Result<ReflectionNotices, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/wake-replies"))
                    .query(&[("after", after)]),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/context`.
    pub async fn context(&self, session_id: &str) -> Result<ContextReport, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/context")),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/trace`.
    pub async fn trace(&self, session_id: &str) -> Result<otto_core::session::trace::Trace, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/trace")),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/trace/{span_id}`. `None` means the session or
    /// the span is not found.
    pub async fn trace_span(
        &self,
        session_id: &str,
        span_id: &str,
    ) -> Result<Option<otto_core::session::trace::SpanContent>, Error> {
        let span_id = percent_encode_segment(span_id);
        let result = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/trace/{span_id}")),
            )
            .await;
        let response = match result {
            Ok(response) => response,
            Err(Error::Http { status: 404, .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(decode(response).await?))
    }

    /// `GET /v1/sessions/{id}/tasks`: the open session's sub-agent tasks.
    /// `prompt` and `context` are empty; the wire omits them.
    pub async fn session_tasks(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::subagent::tasks::Task>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/tasks")),
            )
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            tasks: Vec<crate::app::tasks::Task>,
        }
        Ok(decode::<Listed>(response)
            .await?
            .tasks
            .into_iter()
            .map(crate::app::tasks::from_wire)
            .collect())
    }

    /// `GET /v1/tasks`, from `tasks.db`; only the `Some` fields of `query`
    /// are sent.
    pub async fn tasks(&self, query: &ListQuery) -> Result<ListResult, Error> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(status) = &query.status {
            params.push(("status", status.clone()));
        }
        if let Some(workspace) = &query.workspace {
            params.push(("workspace", workspace.clone()));
        }
        if let Some(limit) = query.limit {
            params.push(("limit", limit.to_string()));
        }
        if let Some(before) = &query.before {
            params.push(("before", before.clone()));
        }
        let response = self
            .send(self.http.get(format!("{BASE}/v1/tasks")).query(&params))
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            tasks: Vec<TaskRow>,
            #[serde(default)]
            next_before: String,
        }
        let listed: Listed = decode(response).await?;
        Ok(ListResult {
            tasks: listed.tasks,
            next_before: listed.next_before,
        })
    }

    /// `GET /v1/tasks/{parent_session}/{task_id}`; `None` on 404.
    pub async fn task(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Option<TaskRow>, Error> {
        Ok(self
            .task_detail(parent_session, task_id)
            .await?
            .map(|detail| detail.task))
    }

    /// `GET /v1/tasks/{parent_session}/{task_id}`, including its server-read
    /// child transcript. `None` means the task row is not found.
    pub async fn task_detail(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Option<TaskDetail>, Error> {
        let result = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/tasks/{parent_session}/{task_id}")),
            )
            .await;
        let response = match result {
            Ok(response) => response,
            Err(Error::Http { status: 404, .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(decode(response).await?))
    }

    /// The server-read transcript for a recorded sub-agent task.
    /// A missing task or transcript is returned as a 404 error.
    pub async fn task_history(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Vec<Message>, Error> {
        let Some(detail) = self.task_detail(parent_session, task_id).await? else {
            return Err(Error::Http {
                status: 404,
                code: "not_found".into(),
                message: "task not found".into(),
            });
        };
        if detail.transcript_missing {
            return Err(Error::Http {
                status: 404,
                code: "transcript_missing".into(),
                message: "task transcript is missing or unavailable".into(),
            });
        }
        Ok(detail.history)
    }

    /// `POST /v1/sandbox/reload`; returns the sandbox summary line.
    pub async fn reload_sandbox(&self) -> Result<String, Error> {
        let response = self
            .send(self.http.post(format!("{BASE}/v1/sandbox/reload")))
            .await?;
        #[derive(Deserialize)]
        struct Reloaded {
            #[serde(default)]
            summary: String,
        }
        Ok(decode::<Reloaded>(response).await?.summary)
    }

    /// `POST /v1/sessions/{id}/turns/{turn_id}/cancel`, for a queued or a
    /// running turn.
    pub async fn cancel_turn(&self, session_id: &str, turn_id: &str) -> Result<(), Error> {
        self.send(self.http.post(format!(
            "{BASE}/v1/sessions/{session_id}/turns/{turn_id}/cancel"
        )))
        .await
        .map(drop)
    }

    /// `POST /v1/sessions/{id}/approvals/{approval_id}`; `allow` is the
    /// decision `allow`, otherwise `deny`.
    pub async fn decide_approval(
        &self,
        session_id: &str,
        approval_id: &str,
        allow: bool,
    ) -> Result<(), Error> {
        let decision = if allow { "allow" } else { "deny" };
        self.send(self.post(
            &format!("{BASE}/v1/sessions/{session_id}/approvals/{approval_id}"),
            &json!({ "decision": decision }),
        ))
        .await
        .map(drop)
    }

    fn post(&self, url: &str, body: &Value) -> reqwest::RequestBuilder {
        self.http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, Error> {
        let response = if let Some(router) = &self.router {
            use tower::ServiceExt;
            let request = request.build().map_err(unreachable_error)?;
            let mut local = axum::http::Request::builder()
                .method(request.method().clone())
                .uri(request.url().as_str());
            *local.headers_mut().expect("request headers") = request.headers().clone();
            let body = match request.body() {
                None => Vec::new(),
                Some(body) => body
                    .as_bytes()
                    .ok_or_else(|| {
                        Error::Unreachable("in-process request body must be buffered".into())
                    })?
                    .to_vec(),
            };
            let response = router
                .clone()
                .oneshot(
                    local
                        .body(axum::body::Body::from(body))
                        .map_err(unreachable_error)?,
                )
                .await
                .map_err(unreachable_error)?;
            let (parts, body) = response.into_parts();
            reqwest::Response::from(axum::http::Response::from_parts(
                parts,
                reqwest::Body::wrap_stream(body.into_data_stream()),
            ))
        } else {
            request.send().await.map_err(unreachable_error)?
        };
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.bytes().await.unwrap_or_default();
        let error = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|value| value.get("error").cloned())
            .unwrap_or(Value::Null);
        let field = |name: &str| error.get(name).and_then(Value::as_str).unwrap_or_default();
        Err(Error::Http {
            status: status.as_u16(),
            code: field("code").to_string(),
            message: field("message").to_string(),
        })
    }
}

/// Escapes every byte of one URL path segment except unreserved characters.
fn percent_encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

async fn decode<T: for<'de> Deserialize<'de>>(response: reqwest::Response) -> Result<T, Error> {
    let body = response.bytes().await.map_err(unreachable_error)?;
    serde_json::from_slice(&body)
        .map_err(|error| unreachable_text(format!("decode response: {error}")))
}

/// The `GET /v1/sessions/{id}` body, the fields [`crate::app::Info`] reads.
#[derive(Deserialize)]
struct SessionWire {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    context_window: i64,
    #[serde(default)]
    usage: Usage,
    #[serde(default)]
    context_input_tokens: i64,
    #[serde(default)]
    sandbox: SandboxWireIn,
    /// True when the session decides requests automatically, in AUTO or YOLO.
    #[serde(default)]
    auto_mode: bool,
    /// The exact policy; a server older than the field sends only
    /// `auto_mode`, which maps to AUTO or MANUAL.
    #[serde(default)]
    approval_mode: Option<otto_core::approval::ApprovalMode>,
    #[serde(default)]
    approval_settings: Option<otto_core::approval::ApprovalSettings>,
    #[serde(default)]
    forked_from: Option<crate::app::ForkedFrom>,
    #[serde(default)]
    plan: Vec<otto_core::session::PlanStep>,
}

#[derive(Default, Deserialize)]
struct SandboxWireIn {
    #[serde(default)]
    mode: String,
    #[serde(default)]
    network: String,
    #[serde(default)]
    bash_available: bool,
}

impl SessionWire {
    fn into_info(self) -> crate::app::Info {
        // An unknown string maps to the enum's default, the most restrictive
        // reading ("unavailable", "unconfined").
        let mode = match self.sandbox.mode.as_str() {
            "seatbelt" => SandboxMode::Seatbelt,
            "off" => SandboxMode::Off,
            _ => SandboxMode::default(),
        };
        let network = match self.sandbox.network.as_str() {
            "allowed" => SandboxNetwork::Allowed,
            "denied" => SandboxNetwork::Denied,
            _ => SandboxNetwork::default(),
        };
        crate::app::Info {
            approval_settings: self.approval_settings.unwrap_or_else(|| {
                otto_core::approval::ApprovalSettings::from_mode(self.approval_mode.unwrap_or(
                    if self.auto_mode {
                        otto_core::approval::ApprovalMode::Auto
                    } else {
                        otto_core::approval::ApprovalMode::Manual
                    },
                ))
            }),
            approval_mode: self.approval_mode.unwrap_or(if self.auto_mode {
                otto_core::approval::ApprovalMode::Auto
            } else {
                otto_core::approval::ApprovalMode::Manual
            }),
            forked_from: self.forked_from,
            plan: self.plan,
            session_id: self.id,
            session_name: self.name,
            workspace: self.workspace,
            provider: self.provider,
            profile: self.profile,
            model: self.model,
            thinking: self.thinking,
            usage_present: self.usage != Usage::default(),
            usage: self.usage,
            context_window: self.context_window,
            context_input_tokens_present: self.context_input_tokens > 0,
            context_input_tokens: self.context_input_tokens,
            sandbox: crate::runtime::info::SandboxInfo {
                mode,
                network,
                bash_available: self.sandbox.bash_available,
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

/// The SSE reader both streams share: the response body, the frame parser
/// and the frames parsed from chunks but not yet returned.
struct SseBody {
    response: reqwest::Response,
    /// `None` after the body ended.
    parser: Option<SseParser>,
    ready: VecDeque<String>,
}

impl SseBody {
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            parser: Some(SseParser::new(MAX_FRAME_BYTES)),
            ready: VecDeque::new(),
        }
    }

    /// The `data` of the next frame, or `Ok(None)` when serve closed the
    /// stream. Cancel safe: dropping the future between chunks loses no
    /// frame.
    async fn next_data(&mut self) -> Result<Option<String>, Error> {
        loop {
            if let Some(data) = self.ready.pop_front() {
                return Ok(Some(data));
            }
            let Some(parser) = self.parser.as_mut() else {
                return Ok(None);
            };
            match self.response.chunk().await.map_err(unreachable_error)? {
                Some(chunk) => {
                    for frame in parser.feed(&chunk).map_err(unreachable_text)? {
                        self.ready.push_back(frame.data);
                    }
                }
                None => {
                    let trailing = self
                        .parser
                        .take()
                        .and_then(|parser| parser.finish().ok().flatten());
                    if let Some(frame) = trailing {
                        self.ready.push_back(frame.data);
                    }
                }
            }
        }
    }
}

fn decode_frame<T: for<'de> Deserialize<'de>>(data: &str) -> Result<T, Error> {
    serde_json::from_str(data).map_err(|error| unreachable_text(format!("decode frame: {error}")))
}

/// The SSE body of one turn.
pub struct TurnStream {
    pub turn_id: String,
    body: SseBody,
}

impl TurnStream {
    fn new(turn_id: String, response: reqwest::Response) -> Self {
        Self {
            turn_id,
            body: SseBody::new(response),
        }
    }

    /// The next frame, or `Ok(None)` when serve closed the stream. Cancel
    /// safe: dropping the future between chunks loses no frame.
    pub async fn next(&mut self) -> Result<Option<WireEvent>, Error> {
        match self.body.next_data().await? {
            Some(data) => decode_frame(&data).map(Some),
            None => Ok(None),
        }
    }
}

/// One row of a `GET /v1/status` snapshot (server: `StatusSessionWire`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct StatusRow {
    /// True when the session decides requests automatically, in AUTO or YOLO.
    #[serde(default)]
    pub auto_mode: bool,
    /// The exact policy; absent from a server older than the field.
    #[serde(default)]
    pub approval_mode: Option<otto_core::approval::ApprovalMode>,
    pub id: String,
    #[serde(default)]
    pub workspace: String,
    /// queued, running, ok, error or canceled; `None` before the first turn.
    #[serde(default)]
    pub turn: Option<String>,
    /// The running turn, else the newest one.
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub queued: usize,
    #[serde(default)]
    pub approvals: usize,
    #[serde(default)]
    pub tasks: usize,
}

/// The SSE body of `GET /v1/status`.
pub struct StatusStream {
    body: SseBody,
}

impl StatusStream {
    /// The next snapshot's rows, or `Ok(None)` when serve closed the stream.
    /// Cancel safe like [`TurnStream::next`].
    pub async fn next(&mut self) -> Result<Option<Vec<StatusRow>>, Error> {
        #[derive(Deserialize)]
        struct Snapshot {
            sessions: Vec<StatusRow>,
        }
        match self.body.next_data().await? {
            Some(data) => decode_frame::<Snapshot>(&data).map(|s| Some(s.sessions)),
            None => Ok(None),
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
