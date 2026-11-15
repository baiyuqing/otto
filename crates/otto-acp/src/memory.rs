//! The `_otto/memory/*` extension methods: the human side of memory review
//! for ACP clients.
//!
//! `remember` and `forget` only queue candidates. These methods are requests
//! from the ACP client, which the model cannot send, and they are the only way
//! to decide a candidate over ACP; `Service::review` records the decision as
//! human. No model-facing tool calls into this module.

use serde::Deserialize;
use serde_json::{Value, json};

use otto::app::Controller;
use otto::memory::{Candidate, CandidateRef, ErrorKind, ReviewDecision, ReviewRequest};

use super::{
    INTERNAL_ERROR, MEMORY_CONFLICT, MEMORY_UNAVAILABLE, RESOURCE_NOT_FOUND, Reply, error,
    invalid_params,
};

pub(super) const PENDING_METHOD: &str = "_otto/memory/pending";
pub(super) const REVIEW_METHOD: &str = "_otto/memory/review";

const DEFAULT_PAGE: usize = 20;
const MAX_PAGE: usize = 50;
const UNAVAILABLE: &str = "memory is not available in this session";

/// Maps a memory error to the code a client can act on: unavailable memory,
/// a candidate that was already decided or changed, a missing candidate, or
/// bad paging input. Everything else is an internal error.
fn memory_error(failure: &otto::memory::Error) -> super::Error {
    let code = match failure.kind {
        ErrorKind::Disabled
        | ErrorKind::Unavailable
        | ErrorKind::Closed
        | ErrorKind::PersistenceDisabled
        | ErrorKind::Busy => MEMORY_UNAVAILABLE,
        ErrorKind::Conflict => MEMORY_CONFLICT,
        ErrorKind::NotFound => RESOURCE_NOT_FOUND,
        ErrorKind::InvalidRequest | ErrorKind::InvalidCursor => super::INVALID_PARAMS,
        _ => INTERNAL_ERROR,
    };
    error(code, failure.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ReviewParams {
    pub session_id: String,
    candidate_id: String,
    decision: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PendingParams {
    pub session_id: String,
    #[serde(default)]
    cursor: String,
    limit: Option<usize>,
}

fn candidate_json(candidate: &Candidate) -> Value {
    let proposed = &candidate.proposed;
    let mut value = json!({
        "id": candidate.id,
        "action": candidate.action.as_str(),
        "kind": proposed.kind,
        "key": proposed.key,
        "text": proposed.text,
        "reason": candidate.reason,
        "origin": proposed.source.origin.map_or("unknown", |origin| origin.as_str()),
        "scope": {"namespace": proposed.scope.namespace, "id": proposed.scope.id},
    });
    if let Some(line) = otto::memory::advice::advice_line(proposed) {
        value["advice"] = Value::String(line);
    }
    value
}

/// The page size `params` asks for: 1 to [`MAX_PAGE`], [`DEFAULT_PAGE`]
/// when absent.
fn page_limit(params: &PendingParams) -> Result<usize, super::Error> {
    let limit = params.limit.unwrap_or(DEFAULT_PAGE);
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid_params(format!("limit must be 1 to {MAX_PAGE}")));
    }
    Ok(limit)
}

pub(super) fn pending(controller: &Controller, params: &PendingParams) -> Reply {
    let limit = page_limit(params)?;
    let (memory, user_scope, workspace_scope) = controller
        .memory_manager()
        .ok_or_else(|| error(MEMORY_UNAVAILABLE, UNAVAILABLE))?;
    let approvals = controller
        .approval_snapshot()
        .map_err(|failure| error(INTERNAL_ERROR, failure.to_string()))?;
    let page = otto::approval::memory::human_pending_candidate_page(
        &memory,
        &[user_scope, workspace_scope],
        &approvals.requests,
        &params.cursor,
        limit,
    )
    .map_err(|failure| memory_error(&failure))?;
    let candidates: Vec<Value> = page.candidates.iter().map(candidate_json).collect();
    Ok(json!({ "candidates": candidates, "nextCursor": page.next_cursor }))
}

fn review_decision(params: &ReviewParams) -> Result<ReviewDecision, super::Error> {
    ReviewDecision::parse(&params.decision)
        .ok_or_else(|| invalid_params("decision must be accept or reject"))
}

pub(super) fn review(controller: &Controller, params: &ReviewParams) -> Reply {
    let decision = review_decision(params)?;
    let (service, user_scope, workspace_scope) = controller
        .memory_manager()
        .ok_or_else(|| error(MEMORY_UNAVAILABLE, UNAVAILABLE))?;
    let id = &params.candidate_id;
    let reference = [user_scope, workspace_scope]
        .into_iter()
        .map(|scope| CandidateRef {
            scope,
            id: id.clone(),
        })
        .find(|reference| service.get_candidate(reference).is_ok())
        .ok_or_else(|| error(RESOURCE_NOT_FOUND, format!("candidate {id} not found")))?;
    let result = controller
        .review_memory(
            &ReviewRequest {
                reference,
                decision,
                edited: None,
                target_revision: None,
            },
            "acp-client",
        )
        .map_err(|failure| memory_error(&failure))?;
    Ok(json!({
        "decision": decision.as_str(),
        "candidateId": result.candidate.id,
        "record": result.record.map(|record| json!({"id": record.id, "revision": record.revision})),
        "forgotten": result.tombstone.map(|tombstone| tombstone.id),
    }))
}

// ---- attached sessions ----
//
// `otto acp --attach` holds no controller: the session lives in `otto serve`.
// It reviews through serve's session memory routes and returns the same
// shapes as the in-process functions above.

/// The ACP error for a failed memory route, by HTTP status. `None` for a
/// failure to reach serve, which the relay handles itself.
fn attached_error(failure: &otto_client::Error) -> Option<super::Error> {
    let otto_client::Error::Http {
        status, message, ..
    } = failure
    else {
        return None;
    };
    let code = match status {
        501 | 503 => MEMORY_UNAVAILABLE,
        409 => MEMORY_CONFLICT,
        404 => RESOURCE_NOT_FOUND,
        400 => super::INVALID_PARAMS,
        _ => INTERNAL_ERROR,
    };
    Some(error(code, message.clone()))
}

/// One candidate of serve's `MemoryCandidate` JSON in the shape of
/// [`candidate_json`].
fn attached_candidate_json(candidate: &Value) -> Value {
    let mut value = json!({
        "id": candidate["id"],
        "action": candidate["action"],
        "kind": candidate["kind"],
        "key": candidate["key"],
        "text": candidate["text"],
        "reason": candidate["reason"],
        "origin": candidate["origin"],
        "scope": {"namespace": candidate["scope"]["namespace"], "id": candidate["scope"]["id"]},
    });
    if let Some(advice) = candidate.get("advice").filter(|advice| advice.is_string()) {
        value["advice"] = advice.clone();
    }
    value
}

/// Candidate ids are letters, digits, `-` and `_`. Anything else cannot name
/// a candidate and is not placed in a request path.
fn candidate_id_is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// [`pending`] for a session held by serve.
pub(super) async fn attached_pending(
    client: &otto_client::Client,
    params: &PendingParams,
) -> Result<Value, AttachedFailure> {
    let limit = page_limit(params).map_err(AttachedFailure::Reply)?;
    let page = client
        .memory_candidates(&params.session_id, &params.cursor, limit)
        .await
        .map_err(AttachedFailure::from)?;
    let candidates: Vec<Value> = page["candidates"]
        .as_array()
        .map(|rows| rows.iter().map(attached_candidate_json).collect())
        .unwrap_or_default();
    Ok(json!({ "candidates": candidates, "nextCursor": page["next_cursor"] }))
}

/// [`review`] for a session held by serve. Serve records the decision as a
/// human review.
pub(super) async fn attached_review(
    client: &otto_client::Client,
    params: &ReviewParams,
) -> Result<Value, AttachedFailure> {
    let decision = review_decision(params).map_err(AttachedFailure::Reply)?;
    let id = &params.candidate_id;
    if !candidate_id_is_valid(id) {
        return Err(AttachedFailure::Reply(error(
            RESOURCE_NOT_FOUND,
            format!("candidate {id} not found"),
        )));
    }
    let reviewed = client
        .review_memory_candidate(&params.session_id, id, decision.as_str())
        .await
        .map_err(AttachedFailure::from)?;
    let record = reviewed
        .get("record")
        .filter(|record| record.is_object())
        .map_or(
            Value::Null,
            |record| json!({"id": record["id"], "revision": record["revision"]}),
        );
    Ok(json!({
        "decision": reviewed["decision"],
        "candidateId": reviewed["candidate"]["id"],
        "record": record,
        "forgotten": reviewed.get("tombstone").map_or(Value::Null, |tombstone| tombstone["id"].clone()),
    }))
}

/// Why an attached memory request failed: an ACP error to answer, or serve
/// could not be reached.
pub(super) enum AttachedFailure {
    Reply(super::Error),
    Unreachable(otto_client::Error),
}

impl From<otto_client::Error> for AttachedFailure {
    fn from(failure: otto_client::Error) -> Self {
        match attached_error(&failure) {
            Some(reply) => Self::Reply(reply),
            None => Self::Unreachable(failure),
        }
    }
}

#[cfg(test)]
mod attached_tests {
    use super::*;

    #[test]
    fn an_http_candidate_keeps_exactly_the_in_process_fields() {
        let http = json!({
            "id": "c1", "action": "create", "state": "pending", "target_id": "r1",
            "base_revision": 0, "kind": "preference", "key": "editor", "text": "vim",
            "reason": "asked", "origin": "model",
            "scope": {"namespace": "user", "id": "u1"},
            "created_at": "2026-11-14T00:00:00Z", "advice": "check it",
            "proposed": {"id": "r1"},
        });
        let value = attached_candidate_json(&http);
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "action", "advice", "id", "key", "kind", "origin", "reason", "scope", "text"
            ]
        );
        assert_eq!(value["scope"], json!({"namespace": "user", "id": "u1"}));
        assert_eq!(value["advice"], "check it");
    }

    #[test]
    fn http_statuses_map_to_the_in_process_error_codes() {
        let http = |status| otto_client::Error::Http {
            status,
            code: "x".into(),
            message: "m".into(),
        };
        let code = |status| attached_error(&http(status)).unwrap().code;
        let want = |code: i32| error(code, "").code;
        assert_eq!(code(503), want(MEMORY_UNAVAILABLE));
        assert_eq!(code(501), want(MEMORY_UNAVAILABLE));
        assert_eq!(code(409), want(MEMORY_CONFLICT));
        assert_eq!(code(404), want(RESOURCE_NOT_FOUND));
        assert_eq!(code(400), want(super::super::INVALID_PARAMS));
        assert_eq!(code(500), want(INTERNAL_ERROR));
        assert!(attached_error(&otto_client::Error::Unreachable("down".into())).is_none());
    }

    #[test]
    fn a_candidate_id_outside_the_id_alphabet_never_reaches_a_path() {
        assert!(candidate_id_is_valid("0a1b-c_2"));
        for id in ["", "../records", "a/b", "a?x=1", "a b"] {
            assert!(!candidate_id_is_valid(id), "{id}");
        }
    }
}

#[cfg(test)]
mod advice_tests {
    use super::*;
    use otto::memory::advice::testing::{LINE, candidate};

    fn service() -> (tempfile::TempDir, otto::memory::Service) {
        let (directory, store) = otto::memory::files::testsupport::open_temp();
        (
            directory,
            otto::memory::Service::new(store, otto::memory::decide_default_policy),
        )
    }

    #[test]
    fn candidate_json_carries_the_rendered_advice_line_as_a_string() {
        let (_directory, service) = service();
        let scope = otto::memory::Scope::new(otto::memory::NAMESPACE_USER, "user-1");

        let value = candidate_json(&candidate(&service, scope, true));

        assert_eq!(value["advice"], Value::String(LINE.to_owned()));
        assert_eq!(value["key"], "editor");
    }

    #[test]
    fn candidate_json_omits_advice_when_there_is_none() {
        let (_directory, service) = service();
        let scope = otto::memory::Scope::new(otto::memory::NAMESPACE_USER, "user-1");

        let value = candidate_json(&candidate(&service, scope, false));

        assert!(value.get("advice").is_none(), "{value}");
        assert_eq!(value.as_object().expect("object").len(), 8);
    }
}
