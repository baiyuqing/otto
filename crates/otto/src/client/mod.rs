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
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::http::{Method, StatusCode, Uri};
    use axum::response::Response;

    use super::*;

    /// One request as serve saw it: `METHOD /path?query` and the body text.
    type Seen = Arc<Mutex<Vec<(String, String)>>>;

    /// A serve stand-in on a Unix socket in a temp dir. `reply` gets the
    /// method and the path with query and returns the response; every
    /// request is recorded in the returned log.
    fn stub<F>(reply: F) -> (Client, Seen, tempfile::TempDir)
    where
        F: Fn(&str, &str) -> Response + Clone + Send + Sync + 'static,
    {
        let dir = tempfile::tempdir().expect("dir");
        let socket = dir.path().join("serve.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let seen: Seen = Seen::default();
        let log = Arc::clone(&seen);
        let app = Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
            let reply = reply.clone();
            let log = Arc::clone(&log);
            async move {
                let target = uri.path_and_query().map(ToString::to_string).unwrap();
                log.lock().unwrap().push((
                    format!("{method} {target}"),
                    String::from_utf8_lossy(&body).into_owned(),
                ));
                reply(method.as_str(), &target)
            }
        });
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        (Client::new(&socket).expect("client"), seen, dir)
    }

    fn json_body(status: StatusCode, body: &str) -> Response {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn sse(turn_id: &str, frames: &str) -> Response {
        Response::builder()
            .header("content-type", "text/event-stream")
            .header(TURN_ID_HEADER, turn_id)
            .body(Body::from(frames.to_string()))
            .unwrap()
    }

    fn request(seen: &Seen, index: usize) -> (String, Value) {
        let (line, body) = seen.lock().unwrap()[index].clone();
        let body = serde_json::from_str(&body).unwrap_or(Value::Null);
        (line, body)
    }

    /// An error that names only itself, as `reqwest::Error` does.
    #[derive(Debug)]
    struct Bare(&'static str);

    impl fmt::Display for Bare {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Bare {}

    /// [`Bare`] with the OS reason as its only cause.
    #[derive(Debug)]
    struct Wrapped(Bare, Bare);

    impl fmt::Display for Wrapped {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fmt(f)
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.1)
        }
    }

    #[test]
    fn unreachable_errors_name_the_cause_a_display_leaves_out() {
        let error = unreachable_error(Wrapped(
            Bare("error sending request for url (http://otto/healthz)"),
            Bare("No such file or directory (os error 2)"),
        ));
        assert_eq!(
            error.to_string(),
            "error sending request for url (http://otto/healthz): No such file or directory (os error 2)"
        );
    }

    /// Context that is already formatted has no chain to add and keeps its
    /// shape.
    #[test]
    fn unreachable_text_keeps_a_message_verbatim() {
        let error = unreachable_text(format!("decode frame: {}", Bare("truncated")));
        assert_eq!(error.to_string(), "decode frame: truncated");
    }

    #[tokio::test]
    async fn a_missing_socket_is_unreachable_not_an_http_error() {
        let dir = tempfile::tempdir().expect("dir");
        let client = Client::new(&dir.path().join("absent.sock")).expect("client");
        let error = client.healthz().await.expect_err("no server");
        assert!(matches!(error, Error::Unreachable(_)), "{error}");
        // `otto acp --attach` and the attached TUI print this text to explain
        // an absent `otto serve`, so the OS reason has to reach it.
        let text = error.to_string();
        assert!(text.contains("No such file or directory"), "{text}");
    }

    #[test]
    fn http_errors_display_status_code_and_message() {
        let error = Error::Http {
            status: 409,
            code: "approval_decided".into(),
            message: "already decided".into(),
        };
        let text = error.to_string();
        assert!(
            text.contains("409") && text.contains("approval_decided"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn open_session_omits_workspace_when_none() {
        let (client, seen, _dir) = stub(|_, _| json_body(StatusCode::OK, r#"{"id":"s1"}"#));
        assert_eq!(client.open_session(None, Some("old")).await.unwrap(), "s1");
        client.open_session(Some("/w"), None).await.unwrap();
        assert_eq!(
            request(&seen, 0),
            ("POST /v1/sessions".into(), json!({ "resume": "old" }))
        );
        assert_eq!(request(&seen, 1).1, json!({ "workspace": "/w" }));
    }

    #[tokio::test]
    async fn history_sends_before_turn_only_when_given() {
        let (client, seen, _dir) = stub(|_, _| json_body(StatusCode::OK, "[]"));
        assert!(client.history("s1", Some("t9")).await.unwrap().is_empty());
        client.history("s1", None).await.unwrap();
        assert_eq!(
            request(&seen, 0).0,
            "GET /v1/sessions/s1/history?before_turn=t9"
        );
        assert_eq!(request(&seen, 1).0, "GET /v1/sessions/s1/history");
    }

    #[tokio::test]
    async fn start_turn_sends_the_image_as_data_and_mime_type() {
        let frames = "id: 0\nevent: turn_end\ndata: {\"type\":\"turn_end\"}\n\n";
        let (client, seen, _dir) = stub(move |_, _| sse("t1", frames));
        let image = Block::image("QUJD", "image/png");
        let stream = client.start_turn("s1", "look", Some(&image)).await.unwrap();
        assert_eq!(stream.turn_id, "t1");
        client.start_turn("s1", "plain", None).await.unwrap();
        client
            .start_turn_with_channel(
                "s1",
                "chat text",
                None,
                Some(Channel::Feishu),
                Some(&Origin::chat("feishu", "oc_1").unwrap()),
                None,
                Some("chat:feishu:oc_1:m1"),
            )
            .await
            .unwrap();
        assert_eq!(
            request(&seen, 0),
            (
                "POST /v1/sessions/s1/turns".into(),
                json!({
                    "text": "look",
                    "queue": true,
                    "origin": "terminal",
                    "image": { "data": "QUJD", "mime_type": "image/png" }
                })
            )
        );
        assert_eq!(
            request(&seen, 1).1,
            json!({ "text": "plain", "queue": true, "origin": "terminal" })
        );
        assert_eq!(
            request(&seen, 2).1,
            json!({
                "text": "chat text",
                "queue": true,
                "channel": "feishu",
                "origin": "chat:feishu:oc_1",
                "input_id": "chat:feishu:oc_1:m1"
            })
        );
    }

    #[tokio::test]
    async fn start_turn_sends_the_request_id_header_only_when_given() {
        let dir = tempfile::tempdir().expect("dir");
        let socket = dir.path().join("serve.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let log = Arc::clone(&seen);
        let app = Router::new().fallback(move |headers: axum::http::HeaderMap| {
            let log = Arc::clone(&log);
            async move {
                log.lock().unwrap().push(
                    headers
                        .get("x-request-id")
                        .map(|value| value.to_str().unwrap().to_string()),
                );
                sse(
                    "t1",
                    "id: 0\nevent: turn_end\ndata: {\"type\":\"turn_end\"}\n\n",
                )
            }
        });
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = Client::new(&socket).expect("client");
        client
            .start_turn_with_channel("s1", "a", None, None, None, Some("req-1"), None)
            .await
            .unwrap();
        client.start_turn("s1", "b", None).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![Some("req-1".to_string()), None]);
    }

    #[tokio::test]
    async fn turn_events_sends_after_and_takes_the_turn_id_from_the_path() {
        let frames = "id: 3\nevent: turn_end\ndata: {\"type\":\"turn_end\"}\n\n";
        let (client, seen, _dir) =
            stub(move |_, _| Response::builder().body(Body::from(frames)).unwrap());
        let mut stream = client.turn_events("s1", "t7", Some(2)).await.unwrap();
        assert_eq!(stream.turn_id, "t7");
        assert!(stream.next().await.unwrap().is_some());
        assert!(stream.next().await.unwrap().is_none());
        client.turn_events("s1", "t7", None).await.unwrap();
        assert_eq!(
            request(&seen, 0).0,
            "GET /v1/sessions/s1/turns/t7/events?after=2"
        );
        assert_eq!(request(&seen, 1).0, "GET /v1/sessions/s1/turns/t7/events");
    }

    #[tokio::test]
    async fn status_yields_each_snapshot_then_none() {
        let frames = concat!(
            "event: status\ndata: {\"sessions\":[]}\n\n",
            "event: status\ndata: {\"sessions\":[{\"id\":\"s1\",\"workspace\":\"/w\",",
            "\"turn\":\"running\",\"turn_id\":\"t1\",\"queued\":2,\"approvals\":1,\"tasks\":3},",
            "{\"id\":\"s2\",\"workspace\":\"/w\",\"turn\":null,\"turn_id\":null,",
            "\"queued\":0,\"approvals\":0,\"tasks\":0}]}\n\n",
        );
        let (client, seen, _dir) =
            stub(move |_, _| Response::builder().body(Body::from(frames)).unwrap());
        let mut stream = client.status().await.unwrap();
        assert_eq!(stream.next().await.unwrap(), Some(vec![]));
        let rows = stream.next().await.unwrap().expect("second snapshot");
        assert_eq!(
            rows[0],
            StatusRow {
                auto_mode: false,
                approval_mode: None,
                id: "s1".into(),
                workspace: "/w".into(),
                turn: Some("running".into()),
                turn_id: Some("t1".into()),
                queued: 2,
                approvals: 1,
                tasks: 3,
            }
        );
        assert_eq!(rows[1].turn, None);
        assert_eq!(stream.next().await.unwrap(), None);
        assert_eq!(request(&seen, 0).0, "GET /v1/status");
    }

    #[tokio::test]
    async fn legacy_mode_command_labels_actual_never_and_supports_old_boolean_servers() {
        let (client, seen, _dir) = stub(move |_, _| {
            json_body(
                StatusCode::OK,
                r#"{"mode":"auto","automatic":true,"policy":"never"}"#,
            )
        });
        assert_eq!(
            client.auto_mode_command("session", "").await.unwrap(),
            "Approval mode: NEVER (reviewer disabled)"
        );
        assert_eq!(
            request(&seen, 0).0,
            "GET /v1/sessions/session/approval-mode"
        );
        let (client, _, _dir) =
            stub(move |_, _| json_body(StatusCode::OK, r#"{"automatic":false}"#));
        assert_eq!(
            client.auto_mode_command("session", "").await.unwrap(),
            "Approval mode: MANUAL"
        );
    }

    #[tokio::test]
    async fn policy_mutation_keeps_revision_and_actual_never_policy() {
        let (client, seen, _dir) = stub(move |_, _| {
            json_body(
                StatusCode::OK,
                r#"{"policy":"never","reviewer":"user","revision":8,"mode":"auto","automatic":true,"reviewer_active":false}"#,
            )
        });
        let settings = client
            .approval_policy(
                "session",
                Some((
                    7,
                    otto_core::approval::ApprovalPolicy::Never,
                    otto_core::approval::ApprovalReviewer::User,
                )),
            )
            .await
            .unwrap();
        assert_eq!(settings.policy, otto_core::approval::ApprovalPolicy::Never);
        assert_eq!(
            settings.reviewer,
            otto_core::approval::ApprovalReviewer::User
        );
        assert_eq!(settings.revision, 8);
        let (path, body) = request(&seen, 0);
        assert_eq!(path, "PUT /v1/sessions/session/approval-policy");
        assert_eq!(
            body,
            json!({"expected_revision":7,"policy":"never","reviewer":"user"})
        );
    }

    #[tokio::test]
    async fn approval_activity_accepts_legacy_snapshot_fields() {
        let (client, seen, _dir) =
            stub(move |_, _| json_body(StatusCode::OK, r#"{"requests":[],"cursor":9}"#));
        let activity = client.approval_activity("session").await.unwrap();
        assert_eq!(activity.cursor, 9);
        assert_eq!(activity.counts.human_pending, 0);
        assert_eq!(activity.counts.reviewing, 0);
        assert!(activity.recent_decisions.is_empty());
        assert_eq!(request(&seen, 0).0, "GET /v1/approvals?session_id=session");
    }

    #[tokio::test]
    async fn session_maps_the_wire_into_info() {
        let body = json!({
            "id": "s1", "name": "work", "workspace": "/w", "provider": "chatgpt",
            "profile": "p", "model": "m", "thinking": "high",
            "context_window": 1000,
            "usage": { "input_tokens": 5, "output_tokens": 7 },
            "context_input_tokens": 42,
            "sandbox": { "mode": "seatbelt", "network": "denied", "bash_available": true,
                         "summary": "x" },
            "turn": null
        })
        .to_string();
        let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
        let info = client.session("s1").await.unwrap();
        assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1");
        assert_eq!(info.session_id, "s1");
        assert_eq!(info.session_name, "work");
        assert_eq!(info.model, "m");
        assert_eq!(info.context_window, 1000);
        assert_eq!(info.usage.input_tokens, 5);
        assert!(info.usage_present);
        assert_eq!(info.context_input_tokens, 42);
        assert!(info.context_input_tokens_present);
        assert!(!info.context_input_tokens_pending);
        assert!(info.session_path.is_empty());
        assert_eq!(info.sandbox.mode, SandboxMode::Seatbelt);
        assert_eq!(info.sandbox.network, SandboxNetwork::Denied);
        assert!(info.sandbox.bash_available);
    }

    #[tokio::test]
    async fn session_maps_unknown_sandbox_strings_to_defaults() {
        let body =
            r#"{"id":"s1","sandbox":{"mode":"future","network":"future","bash_available":false}}"#;
        let (client, _seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, body));
        let info = client.session("s1").await.unwrap();
        assert_eq!(info.sandbox.mode, SandboxMode::default());
        assert_eq!(info.sandbox.network, SandboxNetwork::default());
        assert!(!info.usage_present);
        assert!(!info.context_input_tokens_present);
    }

    #[tokio::test]
    async fn rename_session_patches_the_name() {
        let (client, seen, _dir) = stub(|_, _| json_body(StatusCode::OK, r#"{"id":"s1"}"#));
        client.rename_session("s1", "new name").await.unwrap();
        assert_eq!(
            request(&seen, 0),
            (
                "PATCH /v1/sessions/s1".into(),
                json!({ "name": "new name" })
            )
        );
    }

    #[tokio::test]
    async fn fork_calls_post_the_fork_point_and_decode_the_new_session() {
        let (client, seen, _dir) = stub(|_, path| {
            if path.ends_with("/fork-points") {
                json_body(
                    StatusCode::OK,
                    r#"{"fork_points":[{"entry_id":"00000001","prompt":"first"}]}"#,
                )
            } else {
                json_body(StatusCode::CREATED, r#"{"id":"s2","prompt":"first"}"#)
            }
        });
        assert_eq!(
            client.fork_points("s1").await.unwrap(),
            [("00000001".to_string(), "first".to_string())]
        );
        assert_eq!(
            client.fork_session("s1", Some("00000001")).await.unwrap(),
            ("s2".to_string(), Some("first".to_string()))
        );
        assert_eq!(
            request(&seen, 1),
            (
                "POST /v1/sessions/s1/fork".into(),
                json!({ "before": "00000001" })
            )
        );
    }

    #[tokio::test]
    async fn compact_decodes_a_compaction_and_maps_an_empty_body_to_none() {
        let (client, seen, _dir) = stub(|_, path| {
            if path.contains("full") {
                json_body(
                    StatusCode::OK,
                    r#"{"reason":"manual","tokens_before":900,"estimated_tokens_after":100}"#,
                )
            } else {
                Response::new(Body::empty())
            }
        });
        let done = client.compact("full", "api").await.unwrap().expect("some");
        assert_eq!(
            (done.tokens_before, done.estimated_tokens_after),
            (Some(900), Some(100))
        );
        assert_eq!(
            request(&seen, 0),
            (
                "POST /v1/sessions/full/compact".into(),
                json!({ "focus": "api" })
            )
        );
        assert_eq!(client.compact("cut", "").await.unwrap(), None);
    }

    #[tokio::test]
    async fn reflection_client_calls_use_the_existing_routes_and_empty_cancel_reply() {
        let (client, seen, _dir) = stub(|_, path| match path.split('?').next().unwrap_or(path) {
            "/v1/sessions/s1/reflect" => json_body(
                StatusCode::OK,
                r#"{"line":"reflection: 1 candidate queued"}"#,
            ),
            "/v1/sessions/s1/reflect/cancel" => json_body(StatusCode::ACCEPTED, ""),
            "/v1/sessions/s1/reflection/skills" => json_body(
                StatusCode::OK,
                r#"{"enabled":true,"skills":[{"name":"lint","run_id":"r1","session_id":"s1","reason":"reason","created_at":"now","updated_at":"now","owned":true}]}"#,
            ),
            "/v1/sessions/s1/notices" => json_body(
                StatusCode::OK,
                r#"{"notices":[{"id":3,"text":"reflection: done","presentation_origin":"chat:telegram:42"}],"last":3}"#,
            ),

            _ => json_body(StatusCode::OK, r#"{"name":"lint","result":"removed"}"#),
        });
        assert_eq!(
            client
                .reflect("s1", "focus", "acp-1")
                .await
                .unwrap()
                .unwrap()
                .line,
            "reflection: 1 candidate queued"
        );
        client.cancel_reflection("s1", "acp-1").await.unwrap();
        let skills = client.generated_skills("s1").await.unwrap();
        assert_eq!(skills.skills[0].name, "lint");
        assert_eq!(
            client.revert_generated_skill("s1", "lint").await.unwrap(),
            "removed"
        );
        let notices = client.reflection_notices("s1", 2).await.unwrap();
        assert_eq!(notices.notices[0].id, 3);
        assert_eq!(notices.notices[0].presentation_origin, "chat:telegram:42");
        assert_eq!(notices.last, 3);
        assert_eq!(
            request(&seen, 0).1,
            json!({"focus":"focus", "request_id":"acp-1"})
        );
        assert_eq!(request(&seen, 1).1, json!({"request_id":"acp-1"}));
        assert_eq!(request(&seen, 4).0, "GET /v1/sessions/s1/notices?after=2");
    }

    #[tokio::test]
    async fn reflection_run_logs_use_their_routes_and_a_missing_run_is_none() {
        let (client, seen, _dir) = stub(|_, path| match path {
            "/v1/sessions/s1/reflection/runs" => json_body(
                StatusCode::OK,
                r#"{"enabled":true,"runs":[{"id":"abcd1234","path":"/h/.otto/reflection/runs/x.md","started":"2026-10-05T02:03:00Z","trigger":"requested","status":"ok","headline":"Read the docs first."}]}"#,
            ),
            "/v1/sessions/s1/reflection/runs/abcd1234" => json_body(
                StatusCode::OK,
                r##"{"id":"abcd1234","path":"/h/.otto/reflection/runs/x.md","content":"# Reflection run"}"##,
            ),
            _ => json_body(
                StatusCode::NOT_FOUND,
                r#"{"error":{"code":"not_found","message":"reflection run not found"}}"#,
            ),
        });
        let list = client.reflection_runs("s1").await.unwrap();
        assert!(list.enabled);
        assert_eq!(list.runs[0].id, "abcd1234");
        assert_eq!(list.runs[0].headline, "Read the docs first.");
        let log = client
            .reflection_run("s1", "abcd1234")
            .await
            .unwrap()
            .expect("log");
        assert_eq!(log.content, "# Reflection run");
        assert!(client.reflection_run("s1", "a/b").await.unwrap().is_none());
        assert_eq!(
            request(&seen, 2).0,
            "GET /v1/sessions/s1/reflection/runs/a%2Fb"
        );
    }

    #[tokio::test]
    async fn trace_span_escapes_the_span_id_and_maps_404_to_none() {
        let (client, seen, _dir) = stub(|_, path| match path {
            "/v1/sessions/s1/trace/t1%3Acall%2F1" => {
                json_body(StatusCode::OK, r#"{"spanId":"t1:call/1","sections":[]}"#)
            }
            _ => json_body(
                StatusCode::NOT_FOUND,
                r#"{"error":{"code":"not_found","message":"span not found"}}"#,
            ),
        });

        let content = client.trace_span("s1", "t1:call/1").await.unwrap();
        assert_eq!(content.expect("content").span_id, "t1:call/1");
        assert!(client.trace_span("s1", "other").await.unwrap().is_none());
        assert_eq!(
            request(&seen, 0).0,
            "GET /v1/sessions/s1/trace/t1%3Acall%2F1"
        );
    }

    #[tokio::test]
    async fn task_control_client_uses_session_routes_and_transient_channel() {
        let (client, seen, _dir) = stub(|_, path| match path {
            "/v1/sessions/s1/tasks/steer" => json_body(
                StatusCode::OK,
                r#"{"id":7,"text":"inspect the result","status":"queued"}"#,
            ),
            "/v1/sessions/s1/tasks/control" => json_body(
                StatusCode::OK,
                r#"{"text":"I can guide the task","queued":false}"#,
            ),
            _ => json_body(StatusCode::NOT_FOUND, ""),
        });

        let steer = client.steer("s1", "inspect the result").await.unwrap();
        assert_eq!(steer.id, 7);
        assert_eq!(steer.text, "inspect the result");
        assert_eq!(steer.status, otto_core::agent::steering::Status::Queued);
        let reply = client
            .control_message_with_channel("s1", "what can you do?", Some(Channel::Telegram))
            .await
            .unwrap()
            .expect("routed reply");
        assert_eq!(reply.text, "I can guide the task");
        assert!(!reply.queued);
        assert_eq!(
            request(&seen, 0),
            (
                "POST /v1/sessions/s1/tasks/steer".into(),
                json!({"text":"inspect the result"})
            )
        );
        assert_eq!(
            request(&seen, 1),
            (
                "POST /v1/sessions/s1/tasks/control".into(),
                json!({"text":"what can you do?","channel":"telegram"})
            )
        );
    }

    #[tokio::test]
    async fn reflection_treats_an_empty_cancelled_response_as_none() {
        let (client, _, _dir) = stub(|_, _| json_body(StatusCode::OK, "  "));
        assert!(client.reflect("s1", "", "acp-2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn context_decodes_the_report() {
        let report = ContextReport {
            model: "m".into(),
            context_window: 1000,
            compaction_threshold: 800,
            estimated_total: 120,
            estimate_complete: true,
            reported_input_tokens: Some(110),
            sections: vec![otto_core::agent::context_report::ContextSection {
                kind: otto_core::agent::context_report::SectionKind::Messages,
                tokens: 120,
                items: vec![],
            }],
        };
        let body = serde_json::to_string(&report).unwrap();
        let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
        assert_eq!(client.context("s1").await.unwrap(), report);
        assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1/context");
    }

    fn sample_task(id: &str) -> crate::app::tasks::Task {
        crate::app::tasks::Task {
            id: id.into(),
            name: String::new(),
            agent: "explorer".into(),
            description: "look around".into(),
            model: String::new(),
            status: crate::app::tasks::TaskStatus::Canceled,
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            started_at: None,
            finished_at: None,
            steps: 3,
            tool_calls: 2,
            last_tool: String::new(),
            last_text: "done".into(),
            usage: Usage::default(),
            usage_present: false,
            result: String::new(),
            error: "boom".into(),
            session_path: String::new(),
        }
    }

    #[tokio::test]
    async fn session_tasks_converts_wire_tasks_to_registry_tasks() {
        let body = serde_json::to_string(&json!({ "tasks": [sample_task("a1")] })).unwrap();
        let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
        let tasks = client.session_tasks("s1").await.unwrap();
        assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1/tasks");
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert_eq!(task.id, "a1");
        assert_eq!(task.agent, "explorer");
        assert_eq!(task.status, crate::subagent::tasks::TaskStatus::Canceled);
        assert_eq!(task.created_at.map(|t| t.timestamp()), Some(1_700_000_000));
        assert_eq!((task.steps, task.tool_calls), (3, 2));
        assert_eq!(task.error, "boom");
        assert!(task.name.is_empty() && task.prompt.is_empty() && task.context.is_empty());
    }

    fn sample_row() -> TaskRow {
        TaskRow {
            parent_session: "s1".into(),
            task_id: "a1".into(),
            workspace: "/w".into(),
            agent: "explorer".into(),
            status: "succeeded".into(),
            created_at: "2026-10-01T00:00:00Z".into(),
            steps: 4,
            result: "ok".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn tasks_sends_only_the_query_fields_that_are_set() {
        let mut row = serde_json::to_value(sample_row()).unwrap();
        row["cancelable"] = json!(false);
        let body = json!({ "tasks": [row], "next_before": "2026-09-30T00:00:00Z" }).to_string();
        let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
        let query = ListQuery {
            status: Some("running".into()),
            workspace: None,
            limit: Some(5),
            before: Some("2026-10-01T00:00:00Z".into()),
        };
        let result = client.tasks(&query).await.unwrap();
        assert_eq!(result.tasks, vec![sample_row()]);
        assert_eq!(result.next_before, "2026-09-30T00:00:00Z");
        client.tasks(&ListQuery::default()).await.unwrap();
        let (line, _) = request(&seen, 0);
        let (path, params) = line.split_once('?').expect("query string");
        assert_eq!(path, "GET /v1/tasks");
        let mut params: Vec<&str> = params.split('&').collect();
        params.sort_unstable();
        assert_eq!(
            params,
            [
                "before=2026-10-01T00%3A00%3A00Z",
                "limit=5",
                "status=running"
            ]
        );
        assert_eq!(request(&seen, 1).0, "GET /v1/tasks");
    }

    #[tokio::test]
    async fn task_decodes_the_detail_and_maps_404_to_none() {
        let mut row = serde_json::to_value(sample_row()).unwrap();
        row["cancelable"] = json!(false);
        let body = json!({ "task": row, "history": [], "transcript_missing": true }).to_string();
        let (client, seen, _dir) = stub(move |_, path| {
            if path.ends_with("/a1") {
                json_body(StatusCode::OK, &body)
            } else {
                json_body(
                    StatusCode::NOT_FOUND,
                    r#"{"error":{"code":"not_found","message":"task not found"}}"#,
                )
            }
        });
        assert_eq!(client.task("s1", "a1").await.unwrap(), Some(sample_row()));
        assert_eq!(client.task("s1", "zz").await.unwrap(), None);
        assert_eq!(request(&seen, 0).0, "GET /v1/tasks/s1/a1");
    }

    #[tokio::test]
    async fn task_detail_keeps_server_history_and_missing_transcript_diagnostic() {
        let history = vec![Message {
            role: otto_core::model::Role::User,
            blocks: vec![Block::text("child transcript")],
            ..Message::default()
        }];
        let mut row = serde_json::to_value(sample_row()).unwrap();
        row["cancelable"] = json!(false);
        let body = json!({
            "task": row,
            "history": history,
            "transcript_missing": false
        })
        .to_string();
        let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
        let detail = client
            .task_detail("s1", "a1")
            .await
            .unwrap()
            .expect("task detail");
        assert_eq!(detail.task, sample_row());
        assert_eq!(detail.history[0].text(), "child transcript");
        assert!(!detail.transcript_missing);
        assert_eq!(request(&seen, 0).0, "GET /v1/tasks/s1/a1");

        let body = json!({
            "task": serde_json::to_value(sample_row()).unwrap(),
            "history": [],
            "transcript_missing": true
        })
        .to_string();
        let (client, _, _dir) = stub(move |_, _| json_body(StatusCode::OK, &body));
        let detail = client
            .task_detail("s1", "a1")
            .await
            .unwrap()
            .expect("task row remains available");
        assert!(detail.history.is_empty());
        assert!(detail.transcript_missing);
        assert!(matches!(
            client.task_history("s1", "a1").await,
            Err(Error::Http { status: 404, code, .. }) if code == "transcript_missing"
        ));
    }

    #[tokio::test]
    async fn task_other_http_errors_are_not_swallowed() {
        let (client, _seen, _dir) = stub(|_, _| {
            json_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":{"code":"internal","message":"x"}}"#,
            )
        });
        let error = client.task("s1", "a1").await.expect_err("500");
        assert!(matches!(error, Error::Http { status: 500, .. }), "{error}");
    }

    #[tokio::test]
    async fn reload_sandbox_returns_the_summary() {
        let (client, seen, _dir) = stub(|_, _| {
            json_body(
                StatusCode::OK,
                r#"{"mode":"seatbelt","network":"allowed","bash_available":true,"summary":"seatbelt line"}"#,
            )
        });
        assert_eq!(client.reload_sandbox().await.unwrap(), "seatbelt line");
        assert_eq!(request(&seen, 0).0, "POST /v1/sandbox/reload");
    }
}
