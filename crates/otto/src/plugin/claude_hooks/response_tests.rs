use super::response::{ResponseContext, translate};
use otto_core::agent::hooks::{Decision, Response};
use serde_json::{Value, json};
use std::path::Path;

/// Otto fields each Claude event may set, as enforced by `plugin::hooks::binding`.
fn allowed_fields(claude_event: &str) -> &'static [&'static str] {
    match claude_event {
        "SessionStart" | "SubagentStart" | "UserPromptSubmit" | "PreCompact" => {
            &["decision", "reason", "context", "notice"]
        }
        "PreToolUse" => &["decision", "reason", "arguments", "notice"],
        "PostToolUse" | "PostToolUseFailure" => &["content", "notice"],
        _ => &["notice"],
    }
}

fn assert_allowed_fields(claude_event: &str, response: &Response) {
    let allowed = allowed_fields(claude_event);
    let set = [
        ("decision", response.decision.is_some()),
        ("reason", response.reason.is_some()),
        ("context", response.context.is_some()),
        ("arguments", response.arguments.is_some()),
        ("content", response.content.is_some()),
        ("notice", response.notice.is_some()),
    ];
    for (name, is_set) in set {
        assert!(
            !is_set || allowed.contains(&name),
            "{claude_event} must not set {name}: {response:?}"
        );
    }
}

fn context_with<'a>(
    otto_tool: Option<&'a str>,
    original_arguments: Option<&'a Value>,
    original_content: Option<&'a str>,
) -> ResponseContext<'a> {
    ResponseContext {
        workspace: Path::new("/ws"),
        otto_tool,
        original_arguments,
        original_content,
    }
}

fn run(claude_event: &str, exit_code: i32, stdout: &str, stderr: &str) -> Response {
    let response = translate(
        claude_event,
        exit_code,
        stdout,
        stderr,
        &context_with(Some("bash"), None, Some("old")),
    )
    .unwrap();
    assert_allowed_fields(claude_event, &response);
    response
}

fn run_json(claude_event: &str, output: Value) -> Response {
    run(claude_event, 0, &output.to_string(), "")
}

fn assert_default(response: &Response) {
    assert!(response.decision.is_none() && response.reason.is_none() && response.context.is_none());
    assert!(
        response.arguments.is_none() && response.content.is_none() && response.notice.is_none()
    );
}

#[test]
fn exit_two_denies_pre_events_with_stderr_or_fallback() {
    let response = run("PreToolUse", 2, r#"{"decision":"approve"}"#, "  no rm\n");
    assert_eq!(response.decision, Some(Decision::Deny));
    assert_eq!(response.reason.as_deref(), Some("no rm"));
    assert_eq!(
        run("PreToolUse", 2, "", "").reason.as_deref(),
        Some("Hook blocked this tool")
    );
    assert_eq!(
        run("UserPromptSubmit", 2, "", "").reason.as_deref(),
        Some("Hook blocked this prompt")
    );
    assert_eq!(
        run("PreCompact", 2, "", "").reason.as_deref(),
        Some("Hook blocked compaction")
    );
    assert_eq!(
        run("PreCompact", 2, "", "keep").reason.as_deref(),
        Some("keep")
    );
}

#[test]
fn exit_two_appends_stderr_to_post_tool_content() {
    let response = run("PostToolUse", 2, "", "bad output");
    assert_eq!(response.content.as_deref(), Some("old\n[hook] bad output"));
    assert_default(&run("PostToolUse", 2, "", " "));
    let failure = run("PostToolUseFailure", 2, "", "note");
    assert_eq!(failure.content.as_deref(), Some("old\n[hook] note"));
}

#[test]
fn exit_two_notice_on_session_and_stop_events() {
    for claude_event in ["SessionStart", "SubagentStart", "Stop", "SubagentStop"] {
        assert_eq!(
            run(claude_event, 2, "", "stderr text").notice.as_deref(),
            Some("stderr text")
        );
        assert_eq!(
            run(claude_event, 2, "", "").notice.as_deref(),
            Some("hook exited 2")
        );
    }
    assert_default(&run("SessionEnd", 2, "", "x"));
    assert_default(&run("PostCompact", 2, "", "x"));
}

#[test]
fn other_exit_codes_produce_a_notice() {
    assert_eq!(
        run("PreToolUse", 1, "", "boom").notice.as_deref(),
        Some("hook exited 1: boom")
    );
    assert_eq!(
        run("Stop", 130, "", "").notice.as_deref(),
        Some("hook exited 130")
    );
    assert_default(&run("SessionEnd", 1, "", "boom"));
    let long_stderr = "e".repeat(5000);
    let notice = run("PostToolUse", 3, "", &long_stderr).notice.unwrap();
    assert_eq!(notice.len(), "hook exited 3: ".len() + 1024);
}

#[test]
fn exit_zero_empty_and_plain_text_stdout() {
    assert_default(&run("PreToolUse", 0, "  \n", ""));
    assert_eq!(
        run("UserPromptSubmit", 0, "extra info\n", "")
            .context
            .as_deref(),
        Some("extra info")
    );
    assert_eq!(
        run("SessionStart", 0, "hello", "").context.as_deref(),
        Some("hello")
    );
    assert_default(&run("PreToolUse", 0, "hello", ""));
    assert_default(&run("Stop", 0, "hello", ""));
}

#[test]
fn json_array_stdout_is_plain_text() {
    assert_eq!(
        run("UserPromptSubmit", 0, "[1,2]", "").context.as_deref(),
        Some("[1,2]")
    );
    assert_default(&run("PostToolUse", 0, "[1,2]", ""));
}

#[test]
fn pre_tool_use_permission_decisions() {
    let allow = run_json(
        "PreToolUse",
        json!({"hookSpecificOutput": {"permissionDecision": "allow"}}),
    );
    assert_eq!(allow.decision, Some(Decision::Allow));
    let deny = run_json(
        "PreToolUse",
        json!({"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "unsafe"}}),
    );
    assert_eq!(
        (deny.decision, deny.reason.as_deref()),
        (Some(Decision::Deny), Some("unsafe"))
    );
    let deny_default = run_json(
        "PreToolUse",
        json!({"hookSpecificOutput": {"permissionDecision": "deny"}}),
    );
    assert_eq!(
        deny_default.reason.as_deref(),
        Some("Hook denied this tool")
    );
    for value in ["ask", "defer"] {
        let ask = run_json(
            "PreToolUse",
            json!({"hookSpecificOutput": {"permissionDecision": value}}),
        );
        assert_eq!(ask.decision, Some(Decision::Deny));
        assert_eq!(
            ask.reason.as_deref(),
            Some("hook requested confirmation; not supported")
        );
        assert_eq!(
            ask.notice.as_deref(),
            Some("hook requested confirmation for bash; Otto denied the call")
        );
    }
    let ask_reason = run_json(
        "PreToolUse",
        json!({"hookSpecificOutput": {"permissionDecision": "ask", "permissionDecisionReason": "why"}}),
    );
    assert_eq!(
        ask_reason.reason.as_deref(),
        Some("hook requested confirmation; not supported: why")
    );
}

#[test]
fn pre_tool_use_deprecated_decision_and_precedence() {
    assert_eq!(
        run_json("PreToolUse", json!({"decision": "approve"})).decision,
        Some(Decision::Allow)
    );
    let block = run_json("PreToolUse", json!({"decision": "block", "reason": "nope"}));
    assert_eq!(
        (block.decision, block.reason.as_deref()),
        (Some(Decision::Deny), Some("nope"))
    );
    assert_eq!(
        run_json("PreToolUse", json!({"decision": "block"}))
            .reason
            .as_deref(),
        Some("Hook denied this tool")
    );
    let both = run_json(
        "PreToolUse",
        json!({"decision": "block", "hookSpecificOutput": {"permissionDecision": "allow"}}),
    );
    assert_eq!(both.decision, Some(Decision::Allow));
}

#[test]
fn updated_input_is_translated_to_otto_arguments() {
    let original = json!({"path": "a.rs"});
    let context = context_with(Some("edit"), Some(&original), None);
    let output = json!({"hookSpecificOutput": {
        "permissionDecision": "allow",
        "updatedInput": {"file_path": "/ws/b.rs", "old_string": "x", "new_string": "y"}
    }});
    let response = translate("PreToolUse", 0, &output.to_string(), "", &context).unwrap();
    assert_allowed_fields("PreToolUse", &response);
    assert_eq!(response.decision, Some(Decision::Allow));
    assert_eq!(
        response.arguments,
        Some(json!({"path": "b.rs", "old_text": "x", "new_text": "y"}))
    );
    let without_decision =
        json!({"hookSpecificOutput": {"updatedInput": {"file_path": "/ws/b.rs"}}});
    let response = translate("PreToolUse", 0, &without_decision.to_string(), "", &context).unwrap();
    assert!(response.decision.is_none() && response.arguments.is_some());
}

#[test]
fn updated_input_is_ignored_on_deny_and_errors_on_misuse() {
    let denied = run_json(
        "PreToolUse",
        json!({"hookSpecificOutput": {"permissionDecision": "deny", "updatedInput": {"command": "ls"}}}),
    );
    assert!(denied.arguments.is_none());
    let output = json!({"hookSpecificOutput": {"updatedInput": {"command": "ls"}}}).to_string();
    let no_context = ResponseContext {
        otto_tool: None,
        ..context_with(None, None, None)
    };
    assert_eq!(
        translate("PreToolUse", 0, &output, "", &no_context).unwrap_err(),
        "hook updatedInput without a tool call"
    );
    let original = json!({"path": "a.rs"});
    let non_object = json!({"hookSpecificOutput": {"updatedInput": "text"}}).to_string();
    let context = context_with(Some("edit"), Some(&original), None);
    assert!(translate("PreToolUse", 0, &non_object, "", &context).is_err());
}

#[test]
fn post_tool_use_content_rules() {
    let replaced = run_json(
        "PostToolUse",
        json!({"hookSpecificOutput": {"updatedToolOutput": "new"}}),
    );
    assert_eq!(replaced.content.as_deref(), Some("new"));
    let blocked = run_json(
        "PostToolUse",
        json!({"decision": "block", "reason": "leak"}),
    );
    assert_eq!(blocked.content.as_deref(), Some("old\n[hook] leak"));
    let added = run_json(
        "PostToolUse",
        json!({"hookSpecificOutput": {"additionalContext": "hint"}}),
    );
    assert_eq!(added.content.as_deref(), Some("old\n[hook] hint"));
    let all = run_json(
        "PostToolUse",
        json!({"decision": "block", "reason": "r", "hookSpecificOutput": {"updatedToolOutput": "n", "additionalContext": "c"}}),
    );
    assert_eq!(all.content.as_deref(), Some("n\n[hook] r\n[hook] c"));
    assert_default(&run_json("PostToolUse", json!({"decision": "block"})));
    assert_default(&run_json("PostToolUse", json!({})));
}

#[test]
fn post_tool_use_failure_appends_additional_context() {
    let response = run_json(
        "PostToolUseFailure",
        json!({"hookSpecificOutput": {"additionalContext": "retry"}}),
    );
    assert_eq!(response.content.as_deref(), Some("old\n[hook] retry"));
    assert_default(&run_json(
        "PostToolUseFailure",
        json!({"decision": "block", "reason": "x"}),
    ));
}

#[test]
fn prompt_session_and_compaction_events() {
    let blocked = run_json(
        "UserPromptSubmit",
        json!({"decision": "block", "reason": "no"}),
    );
    assert_eq!(
        (blocked.decision, blocked.reason.as_deref()),
        (Some(Decision::Deny), Some("no"))
    );
    assert_eq!(
        run_json("UserPromptSubmit", json!({"decision": "block"}))
            .reason
            .as_deref(),
        Some("Hook blocked this prompt")
    );
    let context = run_json(
        "UserPromptSubmit",
        json!({"hookSpecificOutput": {"additionalContext": "ctx"}}),
    );
    assert_eq!(context.context.as_deref(), Some("ctx"));
    let start = run_json(
        "SessionStart",
        json!({"hookSpecificOutput": {"additionalContext": "boot", "initialUserMessage": "hi", "sessionTitle": "t"}}),
    );
    assert_eq!(start.context.as_deref(), Some("boot"));
    assert!(start.content.is_none() && start.decision.is_none());
    let compaction = run_json("PreCompact", json!({"decision": "block", "reason": "busy"}));
    assert_eq!(
        (compaction.decision, compaction.reason.as_deref()),
        (Some(Decision::Deny), Some("busy"))
    );
    assert_eq!(
        run_json("PreCompact", json!({"decision": "block"}))
            .reason
            .as_deref(),
        Some("Hook blocked compaction")
    );
}

#[test]
fn stop_block_becomes_a_notice() {
    let response = run_json("Stop", json!({"decision": "block", "reason": "keep going"}));
    assert_eq!(
        response.notice.as_deref(),
        Some("hook requested continuation; not supported: keep going")
    );
    let response = run_json("SubagentStop", json!({"decision": "block"}));
    assert_eq!(
        response.notice.as_deref(),
        Some("hook requested continuation; not supported")
    );
}

#[test]
fn continue_false_stops_with_system_message() {
    let pre = run_json(
        "PreToolUse",
        json!({"continue": false, "stopReason": "halt", "decision": "approve"}),
    );
    assert_eq!(
        (pre.decision, pre.reason.as_deref()),
        (Some(Decision::Deny), Some("halt"))
    );
    assert_eq!(
        run_json("UserPromptSubmit", json!({"continue": false}))
            .reason
            .as_deref(),
        Some("Hook stopped the session")
    );
    let stop = run_json("Stop", json!({"continue": false, "stopReason": "done"}));
    assert_eq!(stop.notice.as_deref(), Some("done"));
    let default_notice = run_json(
        "PostToolUse",
        json!({"continue": false, "hookSpecificOutput": {"additionalContext": "c"}}),
    );
    assert_eq!(
        default_notice.notice.as_deref(),
        Some("hook requested stop; not supported")
    );
    assert!(default_notice.content.is_none());
    let joined = run_json(
        "Stop",
        json!({"continue": false, "stopReason": "done", "systemMessage": "bye"}),
    );
    assert_eq!(joined.notice.as_deref(), Some("done\nbye"));
    assert_default(&run_json(
        "SessionEnd",
        json!({"continue": false, "systemMessage": "x"}),
    ));
}

#[test]
fn system_message_alone_and_unknown_fields() {
    for claude_event in [
        "PreToolUse",
        "SubagentStart",
        "PostCompact",
        "Stop",
        "PostToolUse",
    ] {
        let response = run_json(
            claude_event,
            json!({"systemMessage": "hello", "futureField": [1], "decision": 7}),
        );
        assert_eq!(response.notice.as_deref(), Some("hello"), "{claude_event}");
        assert!(response.decision.is_none());
    }
    let joined = run_json("Stop", json!({"decision": "block", "systemMessage": "fyi"}));
    assert_eq!(
        joined.notice.as_deref(),
        Some("hook requested continuation; not supported\nfyi")
    );
    assert_default(&run_json("SessionEnd", json!({"systemMessage": "x"})));
    assert_default(&run_json(
        "SubagentStart",
        json!({"hookSpecificOutput": {"additionalContext": "c"}}),
    ));
    assert_default(&run_json(
        "PreToolUse",
        json!({"hookSpecificOutput": "text", "systemMessage": 5}),
    ));
}

#[test]
fn long_text_is_truncated_to_the_text_limit() {
    let long_text = "é".repeat(20_000);
    let response = run_json(
        "UserPromptSubmit",
        json!({"hookSpecificOutput": {"additionalContext": long_text}}),
    );
    assert_eq!(response.context.unwrap().chars().count(), 10_000);
    let response = run("PreToolUse", 2, "", &long_text);
    assert_eq!(response.reason.unwrap().chars().count(), 10_000);
}

#[test]
fn unknown_event_is_an_error() {
    let error =
        translate("Notification", 0, "{}", "", &context_with(None, None, None)).unwrap_err();
    assert_eq!(error, "unknown Claude hook event Notification");
}
