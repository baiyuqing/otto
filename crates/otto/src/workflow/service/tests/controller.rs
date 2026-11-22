//! Controller runs: sequential, handoff, fork, approval and failure handling.

use super::*;

#[tokio::test]
async fn controller_runs_sequential_steps_to_completion() {
    let store: Arc<dyn WorkflowStore> = Arc::new(Store::open_in_memory());
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
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new(
        store,
        Catalog {
            definitions: vec![definition],
            diagnostics: Vec::new(),
        },
        Arc::new(FakeExecutor),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity {
            profile: "default".into(),
            provider: "openai-compatible".into(),
            model: "model".into(),
        },
        2,
    );

    let run = controller.start("flow", "request").await.expect("start");
    let completed = controller.wait(&run.id).await.expect("wait");
    assert_eq!(completed.status, RunStatus::Succeeded);
    assert_eq!(completed.steps[0].result, "first complete");
    assert_eq!(completed.steps[1].result, "second complete");
}

#[tokio::test]
async fn a_finished_run_writes_one_event_per_step_and_one_for_the_run() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
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
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new_with_deadlines(
        Arc::new(Store::open_in_memory()),
        Catalog {
            definitions: vec![definition],
            diagnostics: Vec::new(),
        },
        Arc::new(FakeExecutor),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        2,
        DeadlinePolicy::default(),
        Arc::new(Logger::new(Box::new(CapturedLog(Arc::clone(&bytes))))),
        Default::default(),
    );
    let run = controller
        .start("flow", "secret input")
        .await
        .expect("start");
    controller.wait(&run.id).await.expect("wait");
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let steps: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("msg=workflow_step_finished"))
        .collect();
    assert_eq!(steps.len(), 2, "{text}");
    for (line, id) in steps.iter().zip(["first", "second"]) {
        for field in [
            "level=INFO".to_string(),
            format!("run_id={}", run.id),
            "workflow=flow".to_string(),
            format!("step_id={id}"),
            "status=succeeded".to_string(),
            "duration_ms=".to_string(),
        ] {
            assert!(line.contains(&field), "{field} missing in {line}");
        }
    }
    let finished: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("msg=workflow_run_finished"))
        .collect();
    assert_eq!(finished.len(), 1, "{text}");
    for field in [format!("run_id={}", run.id), "status=succeeded".to_string()] {
        assert!(
            finished[0].contains(&field),
            "{field} missing in {}",
            finished[0]
        );
    }
    assert!(!text.contains("secret input"), "{text}");
}

#[tokio::test]
async fn controller_runs_handoff_steps_to_completion() {
    let store = Arc::new(Store::open_in_memory());
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "research"
agent = "researcher"
prompt = "research"
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "review"
needs = ["research"]
"#,
    );
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new(
        store,
        Catalog::from_definitions(vec![definition]),
        Arc::new(FakeExecutor),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        2,
    );

    let run = controller.start("flow", "").await.expect("start");
    let completed = controller.wait(&run.id).await.expect("wait");
    assert_eq!(completed.status, RunStatus::Succeeded);
    assert_eq!(completed.steps[1].kind, StepKind::Handoff);
    assert_eq!(completed.steps[1].result, "review complete");
}

#[tokio::test]
async fn controller_forks_and_runs_from_the_boundary() {
    let store = Arc::new(Store::open_in_memory());
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
        .create_run("run-1", &definition, workspace(), "", "", "", "")
        .expect("create");
    let attempt = store
        .claim_ready("run-1", "first", "/tmp/first.jsonl")
        .expect("claim");
    store
        .finish_attempt("run-1", "first", attempt, Ok("first done"))
        .expect("finish");
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new(
        store,
        Catalog::from_definitions(vec![definition]),
        Arc::new(FakeExecutor),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        1,
    );

    let fork = controller.fork("run-1", "first").await.expect("fork");
    let done = controller.wait(&fork.id).await.expect("wait");

    assert_eq!(done.status, RunStatus::Succeeded);
    assert_eq!(done.forked_from_run_id.as_deref(), Some("run-1"));
    assert_eq!(done.steps[0].result, "first done");
    assert_eq!(done.steps[1].result, "second complete");
}

#[tokio::test]
async fn controller_waits_for_parallel_approval_then_resumes_dependents() {
    let store = Arc::new(Store::open_in_memory());
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
        store,
        Catalog::from_definitions(vec![definition]),
        Arc::new(FakeExecutor),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        2,
    );

    let run = controller.start("flow", "").await.expect("start");
    let waiting = controller.wait(&run.id).await.expect("wait");
    assert_eq!(waiting.status, RunStatus::Waiting);
    let request = controller
        .requests(&run.id)
        .expect("requests")
        .pop()
        .expect("request");
    let resumed = controller.respond(&request.id, true).expect("approve");
    assert_eq!(resumed.status, RunStatus::Running);
    let completed = controller.wait(&run.id).await.expect("complete");
    assert_eq!(completed.status, RunStatus::Succeeded);
    assert_eq!(completed.steps[2].result, "deliver complete");
}

#[tokio::test]
async fn failure_waits_for_running_siblings_and_never_schedules_dependents() {
    let stopped = Arc::new(AtomicBool::new(false));
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "fail"
agent = "worker"
prompt = "fail"
[[steps]]
id = "sibling"
agent = "worker"
prompt = "wait"
[[steps]]
id = "downstream"
agent = "worker"
prompt = "never"
needs = ["sibling"]
"#,
    );
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new(
        Arc::new(Store::open_in_memory()),
        Catalog::from_definitions(vec![definition]),
        Arc::new(FailAndCancelExecutor {
            sibling_stopped: Arc::clone(&stopped),
        }),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        2,
    );

    let run = controller.start("flow", "").await.expect("start");
    let failed = controller.wait(&run.id).await.expect("wait");
    assert!(stopped.load(Ordering::SeqCst));
    assert_eq!(failed.status, RunStatus::Failed);
    assert_eq!(failed.steps[2].status, StepStatus::Canceled);
}

#[tokio::test]
async fn resume_schedules_a_committed_ready_step_once() {
    let store = Arc::new(Store::open_in_memory());
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
        .create_run("run-1", &definition, workspace(), "", "", "", "")
        .expect("create");
    let attempt = store
        .claim_ready("run-1", "first", "/tmp/first")
        .expect("claim");
    store
        .finish_attempt("run-1", "first", attempt, Ok("first done"))
        .expect("finish");
    let calls = Arc::new(AtomicUsize::new(0));
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new(
        store,
        Catalog::from_definitions(vec![definition]),
        Arc::new(CountingExecutor(Arc::clone(&calls))),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        1,
    );

    controller.resume("run-1", None).await.expect("resume");
    let done = controller.wait("run-1").await.expect("wait");
    assert_eq!(done.status, RunStatus::Succeeded);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct StopFlagDelivery(AtomicBool);

#[async_trait::async_trait]
impl Delivery for StopFlagDelivery {
    fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    async fn join(&self) {}
}

#[test]
fn register_delivery_drops_stopped_bindings() {
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new(
        Arc::new(Store::open_in_memory()),
        Catalog::from_definitions(Vec::new()),
        Arc::new(FakeExecutor),
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        1,
    );
    let first = Arc::new(StopFlagDelivery(AtomicBool::new(false)));
    let second = Arc::new(StopFlagDelivery(AtomicBool::new(false)));
    controller.register_delivery(first.clone());
    first.stop();
    controller.register_delivery(second.clone());
    let deliveries = controller.deliveries.lock().unwrap();
    let held = deliveries.as_ref().expect("open controller");
    assert_eq!(held.len(), 1);
    assert!(!held[0].is_stopped());
}
