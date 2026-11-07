//! Session-scoped memory management routes under `/v1/sessions/{id}/memory`.
//!
//! The routes call the shared management layer (`otto::app::management`), so
//! they have exactly the semantics of `/memory` and `/remember` in the same
//! session. A request carries the server token, which only the operator holds,
//! so writes and reviews are human decisions; review records the actor
//! `http-client`. Spec: docs/specs/2026-11-07-http-memory-api.md.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use chrono::Utc;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use otto::app::Controller;
use otto::app::management::{
    ListFilters, ManagementError, ManagementErrorKind, Reply, Request, ScopeSelector,
    parse_time_or_age, review_deadline,
};
use otto::memory::{RecordState, ReviewDecision};

use super::memory_dto::*;
use super::{Server, bad_request, error_response, internal_error, json_response, not_found};

/// The review actor label of these routes, next to `acp-client` and
/// `memory-command`.
pub const HTTP_REVIEW_TRANSPORT: &str = "http-client";
const DEFAULT_RECORD_PAGE: usize = 50;
const MAX_RECORD_PAGE: usize = otto::memory::MAX_PAGE_SIZE;
const DEFAULT_CANDIDATE_PAGE: usize = 20;
const MAX_CANDIDATE_PAGE: usize = 50;

/// Runs one management request off the async workers: the store does file
/// I/O and `propose` blocks on a model call.
async fn run(server: &Arc<Server>, session_id: &str, request: Request) -> Result<Reply, Response> {
    let Some(ctrl) = server.controller_for_route(session_id).await else {
        return Err(not_found("session not found"));
    };
    let joined = tokio::task::spawn_blocking(move || -> Result<Reply, ManagementError> {
        let ctrl: Arc<Controller> = ctrl;
        ctrl.management_typed(request)
    })
    .await;
    match joined {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(error)) => Err(error_reply(server, &error)),
        Err(join) => Err(internal_error(&server.log, &join.to_string())),
    }
}

/// The documented status for each failure category.
fn error_reply(server: &Server, error: &ManagementError) -> Response {
    let (status, code) = match error.kind {
        ManagementErrorKind::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "memory_unavailable"),
        ManagementErrorKind::Unsupported => (StatusCode::NOT_IMPLEMENTED, "memory_unsupported"),
        ManagementErrorKind::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        ManagementErrorKind::Conflict => (StatusCode::CONFLICT, "memory_conflict"),
        ManagementErrorKind::Invalid => (StatusCode::BAD_REQUEST, "bad_request"),
        ManagementErrorKind::Internal => return internal_error(&server.log, &error.message),
    };
    error_response(status, code, &error.message)
}

fn unexpected(server: &Server) -> Response {
    internal_error(&server.log, "unexpected memory management reply")
}

fn parse_body<T: DeserializeOwned>(body: &Bytes) -> Result<T, Response> {
    serde_json::from_slice(body).map_err(|error| bad_request(&format!("invalid body: {error}")))
}

fn pairs(query: Option<&str>) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

fn flag(value: &str, name: &str) -> Result<bool, Response> {
    match value {
        "" | "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(bad_request(&format!("{name} must be true or false"))),
    }
}

fn page_limit(value: &str, max: usize) -> Result<usize, Response> {
    match value.parse::<usize>() {
        Ok(limit) if (1..=max).contains(&limit) => Ok(limit),
        _ => Err(bad_request(&format!("limit must be 1 to {max}"))),
    }
}

fn list_request(query: Option<&str>) -> Result<Request, Response> {
    let mut scope = ScopeSelector::Current;
    let mut limit = DEFAULT_RECORD_PAGE;
    let mut cursor = String::new();
    let mut filters = ListFilters::default();
    let now = Utc::now();
    let time = |value: &str, name: &str| {
        parse_time_or_age(value, now).ok_or_else(|| {
            bad_request(&format!(
                "{name} must be an RFC 3339 time or an age like 30d"
            ))
        })
    };
    for (name, value) in pairs(query) {
        match name.as_str() {
            "scope" => {
                scope = match value.as_str() {
                    "current" => ScopeSelector::Current,
                    "user" => ScopeSelector::User,
                    "workspace" => ScopeSelector::Workspace,
                    "all" => ScopeSelector::All,
                    _ => return Err(bad_request("scope must be current, user, workspace or all")),
                }
            }
            "limit" => limit = page_limit(&value, MAX_RECORD_PAGE)?,
            "cursor" => cursor = value,
            "state" if value == "all" => filters.states = RecordState::ALL.to_vec(),
            "state" => filters
                .states
                .push(RecordState::parse(&value).ok_or_else(|| {
                    bad_request("state must be active, needs_review, superseded, retracted or all")
                })?),
            "kind" => filters.kinds.push(value),
            "label" => filters.labels.push(value),
            "updated_after" => filters.updated_after = Some(time(&value, "updated_after")?),
            "verified_after" => filters.verified_after = Some(time(&value, "verified_after")?),
            "unverified" => filters.unverified_only = flag(&value, "unverified")?,
            "review_due" => filters.review_due_only = flag(&value, "review_due")?,
            _ => return Err(bad_request(&format!("unknown query parameter {name}"))),
        }
    }
    if filters.unverified_only && filters.verified_after.is_some() {
        return Err(bad_request(
            "unverified cannot be combined with verified_after",
        ));
    }
    filters.states.sort_by_key(|state| state.as_str());
    filters.states.dedup();
    Ok(Request::MemoryList {
        scope,
        limit,
        cursor,
        filters,
    })
}

fn review_at(review_in: Option<&str>) -> Result<Option<chrono::DateTime<Utc>>, Response> {
    review_in
        .map(|age| {
            review_deadline(age)
                .ok_or_else(|| bad_request("review_in must be an age like 30d, 12h or 45m"))
        })
        .transpose()
}

pub async fn list(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let request = match list_request(query.as_deref()) {
        Ok(request) => request,
        Err(response) => return response,
    };
    match run(&server, &id, request).await {
        Ok(Reply::MemoryList(page)) => json_response(
            StatusCode::OK,
            &MemoryRecordPage {
                records: records(&page.records),
                next_cursor: page.next_cursor,
            },
        ),
        Ok(_) => unexpected(&server),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberBody {
    scope: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    key: String,
    text: String,
    #[serde(default)]
    review_in: Option<String>,
}

pub async fn create(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let body: RememberBody = match parse_body(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if body.scope != "user" && body.scope != "workspace" {
        return bad_request("scope must be user or workspace");
    }
    if body.text.trim().is_empty() {
        return bad_request("text must not be empty");
    }
    let review_at = match review_at(body.review_in.as_deref()) {
        Ok(review_at) => review_at,
        Err(response) => return response,
    };
    let request = Request::Remember {
        scope: body.scope,
        kind: body
            .kind
            .filter(|kind| !kind.is_empty())
            .unwrap_or_else(|| "note".into()),
        key: body.key,
        text: body.text.trim().to_owned(),
        review_at,
    };
    match run(&server, &id, request).await {
        Ok(Reply::Remembered(record)) => {
            json_response(StatusCode::CREATED, &MemoryRecord::from(&record))
        }
        Ok(Reply::Usage(message)) => bad_request(&message),
        Ok(_) => unexpected(&server),
        Err(response) => response,
    }
}

pub async fn show(
    State(server): State<Arc<Server>>,
    Path((id, record_id)): Path<(String, String)>,
) -> Response {
    match run(&server, &id, Request::MemoryShow { id: record_id }).await {
        Ok(Reply::MemoryRecord(record)) => {
            json_response(StatusCode::OK, &MemoryRecord::from(&record))
        }
        Ok(_) => unexpected(&server),
        Err(response) => response,
    }
}

fn expected_revision(query: Option<&str>) -> Result<Option<u64>, Response> {
    let mut expected = None;
    for (name, value) in pairs(query) {
        match name.as_str() {
            "expected_revision" => {
                expected = Some(
                    value
                        .parse()
                        .map_err(|_| bad_request("expected_revision must be a revision number"))?,
                )
            }
            _ => return Err(bad_request(&format!("unknown query parameter {name}"))),
        }
    }
    Ok(expected)
}

pub async fn forget(
    State(server): State<Arc<Server>>,
    Path((id, record_id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    let expected_revision = match expected_revision(query.as_deref()) {
        Ok(expected) => expected,
        Err(response) => return response,
    };
    let request = Request::MemoryForget {
        id: record_id,
        expected_revision,
    };
    match run(&server, &id, request).await {
        Ok(Reply::MemoryForgotten {
            tombstone,
            previous_revision,
            ..
        }) => json_response(
            StatusCode::OK,
            &MemoryForgotten {
                tombstone: MemoryTombstone::from(&tombstone),
                previous_revision,
            },
        ),
        Ok(_) => unexpected(&server),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateBody {
    state: String,
    #[serde(default)]
    superseded_by: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    expected_revision: Option<u64>,
}

pub async fn set_state(
    State(server): State<Arc<Server>>,
    Path((id, record_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let body: StateBody = match parse_body(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(state) = RecordState::parse(&body.state) else {
        return bad_request("state must be active, needs_review, superseded or retracted");
    };
    let superseded = state == RecordState::Superseded;
    if superseded == body.superseded_by.is_empty() {
        return bad_request("superseded_by is required for superseded and only for superseded");
    }
    let request = Request::MemoryState {
        id: record_id,
        state,
        superseded_by: body.superseded_by,
        reason: body.reason,
        expected_revision: body.expected_revision,
    };
    record_reply(&server, run(&server, &id, request).await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyBody {
    #[serde(default)]
    review_in: Option<String>,
    #[serde(default)]
    expected_revision: Option<u64>,
}

pub async fn verify(
    State(server): State<Arc<Server>>,
    Path((id, record_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let body: VerifyBody = if body.is_empty() {
        VerifyBody {
            review_in: None,
            expected_revision: None,
        }
    } else {
        match parse_body(&body) {
            Ok(body) => body,
            Err(response) => return response,
        }
    };
    let review_at = match review_at(body.review_in.as_deref()) {
        Ok(review_at) => review_at,
        Err(response) => return response,
    };
    let request = Request::MemoryVerify {
        id: record_id,
        review_at,
        expected_revision: body.expected_revision,
    };
    record_reply(&server, run(&server, &id, request).await)
}

/// The record that a state, verify or restore write produced.
fn record_reply(server: &Server, reply: Result<Reply, Response>) -> Response {
    match reply {
        Ok(
            Reply::MemoryStateChanged(record)
            | Reply::MemoryVerified(record)
            | Reply::MemoryRestored(record),
        ) => json_response(StatusCode::OK, &MemoryRecord::from(&record)),
        Ok(_) => unexpected(server),
        Err(response) => response,
    }
}

pub async fn history(
    State(server): State<Arc<Server>>,
    Path((id, record_id)): Path<(String, String)>,
) -> Response {
    match run(&server, &id, Request::MemoryHistory { id: record_id }).await {
        Ok(Reply::MemoryHistory(history)) => {
            json_response(StatusCode::OK, &MemoryHistory::from(&history))
        }
        Ok(_) => unexpected(&server),
        Err(response) => response,
    }
}

pub async fn diff(
    State(server): State<Arc<Server>>,
    Path((id, record_id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    let (mut from, mut to) = (None, None);
    for (name, value) in pairs(query.as_deref()) {
        let revision = value
            .parse::<u64>()
            .map_err(|_| bad_request(&format!("{name} must be a revision number")));
        match (name.as_str(), revision) {
            ("from", Ok(revision)) => from = Some(revision),
            ("to", Ok(revision)) => to = Some(revision),
            ("from" | "to", Err(response)) => return response,
            _ => return bad_request(&format!("unknown query parameter {name}")),
        }
    }
    let Some(from) = from else {
        return bad_request("from is required");
    };
    let request = Request::MemoryDiff {
        id: record_id,
        from,
        to,
    };
    match run(&server, &id, request).await {
        Ok(Reply::MemoryDiff { from, to }) => json_response(
            StatusCode::OK,
            &MemoryDiff {
                from: MemoryRecord::from(from.as_ref()),
                to: MemoryRecord::from(to.as_ref()),
            },
        ),
        Ok(_) => unexpected(&server),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreBody {
    revision: u64,
    #[serde(default)]
    expected_revision: Option<u64>,
}

pub async fn restore(
    State(server): State<Arc<Server>>,
    Path((id, record_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let body: RestoreBody = match parse_body(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let request = Request::MemoryRestore {
        id: record_id,
        revision: body.revision,
        expected_revision: body.expected_revision,
    };
    record_reply(&server, run(&server, &id, request).await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposeBody {
    evidence: String,
}

pub async fn propose(
    State(server): State<Arc<Server>>,
    Path((id, record_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let body: ProposeBody = match parse_body(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let evidence = body
        .evidence
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if evidence.is_empty() {
        return bad_request("evidence must not be empty");
    }
    let request = Request::MemoryPropose {
        id: record_id,
        evidence,
    };
    match run(&server, &id, request).await {
        Ok(Reply::MemoryProposed(outcome)) => {
            json_response(StatusCode::OK, &MemoryProposal::from(&outcome))
        }
        Ok(_) => unexpected(&server),
        Err(response) => response,
    }
}

pub async fn search(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let mut text = String::new();
    for (name, value) in pairs(query.as_deref()) {
        match name.as_str() {
            "q" => text = value,
            _ => return bad_request(&format!("unknown query parameter {name}")),
        }
    }
    if text.trim().is_empty() {
        return bad_request("q is required");
    }
    match run(&server, &id, Request::MemorySearch { query: text }).await {
        Ok(Reply::MemorySearch(result)) => json_response(
            StatusCode::OK,
            &MemorySearch {
                records: records(&result.records),
            },
        ),
        Ok(_) => unexpected(&server),
        Err(response) => response,
    }
}

pub async fn candidates(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let (mut limit, mut cursor) = (DEFAULT_CANDIDATE_PAGE, String::new());
    for (name, value) in pairs(query.as_deref()) {
        match name.as_str() {
            "limit" => match page_limit(&value, MAX_CANDIDATE_PAGE) {
                Ok(value) => limit = value,
                Err(response) => return response,
            },
            "cursor" => cursor = value,
            _ => return bad_request(&format!("unknown query parameter {name}")),
        }
    }
    match run(&server, &id, Request::MemoryCandidatePage { limit, cursor }).await {
        Ok(Reply::MemoryCandidatePage(page)) => json_response(
            StatusCode::OK,
            &MemoryCandidatePage {
                candidates: page.candidates.iter().map(MemoryCandidate::from).collect(),
                next_cursor: page.next_cursor,
            },
        ),
        Ok(_) => unexpected(&server),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewBody {
    decision: String,
}

pub async fn review(
    State(server): State<Arc<Server>>,
    Path((id, candidate_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let body: ReviewBody = match parse_body(&body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(decision) = ReviewDecision::parse(&body.decision) else {
        return bad_request("decision must be accept or reject");
    };
    let request = Request::MemoryReview {
        id: candidate_id,
        decision,
        transport: HTTP_REVIEW_TRANSPORT,
    };
    match run(&server, &id, request).await {
        Ok(Reply::MemoryReviewed(decision, result)) => json_response(
            StatusCode::OK,
            &MemoryReviewed {
                decision: decision.as_str(),
                candidate: MemoryCandidate::from(&result.candidate),
                record: result.record.as_ref().map(MemoryRecord::from),
                tombstone: result.tombstone.as_ref().map(MemoryTombstone::from),
            },
        ),
        Ok(_) => unexpected(&server),
        Err(response) => response,
    }
}
