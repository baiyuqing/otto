//! Admission identity and shutdown ownership across wake and replacement races.

use super::*;

fn queue_wake(controller: &Controller) {
    controller.current_runner().expect("runner").mailbox().push(
        otto_core::agent::inbox::Notification {
            text: "background work finished".into(),
            ..Default::default()
        },
    );
}

async fn controller_with_failing_journal_sync(
    workspace: &Path,
    sessions: &Path,
) -> (
    Controller,
    SharedSession,
    Arc<crate::faults::FaultPlan<crate::faults::files::FileOp>>,
) {
    use crate::faults::files::{FaultyFileDriver, FileOp};
    use crate::faults::{Fault, FaultError, FaultPlan, Filter, Rule, Trigger};
    use crate::storage::drivers::posix::PosixDriver;

    let builder = builder(workspace, sessions);
    let runtime = initial_runtime(&builder);
    let original = builder.create_session(&runtime).unwrap();
    original.append(user("persist the journal")).await.unwrap();
    let path = original.path();
    original.close().unwrap();
    let plan = FaultPlan::new(vec![
        Rule::new(
            FileOp::FileSync,
            Trigger::Nth(1),
            Fault::Before(FaultError::Os(libc::EIO)),
        )
        .filter(Filter::NameSuffix(".jsonl")),
    ]);
    let driver = Arc::new(FaultyFileDriver::new(Arc::new(PosixDriver), plan.clone()));
    let (store, _) = crate::session::Prepared::prepare_with_driver(Path::new(&path), None, driver)
        .unwrap()
        .activate()
        .unwrap();
    let session = SharedSession::new(Arc::new(store));
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session.clone(), runner, info);
    (controller, session, plan)
}

#[tokio::test]
async fn dropping_a_wake_does_not_release_the_next_operation_during_close() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    queue_wake(&controller);
    controller.prepare_wake().unwrap().unwrap().cancel();

    let admission = controller.begin_operation().expect("ordinary operation");
    controller.request_close();
    assert!(controller.current_session_opt().is_some());
    drop(admission);

    // The old wake flag incorrectly revoked this operation's generation,
    // leaving its deferred cleanup unowned after the guard was dropped.
    assert!(controller.current_session_opt().is_none());
    controller.close().expect("already closed");
}

#[tokio::test]
async fn a_stale_admission_cannot_release_a_later_operation() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let prior = controller.begin_operation().unwrap();
    let prior_generation = prior.generation;
    drop(prior);
    let active = controller.begin_operation().unwrap();

    controller.release(prior_generation);

    assert_eq!(controller.begin_operation().unwrap_err(), PROMPT_ACTIVE);
    controller.request_close();
    assert!(controller.current_session_opt().is_some());
    drop(active);
    assert!(controller.current_session_opt().is_none());
}

#[tokio::test]
async fn close_revokes_an_unstarted_wake_without_running_it() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    queue_wake(&controller);
    let wake = controller.prepare_wake().unwrap().unwrap();

    // This must not wait for a reservation whose owner has not started.
    controller.close().expect("close reserved wake");
    let mut emitted = false;
    let error = wake
        .run(&mut |_| emitted = true, &CancellationToken::new())
        .await
        .expect_err("reservation was revoked");

    assert_eq!(error.to_string(), CLOSED);
    assert!(!emitted, "a revoked wake must never enter the agent loop");
    assert!(controller.current_session_opt().is_none());
    controller.close().expect("same completed close");
}

#[tokio::test]
async fn a_started_wake_keeps_admission_until_its_owner_releases_it() {
    use crate::fixtures::subagent_doubles::{FakeProvider, assistant_text, match_any};
    use crate::subagent::tasks::Tasks;

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let builder = builder(workspace.path(), sessions.path());
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).unwrap();
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let runner = Runner::scripted(session.clone(), provider, Arc::new(Tasks::new()));
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session, runner, info);
    queue_wake(&controller);
    let wake = controller.prepare_wake().unwrap().unwrap();
    let mut requested = false;
    wake.run(
        &mut |event| {
            if matches!(event, otto_core::agent::Event::AgentStarted) {
                controller.request_close();
                requested = true;
                assert!(controller.current_session_opt().is_some());
                assert_eq!(controller.begin_operation().unwrap_err(), CLOSED);
            }
        },
        &CancellationToken::new(),
    )
    .await
    .expect("the started wake retains its session");
    assert!(requested, "the wake reached execution before close");
    assert!(controller.current_session_opt().is_none());
}

#[tokio::test]
async fn concurrent_close_waits_for_the_active_admission() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let admission = controller.begin_operation().unwrap();

    std::thread::scope(|scope| {
        let (requested, requests) = std::sync::mpsc::channel();
        let (finished, results) = std::sync::mpsc::channel();
        let mut closers = Vec::new();
        for _ in 0..2 {
            let requested = requested.clone();
            let finished = finished.clone();
            let controller = &controller;
            closers.push(scope.spawn(move || {
                controller.request_close();
                requested.send(()).unwrap();
                finished.send(controller.close()).unwrap();
            }));
        }
        for _ in 0..2 {
            requests.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert!(results.try_recv().is_err(), "active work owns cleanup");
        assert!(controller.current_session_opt().is_some());
        drop(admission);
        for _ in 0..2 {
            assert_eq!(
                results.recv_timeout(Duration::from_secs(5)).unwrap(),
                Ok(())
            );
        }
        for closer in closers {
            closer.join().unwrap();
        }
    });
    assert!(controller.current_session_opt().is_none());
}

#[tokio::test]
async fn a_replacement_built_before_close_is_discarded_without_publication() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let admission = controller.begin_replacement().unwrap();
    let runtime = controller.current_runtime().unwrap();
    let candidate = controller.fresh_replacement(&runtime).await.unwrap();
    let candidate_session = candidate.session.clone();
    let prior_session = controller.current_session();

    controller.request_close();
    assert_eq!(controller.commit(candidate, admission).unwrap_err(), CLOSED);

    assert!(controller.current_session_opt().is_none());
    for session in [prior_session, candidate_session] {
        assert!(
            session.append(user("must not persist")).await.is_err(),
            "both the displaced session and unpublished candidate must be closed"
        );
    }
}

#[tokio::test]
async fn a_replacement_requires_the_admission_that_built_it() {
    use crate::app::lifecycle::ReplacementCommit;

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let admission = controller.begin_replacement().unwrap();
    let prior_generation = admission.generation;
    let runtime = controller.current_runtime().unwrap();
    let candidate = controller.fresh_replacement(&runtime).await.unwrap();
    drop(admission);
    let active = controller.begin_operation().unwrap();
    let prior_session = controller.info().session_id;

    let rejected = controller
        .lock()
        .install_replacement(candidate, prior_generation);
    let ReplacementCommit::Rejected(candidate) = rejected else {
        panic!("a stale admission published its replacement");
    };

    assert_eq!(controller.info().session_id, prior_session);
    assert_eq!(controller.begin_operation().unwrap_err(), PROMPT_ACTIVE);
    candidate.discard().unwrap();
    drop(active);
    controller.close().unwrap();
}

#[tokio::test]
async fn failed_disposal_stays_closed_and_returns_the_same_error() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, session, plan) =
        controller_with_failing_journal_sync(workspace.path(), sessions.path()).await;

    let first = controller.close().expect_err("injected close sync failure");

    assert!(first.contains("close session file"), "{first}");
    controller.request_close();
    assert_eq!(controller.close(), Err(first));
    assert_eq!(controller.begin_operation().unwrap_err(), CLOSED);
    assert!(controller.current_session_opt().is_none());
    assert!(session.append(user("must not persist")).await.is_err());
    plan.assert_all_fired();
}

#[tokio::test]
async fn concurrent_close_callers_share_failure_after_active_admission_releases() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let (controller, _session, plan) =
        controller_with_failing_journal_sync(workspace.path(), sessions.path()).await;
    let admission = controller.begin_operation().unwrap();

    std::thread::scope(|scope| {
        let (requested, requests) = std::sync::mpsc::channel();
        let (finished, results) = std::sync::mpsc::channel();
        let mut closers = Vec::new();
        for _ in 0..2 {
            let requested = requested.clone();
            let finished = finished.clone();
            let controller = &controller;
            closers.push(scope.spawn(move || {
                controller.request_close();
                requested.send(()).unwrap();
                finished.send(controller.close()).unwrap();
            }));
        }
        for _ in 0..2 {
            requests.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert!(results.try_recv().is_err(), "active work owns cleanup");
        assert!(controller.current_session_opt().is_some());
        assert_eq!(controller.begin_operation().unwrap_err(), CLOSED);
        drop(admission);

        let first = results.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = results.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first, second);
        assert!(first.as_ref().unwrap_err().contains("close session file"));
        for closer in closers {
            closer.join().unwrap();
        }
    });
    assert!(controller.current_session_opt().is_none());
    plan.assert_all_fired();
}
