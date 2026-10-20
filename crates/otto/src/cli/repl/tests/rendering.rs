//! Event rendering and command splitting tests.

use super::*;

#[test]
fn a_bash_approval_request_renders_the_approval_lines() {
    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();
    render_event(
        &mut stdout,
        &mut stderr,
        &Event::ToolCallFinished {
            operation_id: operation_id(),
            attempt: 1,
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            result: ToolResult {
                content: "approval required for unsandboxed bash execution.\n\
Approve in Otto: /approve approval-1\n\
Command: \"git push\"\n\
Justification: \"push branch\"\n\
The command did not run."
                    .to_string(),
                is_error: true,
                ..ToolResult::default()
            },
            outcome: OperationOutcome {
                disposition: OperationDisposition::Error,
                effect_certainty: EffectCertainty::NotStarted,
                stop_reason: None,
            },
        },
    );
    assert_eq!(
        stdout.text(),
        "[tool result] Did not run (operation op_test): approval required for unsandboxed bash execution.\n\
Approve in Otto: /approve approval-1\n\
Command: \"git push\"\n\
Justification: \"push branch\"\n"
    );
}

#[test]
fn unknown_tool_outcome_uses_stable_warning_with_operation_id() {
    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();
    render_event(
        &mut stdout,
        &mut stderr,
        &Event::ToolCallFinished {
            operation_id: operation_id(),
            attempt: 1,
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            result: ToolResult {
                content: "connection lost".to_string(),
                is_error: true,
                ..ToolResult::default()
            },
            outcome: OperationOutcome {
                disposition: OperationDisposition::Interrupted,
                effect_certainty: EffectCertainty::Unknown,
                stop_reason: None,
            },
        },
    );
    assert_eq!(
        stdout.text(),
        "[tool warning] Outcome unknown — check effects before retrying (operation op_test): connection lost\n"
    );
    assert_eq!(stderr.text(), "");
}

#[test]
fn interrupted_provider_output_is_marked_before_replacement_text() {
    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();
    for event in [
        Event::TextDelta {
            text: "partial".into(),
        },
        Event::ProviderRetry {
            operation_id: operation_id(),
            attempt: 2,
            max_attempts: 7,
            delay: std::time::Duration::from_secs(1),
            reason: "connection interrupted".into(),
            interrupted_output: true,
        },
        Event::TextDelta {
            text: "replacement".into(),
        },
    ] {
        assert!(!render_event(&mut stdout, &mut stderr, &event));
    }
    assert!(
        stdout
            .text()
            .starts_with("partial\n[Previous response interrupted")
    );
    assert!(stdout.text().ends_with("]\nreplacement"));
    assert!(stderr.text().contains("attempt 2/7"));
}

#[test]
fn provider_retry_renders_progress_without_suppressing_a_terminal_error() {
    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();

    let rendered_error = render_event(
        &mut stdout,
        &mut stderr,
        &Event::ProviderRetry {
            operation_id: operation_id(),
            attempt: 2,
            max_attempts: 4,
            delay: std::time::Duration::from_millis(1_250),
            reason: "HTTP 503\nresponse body omitted".to_string(),
            interrupted_output: false,
        },
    );

    assert!(!rendered_error);
    assert_eq!(stdout.text(), "");
    assert_eq!(
        stderr.text(),
        "[provider retry] attempt 2/4 in 1250ms: HTTP 503\n"
    );
}

#[test]
fn every_event_renders_to_the_expected_line() {
    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();
    let events = [
        Event::TextDelta {
            text: "done".to_string(),
        },
        Event::ToolCallStarted {
            operation_id: operation_id(),
            attempt: 1,
            tool_name: "read".to_string(),
            tool_call_id: "call-1".to_string(),
            arguments: String::new(),
        },
        Event::ToolCallFinished {
            operation_id: operation_id(),
            attempt: 1,
            tool_name: "read".to_string(),
            tool_call_id: "call-1".to_string(),
            result: ToolResult {
                content: "read README.md\nfull output must not render".to_string(),
                ..ToolResult::default()
            },
            outcome: completed(OperationDisposition::Succeeded),
        },
        Event::Notification {
            kind: None,
            task_id: "t-1".to_string(),
            text: "[task-notification] done".to_string(),
            usage: Default::default(),
            present: false,
            cached_input_tokens_present: false,
        },
    ];
    let mut rendered_error = false;
    for event in events {
        rendered_error |= render_event(&mut stdout, &mut stderr, &event);
    }
    assert!(!rendered_error);
    assert_eq!(
        stdout.text(),
        "done\n[tool] read (call-1)\n[tool result] Completed (operation op_test): read README.md\n\n[task-notification] done\n"
    );
    assert_eq!(stderr.text(), "");

    let mut stdout = Buffer::default();
    let mut stderr = Buffer::default();
    assert!(render_event(
        &mut stdout,
        &mut stderr,
        &Event::AgentError {
            message: "provider unavailable".to_string(),
        }
    ));
    assert_eq!(stderr.text(), "provider unavailable\n");
    assert_eq!(stdout.text(), "");
}

#[test]
fn compaction_lines_match_the_expected_text() {
    assert_eq!(
        compaction_line(&CompactionResult {
            noop: true,
            ..CompactionResult::default()
        }),
        "\n[context] no-op\n"
    );
    assert_eq!(
        compaction_line(&CompactionResult {
            tokens_before: Some(12_000),
            estimated_tokens_after: Some(800),
            ..CompactionResult::default()
        }),
        "\n[context] compacted 12k → 800 tokens\n"
    );
    assert_eq!(
        compaction_line(&CompactionResult {
            tokens_before: Some(999),
            ..CompactionResult::default()
        }),
        "\n[context] compacted 999 tokens\n"
    );
}

#[test]
fn commands_split_on_the_first_space() {
    assert_eq!(split_command("/model beta"), Some(("model", "beta")));
    assert_eq!(split_command("/model\tbeta  "), Some(("model", "beta")));
    assert_eq!(split_command("/exit"), Some(("exit", "")));
    assert_eq!(split_command("/"), None);
    assert_eq!(split_command("model"), None);
}
