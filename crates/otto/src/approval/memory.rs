//! Memory retains review data and the atomic candidate/result receipt.
use super::{Authority, Domain, Error, Kind, Proposal, Request, Result, Service, State, Subject};
use crate::memory::service::PendingSummary;
use crate::memory::{
    CandidateListRequest, CandidateRef, CandidateState, ReviewDecision, ReviewRequest, Scope,
};
use std::sync::Arc;

pub struct Adapter {
    pub memory: Arc<crate::memory::Service>,
    pub scopes: Vec<Scope>,
}

/// Candidate queues show actionable human requests, never an in-flight review.
pub fn is_human_pending(candidate: &crate::memory::Candidate, requests: &[Request]) -> bool {
    let suffix = format!(
        "/{}/{}/{}",
        candidate.proposed.scope.namespace, candidate.proposed.scope.id, candidate.id
    );
    requests.iter().any(|request| {
        request.subject.source == "memory"
            && request.subject.id.ends_with(&suffix)
            && request.is_human_pending()
    })
}

/// Returns the actionable candidates that `/memory review` can decide.
///
/// Storage-level pending candidates are not necessarily waiting for a human:
/// their approval can be under automatic review, already withdrawn, or otherwise
/// non-actionable. Page before filtering so those rows cannot hide a later
/// human-pending candidate.
pub fn human_pending_candidates(
    memory: &crate::memory::Service,
    scopes: &[Scope],
    requests: &[Request],
    limit: usize,
) -> crate::memory::Result<Vec<crate::memory::Candidate>> {
    let mut candidates = Vec::new();
    let mut cursor = String::new();
    while candidates.len() < limit {
        let page = memory.list_candidates(&CandidateListRequest {
            scopes: scopes.to_vec(),
            states: vec![CandidateState::Pending],
            limit: crate::memory::MAX_PAGE_SIZE,
            cursor,
        })?;
        candidates.extend(
            page.candidates
                .into_iter()
                .filter(|candidate| is_human_pending(candidate, requests))
                .take(limit - candidates.len()),
        );
        if page.next_cursor.is_empty() {
            break;
        }
        cursor = page.next_cursor;
    }
    Ok(candidates)
}

/// Counts only candidates that are currently actionable in `/memory review`.
pub fn human_pending_summary(
    memory: &crate::memory::Service,
    scopes: &[Scope],
    requests: &[Request],
) -> crate::memory::Result<PendingSummary> {
    let candidates = human_pending_candidates(memory, scopes, requests, usize::MAX)?;
    Ok(PendingSummary {
        count: candidates.len(),
        oldest: candidates
            .iter()
            .map(|candidate| candidate.created_at)
            .min(),
    })
}
impl Adapter {
    fn reference(&self, request: &Request) -> Result<CandidateRef> {
        let reference: CandidateRef =
            serde_json::from_str(&request.subject.details).map_err(|_| Error::Unavailable)?;
        if !self.scopes.contains(&reference.scope) {
            return Err(Error::Unauthorized);
        }
        Ok(reference)
    }
}
impl Domain for Adapter {
    fn details(&self, request: &Request) -> Result<String> {
        let reference = self.reference(request)?;
        let candidate = self
            .memory
            .get_candidate(&reference)
            .map_err(|_| Error::Unavailable)?;
        let expected = if request.state == State::Approved {
            CandidateState::Accepted
        } else {
            CandidateState::Rejected
        };
        if request.application == super::ApplicationState::Applied
            && candidate.state == expected
            && candidate.decided_at.is_some()
        {
            // Memory intentionally clears decided proposal bodies. Its
            // committed candidate receipt and authority application receipt
            // preserve the original reference/digest without inventing text.
            return Ok(format!(
                "Memory candidate {}: {}. Original authorized subject digest: {}. Result record: {}; revision: {}.",
                candidate.id,
                candidate.state.as_str(),
                request.subject.version,
                candidate.result_record_id,
                candidate.result_revision
            ));
        }
        if super::digest(&format!(
            "{}:{}:{:?}",
            candidate.created_at, candidate.base_revision, candidate.proposed
        )) != request.subject.version
        {
            return Err(Error::Conflict);
        }
        let mut details = format!(
            "{} {}/{}\n\n{}\n\nReason: {}\nBase revision: {}",
            candidate.action.as_str(),
            candidate.proposed.kind,
            candidate.proposed.key,
            candidate.proposed.text,
            candidate.reason,
            candidate.base_revision
        );
        if let Some(advice) = crate::memory::advice::advice_line(&candidate.proposed) {
            details.push_str("\n\n");
            details.push_str(&advice);
        }
        Ok(details)
    }
    fn apply(&self, request: &Request) -> Result<()> {
        let reference = self.reference(request)?;
        let candidate = self
            .memory
            .get_candidate(&reference)
            .map_err(|_| Error::Unavailable)?;
        let expected = if request.state == State::Approved {
            CandidateState::Accepted
        } else {
            CandidateState::Rejected
        };
        if candidate.state == expected {
            return Ok(());
        }
        if candidate.state != CandidateState::Pending {
            return Err(Error::Conflict);
        }
        if super::digest(&format!(
            "{}:{}:{:?}",
            candidate.created_at, candidate.base_revision, candidate.proposed
        )) != request.subject.version
        {
            return Err(Error::Conflict);
        }
        self.memory
            .review(&ReviewRequest {
                reference,
                decision: if expected == CandidateState::Accepted {
                    ReviewDecision::Accept
                } else {
                    ReviewDecision::Reject
                },
                edited: None,
                target_revision: None,
            })
            .map(|_| ())
            .map_err(|_| Error::Conflict)
    }
}

pub fn reconcile(
    service: &Service,
    memory: &crate::memory::Service,
    scopes: &[Scope],
    workspace: &str,
) -> Result<()> {
    let mut cursor = String::new();
    loop {
        let page = memory
            .list_candidates(&CandidateListRequest {
                scopes: scopes.to_vec(),
                states: vec![CandidateState::Pending],
                limit: 50,
                cursor: cursor.clone(),
            })
            .map_err(|_| Error::Unavailable)?;
        for candidate in page.candidates {
            publish(service, scopes, workspace, &candidate)?;
        }
        if page.next_cursor.is_empty() {
            break;
        }
        cursor = page.next_cursor;
    }
    service.apply_available(workspace)
}

fn proposal(
    scopes: &[Scope],
    workspace: &str,
    candidate: &crate::memory::Candidate,
) -> Result<Proposal> {
    let reference = CandidateRef {
        scope: candidate.proposed.scope.clone(),
        id: candidate.id.clone(),
    };
    Ok(Proposal {
        kind: Kind::Authorization,
        authority: Authority::Durable,
        scope: super::Scope {
            workspace: workspace.into(),
            // Memory candidates outlive their originating session. Keep the
            // accept/reject decision one-shot, but make its approval visible
            // to every session in this workspace so `/memory review` can
            // always clear a pending candidate.
            session_id: String::new(),
            task_id: String::new(),
            run_id: String::new(),
        },
        subject: Subject {
            tool_call_id: String::new(),
            source: "memory".into(),
            id: format!(
                "{}/{}/{}/{}",
                scopes.first().map_or("", |s| s.id.as_str()),
                reference.scope.namespace,
                reference.scope.id,
                reference.id
            ),
            version: super::digest(&format!(
                "{}:{}:{:?}",
                candidate.created_at, candidate.base_revision, candidate.proposed
            )),
            details: serde_json::to_string(&reference).map_err(|_| Error::Unavailable)?,
        },
        title: format!("Review memory {}", candidate.action.as_str()),
        reason: "Review this memory change before it is applied.".into(),
        persistent: false,
        blocking: false,
        expires_at: None,
    })
}

pub fn publish(
    service: &Service,
    scopes: &[Scope],
    workspace: &str,
    candidate: &crate::memory::Candidate,
) -> Result<()> {
    if !scopes.contains(&candidate.proposed.scope) {
        return Ok(());
    }
    service.create_with_origin(
        proposal(scopes, workspace, candidate)?,
        candidate
            .proposed
            .metadata
            .get(crate::memory::PRESENTATION_ORIGIN_METADATA)
            .map_or("", String::as_str),
    )?;
    Ok(())
}

pub fn attach(
    service: &Arc<Service>,
    memory: &Arc<crate::memory::Service>,
    scopes: Vec<Scope>,
    workspace: &str,
) -> Result<()> {
    service.register_domain(
        workspace,
        "memory",
        Arc::new(Adapter {
            memory: memory.clone(),
            scopes: scopes.clone(),
        }),
    )?;
    let approval = Arc::downgrade(service);
    let workspace = workspace.to_string();
    let owner = workspace.clone();
    memory.set_review_publisher(
        &owner,
        Arc::new(move |candidates| {
            let service = approval
                .upgrade()
                .ok_or_else(|| crate::memory::Error::new(crate::memory::ErrorKind::Unavailable))?;
            for candidate in candidates {
                publish(&service, &scopes, &workspace, candidate).map_err(|_| {
                    crate::memory::Error::new(crate::memory::ErrorKind::Unavailable)
                })?;
            }
            Ok(())
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_makes_legacy_session_scoped_memory_approval_workspace_visible() {
        let (_dir, store) = crate::memory::turso::testsupport::open_temp();
        let user = store.identity().unwrap().user_scope;
        let memory = Arc::new(crate::memory::Service::new(
            store,
            crate::memory::decide_default_policy,
        ));
        let candidate = memory
            .propose(&crate::memory::ProposeRequest {
                scope: user.clone(),
                kind: "preference".into(),
                key: "legacy".into(),
                text: "legacy candidate".into(),
                reason: "created by an older version".into(),
                source: crate::memory::Provenance {
                    origin: Some(crate::memory::Origin::Model),
                    session_id: "old-session".into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .remove(0);
        let approvals = Arc::new(Service::in_memory().unwrap());
        let mut legacy = proposal(std::slice::from_ref(&user), "/w", &candidate).unwrap();
        legacy.scope.session_id = "old-session".into();
        approvals.create(legacy).unwrap();
        assert!(
            approvals
                .snapshot("/w", Some("new-session"))
                .unwrap()
                .requests
                .is_empty(),
            "the legacy request starts scoped to its originating session"
        );

        attach(&approvals, &memory, vec![user.clone()], "/w").unwrap();
        reconcile(&approvals, &memory, &[user], "/w").unwrap();

        assert_eq!(
            approvals
                .snapshot("/w", Some("new-session"))
                .unwrap()
                .requests
                .len(),
            1,
            "republication adds workspace visibility to the existing approval"
        );
    }

    #[test]
    fn human_pending_query_pages_past_non_actionable_storage_candidates() {
        let (_dir, store) = crate::memory::turso::testsupport::open_temp();
        let user = store.identity().unwrap().user_scope;
        let memory = Arc::new(crate::memory::Service::new(
            store,
            crate::memory::decide_default_policy,
        ));
        for index in 0..20 {
            memory
                .propose(&crate::memory::ProposeRequest {
                    scope: user.clone(),
                    kind: "preference".into(),
                    key: format!("orphan-{index}"),
                    text: format!("orphan {index}"),
                    reason: "no approval publisher was attached".into(),
                    source: crate::memory::Provenance {
                        origin: Some(crate::memory::Origin::Model),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .unwrap();
        }

        let approvals = Arc::new(Service::in_memory().unwrap());
        attach(&approvals, &memory, vec![user.clone()], "/w").unwrap();
        let actionable = memory
            .propose(&crate::memory::ProposeRequest {
                scope: user.clone(),
                kind: "preference".into(),
                key: "actionable".into(),
                text: "actionable".into(),
                reason: "awaiting human review".into(),
                source: crate::memory::Provenance {
                    origin: Some(crate::memory::Origin::Model),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .remove(0);
        let requests = approvals.snapshot("/w", Some("session")).unwrap().requests;

        let candidates = human_pending_candidates(&memory, &[user.clone()], &requests, 20).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, actionable.id);
        assert_eq!(
            human_pending_summary(&memory, &[user], &requests).unwrap(),
            PendingSummary {
                count: 1,
                oldest: Some(actionable.created_at),
            }
        );
    }

    #[test]
    fn unified_review_preserves_advice_without_turning_it_into_a_decision() {
        let (_dir, store) = crate::memory::turso::testsupport::open_temp();
        let user = store.identity().unwrap().user_scope;
        let memory = Arc::new(crate::memory::Service::new(
            store,
            crate::memory::decide_default_policy,
        ));
        let service = Arc::new(Service::in_memory().unwrap());
        attach(&service, &memory, vec![user.clone()], "/w").unwrap();
        let candidate = crate::memory::advice::testing::candidate(&memory, user.clone(), true);
        let requests = service.snapshot("/w", Some("session")).unwrap().requests;
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .subject
                .details
                .contains(crate::memory::advice::testing::LINE)
        );
        assert_eq!(requests[0].state, State::Pending);
        assert_eq!(
            memory
                .get_candidate(&CandidateRef {
                    scope: user,
                    id: candidate.id
                })
                .unwrap()
                .state,
            CandidateState::Pending
        );
    }
    #[test]
    fn one_memory_request_is_shared_across_authorized_workspaces_without_copying_text() {
        let (_dir, store) = crate::memory::turso::testsupport::open_temp();
        let user = store.identity().unwrap().user_scope;
        let memory = Arc::new(crate::memory::Service::new(
            store,
            crate::memory::decide_default_policy,
        ));
        let service = Arc::new(Service::in_memory().unwrap());
        attach(&service, &memory, vec![user.clone()], "/one").unwrap();
        attach(&service, &memory, vec![user.clone()], "/two").unwrap();
        let candidate = memory
            .propose(&crate::memory::ProposeRequest {
                scope: user.clone(),
                kind: "preference".into(),
                key: "tabs".into(),
                text: "prefers tabs".into(),
                reason: "private reason: prefers tabs".into(),
                metadata: std::collections::BTreeMap::from([(
                    crate::memory::PRESENTATION_ORIGIN_METADATA.into(),
                    "chat:telegram:42".into(),
                )]),
                source: crate::memory::Provenance {
                    origin: Some(crate::memory::Origin::Model),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            candidate.proposed.metadata[crate::memory::PRESENTATION_ORIGIN_METADATA],
            "chat:telegram:42"
        );
        let one = service
            .snapshot("/one", Some("s1"))
            .unwrap()
            .requests
            .pop()
            .unwrap();
        let two = service
            .snapshot("/two", Some("s2"))
            .unwrap()
            .requests
            .pop()
            .unwrap();
        assert_eq!(one.id, two.id);
        assert_eq!(one.presentation_origin, "chat:telegram:42");
        assert_eq!(two.presentation_origin, "chat:telegram:42");
        assert!(one.subject.details.contains("prefers tabs"));
        assert!(one.subject.details.contains("private reason"));
        let events = service.events("/one", None, 0).unwrap();
        assert!(
            !events
                .iter()
                .any(|event| event.request.subject.details.contains("prefers tabs"))
        );
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("private reason")
        );
        service
            .decide(
                &one.id,
                &one.scope,
                1,
                true,
                "",
                &super::super::Actor::local("test-human"),
            )
            .unwrap();
        service.apply_available("/two").unwrap();
        let accepted = memory
            .get_candidate(&CandidateRef {
                scope: user.clone(),
                id: candidate.id,
            })
            .unwrap();
        assert_eq!(accepted.state, CandidateState::Accepted);
        let record = memory
            .get(&crate::memory::RecordRef {
                scope: user,
                id: accepted.result_record_id,
            })
            .unwrap();
        assert!(
            !record
                .metadata
                .contains_key(crate::memory::PRESENTATION_ORIGIN_METADATA),
            "routing metadata must not become remembered user data"
        );
        assert!(
            service
                .snapshot("/one", Some("s1"))
                .unwrap()
                .requests
                .is_empty()
        );
    }
}
