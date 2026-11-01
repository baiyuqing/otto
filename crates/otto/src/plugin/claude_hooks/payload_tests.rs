//! Tests for the Otto-to-Claude payload translation.
use super::FIELD_LIMIT;
use super::payload::{ClaudePayload, SessionFields, translate};
use serde_json::{Value, json};
use std::path::Path;

const WORKSPACE: &str = "/ws";

fn root_session() -> SessionFields<'static> {
    SessionFields {
        session_id: "s1",
        parent_session_id: None,
        task_id: "t1",
        mode: "new",
        history_path: "/ws/.otto/history.jsonl",
    }
}

fn child_session() -> SessionFields<'static> {
    SessionFields {
        parent_session_id: Some("s0"),
        ..root_session()
    }
}

fn run(otto_event: &str, data: Value) -> Option<ClaudePayload> {
    translate(otto_event, &data, &root_session(), Path::new(WORKSPACE))
}

fn run_child(otto_event: &str, data: Value) -> Option<ClaudePayload> {
    translate(otto_event, &data, &child_session(), Path::new(WORKSPACE))
}

fn success_turn(messages: Value) -> Value {
    json!({"status": "success", "turn": {"messages": messages}})
}

#[test]
fn session_start_new_is_startup() {
    let payload = run("session_start", json!({})).unwrap();
    assert_eq!(payload.event, "SessionStart");
    assert_eq!(payload.matcher_value, "startup");
    assert_eq!(payload.body["source"], "startup");
}

#[test]
fn session_start_resume_is_resume() {
    let session = SessionFields {
        mode: "resume",
        ..root_session()
    };
    let payload = translate("session_start", &json!({}), &session, Path::new(WORKSPACE)).unwrap();
    assert_eq!(payload.event, "SessionStart");
    assert_eq!(payload.matcher_value, "resume");
    assert_eq!(payload.body["source"], "resume");
}

#[test]
fn child_session_start_is_subagent_start() {
    let payload = run_child("session_start", json!({})).unwrap();
    assert_eq!(payload.event, "SubagentStart");
    assert_eq!(payload.matcher_value, "otto-subagent");
    assert_eq!(payload.body["agent_id"], "t1");
    assert_eq!(payload.body["agent_type"], "otto-subagent");
    assert!(payload.body.get("source").is_none());
}

#[test]
fn user_prompt_submit_carries_prompt_only() {
    let payload = run(
        "before_prompt",
        json!({"prompt": "fix it", "followup": true, "has_image": true}),
    )
    .unwrap();
    assert_eq!(payload.event, "UserPromptSubmit");
    assert_eq!(payload.matcher_value, "");
    assert_eq!(payload.body["prompt"], "fix it");
    assert!(payload.body.get("followup").is_none());
    assert!(payload.body.get("has_image").is_none());
}

#[test]
fn user_prompt_submit_defaults_to_empty_prompt() {
    let payload = run("before_prompt", json!({})).unwrap();
    assert_eq!(payload.body["prompt"], "");
}

#[test]
fn pre_tool_use_translates_tool_and_matches_on_tool_name() {
    let payload = run(
        "before_tool",
        json!({"tool": "read", "tool_call_id": "call-1", "arguments": {"path": "a.txt"}}),
    )
    .unwrap();
    assert_eq!(payload.event, "PreToolUse");
    assert_eq!(payload.matcher_value, "Read");
    assert_eq!(payload.body["tool_name"], "Read");
    assert_eq!(
        payload.body["tool_input"],
        json!({"file_path": "/ws/a.txt"})
    );
    assert_eq!(payload.body["tool_use_id"], "call-1");
}

#[test]
fn pre_tool_use_without_arguments_has_empty_input() {
    let payload = run("before_tool", json!({"tool": "bash"})).unwrap();
    assert_eq!(payload.body["tool_input"], json!({}));
    assert_eq!(payload.body["tool_use_id"], "");
}

#[test]
fn mcp_call_tool_yields_mcp_tool_name() {
    let payload = run(
        "before_tool",
        json!({
            "tool": "mcp_call_tool",
            "tool_call_id": "c",
            "arguments": {"tool": "mcp__demo__read", "arguments": {"id": 3}}
        }),
    )
    .unwrap();
    assert_eq!(payload.matcher_value, "mcp__demo__read");
    assert_eq!(payload.body["tool_name"], "mcp__demo__read");
    assert_eq!(payload.body["tool_input"], json!({"id": 3}));
}

#[test]
fn post_tool_use_bash_response_has_stdout_shape() {
    let payload = run(
        "after_tool",
        json!({
            "tool": "bash", "tool_call_id": "c", "arguments": {"command": "ls"},
            "content": "stdout:\nok", "is_error": false
        }),
    )
    .unwrap();
    assert_eq!(payload.event, "PostToolUse");
    assert_eq!(payload.matcher_value, "Bash");
    assert_eq!(payload.body["tool_input"], json!({"command": "ls"}));
    assert_eq!(payload.body["tool_use_id"], "c");
    assert_eq!(
        payload.body["tool_response"],
        json!({"stdout": "stdout:\nok", "stderr": "", "interrupted": false})
    );
}

#[test]
fn post_tool_use_other_tool_response_has_content() {
    let payload = run(
        "after_tool",
        json!({"tool": "read", "arguments": {"path": "a"}, "content": "text"}),
    )
    .unwrap();
    assert_eq!(payload.event, "PostToolUse");
    assert_eq!(payload.matcher_value, "Read");
    assert_eq!(payload.body["tool_response"], json!({"content": "text"}));
}

#[test]
fn post_tool_use_truncates_bash_stdout_at_field_limit() {
    let content = "a".repeat(FIELD_LIMIT + 100);
    let payload = run("after_tool", json!({"tool": "bash", "content": content})).unwrap();
    let stdout = payload.body["tool_response"]["stdout"].as_str().unwrap();
    assert_eq!(stdout.len(), FIELD_LIMIT);
}

#[test]
fn post_tool_use_failure_carries_error() {
    let payload = run(
        "after_tool",
        json!({
            "tool": "bash", "tool_call_id": "c", "arguments": {"command": "false"},
            "content": "exit_code: 1", "is_error": true
        }),
    )
    .unwrap();
    assert_eq!(payload.event, "PostToolUseFailure");
    assert_eq!(payload.matcher_value, "Bash");
    assert_eq!(payload.body["error"], "exit_code: 1");
    assert_eq!(payload.body["is_interrupt"], false);
    assert_eq!(payload.body["tool_name"], "Bash");
    assert_eq!(payload.body["tool_use_id"], "c");
    assert!(payload.body.get("tool_response").is_none());
}

#[test]
fn post_tool_use_failure_truncates_error_at_field_limit() {
    let content = "é".repeat(FIELD_LIMIT);
    let payload = run(
        "after_tool",
        json!({"tool": "read", "content": content, "is_error": true}),
    )
    .unwrap();
    let error = payload.body["error"].as_str().unwrap();
    assert_eq!(error.len(), FIELD_LIMIT);
}

#[test]
fn stop_uses_last_assistant_message_text() {
    let data = success_turn(json!([
        {"role": "user", "blocks": [{"type": "text", "text": "hi"}]},
        {"role": "assistant", "blocks": [{"type": "text", "text": "first"}]},
        {"role": "tool", "blocks": [{"type": "tool_result", "content": "r"}]},
        {"role": "assistant", "blocks": [
            {"type": "text", "text": "line one"},
            {"type": "tool_call", "id": "c", "name": "bash"},
            {"type": "text", "text": "line two"}
        ]},
        {"role": "context", "blocks": [{"type": "text", "text": "ctx"}]}
    ]));
    let payload = run("turn_end", data).unwrap();
    assert_eq!(payload.event, "Stop");
    assert_eq!(payload.matcher_value, "");
    assert_eq!(payload.body["stop_hook_active"], false);
    assert_eq!(payload.body["last_assistant_message"], "line one\nline two");
}

#[test]
fn stop_without_turn_or_assistant_message_is_empty() {
    let without_turn = run("turn_end", json!({"status": "success"})).unwrap();
    assert_eq!(without_turn.body["last_assistant_message"], "");
    let without_assistant = run(
        "turn_end",
        success_turn(json!([{"role": "user", "blocks": [{"type": "text", "text": "hi"}]}])),
    )
    .unwrap();
    assert_eq!(without_assistant.body["last_assistant_message"], "");
}

#[test]
fn stop_truncates_last_assistant_message_at_field_limit() {
    let text = "b".repeat(FIELD_LIMIT + 4096);
    let data =
        success_turn(json!([{"role": "assistant", "blocks": [{"type": "text", "text": text}]}]));
    let payload = run("turn_end", data).unwrap();
    assert_eq!(
        payload.body["last_assistant_message"]
            .as_str()
            .unwrap()
            .len(),
        FIELD_LIMIT
    );
}

#[test]
fn child_turn_end_is_subagent_stop() {
    let payload = run_child("turn_end", success_turn(json!([]))).unwrap();
    assert_eq!(payload.event, "SubagentStop");
    assert_eq!(payload.matcher_value, "otto-subagent");
    assert_eq!(payload.body["agent_id"], "t1");
    assert_eq!(payload.body["stop_hook_active"], false);
}

#[test]
fn cancelled_and_error_turns_produce_nothing() {
    assert!(run("turn_end", json!({"status": "cancelled"})).is_none());
    assert!(run("turn_end", json!({"status": "error", "error": "boom"})).is_none());
    assert!(run_child("turn_end", json!({"status": "cancelled"})).is_none());
}

#[test]
fn pre_compact_manual_and_auto_triggers() {
    let manual = run(
        "before_compaction",
        json!({"reason": "Manual", "focus": "keep API"}),
    )
    .unwrap();
    assert_eq!(manual.event, "PreCompact");
    assert_eq!(manual.matcher_value, "manual");
    assert_eq!(manual.body["trigger"], "manual");
    assert_eq!(manual.body["custom_instructions"], "keep API");
    for reason in ["Threshold", "Idle", "Overflow"] {
        let auto = run("before_compaction", json!({"reason": reason})).unwrap();
        assert_eq!(auto.matcher_value, "auto");
        assert_eq!(auto.body["trigger"], "auto");
        assert_eq!(auto.body["custom_instructions"], "");
    }
}

#[test]
fn post_compact_trigger() {
    let manual = run(
        "after_compaction",
        json!({"checkpoint_id": "k", "reason": "Manual"}),
    )
    .unwrap();
    assert_eq!(manual.event, "PostCompact");
    assert_eq!(manual.matcher_value, "manual");
    assert_eq!(manual.body["trigger"], "manual");
    let auto = run("after_compaction", json!({"reason": "Threshold"})).unwrap();
    assert_eq!(auto.matcher_value, "auto");
    assert_eq!(auto.body["trigger"], "auto");
}

#[test]
fn session_end_reason_is_other() {
    let payload = run("session_end", json!({})).unwrap();
    assert_eq!(payload.event, "SessionEnd");
    assert_eq!(payload.matcher_value, "other");
    assert_eq!(payload.body["reason"], "other");
}

#[test]
fn unknown_otto_event_produces_nothing() {
    assert!(run("before_model", json!({})).is_none());
    assert!(run("", json!({})).is_none());
}

#[test]
fn prompt_id_present_only_when_turn_id_is_a_string() {
    let with_id = run("before_prompt", json!({"turn_id": "turn-7"})).unwrap();
    assert_eq!(with_id.body["prompt_id"], "turn-7");
    let numeric = run("before_prompt", json!({"turn_id": 7})).unwrap();
    assert!(numeric.body.get("prompt_id").is_none());
    let absent = run("before_prompt", json!({})).unwrap();
    assert!(absent.body.get("prompt_id").is_none());
}

fn all_event_samples() -> Vec<(&'static str, Value)> {
    vec![
        ("session_start", json!({})),
        ("before_prompt", json!({"prompt": "p"})),
        (
            "before_tool",
            json!({"tool": "bash", "arguments": {"command": "ls"}}),
        ),
        (
            "after_tool",
            json!({"tool": "bash", "content": "x", "is_error": false}),
        ),
        (
            "after_tool",
            json!({"tool": "bash", "content": "x", "is_error": true}),
        ),
        ("turn_end", success_turn(json!([]))),
        ("before_compaction", json!({"reason": "Manual"})),
        ("after_compaction", json!({"reason": "Idle"})),
        ("session_end", json!({})),
    ]
}

#[test]
fn common_fields_are_on_every_payload() {
    for (otto_event, data) in all_event_samples() {
        let payload = run(otto_event, data).unwrap();
        let body = &payload.body;
        assert_eq!(body["session_id"], "s1", "{otto_event}");
        assert_eq!(
            body["transcript_path"], "/ws/.otto/history.jsonl",
            "{otto_event}"
        );
        assert_eq!(body["cwd"], "/ws", "{otto_event}");
        assert_eq!(body["hook_event_name"], payload.event, "{otto_event}");
        for absent in [
            "agent_id",
            "agent_type",
            "permission_mode",
            "effort",
            "scratchpad_dir",
        ] {
            assert!(body.get(absent).is_none(), "{otto_event} sent {absent}");
        }
    }
}

#[test]
fn child_identity_is_on_every_child_payload() {
    for (otto_event, data) in all_event_samples() {
        let payload = run_child(otto_event, data).unwrap();
        assert_eq!(payload.body["agent_id"], "t1", "{otto_event}");
        assert_eq!(payload.body["agent_type"], "otto-subagent", "{otto_event}");
    }
}
