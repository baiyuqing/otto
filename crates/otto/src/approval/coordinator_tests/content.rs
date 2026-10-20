//! Tests that approval content is not secret-scanned and policy still applies.

use super::*;

// Deliberately synthetic: exercise shape checks, markers and exact matching without credentials.
const SYNTHETIC_APPROVAL_TEXT: &str = "Authorization: Bearer synthetic-approval-header\napi_key=synthetic-approval-assignment\n[REDACTED]\nsynthetic-exact-approval-value";

#[test]
#[allow(deprecated)]
fn approval_content_and_feedback_are_not_secret_scanned_and_policy_still_applies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("approvals.db");
    let service = Service::open(&path).unwrap();
    service
        .protect_secrets(&["synthetic-exact-approval-value".into()])
        .unwrap();
    for (mode, authorization_state) in [
        (ApprovalMode::Manual, State::Pending),
        (ApprovalMode::Auto, State::Pending),
        (ApprovalMode::Yolo, State::Approved),
    ] {
        service.set_mode(&scope(), mode).unwrap();
        let mut input = proposal(&format!("unscanned-{mode:?}"));
        input.title = SYNTHETIC_APPROVAL_TEXT.into();
        input.reason = SYNTHETIC_APPROVAL_TEXT.into();
        input.subject.details = SYNTHETIC_APPROVAL_TEXT.into();
        let request = service.create(input).unwrap();
        assert_eq!(request.state, authorization_state);
        if request.state == State::Pending {
            assert!(
                service
                    .decide(
                        &request.id,
                        &request.scope,
                        request.revision,
                        true,
                        &"x".repeat(8193),
                        &Actor::local("test")
                    )
                    .is_err()
            );
            let decided = service
                .decide(
                    &request.id,
                    &request.scope,
                    request.revision,
                    true,
                    SYNTHETIC_APPROVAL_TEXT,
                    &Actor::local("test"),
                )
                .unwrap();
            assert_eq!(decided.state, State::Approved);
            assert_eq!(
                decided.decision.as_ref().unwrap().feedback,
                SYNTHETIC_APPROVAL_TEXT
            );
        }
        let stored = service
            .get(&request.id, &scope().workspace, Some(&scope().session_id))
            .unwrap();
        assert_eq!(stored.title, SYNTHETIC_APPROVAL_TEXT);
        assert_eq!(stored.reason, SYNTHETIC_APPROVAL_TEXT);
        assert_eq!(stored.subject.details, SYNTHETIC_APPROVAL_TEXT);
    }
    service.set_mode(&scope(), ApprovalMode::Auto).unwrap();
    let mut checkpoint = proposal("unscanned-checkpoint");
    checkpoint.kind = Kind::Confirmation;
    checkpoint.authority = Authority::Consent;
    checkpoint.title = SYNTHETIC_APPROVAL_TEXT.into();
    assert!(service.create(checkpoint).unwrap().is_human_pending());
    let settings = service.settings(&scope()).unwrap();
    service
        .set_settings(
            &scope(),
            settings.revision,
            ApprovalPolicy::Never,
            ApprovalReviewer::User,
        )
        .unwrap();
    let mut denied = proposal("unscanned-never");
    denied.title = SYNTHETIC_APPROVAL_TEXT.into();
    denied.reason = SYNTHETIC_APPROVAL_TEXT.into();
    denied.subject.details = SYNTHETIC_APPROVAL_TEXT.into();
    let denied = service.create(denied).unwrap();
    assert_eq!(denied.state, State::Rejected);
    drop(service);
    let reopened = Service::open(&path).unwrap();
    let stored = reopened
        .get(&denied.id, &scope().workspace, Some(&scope().session_id))
        .unwrap();
    assert_eq!(stored.title, SYNTHETIC_APPROVAL_TEXT);
    assert_eq!(stored.subject.details, SYNTHETIC_APPROVAL_TEXT);
}

struct ContentReviewer;
#[async_trait::async_trait]
impl ReviewProvider for ContentReviewer {
    async fn assess(
        &self,
        request: &Request,
        context: &ReviewContext,
        _: &CancellationToken,
    ) -> std::result::Result<ReviewAssessment, String> {
        assert_eq!(request.subject.details, SYNTHETIC_APPROVAL_TEXT);
        assert_eq!(context.trusted_user_messages, [SYNTHETIC_APPROVAL_TEXT]);
        assert_eq!(context.instructions, [SYNTHETIC_APPROVAL_TEXT]);
        Ok(ReviewAssessment {
            risk: RiskLevel::Medium,
            user_authorization: UserAuthorization::High,
            outcome: ReviewOutcome::Allow,
            reason: SYNTHETIC_APPROVAL_TEXT.into(),
        })
    }
}

#[tokio::test]
#[allow(deprecated)]
async fn review_context_hydrated_details_and_reviewer_reason_are_not_secret_scanned() {
    let service = Service::in_memory().unwrap();
    service
        .protect_secrets(&["synthetic-exact-approval-value".into()])
        .unwrap();
    service
        .register_reviewer(
            &scope().workspace,
            &scope().session_id,
            Arc::new(ContentReviewer),
        )
        .unwrap();
    let mut context = ReviewContext {
        trusted_user_messages: vec![SYNTHETIC_APPROVAL_TEXT.into()],
        instructions: vec![SYNTHETIC_APPROVAL_TEXT.into()],
        policy_digest: "synthetic-policy".into(),
        ..Default::default()
    };
    context.refresh_digest();
    service
        .set_review_context(&scope().workspace, &scope().session_id, context)
        .unwrap();
    let mut input = proposal("unscanned-review");
    input.subject.details = SYNTHETIC_APPROVAL_TEXT.into();
    let request = service.create(input).unwrap();
    let reviewed = service
        .wait_actionable(&request.id, &request.scope, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(reviewed.state, State::Approved);
    assert_eq!(
        reviewed.review.unwrap().assessment.unwrap().reason,
        SYNTHETIC_APPROVAL_TEXT
    );
}
