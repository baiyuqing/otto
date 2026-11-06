//! Handlers for the `/memory` management requests over the built-in store.

use super::{Reply, Request, SEARCH_LIMIT, SEARCH_TOKEN_BUDGET, ScopeSelector};
use crate::app::Controller;
use crate::memory::{
    self, CandidateRef, ForgetRequest, ListRequest, RecordRef, ReviewRequest, SearchRequest,
};
use chrono::Utc;

impl Controller {
    /// Handles the `Memory*` requests; any other request is a caller bug.
    pub(super) fn memory_request(&self, request: Request) -> Result<Reply, String> {
        use Request::*;
        match request {
            MemoryList {
                scope,
                limit,
                cursor,
                filters,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let (all_scopes, scopes) = match scope {
                    ScopeSelector::All => (true, Vec::new()),
                    ScopeSelector::User => (false, vec![user]),
                    ScopeSelector::Workspace => (false, vec![workspace]),
                    ScopeSelector::Current => (false, vec![user, workspace]),
                };
                Ok(Reply::MemoryList(
                    service
                        .list(&ListRequest {
                            all_scopes,
                            scopes,
                            kinds: filters.kinds,
                            labels: filters.labels,
                            limit,
                            cursor,
                            now: Utc::now(),
                            include_expired: true,
                            states: filters.states,
                            updated_after: filters.updated_after,
                            verified_after: filters.verified_after,
                            unverified_only: filters.unverified_only,
                            review_due_only: filters.review_due_only,
                        })
                        .map_err(|e| e.to_string())?,
                ))
            }
            MemoryState {
                id,
                state,
                superseded_by,
                reason,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let (reference, current) = current_record(&service, [user, workspace], &id)?;
                service
                    .set_state(&memory::SetStateRequest {
                        reference,
                        expected_revision: current.revision,
                        state,
                        superseded_by,
                        reason,
                    })
                    .map(Reply::MemoryStateChanged)
                    .map_err(|error| error.to_string())
            }
            MemoryVerify { id, review_at } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let (reference, current) = current_record(&service, [user, workspace], &id)?;
                service
                    .verify(&memory::VerifyRequest {
                        reference,
                        expected_revision: current.revision,
                        review_at,
                    })
                    .map(Reply::MemoryVerified)
                    .map_err(|error| error.to_string())
            }
            MemoryShow { id } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                for scope in [user, workspace] {
                    match service.get(&RecordRef {
                        scope,
                        id: id.clone(),
                    }) {
                        Ok(r) => return Ok(Reply::MemoryRecord(r)),
                        Err(error) if error.kind == memory::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.to_string()),
                    }
                }
                Err(format!("record {id} not found"))
            }
            MemorySearch { query } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                Ok(Reply::MemorySearch(
                    service
                        .search(&SearchRequest {
                            query,
                            scopes: vec![user, workspace],
                            limit: SEARCH_LIMIT,
                            token_budget: SEARCH_TOKEN_BUDGET,
                            now: Utc::now(),
                            ..Default::default()
                        })
                        .map_err(|e| e.to_string())?,
                ))
            }
            MemoryForget { id } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let mut last = None;
                for scope in [user, workspace] {
                    let reference = RecordRef {
                        scope,
                        id: id.clone(),
                    };
                    match service.get(&reference) {
                        Ok(r) => {
                            let revision = r.revision;
                            let result = service
                                .forget(&ForgetRequest {
                                    reference,
                                    expected_revision: revision,
                                    purge_backups: false,
                                    confirm_purge: false,
                                })
                                .map_err(|e| e.to_string())?;
                            return Ok(Reply::MemoryForgotten {
                                tombstone: result.tombstone,
                                previous_revision: revision,
                                cleanup: result.cleanup,
                            });
                        }
                        Err(e) if e.kind == memory::ErrorKind::NotFound => last = Some(e),
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Err(format!(
                    "record {id} not found{}",
                    last.map(|e| format!(": {e}")).unwrap_or_default()
                ))
            }
            MemoryHistory { id } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let history = record_history(&service, [user, workspace], &id)?;
                Ok(Reply::MemoryHistory(history))
            }
            MemoryDiff { id, from, to } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let history = record_history(&service, [user, workspace], &id)?;
                let revision_at = |revision: u64| {
                    history
                        .revisions
                        .iter()
                        .find(|record| record.revision == revision)
                        .cloned()
                        .ok_or_else(|| format!("record {id} has no revision {revision}"))
                };
                let from_record = revision_at(from)?;
                let to_record = match to {
                    Some(to) => revision_at(to)?,
                    None => history
                        .revisions
                        .last()
                        .cloned()
                        .ok_or_else(|| format!("record {id} has no revisions"))?,
                };
                Ok(Reply::MemoryDiff {
                    from: Box::new(from_record),
                    to: Box::new(to_record),
                })
            }
            MemoryRestore { id, revision } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let mut last = None;
                for scope in [user, workspace] {
                    let reference = RecordRef {
                        scope,
                        id: id.clone(),
                    };
                    match service.get(&reference) {
                        Ok(current) => {
                            let restored = service
                                .restore(&memory::RestoreRequest {
                                    reference,
                                    revision,
                                    expected_revision: current.revision,
                                })
                                .map_err(|e| e.to_string())?;
                            return Ok(Reply::MemoryRestored(restored));
                        }
                        Err(e) if e.kind == memory::ErrorKind::NotFound => last = Some(e),
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Err(format!(
                    "record {id} not found{}",
                    last.map(|e| format!(": {e}")).unwrap_or_default()
                ))
            }
            MemoryReviewList => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let pending = self
                    .approval_snapshot()
                    .map_err(|error| error.to_string())?;
                let candidates = crate::approval::memory::human_pending_candidates(
                    &service,
                    &[user, workspace],
                    &pending.requests,
                    SEARCH_LIMIT,
                )
                .map_err(|error| error.to_string())?;
                Ok(Reply::MemoryCandidates(candidates))
            }
            MemoryReview { id, decision } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err("memory is not available".into());
                };
                let mut reference = None;
                for scope in [user, workspace] {
                    let candidate_ref = CandidateRef {
                        scope,
                        id: id.clone(),
                    };
                    match service.get_candidate(&candidate_ref) {
                        Ok(_) => {
                            reference = Some(candidate_ref);
                            break;
                        }
                        Err(error) if error.kind == memory::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.to_string()),
                    }
                }
                let reference = reference.ok_or_else(|| format!("candidate {id} not found"))?;
                let result = self
                    .review_memory(
                        &ReviewRequest {
                            reference,
                            decision,
                            edited: None,
                            target_revision: None,
                        },
                        "memory-command",
                    )
                    .map_err(|e| e.to_string())?;
                Ok(Reply::MemoryReviewed(decision, Box::new(result)))
            }
            _ => unreachable!("not a memory request"),
        }
    }
}

/// The active record `id` in the first of `scopes` that holds it, with its
/// reference, for a write that names the current revision.
fn current_record(
    service: &memory::Service,
    scopes: [memory::Scope; 2],
    id: &str,
) -> Result<(RecordRef, memory::Record), String> {
    let mut last = None;
    for scope in scopes {
        let reference = RecordRef {
            scope,
            id: id.to_owned(),
        };
        match service.get(&reference) {
            Ok(record) => return Ok((reference, record)),
            Err(error) if error.kind == memory::ErrorKind::NotFound => last = Some(error),
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "record {id} not found{}",
        last.map(|error| format!(": {error}")).unwrap_or_default()
    ))
}

/// The history of record `id` in the first of `scopes` that holds it.
fn record_history(
    service: &memory::Service,
    scopes: [memory::Scope; 2],
    id: &str,
) -> Result<memory::RecordHistory, String> {
    let mut last = None;
    for scope in scopes {
        match service.history(&RecordRef {
            scope,
            id: id.to_owned(),
        }) {
            Ok(history) => return Ok(history),
            Err(error) if error.kind == memory::ErrorKind::NotFound => last = Some(error),
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "record {id} not found{}",
        last.map(|error| format!(": {error}")).unwrap_or_default()
    ))
}
