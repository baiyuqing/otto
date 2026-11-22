//! Deadlines, graceful close, crash recovery and run listing.

use super::*;

#[tokio::test(start_paused = true)]
async fn executor_completion_wins_at_exact_deadline_boundary() {
    let started = Arc::new(Notify::new());
    let definition = definition(
        br#"version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
    );
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new_with_deadlines(
        Arc::new(Store::open_in_memory()),
        Catalog::from_definitions(vec![definition]),
        Arc::new(BoundaryExecutor(Arc::clone(&started))),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        1,
        DeadlinePolicy {
            step_timeout: Some(Duration::from_secs(1)),
            cancellation_grace: Duration::from_secs(1),
        },
        Arc::new(Logger::discard()),
        Default::default(),
    );
    let run = controller.start("flow", "").await.expect("start");
    started.notified().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    let done = controller.wait(&run.id).await.expect("wait");
    assert_eq!(done.status, RunStatus::Succeeded);
    assert_eq!(done.steps[0].result, "boundary");
}

#[tokio::test(start_paused = true)]
async fn timeout_is_not_retryable_before_parallel_siblings_settle() {
    let sibling_started = Arc::new(Notify::new());
    let sibling_release = CancellationToken::new();
    let definition = definition(
        br#"version = 1
[[steps]]
id = "timeout"
agent = "worker"
prompt = "timeout"
[[steps]]
id = "sibling"
agent = "worker"
prompt = "sibling"
"#,
    );
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new_with_deadlines(
        Arc::new(Store::open_in_memory()),
        Catalog::from_definitions(vec![definition]),
        Arc::new(TimeoutBatchExecutor {
            sibling_started: Arc::clone(&sibling_started),
            sibling_release: sibling_release.clone(),
        }),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        2,
        DeadlinePolicy {
            step_timeout: Some(Duration::from_secs(1)),
            cancellation_grace: Duration::from_millis(100),
        },
        Arc::new(Logger::discard()),
        Default::default(),
    );
    let run = controller.start("flow", "").await.expect("start");
    sibling_started.notified().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        controller.get(&run.id).expect("run").status,
        RunStatus::Running
    );
    assert!(controller.resume(&run.id, Some("timeout")).await.is_err());
    sibling_release.cancel();
    assert_eq!(
        controller.wait(&run.id).await.expect("wait").status,
        RunStatus::Paused
    );
}

#[tokio::test]
async fn graceful_close_persists_an_interrupted_attempt() {
    let started = Arc::new(Notify::new());
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
    );
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new(
        Arc::new(Store::open_in_memory()),
        Catalog::from_definitions(vec![definition]),
        Arc::new(BlockingExecutor {
            started: Arc::clone(&started),
        }),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        1,
    );
    let run = controller.start("flow", "").await.expect("start");
    started.notified().await;

    controller.close().await;
    let paused = controller.get(&run.id).expect("run");
    assert_eq!(paused.status, RunStatus::Paused);
    assert_eq!(paused.steps[0].status, StepStatus::Interrupted);
}

#[tokio::test]
async fn active_runs_counts_in_flight_runs_and_drops_to_zero_after_close() {
    let started = Arc::new(Notify::new());
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
    );
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new(
        Arc::new(Store::open_in_memory()),
        Catalog::from_definitions(vec![definition]),
        Arc::new(BlockingExecutor {
            started: Arc::clone(&started),
        }),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        1,
    );
    assert_eq!(controller.active_runs(), 0);

    controller.start("flow", "").await.expect("start");
    started.notified().await;
    assert_eq!(controller.active_runs(), 1);

    controller.close().await;
    assert_eq!(controller.active_runs(), 0);
}

#[test]
fn crash_after_failure_settles_the_run_and_failed_steps_are_retryable() {
    let store = Store::open_in_memory();
    let definition = definition(TWO_STEP);
    store
        .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
        .expect("create");
    store
        .claim_ready("run-1", "a", "/tmp/a1.jsonl")
        .expect("claim a");
    store
        .claim_ready("run-1", "b", "/tmp/b1.jsonl")
        .expect("claim b");
    store
        .finish_attempt("run-1", "a", 1, Err("boom"))
        .expect("fail a");

    // Process death here: `b` is still running, so recovery interrupts it and
    // pauses the run instead of leaving it `running` with no owner.
    assert_eq!(store.recover("/w").expect("recover"), 1);
    assert_eq!(
        store.get_run("run-1").expect("get").expect("run").status,
        RunStatus::Paused
    );

    // Retrying the surviving sibling must not hide the failed step: once it
    // succeeds the run reaches its durable failure rather than hanging.
    store.retry("run-1", "b").expect("retry b");
    let attempt = store
        .claim_ready("run-1", "b", "/tmp/b2.jsonl")
        .expect("claim b");
    store
        .finish_attempt("run-1", "b", attempt, Ok("b done"))
        .expect("finish b");
    let run = store.get_run("run-1").expect("get").expect("run");
    assert_eq!(run.status, RunStatus::Failed, "{:?}", run.steps);
    assert_eq!(run.steps[0].status, StepStatus::Failed);

    // A failed step is retryable, and retrying it revives the run.
    store.retry("run-1", "a").expect("retry a");
    let attempt = store
        .claim_ready("run-1", "a", "/tmp/a2.jsonl")
        .expect("claim a");
    store
        .finish_attempt("run-1", "a", attempt, Ok("a done"))
        .expect("finish a");
    assert_eq!(
        store.get_run("run-1").expect("get").expect("run").status,
        RunStatus::Succeeded
    );
}

#[test]
fn retry_revives_steps_canceled_by_the_failure() {
    let store = Store::open_in_memory();
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "a"
agent = "worker"
prompt = "a"
[[steps]]
id = "b"
agent = "worker"
prompt = "b"
[[steps]]
id = "c"
agent = "worker"
prompt = "c"
needs = ["b"]
"#,
    );
    store
        .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
        .expect("create");
    store
        .claim_ready("run-1", "a", "/tmp/a1.jsonl")
        .expect("claim a");
    store
        .claim_ready("run-1", "b", "/tmp/b1.jsonl")
        .expect("claim b");
    store
        .finish_attempt("run-1", "a", 1, Err("boom"))
        .expect("fail a");
    store.cancel_attempt("run-1", "b", 1).expect("cancel b");
    store.settle("run-1").expect("settle");

    let run = store.get_run("run-1").expect("get").expect("run");
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.steps[2].status, StepStatus::Canceled);

    // Retrying the failed step re-admits the work the failure canceled.
    store.retry("run-1", "a").expect("retry a");
    let run = store.get_run("run-1").expect("get").expect("run");
    assert_eq!(run.steps[1].status, StepStatus::Ready, "{:?}", run.steps);
    assert_eq!(run.steps[2].status, StepStatus::Pending, "{:?}", run.steps);

    let attempt = store
        .claim_ready("run-1", "a", "/tmp/a2.jsonl")
        .expect("claim a");
    store
        .finish_attempt("run-1", "a", attempt, Ok("a done"))
        .expect("finish a");
    let attempt = store
        .claim_ready("run-1", "b", "/tmp/b2.jsonl")
        .expect("claim b");
    store
        .finish_attempt("run-1", "b", attempt, Ok("b done"))
        .expect("finish b");
    let attempt = store
        .claim_ready("run-1", "c", "/tmp/c1.jsonl")
        .expect("claim c");
    store
        .finish_attempt("run-1", "c", attempt, Ok("c done"))
        .expect("finish c");
    assert_eq!(
        store.get_run("run-1").expect("get").expect("run").status,
        RunStatus::Succeeded
    );
}

#[test]
fn oversized_dependency_results_are_truncated_not_rejected() {
    let store = Store::open_in_memory();
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#,
    );
    store
        .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
        .expect("create");
    let attempt = store
        .claim_ready("run-1", "first", "/tmp/first.jsonl")
        .expect("claim");
    let huge = "x".repeat(MAX_TEXT_BYTES + 4096);
    store
        .finish_attempt("run-1", "first", attempt, Ok(&huge))
        .expect("finish");

    let run = store.get_run("run-1").expect("get").expect("run");
    assert!(
        run.steps[0].result.len() <= MAX_TEXT_BYTES + 64,
        "stored result was not bounded: {}",
        run.steps[0].result.len()
    );
    let prompt = attempt_prompt(&run, &run.steps[1]).expect("prompt");
    assert!(
        prompt.len() <= MAX_TEXT_BYTES,
        "prompt size {}",
        prompt.len()
    );
    assert!(prompt.contains("[truncated"), "prompt was not marked");
}

#[test]
fn list_runs_pages_newest_first_with_a_cursor() {
    let store = Store::open_in_memory();
    let definition = definition(TWO_STEP);
    for id in ["run-1", "run-2", "run-3"] {
        store
            .create_run(id, &definition, "/w", "p", "provider", "m", "")
            .expect("create");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let page = store
        .list_runs("/w", &ListQuery::page(2, None))
        .expect("page");
    let ids: Vec<&str> = page.iter().map(|run| run.id.as_str()).collect();
    assert_eq!(ids, ["run-3", "run-2"]);

    let next = store
        .list_runs("/w", &ListQuery::page(2, Some(page[1].created_at.clone())))
        .expect("next page");
    let ids: Vec<&str> = next.iter().map(|run| run.id.as_str()).collect();
    assert_eq!(ids, ["run-1"]);
    assert!(next[0].steps.len() == 2, "a page keeps each run's steps");
    assert!(
        store
            .list_runs("/other", &ListQuery::all())
            .expect("other workspace")
            .is_empty()
    );
}

#[tokio::test]
async fn rejecting_a_gate_cancels_a_running_sibling() {
    let started = Arc::new(AtomicBool::new(false));
    let canceled = Arc::new(AtomicBool::new(false));
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship?"
[[steps]]
id = "deliver"
agent = "worker"
prompt = "deliver"
needs = ["work", "approve"]
"#,
    );
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new(
        Arc::new(Store::open_in_memory()),
        Catalog::from_definitions(vec![definition]),
        Arc::new(GateExecutor {
            started: Arc::clone(&started),
            canceled: Arc::clone(&canceled),
        }),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        2,
    );

    let run = controller.start("flow", "").await.expect("start");
    for _ in 0..200 {
        if started.load(Ordering::SeqCst)
            && controller.requests(&run.id).expect("requests").len() == 1
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        started.load(Ordering::SeqCst),
        "the sibling attempt never started"
    );
    let request = controller
        .requests(&run.id)
        .expect("requests")
        .pop()
        .expect("request");

    let rejected = controller.respond(&request.id, false).expect("reject");
    assert_eq!(rejected.status, RunStatus::Canceled);
    for _ in 0..200 {
        if canceled.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        canceled.load(Ordering::SeqCst),
        "a rejected run kept executing its sibling"
    );
}
