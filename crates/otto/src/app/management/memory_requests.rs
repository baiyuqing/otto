//! Handlers for the `/memory` management requests over the built-in store.

use super::{
    ManagementError, ManagementErrorKind, Reply, Request, SEARCH_LIMIT, SEARCH_TOKEN_BUDGET,
    ScopeSelector,
};
use crate::app::Controller;
use crate::memory::{
    self, CandidateRef, ForgetRequest, ListRequest, RecordRef, ReviewRequest, SearchRequest,
};
use chrono::Utc;

impl Controller {
    /// Handles the `Memory*` requests; any other request is a caller bug.
    pub(super) fn memory_request(&self, request: Request) -> Result<Reply, ManagementError> {
        use Request::*;
        match request {
            MemoryList {
                scope,
                limit,
                cursor,
                filters,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
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
                        .map_err(ManagementError::from)?,
                ))
            }
            MemoryBind {
                id,
                expected_revision,
                evidence,
                applicability,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
                };
                let (_, current) = current_record(&service, [user, workspace], &id)?;
                let mut metadata = current.metadata.clone();
                memory::evidence::bind(&mut metadata, &evidence, &applicability, false)
                    .map_err(ManagementError::from)?;
                service
                    .remember(&memory::RememberRequest {
                        id: current.id,
                        scope: current.scope,
                        kind: current.kind,
                        key: current.key,
                        text: current.text,
                        labels: current.labels,
                        metadata,
                        confidence: current.confidence,
                        expires_at: current.expires_at,
                        expected_revision: Some(expected_revision),
                        source: current.source,
                        review_at: current.review_at,
                    })
                    .map(Reply::Remembered)
                    .map_err(ManagementError::from)
            }
            MemoryState {
                id,
                state,
                superseded_by,
                reason,
                expected_revision,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
                };
                let (reference, current) = current_record(&service, [user, workspace], &id)?;
                service
                    .set_state(&memory::SetStateRequest {
                        reference,
                        expected_revision: expected_revision.unwrap_or(current.revision),
                        state,
                        superseded_by,
                        reason,
                    })
                    .map(Reply::MemoryStateChanged)
                    .map_err(ManagementError::from)
            }
            MemoryVerify {
                id,
                review_at,
                expected_revision,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
                };
                let (reference, current) = current_record(&service, [user, workspace], &id)?;
                service
                    .verify(&memory::VerifyRequest {
                        reference,
                        expected_revision: expected_revision.unwrap_or(current.revision),
                        review_at,
                    })
                    .map(Reply::MemoryVerified)
                    .map_err(ManagementError::from)
            }
            MemoryPropose { id, evidence } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
                };
                let generator = self
                    .current_runner()
                    .and_then(|runner| runner.memory_proposals.clone())
                    .ok_or_else(|| {
                        ManagementError::new(
                            ManagementErrorKind::Unavailable,
                            "memory proposals are not available",
                        )
                    })?;
                let (_, target) = current_record(&service, [user, workspace], &id)?;
                let input = memory::proposals::ProposalInput {
                    target,
                    evidence: vec![memory::proposals::EvidenceItem {
                        id: "e1".into(),
                        text: evidence,
                        trust: otto_core::memory::Trust::Local,
                        source: "user".into(),
                    }],
                    permitted: memory::proposals::ProposalAction::ALL.to_vec(),
                    session_id: String::new(),
                };
                // The generator is async; the management path is synchronous,
                // like the selected-memory `/remember` path above it.
                let handle = tokio::runtime::Handle::try_current().map_err(|_| {
                    ManagementError::new(
                        ManagementErrorKind::Internal,
                        "memory proposals require an async runtime",
                    )
                })?;
                if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
                    return Err(ManagementError::new(
                        ManagementErrorKind::Internal,
                        "memory proposals require a multi-thread runtime",
                    ));
                }
                let cancel = tokio_util::sync::CancellationToken::new();
                let outcome = tokio::task::block_in_place(|| {
                    handle.block_on(generator.propose(&service, input, &cancel))
                });
                Ok(Reply::MemoryProposed(outcome))
            }
            MemoryShow { id } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
                };
                for scope in [user, workspace] {
                    match service.get(&RecordRef {
                        scope,
                        id: id.clone(),
                    }) {
                        Ok(r) => return Ok(Reply::MemoryRecord(r)),
                        Err(error) if error.kind == memory::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Err(ManagementError::not_found(format!("record {id} not found")))
            }
            MemorySearch { query } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
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
                        .map_err(ManagementError::from)?,
                ))
            }
            MemoryForget {
                id,
                expected_revision,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
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
                                    expected_revision: expected_revision.unwrap_or(revision),
                                    purge_backups: false,
                                    confirm_purge: false,
                                })
                                .map_err(ManagementError::from)?;
                            return Ok(Reply::MemoryForgotten {
                                tombstone: result.tombstone,
                                previous_revision: revision,
                                cleanup: result.cleanup,
                            });
                        }
                        Err(e) if e.kind == memory::ErrorKind::NotFound => last = Some(e),
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(ManagementError::not_found(format!(
                    "record {id} not found{}",
                    last.map(|e| format!(": {e}")).unwrap_or_default()
                )))
            }
            MemoryHistory { id } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
                };
                let history = record_history(&service, [user, workspace], &id)?;
                Ok(Reply::MemoryHistory(history))
            }
            MemoryDiff { id, from, to } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
                };
                let history = record_history(&service, [user, workspace], &id)?;
                let revision_at = |revision: u64| {
                    history
                        .revisions
                        .iter()
                        .find(|record| record.revision == revision)
                        .cloned()
                        .ok_or_else(|| {
                            ManagementError::not_found(format!(
                                "record {id} has no revision {revision}"
                            ))
                        })
                };
                let from_record = revision_at(from)?;
                let to_record = match to {
                    Some(to) => revision_at(to)?,
                    None => history.revisions.last().cloned().ok_or_else(|| {
                        ManagementError::not_found(format!("record {id} has no revisions"))
                    })?,
                };
                Ok(Reply::MemoryDiff {
                    from: Box::new(from_record),
                    to: Box::new(to_record),
                })
            }
            MemoryRestore {
                id,
                revision,
                expected_revision,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
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
                                    expected_revision: expected_revision
                                        .unwrap_or(current.revision),
                                })
                                .map_err(ManagementError::from)?;
                            return Ok(Reply::MemoryRestored(restored));
                        }
                        Err(e) if e.kind == memory::ErrorKind::NotFound => last = Some(e),
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(ManagementError::not_found(format!(
                    "record {id} not found{}",
                    last.map(|e| format!(": {e}")).unwrap_or_default()
                )))
            }
            MemoryReviewList => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
                };
                let pending = self.approval_snapshot().map_err(|error| {
                    ManagementError::new(ManagementErrorKind::Internal, error.to_string())
                })?;
                let candidates = crate::memory::approval::human_pending_candidates(
                    &service,
                    &[user, workspace],
                    &pending.requests,
                    SEARCH_LIMIT,
                )
                .map_err(ManagementError::from)?;
                Ok(Reply::MemoryCandidates(candidates))
            }
            MemoryCandidatePage { limit, cursor } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
                };
                let pending = self.approval_snapshot().map_err(|error| {
                    ManagementError::new(ManagementErrorKind::Internal, error.to_string())
                })?;
                crate::memory::approval::human_pending_candidate_page(
                    &service,
                    &[user, workspace],
                    &pending.requests,
                    &cursor,
                    limit,
                )
                .map(Reply::MemoryCandidatePage)
                .map_err(ManagementError::from)
            }
            MemoryReview {
                id,
                decision,
                transport,
            } => {
                let Some((service, user, workspace)) = self.memory_manager() else {
                    return Err(ManagementError::unavailable());
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
                        Err(error) => return Err(error.into()),
                    }
                }
                let reference = reference.ok_or_else(|| {
                    ManagementError::not_found(format!("candidate {id} not found"))
                })?;
                let result = self
                    .review_memory(
                        &ReviewRequest {
                            reference,
                            decision,
                            edited: None,
                            target_revision: None,
                        },
                        transport,
                    )
                    .map_err(ManagementError::from)?;
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
) -> Result<(RecordRef, memory::Record), ManagementError> {
    let mut last = None;
    for scope in scopes {
        let reference = RecordRef {
            scope,
            id: id.to_owned(),
        };
        match service.get(&reference) {
            Ok(record) => return Ok((reference, record)),
            Err(error) if error.kind == memory::ErrorKind::NotFound => last = Some(error),
            Err(error) => return Err(error.into()),
        }
    }
    Err(ManagementError::not_found(format!(
        "record {id} not found{}",
        last.map(|error| format!(": {error}")).unwrap_or_default()
    )))
}

/// The history of record `id` in the first of `scopes` that holds it.
fn record_history(
    service: &memory::Service,
    scopes: [memory::Scope; 2],
    id: &str,
) -> Result<memory::RecordHistory, ManagementError> {
    let mut last = None;
    for scope in scopes {
        match service.history(&RecordRef {
            scope,
            id: id.to_owned(),
        }) {
            Ok(history) => return Ok(history),
            Err(error) if error.kind == memory::ErrorKind::NotFound => last = Some(error),
            Err(error) => return Err(error.into()),
        }
    }
    Err(ManagementError::not_found(format!(
        "record {id} not found{}",
        last.map(|error| format!(": {error}")).unwrap_or_default()
    )))
}
