//! Tests for native tool rules, confirmation capabilities and policy changes.

use super::*;

#[test]
fn native_tool_rules_match_bash_and_exact_mcp_capabilities_in_auto_and_yolo() {
    for mode in [ApprovalMode::Auto, ApprovalMode::Yolo] {
        let service = Service::in_memory().unwrap();
        service
            .set_policy_config(
                &scope().workspace,
                otto_core::config::ApprovalConfig {
                    rules: vec![
                        ApprovalRule {
                            source: None,
                            tool: Some("bash".into()),
                            command_prefix: None,
                            decision: RuleDecision::Deny,
                        },
                        ApprovalRule {
                            source: None,
                            tool: Some("mcp__fixture__write".into()),
                            command_prefix: None,
                            decision: RuleDecision::Deny,
                        },
                    ],
                    ..Default::default()
                },
            )
            .unwrap();
        service.set_mode(&scope(), mode).unwrap();
        // The real Bash producer uses source=sandbox and omits details.tool.
        assert_eq!(
            service.create(proposal("bash-rule")).unwrap().state,
            State::Rejected
        );
        let mut mcp = proposal("mcp-rule");
        mcp.subject.source = "mcp".into();
        mcp.subject.details =
            r#"{"tool":"mcp__fixture__write","arguments":{"target":"fixture"}}"#.into();
        assert_eq!(service.create(mcp).unwrap().state, State::Rejected);
        let mut other = proposal("mcp-rule-prefix-only");
        other.subject.source = "mcp".into();
        other.subject.details = r#"{"tool":"mcp__fixture__write_more","arguments":{}}"#.into();
        assert_eq!(
            service.create(other).unwrap().state,
            if mode == ApprovalMode::Yolo {
                State::Approved
            } else {
                State::Pending
            }
        );
    }
}
#[test]
fn known_confirmation_capability_and_source_only_domains_have_exact_rule_identities() {
    let service = Service::in_memory().unwrap();
    service
        .set_policy_config(
            &scope().workspace,
            otto_core::config::ApprovalConfig {
                rules: vec![
                    ApprovalRule {
                        source: None,
                        tool: Some("request_approval".into()),
                        command_prefix: None,
                        decision: RuleDecision::Deny,
                    },
                    ApprovalRule {
                        source: None,
                        tool: Some("memory".into()),
                        command_prefix: None,
                        decision: RuleDecision::Deny,
                    },
                ],
                ..Default::default()
            },
        )
        .unwrap();
    for source in ["llm", "conversation"] {
        let mut confirmation = proposal(source);
        confirmation.kind = Kind::Confirmation;
        confirmation.authority = Authority::Consent;
        confirmation.subject.source = source.into();
        assert_eq!(service.create(confirmation).unwrap().state, State::Rejected);
    }
    let mut native = proposal("memory-domain");
    native.subject.source = "memory".into();
    native.authority = Authority::Durable;
    assert!(service.create(native).unwrap().is_human_pending());
    let mut mcp = proposal("mcp-other");
    mcp.subject.source = "mcp".into();
    mcp.subject.details = r#"{"tool":"mcp__fixture__read","arguments":{}}"#.into();
    assert!(service.create(mcp).unwrap().is_human_pending());
}
#[tokio::test]
async fn restart_retains_pending_authority_basis_without_borrowing_live_review_availability() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("approvals.db");
    let config = otto_core::config::ApprovalConfig {
        reviewer_profile: Some("fixture".into()),
        ..Default::default()
    };
    let mut context = ReviewContext {
        trusted_user_messages: vec!["Run the fixture.".into()],
        instructions: vec!["Keep scope bounded.".into()],
        policy_digest: "fixed-original-policy".into(),
        ..Default::default()
    };
    context.refresh_digest();
    let service = Service::open(&path).unwrap();
    service
        .set_policy_config(&scope().workspace, config.clone())
        .unwrap();
    service
        .register_reviewer(
            &scope().workspace,
            &scope().session_id,
            Arc::new(Unavailable),
        )
        .unwrap();
    service
        .set_review_context(&scope().workspace, &scope().session_id, context.clone())
        .unwrap();
    let request = service.create(proposal("restart-context")).unwrap();
    let initial = service
        .wait_actionable(&request.id, &request.scope, &CancellationToken::new())
        .await
        .unwrap();
    assert!(initial.is_human_pending());
    drop(service);
    let restored = Service::open(&path).unwrap();
    restored
        .set_policy_config(&scope().workspace, config)
        .unwrap();
    restored
        .set_managed_policy(&scope().workspace, Default::default())
        .unwrap();
    let unloaded = restored.store.load(&initial.id).unwrap();
    assert_eq!(unloaded.revision, initial.revision);
    assert_eq!(unloaded.review, initial.review);
    let provider = Arc::new(Fixed {
        calls: AtomicUsize::new(0),
        outcome: ReviewOutcome::Allow,
    });
    restored
        .register_reviewer(&scope().workspace, &scope().session_id, provider.clone())
        .unwrap();
    let unavailable = restored
        .wait_actionable(&initial.id, &initial.scope, &CancellationToken::new())
        .await
        .unwrap();
    assert!(unavailable.is_human_pending());
    assert_eq!(unavailable.revision, initial.revision);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    restored
        .set_review_context(&scope().workspace, &scope().session_id, context)
        .unwrap();
    let unchanged = restored.store.load(&initial.id).unwrap();
    assert_eq!(unchanged.revision, initial.revision);
    assert_eq!(unchanged.review, initial.review);
    // A real root-evidence update still invalidates the existing card.
    let mut updated = ReviewContext {
        trusted_user_messages: vec!["A new exact fixture choice.".into()],
        instructions: vec!["Keep scope bounded.".into()],
        policy_digest: "fixed-original-policy".into(),
        ..Default::default()
    };
    updated.refresh_digest();
    restored
        .set_review_context(&scope().workspace, &scope().session_id, updated)
        .unwrap();
    assert_eq!(
        restored.store.load(&initial.id).unwrap().revision,
        initial.revision + 1
    );
}
#[tokio::test]
async fn real_workspace_policy_changes_invalidate_unloaded_roots_without_live_context_or_provider()
{
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("approvals.db");
    let service = Service::open(&path).unwrap();
    service
        .set_policy_config(&scope().workspace, Default::default())
        .unwrap();
    let mut context = ReviewContext {
        trusted_user_messages: vec!["Run the fixture.".into()],
        policy_digest: "policy".into(),
        ..Default::default()
    };
    context.refresh_digest();
    let mut requests = Vec::new();
    for id in ["root", "unloaded-root"] {
        service
            .register_reviewer(&scope().workspace, id, Arc::new(Unavailable))
            .unwrap();
        service
            .set_review_context(&scope().workspace, id, context.clone())
            .unwrap();
        let mut proposed = proposal(id);
        proposed.scope.session_id = id.into();
        let request = service.create(proposed).unwrap();
        requests.push(
            service
                .wait_actionable(&request.id, &request.scope, &CancellationToken::new())
                .await
                .unwrap(),
        );
    }
    drop(service);
    let restored = Service::open(&path).unwrap();
    restored
        .set_policy_config(&scope().workspace, Default::default())
        .unwrap();
    for request in &requests {
        assert_eq!(
            restored.store.load(&request.id).unwrap().revision,
            request.revision
        );
    }
    restored
        .set_policy_config(
            &scope().workspace,
            otto_core::config::ApprovalConfig {
                rules: vec![ApprovalRule {
                    source: Some("unrelated".into()),
                    tool: None,
                    command_prefix: None,
                    decision: RuleDecision::Ask,
                }],
                ..Default::default()
            },
        )
        .unwrap();
    for request in &requests {
        let changed = restored.store.load(&request.id).unwrap();
        assert!(changed.is_human_pending());
        assert_eq!(changed.revision, request.revision + 1);
        assert_ne!(
            changed.review.as_ref().unwrap().policy_digest,
            request.review.as_ref().unwrap().policy_digest
        );
    }
    // A genuinely new root has no inherited root-evidence snapshot.
    let mut other = proposal("new-root");
    other.scope.session_id = "new-root".into();
    let other = restored.create(other).unwrap();
    assert!(other.is_human_pending());
    assert_eq!(other.review.unwrap().evidence_digest, "missing");
}
