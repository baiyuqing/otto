
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use otto_core::operation::OperationControl;
use tokio::sync::Notify;

fn definition(source: &[u8]) -> Definition {
    let mut definition = parse_definition("flow", source, &|_| true).expect("definition");
    let names: HashSet<String> = definition
        .steps
        .iter()
        .filter(|step| matches!(step.kind, StepKind::Agent | StepKind::Handoff))
        .map(|step| step.agent.clone())
        .collect();
    definition.agents = names
        .into_iter()
        .map(|name| AgentSnapshot {
            name,
            write_policy: SnapshotWritePolicy::ReadOnly,
            ..AgentSnapshot::default()
        })
        .collect();
    definition
}

#[test]
fn definition_accepts_a_bounded_dag() {
    let definition = parse_definition(
        "review-change",
        br#"
version = 1
description = "Review a change"

[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence"

[[steps]]
id = "review"
agent = "reviewer"
prompt = "Review it"
needs = ["research"]

[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship it?"
needs = ["review"]
"#,
        &|name| matches!(name, "researcher" | "reviewer"),
    )
    .expect("definition");

    assert_eq!(definition.name, "review-change");
    assert_eq!(definition.steps[0].kind, StepKind::Agent);
    assert_eq!(definition.steps[2].kind, StepKind::Approval);
    assert_eq!(definition.steps[2].needs, ["review"]);
    assert_eq!(definition.hash.len(), 64);
}

fn agent_snapshot(
    name: &str,
    tools: Option<&[&str]>,
    write_policy: SnapshotWritePolicy,
    write_paths: &[&str],
) -> AgentSnapshot {
    AgentSnapshot {
        name: name.to_string(),
        tools: tools.map(|tools| tools.iter().map(|tool| (*tool).to_string()).collect()),
        write_policy,
        write_paths: write_paths.iter().map(|path| (*path).to_string()).collect(),
        ..AgentSnapshot::default()
    }
}

#[test]
fn write_coordination_rejects_parallel_global_writers() {
    let mut definition = definition(
        br#"
version = 1
[[steps]]
id = "left"
agent = "left"
prompt = "left"
[[steps]]
id = "right"
agent = "right"
prompt = "right"
"#,
    );
    definition.agents = vec![
        agent_snapshot(
            "left",
            Some(&["read", "edit"]),
            SnapshotWritePolicy::SingleWriter,
            &[],
        ),
        agent_snapshot(
            "right",
            Some(&["write"]),
            SnapshotWritePolicy::SingleWriter,
            &[],
        ),
    ];

    let error = validate_write_coordination(&definition).expect_err("parallel writers fail");

    assert!(error.message().contains("may run concurrently"), "{error}");
}

#[test]
fn write_coordination_allows_readonly_parallel_and_serial_writers() {
    let mut serial = definition(
        br#"
version = 1
[[steps]]
id = "left"
agent = "left"
prompt = "left"
[[steps]]
id = "right"
agent = "right"
prompt = "right"
needs = ["left"]
[[steps]]
id = "plan"
agent = "planner"
prompt = "plan"
"#,
    );
    serial.agents = vec![
        agent_snapshot(
            "left",
            Some(&["edit"]),
            SnapshotWritePolicy::SingleWriter,
            &[],
        ),
        agent_snapshot(
            "right",
            Some(&["write"]),
            SnapshotWritePolicy::SingleWriter,
            &[],
        ),
        agent_snapshot(
            "planner",
            Some(&["read", "edit"]),
            SnapshotWritePolicy::ProposeOnly,
            &[],
        ),
    ];

    validate_write_coordination(&serial).expect("serial writers and propose-only planner are ok");
}

#[test]
fn write_coordination_allows_disjoint_owned_paths() {
    let mut definition = definition(
        br#"
version = 1
[[steps]]
id = "core"
agent = "core"
prompt = "core"
[[steps]]
id = "docs"
agent = "docs"
prompt = "docs"
"#,
    );
    definition.agents = vec![
        agent_snapshot(
            "core",
            Some(&["edit"]),
            SnapshotWritePolicy::OwnedPaths,
            &["crates/otto-core/**"],
        ),
        agent_snapshot(
            "docs",
            Some(&["write"]),
            SnapshotWritePolicy::OwnedPaths,
            &["docs/**"],
        ),
    ];

    validate_write_coordination(&definition).expect("disjoint owned paths are ok");
}

#[test]
fn write_coordination_rejects_overlapping_owned_paths() {
    let mut definition = definition(
        br#"
version = 1
[[steps]]
id = "a"
agent = "a"
prompt = "a"
[[steps]]
id = "b"
agent = "b"
prompt = "b"
"#,
    );
    definition.agents = vec![
        agent_snapshot(
            "a",
            Some(&["edit"]),
            SnapshotWritePolicy::OwnedPaths,
            &["crates/otto/**"],
        ),
        agent_snapshot(
            "b",
            Some(&["write"]),
            SnapshotWritePolicy::OwnedPaths,
            &["crates/otto/src/**"],
        ),
    ];

    let error = validate_write_coordination(&definition).expect_err("overlap fails");

    assert!(error.message().contains("may run concurrently"), "{error}");
}

#[test]
fn definition_accepts_handoff_to_a_known_agent() {
    let definition = parse_definition(
        "handoff-change",
        br#"
version = 1
[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence"
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over and review it"
needs = ["research"]
"#,
        &|name| matches!(name, "researcher" | "reviewer"),
    )
    .expect("definition");

    assert_eq!(definition.steps[1].kind, StepKind::Handoff);
    assert_eq!(definition.steps[1].agent, "reviewer");
    assert_eq!(definition.steps[1].needs, ["research"]);
}

#[test]
fn definition_rejects_a_root_handoff() {
    let error = parse_definition(
        "handoff",
        br#"
version = 1
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over"
"#,
        &|_| true,
    )
    .expect_err("root handoff");

    assert_eq!(
        error.message(),
        "workflow handoff handoff step \"review\" requires a dependency"
    );
}

#[test]
fn definition_rejects_cycles_before_execution() {
    let error = parse_definition(
        "cycle",
        br#"
version = 1
[[steps]]
id = "one"
agent = "worker"
prompt = "one"
needs = ["two"]
[[steps]]
id = "two"
agent = "worker"
prompt = "two"
needs = ["one"]
"#,
        &|_| true,
    )
    .expect_err("cycle");

    assert_eq!(
        error.message(),
        "workflow cycle contains a dependency cycle"
    );
}

#[test]
fn definition_read_refuses_a_symlink() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("directory");
    let source = directory.path().join("source.toml");
    let link = directory.path().join("link.toml");
    std::fs::write(&source, "version = 1").expect("write");
    symlink(&source, &link).expect("symlink");
    assert!(read_definition_file(&link).is_err());
}

#[test]
fn store_commits_dependencies_and_terminal_run() {
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
    let run = store
        .create_run(
            "run-1",
            &definition,
            "/workspace",
            "default",
            "openai-compatible",
            "model",
            "input",
        )
        .expect("create");
    assert_eq!(run.steps[0].status, StepStatus::Ready);
    assert_eq!(run.steps[1].status, StepStatus::Pending);

    let attempt = store
        .claim_ready("run-1", "first", "/tmp/first.jsonl")
        .expect("claim");
    store
        .finish_attempt("run-1", "first", attempt, Ok("result one"))
        .expect("finish first");
    let run = store.get_run("run-1").expect("get").expect("run");
    assert_eq!(run.steps[1].status, StepStatus::Ready);

    let attempt = store
        .claim_ready("run-1", "second", "/tmp/second.jsonl")
        .expect("claim second");
    store
        .finish_attempt("run-1", "second", attempt, Ok("done"))
        .expect("finish second");
    assert_eq!(
        store.get_run("run-1").expect("get").expect("run").status,
        RunStatus::Succeeded
    );
}

#[test]
fn fork_copies_the_committed_event_boundary_state() {
    let store = Store::open_in_memory();
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "left"
agent = "worker"
prompt = "left"
[[steps]]
id = "right"
agent = "worker"
prompt = "right"
[[steps]]
id = "join"
agent = "worker"
prompt = "join"
needs = ["left", "right"]
"#,
    );
    store
        .create_run("run-1", &definition, "/w", "p", "provider", "m", "input")
        .expect("create");
    let right = store
        .claim_ready("run-1", "right", "/tmp/right.jsonl")
        .expect("claim right");
    store
        .finish_attempt("run-1", "right", right, Ok("right done"))
        .expect("finish right");
    let left = store
        .claim_ready("run-1", "left", "/tmp/left.jsonl")
        .expect("claim left");
    store
        .finish_attempt("run-1", "left", left, Ok("left done"))
        .expect("finish left");

    let fork = store
        .fork_run("fork-1", "run-1", "left")
        .expect("fork after left");

    assert_eq!(fork.status, RunStatus::Running);
    assert_eq!(fork.forked_from_run_id.as_deref(), Some("run-1"));
    assert_eq!(fork.forked_from_step_id.as_deref(), Some("left"));
    assert!(fork.forked_from_event_seq.is_some());
    assert_eq!(fork.steps[0].status, StepStatus::Succeeded);
    assert_eq!(fork.steps[0].result, "left done");
    assert_eq!(fork.steps[0].source_run_id.as_deref(), Some("run-1"));
    assert_eq!(fork.steps[0].source_step_id.as_deref(), Some("left"));
    assert_eq!(fork.steps[0].source_attempt, Some(1));
    assert_eq!(fork.steps[1].status, StepStatus::Succeeded);
    assert_eq!(fork.steps[2].status, StepStatus::Ready);
}

#[test]
fn fork_of_a_terminal_boundary_is_terminal() {
    let store = Store::open_in_memory();
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
    );
    store
        .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
        .expect("create");
    let attempt = store
        .claim_ready("run-1", "work", "/tmp/work.jsonl")
        .expect("claim");
    store
        .finish_attempt("run-1", "work", attempt, Ok("done"))
        .expect("finish");

    let fork = store
        .fork_run("fork-1", "run-1", "work")
        .expect("fork terminal");

    assert_eq!(fork.status, RunStatus::Succeeded);
    assert_eq!(fork.steps[0].status, StepStatus::Succeeded);
}

#[test]
fn fork_requires_a_committed_success_boundary() {
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

    assert_eq!(
        store
            .fork_run("fork-1", "run-1", "second")
            .expect_err("pending step")
            .message(),
        "workflow fork boundary is not a committed success"
    );
}

#[test]
fn recovery_pauses_running_attempt_without_retrying_it() {
    let store = Store::open_in_memory();
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
    );
    store
        .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
        .expect("create");
    store
        .claim_ready("run-1", "work", "/tmp/work.jsonl")
        .expect("claim");
    store
        .create_run("run-2", &definition, "/other", "p", "provider", "m", "")
        .expect("other create");
    store
        .claim_ready("run-2", "work", "/tmp/other.jsonl")
        .expect("other claim");

    assert_eq!(store.recover("/w").expect("recover"), 1);
    let run = store.get_run("run-1").expect("get").expect("run");
    assert_eq!(run.status, RunStatus::Paused);
    assert_eq!(run.steps[0].status, StepStatus::Interrupted);
    assert_eq!(run.steps[0].attempt, 1);
    assert_eq!(
        store.get_run("run-2").expect("get").expect("other").status,
        RunStatus::Running
    );
}

#[test]
fn manual_retry_creates_a_new_attempt_and_keeps_the_old_transcript() {
    let store = Store::open_in_memory();
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "work"
agent = "worker"
prompt = "work"
"#,
    );
    store
        .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
        .expect("create");
    store
        .claim_ready("run-1", "work", "/tmp/attempt-1.jsonl")
        .expect("first");
    store.recover("/w").expect("recover");
    store.retry("run-1", "work").expect("retry");
    assert_eq!(
        store
            .claim_ready("run-1", "work", "/tmp/attempt-2.jsonl")
            .expect("second"),
        2
    );
    assert_eq!(
        store.get_run("run-1").expect("run").unwrap().steps[0].transcript_path,
        "/tmp/attempt-2.jsonl"
    );
}

#[test]
fn approval_response_is_durable_and_idempotent() {
    let store = Store::open_in_memory();
    let definition = definition(
        br#"
version = 1
[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship?"
"#,
    );
    store
        .create_run("run-1", &definition, "/w", "p", "provider", "m", "")
        .expect("create");
    let request = store
        .request_approval("request-1", "run-1", "approve")
        .expect("request");
    assert_eq!(request.status, "pending");
    assert_eq!(
        store.get_run("run-1").expect("get").expect("run").status,
        RunStatus::Waiting
    );

    store.respond("request-1", true).expect("approve");
    store.respond("request-1", true).expect("same response");
    assert_eq!(
        store.get_run("run-1").expect("get").expect("run").status,
        RunStatus::Succeeded
    );
    assert_eq!(
        store
            .respond("request-1", false)
            .expect_err("conflict")
            .message(),
        "approval request already answered differently"
    );
}

#[test]
fn handoff_prompt_marks_the_transfer() {
    let store = Store::open_in_memory();
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
    store
        .create_run("run-1", &definition, "/w", "p", "provider", "m", "input")
        .expect("create");
    let attempt = store
        .claim_ready("run-1", "research", "/tmp/research.jsonl")
        .expect("claim");
    store
        .finish_attempt("run-1", "research", attempt, Ok("facts"))
        .expect("finish");
    let run = store.get_run("run-1").expect("get").expect("run");
    let prompt = attempt_prompt(&run, &run.steps[1]).expect("prompt");

    assert!(prompt.contains("## Handoff"));
    assert!(prompt.contains("You are receiving control from: research"));
}

struct FakeExecutor;

#[async_trait::async_trait]
impl Executor for FakeExecutor {
    async fn execute(&self, attempt: Attempt, _control: &Control) -> Result<String, String> {
        Ok(format!("{} complete", attempt.step_id))
    }
}

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
        "/workspace".into(),
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

struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
        "/workspace".into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        2,
        DeadlinePolicy::default(),
        Arc::new(Logger::new(Box::new(CapturedLog(Arc::clone(&bytes))))),
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
        "/workspace".into(),
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
        .create_run("run-1", &definition, "/workspace", "", "", "", "")
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
        "/workspace".into(),
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
        "/workspace".into(),
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

struct FailAndCancelExecutor {
    sibling_stopped: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Executor for FailAndCancelExecutor {
    async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
        if attempt.step_id == "fail" {
            return Err("failed".to_string());
        }
        control.cancellation_token().cancelled().await;
        self.sibling_stopped.store(true, Ordering::SeqCst);
        Err("context canceled".to_string())
    }
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
        "/workspace".into(),
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

struct CountingExecutor(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, attempt: Attempt, _control: &Control) -> Result<String, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(format!("{} done", attempt.step_id))
    }
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
        .create_run("run-1", &definition, "/workspace", "", "", "", "")
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
        "/workspace".into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        1,
    );

    controller.resume("run-1", None).await.expect("resume");
    let done = controller.wait("run-1").await.expect("wait");
    assert_eq!(done.status, RunStatus::Succeeded);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct BoundaryExecutor(Arc<Notify>);

#[async_trait::async_trait]
impl Executor for BoundaryExecutor {
    async fn execute(&self, _attempt: Attempt, _control: &Control) -> Result<String, String> {
        self.0.notify_one();
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok("boundary".into())
    }
}

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
        "/workspace".into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        1,
        DeadlinePolicy {
            step_timeout: Some(Duration::from_secs(1)),
            cancellation_grace: Duration::from_secs(1),
        },
        Arc::new(Logger::discard()),
    );
    let run = controller.start("flow", "").await.expect("start");
    started.notified().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    let done = controller.wait(&run.id).await.expect("wait");
    assert_eq!(done.status, RunStatus::Succeeded);
    assert_eq!(done.steps[0].result, "boundary");
}

struct TimeoutBatchExecutor {
    sibling_started: Arc<Notify>,
    sibling_release: CancellationToken,
}

#[async_trait::async_trait]
impl Executor for TimeoutBatchExecutor {
    async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
        if attempt.step_id == "timeout" {
            control.cancellation_token().cancelled().await;
            return Err("stopped".into());
        }
        self.sibling_started.notify_one();
        self.sibling_release.cancelled().await;
        Ok("settled".into())
    }
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
        "/workspace".into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        2,
        DeadlinePolicy {
            step_timeout: Some(Duration::from_secs(1)),
            cancellation_grace: Duration::from_millis(100),
        },
        Arc::new(Logger::discard()),
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

struct BlockingExecutor {
    started: Arc<Notify>,
}

#[async_trait::async_trait]
impl Executor for BlockingExecutor {
    async fn execute(&self, _attempt: Attempt, control: &Control) -> Result<String, String> {
        self.started.notify_one();
        control.cancellation_token().cancelled().await;
        Err("context canceled".to_string())
    }
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
        "/workspace".into(),
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
        "/workspace".into(),
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

const TWO_STEP: &[u8] = br#"
version = 1
[[steps]]
id = "a"
agent = "worker"
prompt = "a"
[[steps]]
id = "b"
agent = "worker"
prompt = "b"
"#;

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

struct GateExecutor {
    started: Arc<AtomicBool>,
    canceled: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Executor for GateExecutor {
    async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
        if attempt.step_id == "work" {
            self.started.store(true, Ordering::SeqCst);
            control.cancellation_token().cancelled().await;
            self.canceled.store(true, Ordering::SeqCst);
            return Err("context canceled".to_string());
        }
        Ok(format!("{} complete", attempt.step_id))
    }
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
        "/workspace".into(),
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
