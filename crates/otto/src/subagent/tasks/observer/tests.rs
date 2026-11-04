use super::*;
use otto_core::agent::redactor::Redactor;
use otto_core::model::{
    Block, EffectCertainty, OperationDisposition, OperationId, OperationOutcome,
};
use otto_core::tool::ToolResult;

#[test]
fn observer_streams_cumulative_safe_text_before_usage_or_completion() {
    let tasks = Tasks::new();
    let task = tasks.add(Task::default(), None, None).expect("task");
    tasks.mark_running(&task.id, Utc::now());
    let redactor = Redactor::new(&["fixture-secret".into()]);
    let mut stream = redactor.new_stream();
    for delta in [
        "visible ",
        "fixture-",
        "secret",
        " after text sufficiently long to flush the visible prefix",
    ] {
        tasks.observe(
            &task.id,
            &Event::TextDelta {
                text: stream.write(delta),
            },
        );
    }
    tasks.observe(
        &task.id,
        &Event::ReasoningDelta {
            text: "private reasoning".into(),
        },
    );
    let (snapshot, _) = tasks.observer_tasks();
    assert_eq!(snapshot[0].row.status, "running");
    let text: String = snapshot[0]
        .entries
        .iter()
        .map(|entry| entry.text.as_str())
        .collect();
    assert!(text.contains("visible"));
    assert!(!text.contains("fixture-secret"));
    assert!(!text.contains("private reasoning"));
    tasks.observe(
        &task.id,
        &Event::TextDelta {
            text: " more".into(),
        },
    );
    let (next, _) = tasks.observer_tasks();
    assert!(next[0].entries[0].text.ends_with(" more"));
    assert!(next[0].entries[0].text.starts_with(&text));
}

#[test]
fn observer_tool_results_follow_operation_attempt_and_call_not_name() {
    let mut projection = Projection::default();
    for (operation, args) in [("op1", "one"), ("op2", "two")] {
        projection.event(&Event::ToolCallStarted {
            operation_id: OperationId::new(operation).expect("operation"),
            attempt: 1,
            tool_name: "read".into(),
            tool_call_id: "reused".into(),
            arguments: args.into(),
        });
    }
    projection.event(&Event::ToolCallFinished {
        operation_id: OperationId::new("op1").expect("operation"),
        attempt: 1,
        tool_name: "read".into(),
        tool_call_id: "reused".into(),
        result: ToolResult::error("one failed"),
        outcome: OperationOutcome {
            disposition: OperationDisposition::Error,
            effect_certainty: EffectCertainty::KnownNoEffect,
            stop_reason: None,
        },
    });
    let entries = projection.snapshot();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].tool_args, "one");
    assert_eq!(entries[0].tool_output, "one failed");
    assert!(entries[0].tool_done && entries[0].tool_error);
    assert_eq!(entries[1].tool_args, "two");
    assert!(!entries[1].tool_done);
}

#[test]
fn observer_bounds_entries_bytes_and_registry_capture() {
    let mut projection = Projection::default();
    for index in 0..200 {
        projection.tool(index.to_string(), "read", &"界".repeat(5000));
        projection.tool_result(index.to_string(), "read", &"output".repeat(2000), false);
    }
    assert!(projection.entries.len() <= MAX_ENTRIES);
    assert!(projection.bytes() <= MAX_TAIL_BYTES);
    assert!(projection.snapshot()[0].text.contains("truncated"));
    let tasks = Tasks::new();
    for _ in 0..MAX_TASKS + 4 {
        tasks.add(Task::default(), None, None).expect("task");
    }
    let (snapshot, truncated) = tasks.observer_tasks();
    assert!(truncated);
    assert_eq!(snapshot.len(), MAX_TASKS);
    assert_eq!(
        tasks
            .lock()
            .entries
            .values()
            .filter(|entry| entry.observer.is_some())
            .count(),
        MAX_TASKS
    );
    assert!(snapshot.iter().all(|task| !task.missing));
}

#[test]
fn observer_transcript_omits_prompts_and_reasoning() {
    let messages = vec![
        Message {
            role: Role::User,
            blocks: vec![Block::text("private prompt")],
            ..Default::default()
        },
        Message {
            role: Role::Assistant,
            blocks: vec![Block::text("visible answer")],
            ..Default::default()
        },
    ];
    let projection = Projection::transcript(&messages);
    let entries = projection.snapshot();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "visible answer");
}
