//! Durable run store: dependencies, forks, retries and approvals.

use super::*;

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
