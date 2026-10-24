//! Structured approval projection regressions, shared by local and attached events.
use super::*;
use otto_core::approval::{ApplicationState, Decision, ReviewMetadata, ReviewStatus, State};

fn emit(app: &mut App, request: &otto_core::approval::Request) {
    app.apply_event(&WireEvent {
        event_type: "approval_changed".into(),
        approval: Some(request.clone()),
        ..Default::default()
    });
}

fn rendered(app: &App, width: usize) -> String {
    crate::tui::transcript::lines(&app.entries, false, width)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn structured_details_decode_json_once_preserving_commands_and_safe_controls() {
    let (mut app, _) = app();
    let mut request = approval("structured");
    request.reason = "first\r\n\tindented\rthird".into();
    let command = "printf '\\n\\t'; rg '\\w+\\t'\n\techo next\r\nlast\u{1b}[31m";
    request.subject.details = serde_json::json!({
        "command": command,
        "nested": {"items": ["line one\nline two", "x".repeat(500)]},
        "empty": "",
        "nothing": null,
    })
    .to_string();
    emit(&mut app, &request);
    let text = rendered(&app, 160);
    assert!(text.contains("printf '\\n\\t'; rg '\\w+\\t'"), "{text}");
    assert!(text.contains("    echo next"));
    assert!(text.contains("first\n"));
    assert!(text.contains("    indented\n"));
    assert!(text.contains("third"));
    assert!(text.contains("line one\n"));
    assert!(text.contains("line two"));
    assert!(text.contains("nested:"));
    assert!(text.contains("[1]:"));
    assert_eq!(text.matches('x').count(), 502); // nested value, "next", visible \\x1b
    assert!(!text.contains("empty:"));
    assert!(!text.contains("nothing:"));
    assert!(!text.contains("Feedback:"));
    assert!(!text.contains('\u{1b}'));
    assert!(text.contains("\\x1b[31m"));
    assert!(!text.contains('\t'));
    assert!(!text.contains('\r'));
    assert!(!text.contains("\\r\\n"));
}

#[test]
fn plain_details_and_json_string_literals_are_not_heuristically_unescaped() {
    for details in [
        r"printf '\n\t'; rg '\w+'",
        r#""line\n\t""#,
        "# literal **code**\n```\n\tvalue\n```",
    ] {
        let (mut app, _) = app();
        let mut request = approval("literal");
        request.subject.details = details.into();
        emit(&mut app, &request);
        let text = rendered(&app, 160);
        for line in details.lines() {
            assert!(text.contains(&line.replace('\t', "    ")), "{text}");
        }
    }
}

#[test]
fn local_and_attached_approval_updates_replace_one_entry_with_feedback_and_errors() {
    for attached in [false, true] {
        let (mut app, _) = app();
        app.attached = attached;
        let mut request = approval("same");
        request.review = Some(ReviewMetadata {
            status: ReviewStatus::Reviewing,
            assessment: None,
            settings_revision: 1,
            evidence_digest: "fixture".into(),
            policy_digest: "fixture".into(),
        });
        emit(&mut app, &request);
        assert!(rendered(&app, 100).contains("reviewing"));
        request.revision += 1;
        request.state = State::Approved;
        request.decision = Some(Decision {
            option_id: "approve".into(),
            feedback: "looks good\nkeep scope".into(),
            actor: "fixture".into(),
            transport: "auto-mode".into(),
            decided_at: 1,
        });
        request.application = ApplicationState::Applying;
        emit(&mut app, &request);
        request.revision += 1;
        request.application = ApplicationState::Failed;
        request.application_error = "retry after fixing fixture\u{1b}[0m".into();
        emit(&mut app, &request);
        assert_eq!(app.entries.len(), 1);
        let text = rendered(&app, 100);
        assert!(text.contains("approved"));
        assert!(text.contains("failed"));
        assert!(text.contains("auto-mode"));
        assert!(text.contains("looks good\n"));
        assert!(text.contains("Application error:"));
        assert!(!text.contains('\u{1b}'));
        request.revision += 1;
        request.state = State::Rejected;
        request.decision.as_mut().unwrap().feedback = "not this command".into();
        emit(&mut app, &request);
        let mut stale = request.clone();
        stale.revision -= 1;
        stale.state = State::Pending;
        emit(&mut app, &stale);
        assert_eq!(app.entries.len(), 1);
        assert!(rendered(&app, 100).contains("not this command"));
        assert!(rendered(&app, 100).contains("rejected"));
    }
}

fn reload_approvals(app: &mut App, history: Vec<otto_core::model::Message>, receipt: bool) {
    if receipt {
        app.apply_receipt(
            OperationReceipt {
                message: String::new(),
                info: app.info.clone(),
                history: Some(history),
                prompt: None,
                draft: None,
            },
            false,
        );
    } else {
        app.refresh(&Backend::Attach(&Remote::offline(history)));
    }
}

fn transcript_order(app: &App) -> Vec<String> {
    app.entries
        .iter()
        .map(|entry| {
            entry
                .approval
                .as_ref()
                .map(|request| format!("approval:{}", request.id))
                .unwrap_or_else(|| {
                    if entry.operation_id.is_some() {
                        "operation".into()
                    } else {
                        entry.raw.clone()
                    }
                })
        })
        .collect()
}

#[test]
fn both_reload_paths_keep_interleaved_approvals_in_place_through_updates() {
    use otto_core::model::{Block, BlockType, Message, Role};
    for receipt in [false, true] {
        let (mut app, _) = app();
        let history = vec![
            crate::cli::fixture_tests::user("same input"),
            Message {
                role: Role::Assistant,
                blocks: vec![Block {
                    block_type: BlockType::ToolCall,
                    tool_call_id: "call".into(),
                    invocation_id: "operation".into(),
                    tool_name: "bash".into(),
                    ..Block::default()
                }],
                ..Message::default()
            },
            Message {
                role: Role::Tool,
                blocks: vec![Block {
                    block_type: BlockType::ToolResult,
                    tool_call_id: "call".into(),
                    text: "completed output".into(),
                    ..Block::default()
                }],
                ..Message::default()
            },
            Message {
                role: Role::Assistant,
                blocks: vec![Block::text("reply")],
                ..Message::default()
            },
            crate::cli::fixture_tests::user("same input"),
            Message {
                role: Role::Assistant,
                blocks: vec![Block::text("last reply")],
                ..Message::default()
            },
        ];
        let (rows, _) = crate::tui::entries::entries_from_history(&history);
        let mut requests: Vec<_> = ["first", "second", "third", "fourth"]
            .into_iter()
            .map(approval)
            .collect();
        for (index, mut row) in rows.into_iter().enumerate() {
            // Live IDs differ from the IDs supplied by the history reducer.
            row.id = format!("live-{index}");
            if row.operation_id.is_some() {
                row.tool_done = false;
                row.tool_output.clear();
            }
            app.entries.push(row);
            match index {
                0 => {
                    emit(&mut app, &requests[0]);
                    emit(&mut app, &requests[1]);
                }
                1 => emit(&mut app, &requests[2]),
                3 => emit(&mut app, &requests[3]),
                _ => {}
            }
        }
        let expected = vec![
            "same input",
            "approval:first",
            "approval:second",
            "operation",
            "approval:third",
            "reply",
            "same input",
            "approval:fourth",
            "last reply",
        ];
        assert_eq!(transcript_order(&app), expected);
        for round in 0..4 {
            // Exercise repeated and alternating reloads of both entry points.
            reload_approvals(&mut app, history.clone(), receipt ^ (round % 2 == 1));
            assert_eq!(transcript_order(&app), expected);
            for request in &mut requests {
                request.revision += 1;
                request.state = State::Approved;
                request.application = ApplicationState::Applied;
                emit(&mut app, request);
                let mut stale = request.clone();
                stale.revision -= 1;
                stale.state = State::Pending;
                emit(&mut app, &stale);
            }
            assert_eq!(transcript_order(&app), expected);
            let retained: Vec<_> = app
                .entries
                .iter()
                .filter_map(|row| row.approval.as_ref())
                .collect();
            assert_eq!(retained, requests.iter().collect::<Vec<_>>());
        }
        let mut extended = history.clone();
        extended.push(crate::cli::fixture_tests::user("new input"));
        reload_approvals(&mut app, extended, receipt);
        let mut expected_extended = expected;
        expected_extended.push("new input");
        assert_eq!(transcript_order(&app), expected_extended);
    }
}

#[test]
fn both_reload_paths_anchor_before_surviving_successor_when_predecessor_is_gone() {
    for receipt in [false, true] {
        let (mut app, _) = app();
        let history = vec![
            crate::cli::fixture_tests::user("removed"),
            crate::cli::fixture_tests::user("surviving"),
        ];
        let (rows, _) = crate::tui::entries::entries_from_history(&history);
        app.entries.push(rows[0].clone());
        emit(&mut app, &approval("first"));
        emit(&mut app, &approval("second"));
        app.entries.push(rows[1].clone());
        for _ in 0..3 {
            reload_approvals(&mut app, history[1..].to_vec(), receipt);
            assert_eq!(
                transcript_order(&app),
                ["approval:first", "approval:second", "surviving"]
            );
        }
    }
}

#[test]
fn same_session_history_refresh_retains_live_projection_but_not_new_session_or_process() {
    let (mut app, remote) = app();
    let request = approval("refresh");
    emit(&mut app, &request);
    app.refresh(&Backend::Attach(&remote));
    assert_eq!(
        app.entries
            .iter()
            .filter(|entry| entry.approval.is_some())
            .count(),
        1
    );
    assert!(rendered(&app, 100).contains("Request details:"));
    app.apply_receipt(
        OperationReceipt {
            message: String::new(),
            info: app.info.clone(),
            history: Some(vec![crate::cli::fixture_tests::user("retained user input")]),
            prompt: None,
            draft: None,
        },
        false,
    );
    assert_eq!(
        app.entries
            .iter()
            .filter(|entry| entry.approval.is_some())
            .count(),
        1
    );
    assert!(rendered(&app, 100).contains("retained user input"));
    assert!(rendered(&app, 100).contains("Request details:"));
    // Fresh history is still the ordinary message reducer, with no persisted
    // structured approval events. We intentionally make no replay claim.
    let (history, _) = crate::tui::entries::entries_from_history(&[]);
    assert!(history.is_empty());
    let fresh = App::new(&Backend::Attach(&remote));
    assert!(fresh.entries.iter().all(|entry| entry.approval.is_none()));
    app.info.session_id = "different".into();
    app.refresh(&Backend::Attach(&remote));
    assert!(app.entries.iter().all(|entry| entry.approval.is_none()));
}
