//! The `_otto/memory/*` extension methods: the human side of memory review
//! for ACP clients (docs/specs/2026-10-02-connect-memory-review.md).
//!
//! `remember` and `forget` only queue candidates. These methods are requests
//! from the ACP client, which the model cannot send, and they are the only way
//! to decide a candidate over ACP; `Service::review` records the decision as
//! human. No model-facing tool calls into this module.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::Controller;
use crate::memory::{
    Candidate, CandidateListRequest, CandidateRef, CandidateState, ErrorKind, ReviewDecision,
    ReviewRequest,
};

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
fn memory_error(failure: &crate::memory::Error) -> super::Error {
    let code = match failure.kind {
        ErrorKind::Disabled
        | ErrorKind::Unavailable
        | ErrorKind::Closed
        | ErrorKind::PersistenceDisabled
        | ErrorKind::MemoryInUse
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
    json!({
        "id": candidate.id,
        "action": candidate.action.as_str(),
        "kind": proposed.kind,
        "key": proposed.key,
        "text": proposed.text,
        "reason": candidate.reason,
        "origin": proposed.source.origin.map_or("unknown", |origin| origin.as_str()),
        "scope": {"namespace": proposed.scope.namespace, "id": proposed.scope.id},
    })
}

pub(super) fn pending(controller: &Controller, params: &PendingParams) -> Reply {
    let (service, user_scope, workspace_scope) = controller
        .memory_manager()
        .ok_or_else(|| error(MEMORY_UNAVAILABLE, UNAVAILABLE))?;
    let limit = params.limit.unwrap_or(DEFAULT_PAGE);
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid_params(format!("limit must be 1 to {MAX_PAGE}")));
    }
    let page = service
        .list_candidates(&CandidateListRequest {
            scopes: vec![user_scope, workspace_scope],
            states: vec![CandidateState::Pending],
            limit,
            cursor: params.cursor.clone(),
        })
        .map_err(|failure| memory_error(&failure))?;
    let candidates: Vec<Value> = page.candidates.iter().map(candidate_json).collect();
    Ok(json!({ "candidates": candidates, "nextCursor": page.next_cursor }))
}

pub(super) fn review(controller: &Controller, params: &ReviewParams) -> Reply {
    let decision = ReviewDecision::parse(&params.decision)
        .ok_or_else(|| invalid_params("decision must be accept or reject"))?;
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
    let result = service
        .review(&ReviewRequest {
            reference,
            decision,
            edited: None,
            target_revision: None,
        })
        .map_err(|failure| memory_error(&failure))?;
    Ok(json!({
        "decision": decision.as_str(),
        "candidateId": result.candidate.id,
        "record": result.record.map(|record| json!({"id": record.id, "revision": record.revision})),
        "forgotten": result.tombstone.map(|tombstone| tombstone.id),
    }))
}
