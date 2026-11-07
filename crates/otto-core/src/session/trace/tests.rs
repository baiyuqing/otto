use chrono::{DateTime, SecondsFormat, Utc};

use super::*;
use crate::model::{Block, ContextMetadata, EffectCertainty, OperationId, OperationOutcome, Role};
use crate::session::operation::encode_operation_fact;
use crate::session::pi::{PiCompaction, PiCustom};
use crate::session::turn::encode_turn_record;

const BASE: i64 = 1_800_000_000_000;

fn time(offset: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(BASE + offset).expect("in range")
}

fn custom(at: i64, custom_type: &str, data: &str) -> PiEntry {
    let mut entry = PiEntry::new(
        "custom",
        &format!("e{at}"),
        None,
        &time(at).to_rfc3339_opts(SecondsFormat::Millis, true),
    );
    entry.custom = Some(PiCustom {
        custom_type: custom_type.into(),
        data: Some(RawValue::from_string(data.into()).expect("json")),
    });
    entry
}

fn turn_start(at: i64, id: &str, trigger: TurnTrigger) -> PiEntry {
    let record = TurnRecord::started(id, trigger, 1, None);
    custom(at, TURN_CUSTOM_TYPE, &encode_turn_record(&record))
}

fn turn_end(at: i64, id: &str, outcome: TurnOutcome) -> PiEntry {
    let record = TurnRecord::ended(id, outcome);
    custom(at, TURN_CUSTOM_TYPE, &encode_turn_record(&record))
}

fn tool_attempt(at: i64, op: &str, call: &str, name: &str) -> PiEntry {
    let fact = OperationFact::attempt(OperationId::new(op).unwrap(), 1, call, name);
    custom(
        at,
        OPERATION_CUSTOM_TYPE,
        &encode_operation_fact(&fact).unwrap(),
    )
}

fn tool_terminal(at: i64, op: &str, call: &str, name: &str, ok: bool) -> PiEntry {
    let outcome = OperationOutcome {
        disposition: if ok {
            OperationDisposition::Succeeded
        } else {
            OperationDisposition::Error
        },
        effect_certainty: EffectCertainty::Completed,
        stop_reason: None,
    };
    let fact = OperationFact::terminal(OperationId::new(op).unwrap(), 1, call, name, outcome);
    custom(
        at,
        OPERATION_CUSTOM_TYPE,
        &encode_operation_fact(&fact).unwrap(),
    )
}

fn model_call(at: i64, op: &str, kind: ProviderCallKind, started: i64, ms: u64) -> PiEntry {
    let record = ProviderCallRecord {
        schema_version: 1,
        operation_id: op.into(),
        kind,
        started_at: BASE + started,
        duration_ms: ms,
        first_output_ms: Some(5),
        attempts: 2,
        retries: vec![ProviderRetryRecord {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 10,
            reason: "timeout".into(),
            interrupted_output: false,
            at_ms: 3,
        }],
        outcome: OperationOutcome {
            disposition: OperationDisposition::Succeeded,
            effect_certainty: EffectCertainty::Completed,
            stop_reason: None,
        },
        finish_reason: Some(FinishReason::ToolCalls),
        usage: Some(Usage {
            input_tokens: 100,
            output_tokens: 7,
            cached_input_tokens: 0,
            ..Default::default()
        }),
        request: Some(RequestEstimate {
            system_prompt_tokens: 11,
            tool_tokens: 22,
            message_tokens: 33,
            memory_tokens: 44,
            memory_record_ids: vec!["m1".into(), "m2".into()],
        }),
    };
    custom(
        at,
        PROVIDER_CALL_CUSTOM_TYPE,
        &serde_json::to_string(&record).unwrap(),
    )
}

fn message(at: i64, role: Role, blocks: Vec<Block>) -> Message {
    Message {
        role,
        blocks,
        created_at: time(at),
        ..Message::default()
    }
}

fn text(at: i64, role: Role, text: &str) -> Message {
    message(at, role, vec![Block::text(text)])
}

fn tool_call(call: &str, name: &str, arguments: &str) -> Block {
    Block {
        block_type: BlockType::ToolCall,
        tool_call_id: call.into(),
        tool_name: name.into(),
        arguments: Some(RawValue::from_string(arguments.into()).unwrap()),
        ..Block::default()
    }
}

fn tool_result(call: &str, text: &str) -> Block {
    Block {
        block_type: BlockType::ToolResult,
        tool_call_id: call.into(),
        text: text.into(),
        ..Block::default()
    }
}

fn marker(at: i64) -> PiEntry {
    PiEntry::new(
        "message",
        &format!("m{at}"),
        None,
        &time(at).to_rfc3339_opts(SecondsFormat::Millis, true),
    )
}

/// One finished user turn: two model requests around a bash call, then a
/// compaction.
fn finished_turn() -> (Vec<PiEntry>, Vec<Message>) {
    let mut compaction = PiEntry::new(
        "compaction",
        "c1",
        None,
        &time(560).to_rfc3339_opts(SecondsFormat::Millis, true),
    );
    compaction.compaction = Some(Box::new(PiCompaction {
        summary: "the summary".into(),
        ..PiCompaction::default()
    }));
    let entries = vec![
        turn_start(0, "T1", TurnTrigger::User),
        model_call(100, "op1", ProviderCallKind::Ordinary, 10, 90),
        tool_attempt(110, "op2", "c1", "bash"),
        tool_terminal(400, "op2", "c1", "bash", true),
        model_call(500, "op3", ProviderCallKind::Ordinary, 410, 90),
        model_call(550, "op4", ProviderCallKind::LocalCompaction, 520, 30),
        compaction,
        turn_end(600, "T1", TurnOutcome::Completed),
    ];
    let messages = vec![
        text(5, Role::User, "run the tests"),
        message(
            100,
            Role::Assistant,
            vec![tool_call(
                "c1",
                "bash",
                r#"{"command":"cargo   test\n --all","timeout":5}"#,
            )],
        ),
        message(400, Role::Tool, vec![tool_result("c1", "all passed")]),
        text(500, Role::Assistant, "done"),
    ];
    (entries, messages)
}

fn source<'a>(entries: &'a [PiEntry], messages: &'a [Message], active: bool) -> TraceSource<'a> {
    TraceSource {
        entries,
        messages,
        active,
    }
}

fn ids(spans: &[Span]) -> Vec<&str> {
    spans.iter().map(|span| span.id.as_str()).collect()
}

#[test]
fn a_finished_turn_nests_provider_tool_and_compaction_spans_in_time_order() {
    let (entries, messages) = finished_turn();
    let trace = build("s1", &source(&entries, &messages, false), &[]);
    assert_eq!(trace.session_id, "s1");
    assert_eq!(trace.turns.len(), 1);
    let turn = &trace.turns[0];
    assert_eq!(
        (turn.id.as_str(), turn.label.as_str(), turn.status),
        ("T1", "user", SpanStatus::Ok)
    );
    assert_eq!((turn.started_at, turn.ended_at), (BASE, Some(BASE + 600)));
    assert_eq!(ids(&turn.children), ["op1", "op2", "op3", "op4"]);
    let labels: Vec<_> = turn.children.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "model #1",
            "bash cargo test --all",
            "model #2",
            "compaction"
        ]
    );
    let kinds: Vec<_> = turn.children.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        [
            SpanKind::Provider,
            SpanKind::Tool,
            SpanKind::Provider,
            SpanKind::Compaction
        ]
    );
    let first = &turn.children[0];
    assert_eq!(
        (first.started_at, first.ended_at),
        (BASE + 10, Some(BASE + 100))
    );
    let timing = first.provider.as_ref().expect("timing");
    assert_eq!((timing.first_output_ms, timing.attempts), (Some(5), 2));
    assert_eq!(timing.retries[0].at_ms, 3);
    assert_eq!(timing.usage.map(|usage| usage.input_tokens), Some(100));
    assert_eq!(timing.finish_reason, Some(FinishReason::ToolCalls));
    let tool = &turn.children[1];
    assert_eq!(
        (tool.started_at, tool.ended_at),
        (BASE + 110, Some(BASE + 400))
    );
    assert_eq!(
        (tool.tool_call_id.as_str(), tool.status),
        ("c1", SpanStatus::Ok)
    );
    assert!(!first.estimated);
}

#[test]
fn a_failed_tool_terminal_marks_the_span_as_an_error() {
    let entries = vec![
        turn_start(0, "T1", TurnTrigger::User),
        tool_attempt(10, "op1", "c1", "bash"),
        tool_terminal(20, "op1", "c1", "bash", false),
        turn_end(30, "T1", TurnOutcome::Failed),
    ];
    let trace = build("s", &source(&entries, &[], false), &[]);
    assert_eq!(trace.turns[0].status, SpanStatus::Error);
    assert_eq!(trace.turns[0].children[0].status, SpanStatus::Error);
}

#[test]
fn an_open_active_turn_gets_a_running_model_span_unless_a_tool_is_open() {
    let mut entries = vec![
        turn_start(0, "T1", TurnTrigger::User),
        model_call(100, "op1", ProviderCallKind::Ordinary, 10, 90),
        tool_attempt(110, "op2", "c1", "bash"),
        tool_terminal(300, "op2", "c1", "bash", true),
    ];
    let trace = build("s", &source(&entries, &[], true), &[]);
    let turn = &trace.turns[0];
    assert_eq!((turn.status, turn.ended_at), (SpanStatus::Running, None));
    let pending = turn.children.last().expect("pending span");
    assert_eq!(pending.id, "T1:pending");
    assert_eq!(pending.label, "model #2");
    assert_eq!(
        (pending.started_at, pending.ended_at, pending.status),
        (BASE + 300, None, SpanStatus::Running)
    );

    // With the tool still open there is no pending model span.
    entries.pop();
    let trace = build("s", &source(&entries, &[], true), &[]);
    let turn = &trace.turns[0];
    assert_eq!(ids(&turn.children), ["op1", "op2"]);
    assert_eq!(turn.children[1].status, SpanStatus::Running);
    assert_eq!(turn.children[1].ended_at, None);
}

#[test]
fn an_open_turn_in_an_idle_session_is_interrupted() {
    let entries = vec![
        turn_start(0, "T1", TurnTrigger::User),
        tool_attempt(110, "op2", "c1", "bash"),
    ];
    let trace = build("s", &source(&entries, &[], false), &[]);
    let turn = &trace.turns[0];
    assert_eq!(
        (turn.status, turn.ended_at),
        (SpanStatus::Interrupted, None)
    );
    assert_eq!(turn.children.len(), 1);
    assert_eq!(turn.children[0].status, SpanStatus::Interrupted);
}

#[test]
fn a_wake_turn_lists_the_tasks_whose_notifications_it_delivered() {
    let entries = vec![
        turn_start(0, "T1", TurnTrigger::User),
        turn_end(100, "T1", TurnOutcome::Completed),
        turn_start(1000, "T2", TurnTrigger::Wake),
        turn_end(1100, "T2", TurnOutcome::Completed),
    ];
    let notification = |at: i64, task: &str| Message {
        context_metadata: Some(ContextMetadata {
            task_id: task.into(),
        }),
        ..text(at, Role::Context, "finished")
    };
    let messages = vec![notification(50, "t9"), notification(1005, "t3")];
    let trace = build("s", &source(&entries, &messages, false), &[]);
    assert_eq!(trace.turns[1].label, "wake");
    assert_eq!(trace.turns[1].triggered_by, ["t3"]);
    assert_eq!(trace.turns[0].triggered_by, ["t9"]);
}

fn child_entries(task: &str, extra_spec: &str, at: i64) -> Vec<PiEntry> {
    let spec = format!(
        r#"{{"id":"{task}","name":"review","description":"review the diff","prompt":"look at it"{extra_spec}}}"#
    );
    vec![
        custom(at, TASK_SPEC_CUSTOM_TYPE, &spec),
        turn_start(at + 10, "CT", TurnTrigger::User),
        model_call(at + 70, "op1", ProviderCallKind::Ordinary, at + 20, 50),
        tool_attempt(at + 80, "op2", "k1", "bash"),
        tool_terminal(at + 150, "op2", "k1", "bash", true),
        turn_end(at + 160, "CT", TurnOutcome::Completed),
    ]
}

#[test]
fn a_child_is_nested_by_parent_tool_call_id_or_else_by_time() {
    let entries = vec![
        turn_start(0, "T1", TurnTrigger::User),
        model_call(90, "p1", ProviderCallKind::Ordinary, 20, 70),
        tool_attempt(100, "op1", "c7", "agent"),
        tool_terminal(900, "op1", "c7", "agent", true),
        turn_end(1000, "T1", TurnOutcome::Completed),
    ];
    let messages = vec![message(
        100,
        Role::Assistant,
        vec![tool_call("c7", "agent", r#"{"prompt":"review"}"#)],
    )];
    let linked = child_entries("t1", r#","parentToolCallId":"c7""#, 150);
    let linked_messages = vec![
        text(180, Role::Assistant, "first"),
        text(300, Role::Assistant, "the review"),
    ];
    let unlinked = child_entries("t2", "", 500);
    let trace = build(
        "s",
        &source(&entries, &messages, false),
        &[
            source(&linked, &linked_messages, false),
            source(&unlinked, &[], true),
        ],
    );
    let turn = &trace.turns[0];
    assert_eq!(ids(&turn.children), ["p1", "op1", "t2"]);

    let agent = &turn.children[1].children[0];
    assert_eq!(agent.kind, SpanKind::Subagent);
    assert_eq!(agent.id, "t1");
    assert_eq!(agent.label, r#"agent t1 "review the diff""#);
    assert_eq!(agent.task_id, "t1");
    assert_eq!(
        (agent.started_at, agent.ended_at),
        (BASE + 150, Some(BASE + 310))
    );
    assert_eq!(agent.status, SpanStatus::Ok);
    // Child ids are prefixed, so equal operation ids do not collide.
    assert_eq!(ids(&agent.children), ["t1:op1", "t1:op2"]);
    assert_eq!(agent.children[0].label, "model #1");
    assert!(agent.children.iter().all(|span| span.task_id == "t1"));

    let by_time = &turn.children[2];
    assert_eq!(
        (by_time.id.as_str(), by_time.status),
        ("t2", SpanStatus::Running)
    );
    assert_eq!(by_time.ended_at, None);
    assert_eq!(by_time.task_id, "t2");
}

#[test]
fn a_session_without_provider_calls_gets_estimated_provider_spans() {
    let entries = vec![
        turn_start(0, "T1", TurnTrigger::User),
        marker(5),
        marker(300),
        marker(500),
        turn_end(600, "T1", TurnOutcome::Completed),
    ];
    let mut reply = text(500, Role::Assistant, "hi");
    reply.usage = Some(Usage {
        input_tokens: 9,
        output_tokens: 1,
        cached_input_tokens: 0,
        ..Default::default()
    });
    reply.finish_reason = Some(FinishReason::Stop);
    let messages = vec![text(5, Role::User, "hello"), reply];
    let trace = build("s", &source(&entries, &messages, false), &[]);
    let spans = &trace.turns[0].children;
    assert_eq!(spans.len(), 1);
    let provider = &spans[0];
    assert_eq!(provider.label, "model #1");
    assert!(provider.estimated);
    assert_eq!(
        (provider.started_at, provider.ended_at),
        (BASE + 300, Some(BASE + 500))
    );
    let timing = provider.provider.as_ref().expect("timing");
    assert_eq!(timing.usage.map(|usage| usage.input_tokens), Some(9));
    assert_eq!(timing.finish_reason, Some(FinishReason::Stop));
    assert_eq!(timing.attempts, 0);
    assert!(timing.request.is_none());
}

fn section<'a>(content: &'a SpanContent, title: &str) -> &'a str {
    &content
        .sections
        .iter()
        .find(|section| section.title == title)
        .unwrap_or_else(|| panic!("no section {title}"))
        .text
}

#[test]
fn span_content_reads_tool_provider_turn_and_compaction_text() {
    let (entries, messages) = finished_turn();
    let parent = source(&entries, &messages, false);

    let tool = span_content(&parent, &[], "op2").expect("tool content");
    assert!(section(&tool, "arguments").contains("\"command\": \"cargo   test\\n --all\""));
    assert_eq!(section(&tool, "result"), "all passed");

    let second = span_content(&parent, &[], "op3").expect("provider content");
    // Input: the reply and tool result since the previous request began.
    let input = section(&second, "input");
    assert!(input.starts_with("assistant: bash({"), "{input}");
    assert!(input.contains("tool: all passed"));
    assert!(!input.contains("run the tests"));
    assert_eq!(section(&second, "reply"), "done");
    let request = section(&second, "request");
    assert!(request.contains("system prompt  11 tokens"));
    assert!(request.contains("memory records: m1, m2"));

    let first = span_content(&parent, &[], "op1").expect("provider content");
    assert!(section(&first, "input").contains("user: run the tests"));
    assert!(section(&first, "reply").starts_with("bash({"));

    let turn = span_content(&parent, &[], "T1").expect("turn content");
    assert_eq!(section(&turn, "input"), "run the tests");

    let compaction = span_content(&parent, &[], "op4").expect("compaction content");
    assert_eq!(section(&compaction, "summary"), "the summary");

    assert_eq!(span_content(&parent, &[], "nope"), None);
}

#[test]
fn span_content_of_a_subagent_reads_the_child_session() {
    let entries = vec![
        turn_start(0, "T1", TurnTrigger::User),
        turn_end(1000, "T1", TurnOutcome::Completed),
    ];
    let child = child_entries("t1", "", 150);
    let child_messages = vec![
        text(180, Role::Assistant, "first"),
        text(300, Role::Assistant, "the review"),
    ];
    let parent = source(&entries, &[], false);
    let children = [source(&child, &child_messages, false)];
    let agent = span_content(&parent, &children, "t1").expect("agent content");
    assert_eq!(section(&agent, "prompt"), "look at it");
    assert_eq!(section(&agent, "result"), "the review");
    let tool = span_content(&parent, &children, "t1:op2").expect("child tool");
    assert!(tool.sections.is_empty());
}

#[test]
fn trace_json_is_camel_case_and_omits_empty_fields() {
    let (entries, messages) = finished_turn();
    let trace = build("s1", &source(&entries, &messages, false), &[]);
    let json = serde_json::to_value(&trace).expect("encode");
    assert_eq!(json["sessionId"], "s1");
    let tool = &json["turns"][0]["children"][1];
    assert_eq!(tool["startedAt"], BASE + 110);
    assert_eq!(tool["toolCallId"], "c1");
    assert_eq!(tool["kind"], "tool");
    for key in ["estimated", "provider", "taskId", "triggeredBy", "children"] {
        assert!(tool.get(key).is_none(), "{key} present on a leaf tool span");
    }
    let model = &json["turns"][0]["children"][0];
    assert_eq!(model["provider"]["firstOutputMs"], 5);
    assert_eq!(model["provider"]["retries"][0]["atMs"], 3);
    assert_eq!(model["provider"]["request"]["memoryRecordIds"][1], "m2");
    let back: Trace = serde_json::from_value(json).expect("decode");
    assert_eq!(back, trace);
}

#[test]
fn previews_are_cut_on_character_boundaries() {
    let long = "界".repeat(70);
    let preview = argument_preview(&serde_json::json!({ "path": long }));
    assert_eq!(preview.chars().count(), TOOL_PREVIEW_CHARS + 1);
    assert!(preview.ends_with('…'));
    assert_eq!(
        argument_preview(&serde_json::json!({ "n": 1 })),
        r#"{"n":1}"#
    );
    assert_eq!(cut("short", 60), "short");
}
