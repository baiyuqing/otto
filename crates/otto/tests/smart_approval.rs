//! Offline coordinator contract tests; ACP tests separately verify startup wiring.

use otto::approval::{
    Proposal, Service,
    reviewer::{ReviewContext, ReviewProvider},
};
use otto_core::approval::{
    ApprovalPolicy, ApprovalReviewer, Authority, Kind, Request, ReviewAssessment, ReviewOutcome,
    ReviewStatus, RiskLevel, Scope, State, Subject, UserAuthorization,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

struct HeldReviewer {
    entered: Semaphore,
    release: Semaphore,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ReviewProvider for HeldReviewer {
    async fn assess(
        &self,
        _: &Request,
        _: &ReviewContext,
        cancel: &CancellationToken,
    ) -> std::result::Result<ReviewAssessment, String> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        tokio::select! {
            permit = self.release.acquire() => permit.unwrap().forget(),
            () = cancel.cancelled() => return Err("cancelled".into()),
        }
        Ok(ReviewAssessment {
            risk: RiskLevel::Medium,
            user_authorization: UserAuthorization::High,
            outcome: if index == 0 {
                ReviewOutcome::Allow
            } else {
                ReviewOutcome::Ask
            },
            reason: "Reviewed the exact operation.".into(),
        })
    }
}

fn scope() -> Scope {
    Scope {
        workspace: "/review-fixture".into(),
        session_id: "root".into(),
        task_id: String::new(),
        run_id: String::new(),
    }
}

fn proposal() -> Proposal {
    Proposal {
        kind: Kind::Authorization,
        authority: Authority::Boundary,
        scope: scope(),
        subject: Subject {
            source: "sandbox".into(),
            id: "exact-call".into(),
            version: "1".into(),
            tool_call_id: "call-1".into(),
            details: r#"{"command":"printf reviewed"}"#.into(),
        },
        title: "Run fixture".into(),
        reason: "Requested fixture operation".into(),
        persistent: false,
        blocking: true,
        expires_at: None,
    }
}

fn context(text: &str) -> ReviewContext {
    let mut context = ReviewContext {
        trusted_user_messages: vec![text.into()],
        instructions: vec!["Use the exact fixture target.".into()],
        policy_digest: "fixed-instructions-v1".into(),
        ..Default::default()
    };
    context.refresh_digest();
    context
}

fn configured() -> (Arc<Service>, Arc<HeldReviewer>) {
    let service = Arc::new(Service::in_memory().unwrap());
    let reviewer = Arc::new(HeldReviewer {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
        calls: AtomicUsize::new(0),
    });
    service
        .register_reviewer(&scope().workspace, &scope().session_id, reviewer.clone())
        .unwrap();
    service
        .set_review_context(
            &scope().workspace,
            &scope().session_id,
            context("Run the fixture."),
        )
        .unwrap();
    service.start();
    (service, reviewer)
}

async fn entered(reviewer: &HeldReviewer) {
    tokio::time::timeout(Duration::from_secs(3), reviewer.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
}

async fn human_required(service: &Service, id: &str) -> Request {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let request = service
                .get(id, &scope().workspace, Some(&scope().session_id))
                .unwrap();
            if request
                .review
                .as_ref()
                .is_some_and(|m| m.status == ReviewStatus::HumanRequired)
            {
                return request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request must become human-required")
}

#[tokio::test]
async fn tightening_settings_invalidates_an_inflight_allow_and_requires_a_user() {
    let (service, reviewer) = configured();
    let request = service.create(proposal()).unwrap();
    entered(&reviewer).await;
    let activity = service
        .activity_snapshot(&scope().workspace, Some(&scope().session_id))
        .unwrap();
    assert!(
        activity.requests.is_empty(),
        "reviewing must not become a human card"
    );
    assert_eq!(activity.counts.human_pending, 0);
    assert_eq!(activity.counts.reviewing, 1);
    let settings = service.settings(&scope()).unwrap();
    service
        .set_settings(
            &scope(),
            settings.revision,
            ApprovalPolicy::OnRequest,
            ApprovalReviewer::User,
        )
        .unwrap();
    reviewer.release.add_permits(1);
    let request = human_required(&service, &request.id).await;
    assert_eq!(request.state, State::Pending);
    assert!(request.decision.is_none());
    assert_eq!(reviewer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn changing_user_evidence_discards_stale_allow_and_reviews_the_new_context() {
    let (service, reviewer) = configured();
    let request = service.create(proposal()).unwrap();
    entered(&reviewer).await;
    service
        .set_review_context(
            &scope().workspace,
            &scope().session_id,
            context("Stop and ask before running the fixture."),
        )
        .unwrap();
    reviewer.release.add_permits(1);
    entered(&reviewer).await;
    let stale = service
        .get(&request.id, &scope().workspace, Some(&scope().session_id))
        .unwrap();
    assert_eq!(
        stale.state,
        State::Pending,
        "old evidence must not authorize the operation"
    );
    reviewer.release.add_permits(1);
    let request = human_required(&service, &request.id).await;
    assert_eq!(request.state, State::Pending);
    assert!(request.decision.is_none());
    assert_eq!(reviewer.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn explicit_checkpoint_and_missing_evidence_never_reach_the_reviewer() {
    let (service, reviewer) = configured();
    let mut checkpoint = proposal();
    checkpoint.kind = Kind::Confirmation;
    checkpoint.authority = Authority::Consent;
    checkpoint.subject.id = "checkpoint".into();
    let checkpoint = service.create(checkpoint).unwrap();
    assert_eq!(
        checkpoint.review.as_ref().unwrap().status,
        ReviewStatus::HumanRequired
    );
    service
        .set_review_context(
            &scope().workspace,
            &scope().session_id,
            ReviewContext {
                evidence_digest: "missing".into(),
                policy_digest: "fixed".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let request = service.create(proposal()).unwrap();
    assert_eq!(
        request.review.as_ref().unwrap().status,
        ReviewStatus::HumanRequired
    );
    assert_eq!(reviewer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        service
            .snapshot(&scope().workspace, Some(&scope().session_id))
            .unwrap()
            .requests
            .len(),
        2
    );
}

#[tokio::test]
async fn oversized_operation_asks_the_user_without_sending_a_truncated_review() {
    let (service, reviewer) = configured();
    let mut proposal = proposal();
    proposal.subject.details = "x".repeat(65_536);
    let request = service.create(proposal).unwrap();
    let request = human_required(&service, &request.id).await;
    assert_eq!(request.state, State::Pending);
    assert!(request.decision.is_none());
    assert_eq!(reviewer.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn withdrawing_an_inflight_review_cannot_be_overwritten_by_its_allow() {
    let (service, reviewer) = configured();
    let request = service.create(proposal()).unwrap();
    entered(&reviewer).await;
    let reviewing = service
        .get(&request.id, &scope().workspace, Some(&scope().session_id))
        .unwrap();
    service
        .withdraw(&request.id, &scope(), reviewing.revision)
        .unwrap();
    reviewer.release.add_permits(1);
    // A second request's completed assessment proves the first provider returned
    // and coordination continued, without a timing-based sleep assertion.
    let mut next = proposal();
    next.subject.id = "next-call".into();
    let next = service.create(next).unwrap();
    entered(&reviewer).await;
    reviewer.release.add_permits(1);
    human_required(&service, &next.id).await;
    let withdrawn = service
        .get(&request.id, &scope().workspace, Some(&scope().session_id))
        .unwrap();
    assert_eq!(withdrawn.state, State::Withdrawn);
    assert!(withdrawn.decision.is_none());
}

#[tokio::test]
async fn auto_memory_review_applies_exact_content_once_and_retains_the_domain_receipt() {
    use otto::memory::{self, CandidateRef, CandidateState, ProposeRequest, RecordKey};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("memory");
    let memory_scope = memory::Scope::new("workspace", "review-fixture");
    let memory = Arc::new(memory::Service::new(
        memory::files::FileStore::open(&path, Default::default()).unwrap(),
        memory::decide_default_policy,
    ));
    let (service, reviewer) = configured();
    service
        .set_review_context(
            &scope().workspace,
            &scope().session_id,
            context("Remember in this workspace: use tabs in fixture files."),
        )
        .unwrap();
    otto::memory::approval::attach(
        &service,
        &memory,
        vec![memory_scope.clone()],
        &scope().workspace,
    )
    .unwrap();
    let candidates = memory
        .propose(&ProposeRequest {
            scope: memory_scope.clone(),
            kind: "preference".into(),
            key: "fixture".into(),
            text: "Use tabs in fixture files.".into(),
            confidence: 1.0,
            source: memory::Provenance {
                origin: Some(memory::Origin::Model),
                session_id: scope().session_id,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    let reference = CandidateRef {
        scope: memory_scope.clone(),
        id: candidates[0].id.clone(),
    };
    entered(&reviewer).await;
    let activity = service
        .activity_snapshot(&scope().workspace, Some(&scope().session_id))
        .unwrap();
    assert!(
        activity.requests.is_empty(),
        "a memory review must not become a human card"
    );
    assert_eq!(activity.reviewing.len(), 1);
    assert!(
        activity.reviewing[0]
            .subject
            .details
            .contains("Use tabs in fixture files.")
    );
    let approval_id = activity.reviewing[0].id.clone();
    reviewer.release.add_permits(1);
    let accepted = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let candidate = memory.get_candidate(&reference).unwrap();
            if candidate.state == CandidateState::Accepted {
                break candidate;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(accepted.result_revision, 1);
    let key = RecordKey {
        scope: memory_scope.clone(),
        kind: "preference".into(),
        key: "fixture".into(),
    };
    let written = memory.get_by_key(&key).unwrap();
    assert_eq!(written.text, "Use tabs in fixture files.");
    assert_eq!(written.revision, 1);
    for _ in 0..3 {
        service.apply_available(&scope().workspace).unwrap();
    }
    assert_eq!(
        memory.get_by_key(&key).unwrap(),
        written,
        "receipts prevent duplicate application"
    );
    let approval = service
        .get(&approval_id, &scope().workspace, Some(&scope().session_id))
        .unwrap();
    assert_eq!(approval.decision.as_ref().unwrap().transport, "auto-review");
    assert_eq!(
        approval.application,
        otto_core::approval::ApplicationState::Applied
    );
    assert_eq!(reviewer.calls.load(Ordering::SeqCst), 1);
    drop(service);
    drop(memory);
    let reopened = memory::Service::new(
        memory::files::FileStore::open(&path, Default::default()).unwrap(),
        memory::decide_default_policy,
    );
    assert_eq!(reopened.get_by_key(&key).unwrap(), written);
    assert_eq!(
        reopened.get_candidate(&reference).unwrap().result_revision,
        1
    );
}
