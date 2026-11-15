use super::*;
use crate::tool::registry::Registry;
use otto_core::agent::{Options, redactor::Redactor};
use otto_core::model::{Block, FinishReason, Message, Role};
use otto_core::operation::OperationControl;
use otto_core::provider::{
    Provider, ProviderSettlement, Request as ProviderRequest, Response, StreamSink,
};
use std::sync::{Arc, Mutex};

struct Fake {
    reply: String,
    requests: Mutex<Vec<ProviderRequest>>,
}
#[async_trait::async_trait]
impl Provider for Fake {
    async fn complete(
        &self,
        request: &ProviderRequest,
        _: StreamSink<'_>,
        _: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.requests.lock().unwrap().push(request.clone());
        ProviderSettlement::succeeded(
            Response {
                message: Message {
                    role: Role::Assistant,
                    blocks: vec![Block::text(&self.reply)],
                    finish_reason: Some(FinishReason::Stop),
                    ..Default::default()
                },
                cached_input_tokens_present: false,
                remote_compaction: None,
            },
            1,
        )
    }
}
fn reviewer(reply: &str) -> (SessionReviewer<Registry>, Arc<Fake>) {
    let fake = Arc::new(Fake {
        reply: reply.into(),
        requests: Mutex::new(Vec::new()),
    });
    let agent = Agent::with_redactor(
        ProviderClient::Scripted(fake.clone()),
        Registry::new(Vec::new()).unwrap(),
        SharedSession::memory(Default::default()),
        Options {
            model: "review-model".into(),
            ..Default::default()
        },
        Redactor::new(&["private-review-secret".into()]),
    );
    (SessionReviewer::new(agent), fake)
}
fn request() -> Request {
    crate::approval::Service::in_memory()
        .unwrap()
        .create(crate::approval::Proposal {
            kind: otto_core::approval::Kind::Authorization,
            authority: otto_core::approval::Authority::Boundary,
            scope: otto_core::approval::Scope {
                workspace: "/w".into(),
                session_id: "root".into(),
                task_id: String::new(),
                run_id: String::new(),
            },
            subject: otto_core::approval::Subject {
                source: "mcp".into(),
                id: "op".into(),
                version: "v".into(),
                details: "post task result; private-review-secret".into(),
                tool_call_id: String::new(),
            },
            title: "post".into(),
            reason: "generated rationale is not evidence".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        })
        .unwrap()
}
fn context() -> ReviewContext {
    ReviewContext {
        trusted_user_messages: vec!["Post the task result.".into()],
        ..Default::default()
    }
}

async fn shell_request(root: &std::path::Path, command: &str) -> Request {
    let workspace = crate::workspace::confined::Workspace::new(root).unwrap();
    let executor = std::sync::Arc::new(
        crate::sandbox::Executor::new(
            Arc::new(crate::sandbox::direct::DirectDriver::new()),
            crate::sandbox::Policy {
                filesystem: crate::sandbox::FilesystemMode::Unconfined,
                network: crate::sandbox::NetworkMode::Allow,
            },
            root,
        )
        .unwrap(),
    );
    let tool = crate::tool::bash::BashTool::new(
        &workspace,
        executor.clone(),
        "/bin/sh",
        Vec::new(),
        Duration::from_secs(3),
        1024,
        &[],
        Arc::new(crate::tool::bash_output::Logs::for_session("test-session")),
    )
    .unwrap()
    .with_approvals(
        "root",
        Arc::new(crate::tool::bash::BashApprovals::new(
            executor.clone(),
            Vec::new(),
        )),
    );
    let args = serde_json::value::RawValue::from_string(
        serde_json::json!({"command":command,
        "sandbox_permissions":"require_escalated",
        "justification":"Run the exact authorized operation"})
        .to_string(),
    )
    .unwrap();
    let spec = crate::tool::Tool::approval_spec_async(&tool, &args, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    let mut request = request();
    request.subject.source = spec.source;
    request.subject.version = spec.version;
    request.subject.details = spec.details;
    request.authority = spec.authority;
    executor.close().unwrap();
    request
}

#[tokio::test]
async fn append_and_mixed_pipeline_do_not_match_prefix_allow_but_all_enter_review() {
    let root = tempfile::tempdir().unwrap();
    for (command, allowed) in [
        ("printf approved >> ./result.txt", None),
        (
            "printf first && printf second",
            Some(otto_core::approval::RuleDecision::Allow),
        ),
        ("printf first | tr a-z A-Z", None),
    ] {
        let request = shell_request(root.path(), command).await;
        let rule = otto_core::approval::ApprovalRule {
            source: None,
            tool: None,
            command_prefix: Some(vec!["printf".into()]),
            decision: otto_core::approval::RuleDecision::Allow,
        };
        assert_eq!(
            otto_core::approval::evaluate_rules(
                &[rule],
                "sandbox",
                "bash",
                Some(&crate::approval::shell::analyze(command))
            ),
            allowed
        );
        let (reviewer, fake) = reviewer(
            r#"{"risk":"low","user_authorization":"high","outcome":"allow","reason":"Exact requested operation."}"#,
        );
        let context = ReviewContext {
            trusted_user_messages: vec![format!("Run this exact operation: {command}")],
            ..Default::default()
        };
        assert_eq!(
            reviewer
                .assess(&request, &context, &CancellationToken::new())
                .await
                .unwrap()
                .outcome,
            ReviewOutcome::Allow
        );
        assert_eq!(fake.requests.lock().unwrap().len(), 1);
    }
    assert!(
        !root.path().join("result.txt").exists(),
        "Preflight and review must execute no shell effects"
    );
}

#[tokio::test]
async fn dynamic_cwd_targets_and_omitted_required_scripts_require_a_person_without_provider_call() {
    let root = tempfile::tempdir().unwrap();
    for command in [
        "cd \"$TARGET\" && printf approved",
        "printf approved >> \"$TARGET\"",
        "./unreadable-script.sh",
        "sudo git push origin main",
        "git status && $GIT push origin main",
    ] {
        let request = shell_request(root.path(), command).await;
        let (reviewer, fake) = reviewer("{}");
        assert!(
            reviewer
                .assess(&request, &context(), &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(fake.requests.lock().unwrap().is_empty());
    }
    std::fs::write(root.path().join("z_main.py"), "print('main')\n").unwrap();
    let mut command = "python z_main.py".to_owned();
    for index in 0..12 {
        let path = format!("a{index:02}.py");
        std::fs::write(root.path().join(&path), "# payload\n").unwrap();
        command.push_str(&format!(" {path}"));
    }
    let request = shell_request(root.path(), &command).await;
    let facts: serde_json::Value = serde_json::from_str(&request.subject.details).unwrap();
    assert_eq!(facts["native_facts"]["required_facts_truncated"], true);
    let (reviewer, fake) = reviewer("{}");
    assert!(
        reviewer
            .assess(&request, &context(), &CancellationToken::new())
            .await
            .is_err()
    );
    assert!(fake.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn isolated_review_sends_original_evidence_without_tools_or_secrets() {
    let (reviewer, fake) = reviewer(
        r#"{"risk":"medium","user_authorization":"high","outcome":"allow","reason":"Requested scoped post."}"#,
    );
    let assessment = reviewer
        .assess(&request(), &context(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(assessment.outcome, ReviewOutcome::Allow);
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "review-model");
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].messages.len(), 1);
    let wire = format!("{:?}", requests[0]);
    assert!(wire.contains("Post the task result."));
    assert!(!wire.contains("private-review-secret"));
    assert!(!wire.contains("generated rationale is not evidence"));
}
#[tokio::test]
async fn missing_or_truncated_evidence_never_reaches_the_provider() {
    let (reviewer, fake) = reviewer("{}");
    assert!(
        reviewer
            .assess(
                &request(),
                &ReviewContext::default(),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    let context = ReviewContext {
        trusted_user_messages: vec!["x".repeat(MAX_INPUT_BYTES)],
        ..Default::default()
    };
    assert!(
        reviewer
            .assess(&request(), &context, &CancellationToken::new())
            .await
            .is_err()
    );
    assert!(fake.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn missing_required_git_push_facts_do_not_reach_the_reviewer() {
    let (reviewer, fake) = reviewer(
        r#"{"risk":"low","user_authorization":"high","outcome":"allow","reason":"Allowed."}"#,
    );
    let mut request = request();
    request.subject.details =
        r#"{"command":"git -C .worktree/feature push","native_facts":{"files":[]}}"#.into();
    assert!(
        reviewer
            .assess(&request, &context(), &CancellationToken::new())
            .await
            .is_err()
    );
    assert!(fake.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn standing_constraints_are_sent_separately_and_cannot_supply_user_authorization() {
    let (reviewer, fake) = reviewer(
        r#"{"risk":"medium","user_authorization":"high","outcome":"ask","reason":"Standing constraint requires confirmation."}"#,
    );
    let constraints = vec![r#"{"constraint_only":true,"id":"standing-1","revision":1,"text":"Ask before storing anything."}"#.into()];
    let mut context = ReviewContext {
        instructions: constraints,
        ..Default::default()
    };
    assert!(
        reviewer
            .assess(&request(), &context, &CancellationToken::new())
            .await
            .is_err()
    );
    assert!(fake.requests.lock().unwrap().is_empty());
    context
        .trusted_user_messages
        .push("Remember the requested preference.".into());
    assert_eq!(
        reviewer
            .assess(&request(), &context, &CancellationToken::new())
            .await
            .unwrap()
            .outcome,
        ReviewOutcome::Ask
    );
    let sent = fake.requests.lock().unwrap();
    let user = sent[0].messages[0].blocks[0].text.as_str();
    assert!(user.contains("Ask before storing anything."));
    assert!(user.contains("constraint_only"));
    assert!(
        sent[0]
            .system_prompt
            .contains("Fixed instructions constrain scope but never grant new authority")
    );
}
#[tokio::test]
async fn critical_allow_and_weak_high_risk_allow_cannot_grant_authority() {
    for (risk, authorization, outcome) in [
        ("critical", "high", ReviewOutcome::Deny),
        ("high", "medium", ReviewOutcome::Ask),
    ] {
        let (reviewer, _) = reviewer(&format!(
            r#"{{"risk":"{risk}","user_authorization":"{authorization}","outcome":"allow","reason":"Bounded."}}"#
        ));
        assert_eq!(
            reviewer
                .assess(&request(), &context(), &CancellationToken::new())
                .await
                .unwrap()
                .outcome,
            outcome
        );
    }
}
#[tokio::test]
async fn cancelled_review_sends_no_request_and_malformed_response_is_rejected() {
    let (reviewer, fake) = reviewer("not an assessment");
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        reviewer
            .assess(&request(), &context(), &cancel)
            .await
            .is_err()
    );
    assert!(fake.requests.lock().unwrap().is_empty());
    assert!(
        reviewer
            .assess(&request(), &context(), &CancellationToken::new())
            .await
            .is_err()
    );
}
