use super::{
    reviewer::{ReviewContext, ReviewProvider},
    *,
};
use otto_core::approval::{
    ApplicationState, ApprovalMode, ApprovalPolicy, ApprovalReviewer, ApprovalRule,
    ApprovalSettings, Authority, Kind, Request, ReviewAssessment, ReviewOutcome, RiskLevel,
    RuleDecision, Scope, State, Subject, UserAuthorization,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

struct Fixed {
    calls: AtomicUsize,
    outcome: ReviewOutcome,
}
struct Unavailable;
#[async_trait::async_trait]
impl ReviewProvider for Unavailable {
    async fn assess(
        &self,
        _: &Request,
        _: &ReviewContext,
        _: &CancellationToken,
    ) -> std::result::Result<ReviewAssessment, String> {
        Err("fixture reviewer unavailable".into())
    }
}
#[async_trait::async_trait]
impl ReviewProvider for Fixed {
    async fn assess(
        &self,
        _: &Request,
        _: &ReviewContext,
        _: &CancellationToken,
    ) -> std::result::Result<ReviewAssessment, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ReviewAssessment {
            risk: RiskLevel::Medium,
            user_authorization: UserAuthorization::High,
            outcome: self.outcome,
            reason: "The exact fixture content and target match the user's request.".into(),
        })
    }
}
fn scope() -> Scope {
    Scope {
        workspace: "/native-review".into(),
        session_id: "root".into(),
        task_id: String::new(),
        run_id: "logical-task".into(),
    }
}
fn proposal(id: &str) -> Proposal {
    Proposal {
        kind: Kind::Authorization,
        authority: Authority::Boundary,
        scope: scope(),
        subject: Subject {
            source: "sandbox".into(),
            id: id.into(),
            version: "1".into(),
            tool_call_id: id.into(),
            details: r#"{"command":"printf fixture"}"#.into(),
        },
        title: "Run fixture".into(),
        reason: "Exact requested command".into(),
        persistent: false,
        blocking: true,
        expires_at: None,
    }
}
fn configured(outcome: ReviewOutcome) -> (Service, Arc<Fixed>) {
    let service = Service::in_memory().unwrap();
    let provider = Arc::new(Fixed {
        calls: AtomicUsize::new(0),
        outcome,
    });
    service
        .register_reviewer(&scope().workspace, &scope().session_id, provider.clone())
        .unwrap();
    let mut context = ReviewContext {
        trusted_user_messages: vec!["Run the exact fixture command.".into()],
        instructions: vec!["Keep targets bounded.".into()],
        policy_digest: "instructions-v1".into(),
        ..Default::default()
    };
    context.refresh_digest();
    service
        .set_review_context(&scope().workspace, &scope().session_id, context)
        .unwrap();
    (service, provider)
}

mod authority;
mod content;
mod review;
