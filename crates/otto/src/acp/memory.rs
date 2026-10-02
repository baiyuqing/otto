//! The `_otto/memory/*` extension methods: the human side of memory review
//! for ACP clients (docs/specs/2026-10-02-connect-memory-review.md).
//!
//! `remember` and `forget` only queue candidates. These methods are requests
//! from the ACP client, which the model cannot send, and they are the only way
//! to decide a candidate over ACP; `Service::review` records the decision as
//! human. No model-facing tool calls into this module.

use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::Controller;
use crate::memory::{
    Candidate, CandidateRef, CandidateState, ReviewDecision, ReviewRequest, SearchRequest,
};

use super::{INTERNAL_ERROR, RESOURCE_NOT_FOUND, Reply, error, invalid_params};

pub(super) const PENDING_METHOD: &str = "_otto/memory/pending";
pub(super) const REVIEW_METHOD: &str = "_otto/memory/review";

const PENDING_LIMIT: usize = 20;
const PENDING_TOKEN_BUDGET: usize = 4000;
const UNAVAILABLE: &str = "memory is not available in this session";

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

pub(super) fn pending(controller: &Controller) -> Reply {
    let (service, user_scope, workspace_scope) = controller
        .memory_manager()
        .ok_or_else(|| error(INTERNAL_ERROR, UNAVAILABLE))?;
    let found = service
        .search(&SearchRequest {
            scopes: vec![user_scope, workspace_scope],
            include_candidates: true,
            candidate_states: vec![CandidateState::Pending],
            limit: PENDING_LIMIT,
            token_budget: PENDING_TOKEN_BUDGET,
            now: Utc::now(),
            ..SearchRequest::default()
        })
        .map_err(|failure| error(INTERNAL_ERROR, failure.to_string()))?;
    let candidates: Vec<Value> = found.candidates.iter().map(candidate_json).collect();
    Ok(json!({ "candidates": candidates }))
}

pub(super) fn review(controller: &Controller, params: &ReviewParams) -> Reply {
    let decision = ReviewDecision::parse(&params.decision)
        .ok_or_else(|| invalid_params("decision must be accept or reject"))?;
    let (service, user_scope, workspace_scope) = controller
        .memory_manager()
        .ok_or_else(|| error(INTERNAL_ERROR, UNAVAILABLE))?;
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
        .map_err(|failure| error(INTERNAL_ERROR, failure.to_string()))?;
    Ok(json!({
        "decision": decision.as_str(),
        "candidateId": result.candidate.id,
        "record": result.record.map(|record| json!({"id": record.id, "revision": record.revision})),
        "forgotten": result.tombstone.map(|tombstone| tombstone.id),
    }))
}
