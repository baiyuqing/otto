//! Memory retains review data and the atomic candidate/result receipt.
use super::{Authority, Domain, Error, Kind, Proposal, Request, Result, Service, State, Subject};
use crate::memory::{
    CandidateListRequest, CandidateRef, CandidateState, ReviewDecision, ReviewRequest, Scope,
};
use std::sync::Arc;

pub struct Adapter {
    pub memory: Arc<crate::memory::Service>,
    pub scopes: Vec<Scope>,
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

pub fn publish(
    service: &Service,
    scopes: &[Scope],
    workspace: &str,
    candidate: &crate::memory::Candidate,
) -> Result<()> {
    if !scopes.contains(&candidate.proposed.scope) {
        return Ok(());
    }
    let reference = CandidateRef {
        scope: candidate.proposed.scope.clone(),
        id: candidate.id.clone(),
    };
    service.create(Proposal {
        kind: Kind::Authorization,
        authority: Authority::Durable,
        scope: super::Scope {
            workspace: workspace.into(),
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
    })?;
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
                source: crate::memory::Provenance {
                    origin: Some(crate::memory::Origin::Model),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap()
            .pop()
            .unwrap();
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
        assert_eq!(
            memory
                .get_candidate(&CandidateRef {
                    scope: user,
                    id: candidate.id
                })
                .unwrap()
                .state,
            CandidateState::Accepted
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
