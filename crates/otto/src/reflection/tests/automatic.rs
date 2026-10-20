//! Tests for the decision gate and automatic triggers.

use super::*;

#[tokio::test]
async fn decision_defer_retains_evidence_and_explicit_runs_bypass_gate() {
    let fixture = Fixture::new(Vec::new()).await;
    fixture
        .say(Role::User, "Remember this lasting preference")
        .await;
    let reflector = fixture
        .reflector(auto_config(Auto::OnWork))
        .with_decision_backend(
            Arc::new(gate::tests::Fixed(Ok(("defer", Some(0.0))))),
            Arc::new(crate::log::Logger::discard()),
        );
    for trigger in [Trigger::OnWork, Trigger::OnCompaction] {
        let report = run_as(&fixture, &reflector, trigger)
            .await
            .expect("deferred");
        assert!(report.is_skipped());
        assert!(report.line().contains("deferred"));
    }
    assert!(fixture.provider.calls().is_empty());
    assert!(
        fixture
            .store
            .runs(&fixture.session_id())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fixture.store.watermark(&fixture.session_id()).unwrap(),
        None
    );
    for trigger in [Trigger::Manual, Trigger::Requested] {
        fixture.say(Role::User, "Another lasting preference").await;
        push(&fixture, Reply::Text(r#"{"memories":[]}"#.into()));
        assert!(run_as(&fixture, &reflector, trigger).await.is_ok());
    }
    assert_eq!(fixture.provider.calls().len(), 2);
    let previous = fixture.store.watermark(&fixture.session_id()).unwrap();
    assert!(previous.is_some());
    fixture
        .say(Role::User, "Evidence after the completed explicit run")
        .await;
    let deferred = reflector.with_minimum_interval(chrono::Duration::zero());
    assert!(
        run_as(&fixture, &deferred, Trigger::OnWork)
            .await
            .unwrap()
            .is_skipped()
    );
    assert_eq!(
        fixture.store.watermark(&fixture.session_id()).unwrap(),
        previous
    );
    let immediate = deferred.with_decision_backend(
        Arc::new(gate::tests::Fixed(Ok(("immediate", Some(1.0))))),
        Arc::new(crate::log::Logger::discard()),
    );
    push(&fixture, Reply::Text(r#"{"memories":[]}"#.into()));
    assert!(run_as(&fixture, &immediate, Trigger::OnWork).await.is_ok());
    assert_ne!(
        fixture.store.watermark(&fixture.session_id()).unwrap(),
        previous
    );
    assert_eq!(fixture.provider.calls().len(), 3);
}

#[tokio::test]
async fn decision_gate_also_defers_eligible_on_exit_reflection() {
    let fixture = Fixture::new(Vec::new()).await;
    fixture.say(Role::User, "A lasting preference").await;
    let reflector = fixture
        .reflector(ReflectionRuntime {
            min_turns: 1,
            ..auto_config(Auto::OnExit)
        })
        .with_decision_backend(
            Arc::new(gate::tests::Fixed(Ok(("defer", None)))),
            Arc::new(crate::log::Logger::discard()),
        );
    assert!(
        run_as(&fixture, &reflector, Trigger::OnExit)
            .await
            .unwrap()
            .is_skipped()
    );
    assert!(fixture.provider.calls().is_empty());
    assert_eq!(
        fixture.store.watermark(&fixture.session_id()).unwrap(),
        None
    );
}

#[tokio::test]
async fn decision_failure_runs_original_reflection_and_cancellation_retains_watermark() {
    let fixture = Fixture::new(Vec::new()).await;
    fixture.say(Role::User, "A lasting preference").await;
    let reflector = fixture
        .reflector(auto_config(Auto::OnWork))
        .with_decision_backend(
            Arc::new(gate::tests::Fixed(Err(crate::decision::Failure::Invalid(
                "invalid".into(),
            )))),
            Arc::new(crate::log::Logger::discard()),
        );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let path = fixture.session.path();
    let header = fixture.session.header();
    let result = reflector
        .run(
            &Context {
                approvals: None,
                runner: &fixture.runner,
                session_id: &header.id,
                presentation_origin: None,
                session_path: &path,
                service: &fixture.service,
                user_scope: &fixture.user_scope,
                workspace_scope: &fixture.workspace_scope,
                skill_roots: Some(&fixture.roots),
            },
            Trigger::OnWork,
            "",
            &cancel,
        )
        .await;
    assert!(matches!(result, Err(Error::Cancelled)));
    assert_eq!(
        fixture.store.watermark(&fixture.session_id()).unwrap(),
        None
    );
    push(&fixture, Reply::Text(r#"{"memories":[]}"#.into()));
    // Canceled rows participate in the existing cooldown; remove only the
    // test interval, not the production scheduling rule.
    let reflector = reflector.with_minimum_interval(chrono::Duration::zero());
    assert!(run_as(&fixture, &reflector, Trigger::OnWork).await.is_ok());
    assert_eq!(fixture.provider.calls().len(), 1);
    assert!(
        fixture
            .store
            .watermark(&fixture.session_id())
            .unwrap()
            .is_some()
    );
}

// -- automatic triggers ---------------------------------------------------

async fn run_as(
    fixture: &Fixture,
    reflector: &Reflector,
    trigger: Trigger,
) -> Result<Report, Error> {
    let path = fixture.session.path();
    let header = fixture.session.header();
    reflector
        .run(
            &Context {
                approvals: None,
                runner: &fixture.runner,
                session_id: &header.id,
                presentation_origin: None,
                session_path: &path,
                service: &fixture.service,
                user_scope: &fixture.user_scope,
                workspace_scope: &fixture.workspace_scope,
                skill_roots: Some(&fixture.roots),
            },
            trigger,
            "",
            &CancellationToken::new(),
        )
        .await
}

fn auto_config(auto: Auto) -> ReflectionRuntime {
    ReflectionRuntime {
        auto,
        ..ReflectionRuntime::default()
    }
}

#[tokio::test]
async fn an_automatic_run_needs_its_own_mode_to_be_selected() {
    let fixture = Fixture::new(Vec::new()).await;
    fixture.say(Role::User, "a topic worth reflecting on").await;
    for (auto, trigger) in [
        (Auto::Off, Trigger::OnCompaction),
        (Auto::OnExit, Trigger::OnCompaction),
        (Auto::OnCompaction, Trigger::OnExit),
        (Auto::Off, Trigger::OnWork),
        (Auto::OnCompaction, Trigger::OnWork),
        (Auto::OnExit, Trigger::OnWork),
        (Auto::OnWork, Trigger::OnExit),
        (Auto::Off, Trigger::OnExit),
    ] {
        let reflector = fixture.reflector(auto_config(auto));
        let error = run_as(&fixture, &reflector, trigger)
            .await
            .expect_err("refused");
        assert!(
            matches!(error, Error::Disabled),
            "{auto:?} {trigger:?}: {error}"
        );
    }
    assert!(fixture.provider.calls().is_empty());
    let disabled = Reflector::new(
        ReflectionRuntime {
            enabled: false,
            ..ReflectionRuntime::default()
        },
        None,
    );
    assert_eq!(disabled.auto(), Auto::Off);
    // A manual run ignores `auto`.
    let off = fixture.reflector(auto_config(Auto::Off));
    push(&fixture, Reply::Text(r#"{"memories":[]}"#.into()));
    assert!(run_as(&fixture, &off, Trigger::Manual).await.is_ok());
}

#[tokio::test]
async fn on_exit_skips_a_session_shorter_than_min_turns_without_recording_a_run() {
    let fixture = Fixture::new(vec![Reply::Text(r#"{"memories":[]}"#.into())]).await;
    fixture.say(Role::User, "first").await;
    fixture.say(Role::User, "second").await;
    let dir = tempfile::tempdir().expect("log dir");
    let runs = dir.path().join("runs");
    let reflector = fixture
        .reflector(ReflectionRuntime {
            min_turns: 3,
            ..auto_config(Auto::OnExit)
        })
        .with_log_dir(runs.clone());

    let report = run_as(&fixture, &reflector, Trigger::OnExit)
        .await
        .expect("skipped");
    assert_eq!(report.status, Status::Noop);
    assert!(
        report.line().contains("shorter than min_turns"),
        "{}",
        report.line()
    );
    assert!(fixture.provider.calls().is_empty());
    assert!(report.log.is_none());
    assert!(!runs.exists(), "a skipped run writes no log");
    let session_id = fixture.session_id();
    assert!(
        fixture.store.runs(&session_id).expect("runs").is_empty(),
        "no row"
    );
    assert_eq!(
        fixture.store.watermark(&session_id).expect("watermark"),
        None
    );

    fixture.say(Role::User, "third").await;
    let report = run_as(&fixture, &reflector, Trigger::OnExit)
        .await
        .expect("runs");
    assert_eq!(report.status, Status::Ok);
    assert_eq!(fixture.provider.calls().len(), 1);
    assert_eq!(fixture.store.runs(&session_id).expect("runs").len(), 1);
}

#[tokio::test]
async fn automatic_compaction_runs_respect_the_minimum_interval_and_never_retry() {
    let fixture = Fixture::new(vec![
        Reply::Fail("boom".into()),
        Reply::Text(r#"{"memories":[]}"#.into()),
    ])
    .await;
    fixture.say(Role::User, "a topic worth reflecting on").await;
    let reflector = fixture.reflector(auto_config(Auto::OnCompaction));

    let error = run_as(&fixture, &reflector, Trigger::OnCompaction)
        .await
        .expect_err("the first run fails");
    assert!(matches!(error, Error::Model(_)), "{error}");
    assert!(
        reflector.too_soon(&fixture.session_id()),
        "a failed run still counts"
    );

    let report = run_as(&fixture, &reflector, Trigger::OnCompaction)
        .await
        .expect("skipped");
    assert!(
        report.line().contains("started recently"),
        "{}",
        report.line()
    );
    assert_eq!(
        fixture.provider.calls().len(),
        1,
        "no retry inside the interval"
    );

    let eager = fixture
        .reflector(auto_config(Auto::OnCompaction))
        .with_minimum_interval(chrono::Duration::zero());
    assert!(!eager.too_soon(&fixture.session_id()));
    let report = run_as(&fixture, &eager, Trigger::OnCompaction)
        .await
        .expect("runs");
    assert_eq!(report.status, Status::Ok);
    assert_eq!(fixture.provider.calls().len(), 2);
}

#[tokio::test]
async fn on_work_and_on_compaction_share_the_automatic_cooldown() {
    let fixture = Fixture::new(vec![
        Reply::Text(r#"{"memories":[]}"#.into()),
        Reply::Text(r#"{"memories":[]}"#.into()),
    ])
    .await;
    fixture.say(Role::User, "a topic worth reflecting on").await;
    let reflector = fixture.reflector(auto_config(Auto::OnWork));

    run_as(&fixture, &reflector, Trigger::OnWork)
        .await
        .expect("work trigger runs");
    let report = run_as(&fixture, &reflector, Trigger::OnCompaction)
        .await
        .expect("compaction trigger is throttled");
    assert!(
        report.line().contains("started recently"),
        "{}",
        report.line()
    );
    assert_eq!(fixture.provider.calls().len(), 1);
}

#[tokio::test]
async fn a_manual_run_is_never_held_back_by_the_interval() {
    let fixture = Fixture::new(vec![
        Reply::Text(r#"{"memories":[]}"#.into()),
        Reply::Text(r#"{"memories":[]}"#.into()),
    ])
    .await;
    fixture.say(Role::User, "first topic").await;
    let reflector = fixture.reflector(auto_config(Auto::OnCompaction));
    run_as(&fixture, &reflector, Trigger::OnCompaction)
        .await
        .expect("auto");
    fixture.say(Role::User, "second topic").await;
    let report = run_as(&fixture, &reflector, Trigger::Manual)
        .await
        .expect("manual");
    assert_eq!(report.status, Status::Ok);
    assert_eq!(fixture.provider.calls().len(), 2);
}
