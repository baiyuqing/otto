//! `/task`, `/agents` and `/timers` command tests.

use super::*;

#[tokio::test]
async fn the_task_commands_report_an_empty_registry_and_reject_bad_arguments() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;

    let (stdout, stderr) = session(
        "/task\n/tasks\n/task cancel\n/task nope\n/exit\n",
        &controller,
    )
    .await;

    assert!(
        controller.subagent_tasks().is_some(),
        "sub-agents must be wired"
    );
    assert_eq!(
        stdout.matches("no tasks in this session").count(),
        2,
        "the canonical command and compatibility alias must both list: {stdout}"
    );
    assert!(
        stderr.contains("usage: /task [<id|name> | cancel <id|name>]"),
        "{stderr}"
    );
    assert!(stderr.contains("unknown task: nope"), "{stderr}");
}

/// A controller whose builder carries a task recorder, in place of the
/// default `None` `fixtures::builder` sets.
async fn controller_with_task_recorder(
    workspace: &Path,
    sessions: &Path,
    store: Arc<record::Store>,
) -> Controller {
    let mut builder = fixtures::builder(workspace, sessions);
    builder.shared_mut().task_recorder = Some(store);
    let runtime = fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    Controller::new(builder, true, session, runner, info)
}

#[tokio::test]
async fn agents_lists_recorded_tasks_across_sessions_and_reports_when_empty() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let store = Arc::new(record::Store::open_in_memory().expect("store"));
    let controller =
        controller_with_task_recorder(workspace.path(), sessions.path(), Arc::clone(&store)).await;

    let (stdout, _) = session("/agents\n/exit\n", &controller).await;
    assert!(stdout.contains("no recorded tasks"), "{stdout}");

    let context = record::TaskContext {
        parent_session: "other-session".into(),
        parent_session_path: "/sessions/other.jsonl".into(),
        workspace: "/work".into(),
        pid: 4_294_967_294,
        process_started_at: "2026-09-25T10:00:00Z".into(),
    };
    record::Recorder::upsert(
        &*store,
        &context,
        &Task {
            id: "t9".into(),
            description: "review the diff".into(),
            status: TaskStatus::Succeeded,
            created_at: Some(Utc::now()),
            ..Task::default()
        },
    );

    let (stdout, _) = session("/agents\n/exit\n", &controller).await;
    assert!(stdout.contains("other-session"), "{stdout}");
    assert!(stdout.contains("t9"), "{stdout}");
    assert!(stdout.contains("review the diff"), "{stdout}");
}

#[tokio::test]
async fn timers_lists_cancels_and_reports_bad_input() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;
    let reminders = controller.reminders().expect("timer registry");
    reminders
        .schedule(std::time::Duration::from_secs(90), "check the build".into())
        .expect("schedule");

    let (stdout, stderr) = session(
        "/timers
/timers cancel r1
/timers
/timers cancel r1
/timers nope
/exit
",
        &controller,
    )
    .await;

    assert!(stdout.contains("check the build"), "{stdout}");
    assert!(stdout.contains("in 1m"), "{stdout}");
    assert!(stdout.contains("canceled r1"), "{stdout}");
    assert!(stdout.contains("no timers in this session"), "{stdout}");
    assert!(stderr.contains("unknown timer: r1"), "{stderr}");
    assert!(stderr.contains("usage: /timers [cancel <id>]"), "{stderr}");
    assert!(reminders.list().is_empty());
}

/// Renders `/task <id>` detail without a provider: the registry is written
/// directly.
#[tokio::test]
async fn task_detail_prints_the_line_model_and_result() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;
    let tasks = controller.subagent_tasks().expect("sub-agent tasks");
    let task = tasks
        .add(
            Task {
                description: "check the tests".into(),
                model: "gpt-alpha".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("add task");
    tasks.finish(&task.id, TaskStatus::Succeeded, Utc::now(), "done", "");
    tasks.set_session_path(&task.id, "/sessions/p/t1-c.jsonl");

    let (stdout, _) = session(&format!("/tasks\n/task {}\n/exit\n", task.id), &controller).await;

    assert!(stdout.contains(&task.id), "{stdout}");
    assert!(stdout.contains("model: gpt-alpha"), "{stdout}");
    assert!(
        stdout.contains("transcript: /sessions/p/t1-c.jsonl"),
        "{stdout}"
    );
    assert!(stdout.contains("result: done"), "{stdout}");
}

#[tokio::test]
async fn cancelling_a_queued_task_reports_it() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;
    let tasks = controller.subagent_tasks().expect("sub-agent tasks");
    let task = tasks
        .add(
            Task {
                description: "cancel me".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("add task");

    let (stdout, stderr) = session(
        &format!("/task cancel {}\n/task cancel missing\n/exit\n", task.id),
        &controller,
    )
    .await;

    assert!(
        stdout.contains(&format!("canceled {}", task.id)),
        "{stdout}"
    );
    assert!(!stderr.is_empty(), "an unknown id must report an error");
}

/// The column spellings themselves are `subagent::format`'s tests; what the
/// REPL owns is one line per task, in registry order.
#[tokio::test]
async fn the_task_list_prints_one_line_per_task() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;
    let tasks = controller.subagent_tasks().expect("sub-agent tasks");

    let started = Utc::now() - chrono::TimeDelta::seconds(42);
    let reviewed = tasks
        .add(
            Task {
                description: "review the diff".into(),
                prompt: "review please".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("add task");
    tasks.mark_running(&reviewed.id, started);
    for _ in 0..7 {
        tasks.record_tool_call(&reviewed.id, "read");
    }
    tasks.record_provider_step(
        &reviewed.id,
        otto_core::model::Usage {
            input_tokens: 10000,
            output_tokens: 2310,
            ..otto_core::model::Usage::default()
        },
        "",
        true,
        true,
    );
    tasks.finish(
        &reviewed.id,
        TaskStatus::Succeeded,
        started + chrono::TimeDelta::seconds(42),
        "",
        "",
    );

    // Canceled while still queued: no start time, so the elapsed column
    // must read "0s" rather than the span from the zero instant.
    let aborted = tasks
        .add(
            Task {
                description: "abort early".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("add task");
    tasks.finish(&aborted.id, TaskStatus::Canceled, Utc::now(), "", "");

    let (stdout, stderr) = session("/tasks\n/exit\n", &controller).await;

    for want in [
        "succeeded",
        "42s",
        "7 tools",
        "12,310 tokens",
        "review the diff",
    ] {
        assert!(stdout.contains(want), "stdout missing {want:?}: {stdout}");
    }
    let line = stdout
        .lines()
        .find(|line| line.starts_with(&aborted.id))
        .unwrap_or_else(|| panic!("no line for {}: {stdout}", aborted.id));
    assert!(line.contains("canceled"), "{line:?}");
    assert!(line.contains("0s"), "{line:?}");
    assert_eq!(stderr, "");
}

/// The step spellings are `subagent::format`'s tests; what the REPL owns is
/// the order of the task line, the model line, the steps and the result.
#[tokio::test]
async fn task_detail_prints_the_child_steps_between_the_model_and_the_result() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;
    let tasks = controller.subagent_tasks().expect("sub-agent tasks");

    let history = vec![
        fixtures::user("review please"),
        otto_core::model::Message {
            role: otto_core::model::Role::Assistant,
            blocks: vec![otto_core::model::Block {
                block_type: otto_core::model::BlockType::ToolCall,
                tool_name: "read".into(),
                arguments: Some(
                    serde_json::value::RawValue::from_string(r#"{"path":"main.go"}"#.into())
                        .expect("valid JSON"),
                ),
                ..otto_core::model::Block::default()
            }],
            ..otto_core::model::Message::default()
        },
        otto_core::model::Message {
            role: otto_core::model::Role::Tool,
            blocks: vec![otto_core::model::Block {
                block_type: otto_core::model::BlockType::ToolResult,
                text: "package main".into(),
                ..otto_core::model::Block::default()
            }],
            ..otto_core::model::Message::default()
        },
        otto_core::model::Message {
            role: otto_core::model::Role::Assistant,
            blocks: vec![otto_core::model::Block {
                block_type: otto_core::model::BlockType::Text,
                text: "looks fine".into(),
                ..otto_core::model::Block::default()
            }],
            ..otto_core::model::Message::default()
        },
    ];
    let task = tasks
        .add(
            Task {
                description: "review".into(),
                model: "gpt-test-model".into(),
                ..Task::default()
            },
            None,
            Some(Arc::new(move || history.clone())),
        )
        .expect("add task");
    tasks.finish(
        &task.id,
        TaskStatus::Succeeded,
        Utc::now(),
        "no issues found",
        "",
    );

    let (stdout, _) = session(&format!("/task {}\n/exit\n", task.id), &controller).await;

    let lines: Vec<&str> = stdout.lines().collect();
    let model_index = lines
        .iter()
        .position(|line| *line == "model: gpt-test-model")
        .unwrap_or_else(|| panic!("no model line: {stdout}"));
    assert!(model_index > 0, "{stdout}");
    assert!(lines[model_index - 1].contains(&task.id), "{stdout}");
    assert!(
        stdout.contains("\u{2192} read {\"path\":\"main.go\"}"),
        "{stdout}"
    );
    assert!(stdout.contains("assistant: looks fine"), "{stdout}");
    assert!(stdout.contains("result: no issues found"), "{stdout}");
    assert!(
        !stdout.contains("review please"),
        "the delegated prompt is not a step: {stdout}"
    );
    assert!(
        !stdout.contains("package main"),
        "tool results are not steps: {stdout}"
    );
}

/// Both `/task` and `/task cancel` resolve a task name as well as an id.
#[tokio::test]
async fn the_task_commands_resolve_a_task_name() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;
    let tasks = controller.subagent_tasks().expect("sub-agent tasks");
    let canceled = Arc::new(CancellationToken::new());
    let task = tasks
        .add(
            Task {
                name: "lint-check".into(),
                description: "review".into(),
                ..Task::default()
            },
            Some((*canceled).clone()),
            None,
        )
        .expect("add task");

    let (stdout, stderr) = session(
        "/task lint-check\n/task cancel lint-check\n/exit\n",
        &controller,
    )
    .await;

    assert!(stdout.contains(&task.id), "{stdout}");
    assert!(stdout.contains("canceled lint-check"), "{stdout}");
    assert!(canceled.is_cancelled(), "the cancel token must fire");
    assert_eq!(stderr, "");
}
