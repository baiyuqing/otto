use super::{
    reviewer::{ReviewContext, ReviewProvider},
    *,
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
#[tokio::test]
async fn first_review_event_never_enters_the_human_queue_and_config_checkpoint_stays_human() {
    let (service, provider) = configured(ReviewOutcome::Allow);
    let request = service.create(proposal("first")).unwrap();
    assert!(
        service
            .events(&scope().workspace, Some(&scope().session_id), 0)
            .unwrap()
            .iter()
            .all(|e| !e.request.is_human_pending())
    );
    assert!(
        service
            .snapshot(&scope().workspace, Some(&scope().session_id))
            .unwrap()
            .requests
            .is_empty()
    );
    service
        .wait_actionable(&request.id, &request.scope, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        service
            .get(&request.id, &scope().workspace, Some(&scope().session_id))
            .unwrap()
            .state,
        State::Approved
    );
    let mut security = proposal("credential-change");
    security.authority = Authority::Consent;
    security.subject.source = "config".into();
    let security = service.create(security).unwrap();
    assert!(security.is_human_pending());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn explicit_rules_precede_presets_and_already_granted_commands_pass_never_user() {
    for mode in [ApprovalMode::Manual, ApprovalMode::Yolo] {
        let service = Service::in_memory().unwrap();
        service
            .set_policy_config(
                &scope().workspace,
                otto_core::config::ApprovalConfig {
                    rules: vec![ApprovalRule {
                        source: Some("sandbox".into()),
                        tool: None,
                        command_prefix: None,
                        decision: RuleDecision::Deny,
                    }],
                    ..Default::default()
                },
            )
            .unwrap();
        service.set_mode(&scope(), mode).unwrap();
        assert_eq!(
            service.create(proposal("denied")).unwrap().state,
            State::Rejected
        );
    }
    let service = Service::in_memory().unwrap();
    service
        .set_policy_config(
            &scope().workspace,
            otto_core::config::ApprovalConfig {
                policy: Some(ApprovalPolicy::Never),
                reviewer: Some(ApprovalReviewer::User),
                rules: vec![ApprovalRule {
                    source: None,
                    tool: None,
                    command_prefix: Some(vec!["printf".into()]),
                    decision: RuleDecision::Allow,
                }],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        service.create(proposal("covered")).unwrap().state,
        State::Approved
    );
    let mut complex = proposal("complex");
    complex.subject.details = r#"{"command":"printf fixture && rm fixture"}"#.into();
    assert_eq!(service.create(complex).unwrap().state, State::Rejected);
}
#[test]
fn bounded_grants_are_invalidated_by_new_rules_and_managed_settings() {
    let service = Service::in_memory().unwrap();
    let mut bounded = proposal("bounded");
    bounded.authority = Authority::Bounded;
    let original = service.create(bounded).unwrap();
    assert_eq!(original.state, State::Approved);
    service
        .set_policy_config(
            &scope().workspace,
            otto_core::config::ApprovalConfig {
                rules: vec![ApprovalRule {
                    source: Some("sandbox".into()),
                    tool: None,
                    command_prefix: None,
                    decision: RuleDecision::Ask,
                }],
                ..Default::default()
            },
        )
        .unwrap();
    let current = service.revalidate_authorization(&original).unwrap();
    assert!(current.is_human_pending());
    assert_eq!(current.revision, original.revision + 1);
    assert!(
        service
            .application(
                &current.id,
                &current.scope,
                ApplicationState::NotStarted,
                ApplicationState::Applying,
                ""
            )
            .is_err()
    );
    service.set_mode(&scope(), ApprovalMode::Yolo).unwrap();
    service
        .set_managed_policy(
            &scope().workspace,
            otto_core::config::ApprovalConfig {
                reviewer: Some(ApprovalReviewer::User),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        service.settings(&scope()).unwrap().reviewer,
        ApprovalReviewer::User
    );
    assert!(service.set_mode(&scope(), ApprovalMode::Yolo).is_err());
    let mut new = proposal("managed");
    new.subject.source = "mcp".into();
    assert!(service.create(new).unwrap().is_human_pending());
}
#[tokio::test]
async fn three_denials_stop_the_same_logical_task_without_another_model_call() {
    let (service, provider) = configured(ReviewOutcome::Deny);
    for index in 0..4 {
        let request = service
            .create(proposal(&format!("attempt-{index}")))
            .unwrap();
        service
            .wait_actionable(&request.id, &request.scope, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            service.store.load(&request.id).unwrap().state,
            State::Rejected
        );
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
}
#[tokio::test]
async fn policy_epoch_invalidates_root_review_snapshots_in_its_committing_transaction() {
    let (service, _) = configured(ReviewOutcome::Allow);
    service.set_mode(&scope(), ApprovalMode::Auto).unwrap();
    let request = service.create(proposal("policy-race")).unwrap();
    service
        .wait_actionable(&request.id, &request.scope, &CancellationToken::new())
        .await
        .unwrap();
    let approved = service.store.load(&request.id).unwrap();
    assert!(service.store.authorization_current(&approved).unwrap());
    let revision = service.settings(&scope()).unwrap().revision;
    // Exercise only the persisted policy transaction, before the service has
    // republished any live root context or refreshed its in-memory policy.
    service
        .store
        .policy_config(
            &scope().workspace,
            ApprovalSettings::default(),
            "changed-policy-digest",
        )
        .unwrap();
    assert_eq!(service.settings(&scope()).unwrap().revision, revision);
    assert!(!service.store.authorization_current(&approved).unwrap());
}
struct HeldActionable {
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    fail: bool,
}
#[async_trait::async_trait]
impl ReviewProvider for HeldActionable {
    async fn assess(
        &self,
        _: &Request,
        _: &ReviewContext,
        cancel: &CancellationToken,
    ) -> std::result::Result<ReviewAssessment, String> {
        self.entered.add_permits(1);
        tokio::select! {
            permit = self.release.acquire() => permit.unwrap().forget(),
            () = cancel.cancelled() => return Err("cancelled".into()),
        }
        if self.fail {
            return Err("fixture unavailable".into());
        }
        Ok(ReviewAssessment {
            risk: RiskLevel::Medium,
            user_authorization: UserAuthorization::Unknown,
            outcome: ReviewOutcome::Ask,
            reason: "An exact user choice is still needed.".into(),
        })
    }
}
#[tokio::test]
async fn actionable_wait_returns_exact_pending_ask_or_failure_without_replaying_review() {
    for fail in [false, true] {
        let (service, _) = configured(ReviewOutcome::Allow);
        let service = Arc::new(service);
        let held = Arc::new(HeldActionable {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            fail,
        });
        service
            .register_reviewer(&scope().workspace, &scope().session_id, held.clone())
            .unwrap();
        let request = service.create(proposal("actionable")).unwrap();
        let expected = request.clone();
        let waiter = service.clone();
        let task = tokio::spawn(async move {
            waiter
                .wait_actionable(&request.id, &request.scope, &CancellationToken::new())
                .await
                .unwrap()
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), held.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert!(!task.is_finished());
        held.release.add_permits(1);
        let actual = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(actual.is_human_pending());
        assert_eq!(actual.id, expected.id);
        assert_eq!(actual.revision, expected.revision);
        let polled = service
            .wait_actionable(&actual.id, &actual.scope, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(polled, actual);
        let terminal = service.clone();
        let original = actual.clone();
        let mut waiting = tokio::spawn(async move {
            terminal
                .wait(&original.id, &original.scope, &CancellationToken::new())
                .await
                .unwrap()
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), &mut waiting)
                .await
                .is_err()
        );
        service
            .withdraw(&actual.id, &actual.scope, actual.revision)
            .unwrap();
        assert_eq!(waiting.await.unwrap().state, State::Withdrawn);
    }
}
#[tokio::test]
async fn actionable_wait_returns_approved_or_withdrawn_terminal_requests() {
    let (service, _) = configured(ReviewOutcome::Allow);
    let request = service.create(proposal("allow-actionable")).unwrap();
    assert_eq!(
        service
            .wait_actionable(&request.id, &request.scope, &CancellationToken::new())
            .await
            .unwrap()
            .state,
        State::Approved
    );
    let request = service.create(proposal("cancel-actionable")).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        service
            .wait_actionable(&request.id, &request.scope, &cancel)
            .await
            .unwrap()
            .state,
        State::Withdrawn
    );
}
#[tokio::test]
async fn changed_human_authority_basis_advances_revision_and_fences_both_old_choices() {
    let (service, _) = configured(ReviewOutcome::Ask);
    let request = service.create(proposal("stale-card")).unwrap();
    let human = service
        .wait_actionable(&request.id, &request.scope, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(human.revision, request.revision); // Status and assessment only.
    service.set_mode(&scope(), ApprovalMode::Manual).unwrap();
    let refreshed = service.store.load(&request.id).unwrap();
    assert_eq!(refreshed.revision, human.revision + 1);
    assert_eq!(refreshed.subject, human.subject);
    for allow in [true, false] {
        assert!(matches!(
            service.decide(
                &request.id,
                &request.scope,
                human.revision,
                allow,
                "",
                &Actor::local("old-card")
            ),
            Err(Error::Conflict)
        ));
    }
    service
        .decide(
            &request.id,
            &request.scope,
            refreshed.revision,
            true,
            "",
            &Actor::local("new-card"),
        )
        .unwrap();
}
#[tokio::test]
async fn context_change_fences_a_human_decision_before_the_pending_row_refresh() {
    let (service, _) = configured(ReviewOutcome::Ask);
    let request = service.create(proposal("context-card")).unwrap();
    let human = service
        .wait_actionable(&request.id, &request.scope, &CancellationToken::new())
        .await
        .unwrap();
    let metadata = human.review.as_ref().unwrap();
    // Exercise the durable context update in isolation, before the live
    // service has republished the request envelope/revision.
    service
        .store
        .review_context(
            &human.scope,
            "changed-user-evidence",
            &metadata.policy_digest,
        )
        .unwrap();
    assert!(matches!(
        service.decide(
            &human.id,
            &human.scope,
            human.revision,
            true,
            "",
            &Actor::local("stale-context-card")
        ),
        Err(Error::Conflict)
    ));
    assert_eq!(service.store.load(&human.id).unwrap().state, State::Pending);
}
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
