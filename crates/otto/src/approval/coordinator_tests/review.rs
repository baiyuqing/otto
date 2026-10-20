//! Tests for review events, rules, grants, denials and the actionable wait.

use super::*;

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

#[tokio::test]
async fn unregister_reviewer_removes_only_the_still_registered_provider() {
    let (service, first) = configured(ReviewOutcome::Allow);
    let first: Arc<dyn ReviewProvider> = first;
    let second: Arc<dyn ReviewProvider> = Arc::new(Unavailable);
    let (workspace, session) = (scope().workspace, scope().session_id);
    service
        .register_reviewer(&workspace, &session, second.clone())
        .unwrap();
    service
        .unregister_reviewer(&workspace, &session, &first)
        .unwrap();
    {
        let state = service.review_state.read().unwrap();
        assert_eq!(state.providers.len(), 1);
        assert_eq!(state.contexts.len(), 1);
    }
    service
        .unregister_reviewer(&workspace, &session, &second)
        .unwrap();
    let state = service.review_state.read().unwrap();
    assert!(state.providers.is_empty());
    assert!(state.contexts.is_empty());
}
