//! Memory retains review data and the atomic candidate/result receipt.
use crate::approval::{Domain, Error, Proposal, Result, Service};
use crate::memory::service::PendingSummary;
use crate::memory::{
    CandidateListRequest, CandidateRef, CandidateState, ReviewDecision, ReviewRequest, Scope,
};
use otto_core::approval::{Authority, Kind, Request, State, Subject};
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

/// Returns one storage-cursor page of candidates that `/memory review` can decide.
///
/// Storage-level pending candidates are not necessarily waiting for a human:
/// their approval can be under automatic review, already withdrawn, or otherwise
/// non-actionable. Continue paging after filtering so those rows cannot hide a
/// later human-pending candidate. The returned cursor retains the memory store's
/// generation check, so a write between pages returns `Conflict` instead of
/// silently skipping or repeating a candidate.
pub fn human_pending_candidate_page(
    memory: &crate::memory::Service,
    scopes: &[Scope],
    requests: &[Request],
    cursor: &str,
    limit: usize,
) -> crate::memory::Result<crate::memory::CandidatePage> {
    if limit == 0 || limit > crate::memory::MAX_PAGE_SIZE {
        return Err(crate::memory::Error::new(
            crate::memory::ErrorKind::InvalidRequest,
        ));
    }
    let mut candidates = Vec::new();
    let mut next_cursor = cursor.to_owned();
    loop {
        let previous_cursor = next_cursor.clone();
        let page = memory.list_candidates(&CandidateListRequest {
            scopes: scopes.to_vec(),
            states: vec![CandidateState::Pending],
            limit: limit - candidates.len(),
            cursor: next_cursor,
        })?;
        candidates.extend(
            page.candidates
                .into_iter()
                .filter(|candidate| is_human_pending(candidate, requests)),
        );
        next_cursor = page.next_cursor;
        if candidates.len() == limit || next_cursor.is_empty() {
            return Ok(crate::memory::CandidatePage {
                candidates,
                next_cursor,
            });
        }
        if next_cursor == previous_cursor {
            return Err(crate::memory::Error::new(
                crate::memory::ErrorKind::Conflict,
            ));
        }
    }
}

/// Returns the first `limit` actionable candidates across storage pages.
pub(crate) fn human_pending_candidates(
    memory: &crate::memory::Service,
    scopes: &[Scope],
    requests: &[Request],
    limit: usize,
) -> crate::memory::Result<Vec<crate::memory::Candidate>> {
    let mut candidates = Vec::new();
    let mut cursor = String::new();
    while candidates.len() < limit {
        let page = human_pending_candidate_page(
            memory,
            scopes,
            requests,
            &cursor,
            (limit - candidates.len()).min(crate::memory::MAX_PAGE_SIZE),
        )?;
        candidates.extend(page.candidates);
        if page.next_cursor.is_empty() {
            break;
        }
        cursor = page.next_cursor;
    }
    Ok(candidates)
}

/// Counts only candidates that are currently actionable in `/memory review`.
pub(crate) fn human_pending_summary(
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
        if request.application == otto_core::approval::ApplicationState::Applied
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
        if crate::approval::digest(&format!(
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
        if crate::approval::digest(&format!(
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
        scope: otto_core::approval::Scope {
            workspace: workspace.into(),
            // Route the decision through the originating session so AUTO can
            // review it under that session's policy. `publish` separately adds
            // workspace visibility to the same deduplicated request.
            session_id: candidate.proposed.source.session_id.clone(),
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
            version: crate::approval::digest(&format!(
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
    let origin = candidate
        .proposed
        .metadata
        .get(crate::memory::PRESENTATION_ORIGIN_METADATA)
        .map_or("", String::as_str);
    service.create_with_origin(proposal(scopes, workspace, candidate)?, origin)?;
    // Approval storage deduplicates memory requests by subject and adds the
    // supplied scope to their visibility set. This second publication does not
    // create another decision; it keeps the one-shot request reachable after
    // its originating session ends and upgrades legacy session-only requests.
    let mut visible = proposal(scopes, workspace, candidate)?;
    visible.scope.session_id.clear();
    service.create_with_origin(visible, origin)?;
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
        let (_dir, store) = crate::memory::files::testsupport::open_temp();
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
        let legacy = proposal(std::slice::from_ref(&user), "/w", &candidate).unwrap();
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
        let (_dir, store) = crate::memory::files::testsupport::open_temp();
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

        let candidates =
            human_pending_candidates(&memory, std::slice::from_ref(&user), &requests, 20).unwrap();
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
    fn human_pending_cursor_resumes_after_a_decision_without_skipping() {
        let (_dir, store) = crate::memory::files::testsupport::open_temp();
        let user = store.identity().unwrap().user_scope;
        let memory = Arc::new(crate::memory::Service::new(
            store,
            crate::memory::decide_default_policy,
        ));
        let approvals = Arc::new(Service::in_memory().unwrap());
        attach(&approvals, &memory, vec![user.clone()], "/w").unwrap();
        for key in ["first", "second"] {
            memory
                .propose(&crate::memory::ProposeRequest {
                    scope: user.clone(),
                    kind: "preference".into(),
                    key: key.into(),
                    text: key.into(),
                    reason: "awaiting human review".into(),
                    source: crate::memory::Provenance {
                        origin: Some(crate::memory::Origin::Model),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .unwrap();
        }
        let requests = approvals.snapshot("/w", Some("session")).unwrap().requests;
        let first =
            human_pending_candidate_page(&memory, std::slice::from_ref(&user), &requests, "", 1)
                .unwrap();
        assert_eq!(first.candidates.len(), 1);
        assert!(!first.next_cursor.is_empty());
        memory
            .review(&ReviewRequest {
                reference: CandidateRef {
                    scope: user.clone(),
                    id: first.candidates[0].id.clone(),
                },
                decision: ReviewDecision::Reject,
                edited: None,
                target_revision: None,
            })
            .unwrap();

        // The file store keeps no generation: the cursor is a keyset position,
        // so a decision on an earlier candidate neither fails nor skips the next.
        let resumed = human_pending_candidate_page(
            &memory,
            std::slice::from_ref(&user),
            &requests,
            &first.next_cursor,
            1,
        )
        .unwrap();
        assert_eq!(resumed.candidates.len(), 1);
        assert_ne!(resumed.candidates[0].id, first.candidates[0].id);
        let restarted =
            human_pending_candidate_page(&memory, std::slice::from_ref(&user), &requests, "", 1)
                .unwrap();
        assert_eq!(restarted.candidates.len(), 1);
        assert_eq!(restarted.candidates[0].id, resumed.candidates[0].id);
        assert!(restarted.next_cursor.is_empty());
    }

    #[test]
    fn unified_review_preserves_advice_without_turning_it_into_a_decision() {
        let (_dir, store) = crate::memory::files::testsupport::open_temp();
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
        let (_dir, store) = crate::memory::files::testsupport::open_temp();
        let root = store.root().to_path_buf();
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
                &crate::approval::Actor::local("test-human"),
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
        assert!(
            !root
                .join("candidates")
                .join(format!("{}.json", accepted.id))
                .exists()
        );
        assert!(
            root.join("candidates/archive")
                .join(format!("{}.json", accepted.id))
                .is_file()
        );
        let applied = service.get(&one.id, "/one", Some("s1")).unwrap();
        assert!(
            applied
                .subject
                .details
                .contains("Original authorized subject digest")
        );
        let adapter = Adapter {
            memory: memory.clone(),
            scopes: vec![user.clone()],
        };
        let persisted = service.stored_for_test(&one.id).unwrap();
        adapter.apply(&persisted).unwrap();
        assert!(adapter.details(&persisted).unwrap().contains(&accepted.id));
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
