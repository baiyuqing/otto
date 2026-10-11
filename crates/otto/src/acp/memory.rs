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
use crate::memory::{Candidate, CandidateRef, ErrorKind, ReviewDecision, ReviewRequest};

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
    if let Some(line) = crate::memory::advice::advice_line(proposed) {
        value["advice"] = Value::String(line);
    }
    value
}

pub(super) fn pending(controller: &Controller, params: &PendingParams) -> Reply {
    let limit = params.limit.unwrap_or(DEFAULT_PAGE);
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid_params(format!("limit must be 1 to {MAX_PAGE}")));
    }
    let page = controller
        .pending_memory_review_page(&params.cursor, limit)
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

#[cfg(test)]
mod advice_tests {
    use super::*;
    use crate::memory::advice::testing::{LINE, candidate};

    fn service() -> (tempfile::TempDir, crate::memory::Service) {
        let (directory, store) = crate::memory::turso::testsupport::open_temp();
        (
            directory,
            crate::memory::Service::new(store, crate::memory::decide_default_policy),
        )
    }

    #[test]
    fn candidate_json_carries_the_rendered_advice_line_as_a_string() {
        let (_directory, service) = service();
        let scope = crate::memory::Scope::new(crate::memory::NAMESPACE_USER, "user-1");

        let value = candidate_json(&candidate(&service, scope, true));

        assert_eq!(value["advice"], Value::String(LINE.to_owned()));
        assert_eq!(value["key"], "editor");
    }

    #[test]
    fn candidate_json_omits_advice_when_there_is_none() {
        let (_directory, service) = service();
        let scope = crate::memory::Scope::new(crate::memory::NAMESPACE_USER, "user-1");

        let value = candidate_json(&candidate(&service, scope, false));

        assert!(value.get("advice").is_none(), "{value}");
        assert_eq!(value.as_object().expect("object").len(), 8);
    }
}
