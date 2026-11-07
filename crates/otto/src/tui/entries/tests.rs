use super::*;
use otto_core::model::{ContextMetadata, OperationId, ToolResultMetadata};

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

#[test]
fn plain_text_messages_become_one_entry_per_role() {
    let history = [
        user("hi"),
        Message {
            role: Role::Assistant,
            blocks: vec![Block::text("hello")],
            ..Message::default()
        },
    ];
    let (entries, _) = entries_from_history(&history);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, Some(EntryKind::User));
    assert_eq!(entries[0].raw, "hi");
    assert_eq!(entries[1].kind, Some(EntryKind::Assistant));
    assert_eq!(entries[1].raw, "hello");
}

#[test]
fn resumed_reasoning_is_its_own_entry_before_the_reply() {
    let history = [Message {
        role: Role::Assistant,
        blocks: vec![Block::reasoning("weigh options"), Block::text("hello")],
        ..Message::default()
    }];
    let (entries, _) = entries_from_history(&history);
    let kinds: Vec<_> = entries
        .iter()
        .map(|entry| (entry.kind, entry.raw.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            (Some(EntryKind::Reasoning), "weigh options"),
            (Some(EntryKind::Assistant), "hello"),
        ]
    );
}

#[test]
fn an_image_is_visible_in_resumed_history() {
    let history = [Message {
        role: Role::User,
        blocks: vec![
            Block::image("iVBORw0KGgo=", "image/png"),
            Block::text("read it"),
        ],
        ..Message::default()
    }];
    let (entries, _) = entries_from_history(&history);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].raw, "[image]");
    assert_eq!(entries[1].raw, "read it");
}

#[test]
fn a_tool_call_and_its_result_pair_into_one_entry() {
    let history = [
        Message {
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "call-1".into(),
                tool_name: "bash".into(),
                arguments: Some(serde_json::value::RawValue::from_string("{}".into()).unwrap()),
                ..Block::default()
            }],
            ..Message::default()
        },
        Message {
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_call_id: "call-1".into(),
                tool_name: "bash".into(),
                text: "ok".into(),
                ..Block::default()
            }],
            ..Message::default()
        },
    ];
    let (entries, _) = entries_from_history(&history);
    assert_eq!(entries.len(), 1, "{entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.kind, Some(EntryKind::Tool));
    assert_eq!(entry.tool_call_id, "call-1");
    assert!(entry.tool_done);
    assert_eq!(entry.tool_output, "ok");
    assert!(!entry.tool_error);
}

#[test]
fn a_tool_result_copies_typed_operation_metadata() {
    let history = [
        Message {
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "call-1".into(),
                tool_name: "bash".into(),
                arguments: Some(serde_json::value::RawValue::from_string("{}".into()).unwrap()),
                ..Block::default()
            }],
            ..Message::default()
        },
        Message {
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_call_id: "call-1".into(),
                tool_name: "bash".into(),
                text: "connection lost".into(),
                is_error: true,
                operation_metadata: Some(ToolResultMetadata {
                    operation_id: Some(OperationId::new("op_1").expect("operation id")),
                    disposition: OperationDisposition::Interrupted,
                    effect_certainty: EffectCertainty::Unknown,
                    stop_reason: Some(OperationStopReason::TransportLost),
                }),
                ..Block::default()
            }],
            ..Message::default()
        },
    ];
    let (entries, _) = entries_from_history(&history);
    let entry = &entries[0];
    assert_eq!(entry.operation_id.as_deref(), Some("op_1"));
    assert_eq!(entry.disposition, Some(OperationDisposition::Interrupted));
    assert_eq!(entry.effect_certainty, Some(EffectCertainty::Unknown));
    assert_eq!(entry.stop_reason, Some(OperationStopReason::TransportLost));
}

#[test]
fn a_legacy_tool_result_has_no_invented_operation_metadata() {
    let history = [Message {
        role: Role::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            text: "looks uncertain in prose".into(),
            is_error: true,
            ..Block::default()
        }],
        ..Message::default()
    }];
    let (entries, _) = entries_from_history(&history);
    assert_eq!(entries[0].operation_id, None);
    assert_eq!(entries[0].disposition, None);
    assert_eq!(entries[0].effect_certainty, None);
    assert_eq!(entries[0].stop_reason, None);
}

#[test]
fn an_undisplayed_context_message_is_skipped() {
    let history = [Message {
        role: Role::Context,
        display: false,
        blocks: vec![Block::text("hidden")],
        ..Message::default()
    }];
    let (entries, _) = entries_from_history(&history);
    assert!(entries.is_empty());
}

#[test]
fn a_displayed_compaction_message_becomes_a_compaction_entry() {
    let history = [Message {
        id: "chk-1".into(),
        role: Role::Context,
        display: true,
        context_type: "compaction".into(),
        context_tokens_before: 4_000,
        blocks: vec![Block::text(
            "[Compaction summary]\nkept the important parts",
        )],
        ..Message::default()
    }];
    let (entries, _) = entries_from_history(&history);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.kind, Some(EntryKind::Compaction));
    assert_eq!(entry.raw, "kept the important parts");
    assert_eq!(entry.checkpoint_id, "chk-1");
    assert_eq!(entry.tokens_before, 4_000);
}

#[test]
fn a_long_task_notification_is_truncated_with_a_task_hint() {
    let mut body = vec!["[task-notification] header".to_string()];
    for line in 0..30 {
        body.push(format!("line {line}"));
    }
    let history = [Message {
        role: Role::Context,
        display: true,
        context_type: "task_notification".into(),
        context_metadata: Some(ContextMetadata {
            task_id: "t7".into(),
        }),
        blocks: vec![Block::text(body.join("\n"))],
        ..Message::default()
    }];
    let (entries, _) = entries_from_history(&history);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.kind, Some(EntryKind::System));
    assert!(
        entry
            .raw
            .starts_with("[task-notification] header\nline 0\n")
    );
    assert!(
        entry.raw.ends_with("… (10 more lines; /task t7)"),
        "{}",
        entry.raw
    );
}

#[test]
fn usage_accumulates_across_messages_and_ignores_missing_usage() {
    let history = [
        Message {
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 2,
                ..Default::default()
            }),
            ..user("a")
        },
        Message {
            usage: Some(Usage {
                input_tokens: 3,
                output_tokens: 1,
                cached_input_tokens: 0,
                ..Default::default()
            }),
            ..user("b")
        },
        user("c"),
    ];
    let (_, usage) = entries_from_history(&history);
    assert_eq!(usage.input_tokens, 13);
    assert_eq!(usage.output_tokens, 6);
    assert_eq!(usage.cached_input_tokens, 2);
}
#[test]
fn execution_clock_updates_freezes_and_preserves_identity_on_replay() {
    use otto_core::wire::events::{WireEvent, WireToolResult};
    let start = Instant::now();
    for disposition in [
        OperationDisposition::Succeeded,
        OperationDisposition::Error,
        OperationDisposition::Cancelled,
        OperationDisposition::DeadlineExceeded,
        OperationDisposition::Interrupted,
    ] {
        let mut entries = Vec::new();
        let mut event = WireEvent {
            event_type: "tool_call_started".into(),
            operation_id: "op_clock".into(),
            tool_call_id: "provider_clock".into(),
            tool_name: "bash".into(),
            attempt: Some(1),
            ..Default::default()
        };
        reduce_tool_event_at(&mut entries, &event, Some(start));
        assert_eq!(
            entries[0].execution_elapsed(start + Duration::from_secs(2)),
            Some(Duration::from_secs(2))
        );
        reduce_tool_event_at(&mut entries, &event, Some(start + Duration::from_secs(1)));
        assert_eq!(
            entries[0].tool_started,
            Some(start),
            "duplicate start must not reset clock"
        );
        event.event_type = "tool_call_finished".into();
        event.result = Some(WireToolResult {
            disposition: Some(disposition),
            is_error: disposition != OperationDisposition::Succeeded,
            ..Default::default()
        });
        reduce_tool_event_at(&mut entries, &event, Some(start + Duration::from_secs(3)));
        reduce_tool_event_at(&mut entries, &event, Some(start + Duration::from_secs(9)));
        assert_eq!(
            entries[0].execution_elapsed(start + Duration::from_secs(20)),
            Some(Duration::from_secs(3))
        );
        event.event_type = "tool_call_started".into();
        reduce_tool_event_at(&mut entries, &event, Some(start + Duration::from_secs(10)));
        assert!(entries[0].tool_done);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].operation_id.as_deref(), Some("op_clock"));
        assert_eq!(entries[0].tool_call_id, "provider_clock");
        event.attempt = Some(2);
        reduce_tool_event_at(&mut entries, &event, Some(start + Duration::from_secs(11)));
        assert_eq!(
            entries[0].execution_elapsed(start + Duration::from_secs(12)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(entries.len(), 1, "retry retains logical invocation");
    }
}

#[test]
fn replay_and_unmatched_finish_do_not_invent_execution_time() {
    use otto_core::wire::events::{WireEvent, WireToolResult};
    let mut entries = Vec::new();
    let mut event = WireEvent {
        event_type: "tool_call_started".into(),
        operation_id: "op_replay".into(),
        attempt: Some(1),
        ..Default::default()
    };
    reduce_tool_event_at(&mut entries, &event, None);
    assert_eq!(entries[0].execution_elapsed(Instant::now()), None);
    event.event_type = "tool_call_finished".into();
    event.result = Some(WireToolResult::default());
    reduce_tool_event_at(&mut entries, &event, None);
    assert_eq!(entries[0].execution_elapsed(Instant::now()), None);
    entries.clear();
    reduce_tool_event_at(&mut entries, &event, Some(Instant::now()));
    assert_eq!(entries[0].execution_elapsed(Instant::now()), None);
    entries.clear();
    event.event_type = "tool_call_started".into();
    reduce_tool_event_at(&mut entries, &event, Some(Instant::now()));
    event.event_type = "tool_call_finished".into();
    event.attempt = Some(2);
    reduce_tool_event_at(&mut entries, &event, Some(Instant::now()));
    assert_eq!(
        entries[0].execution_elapsed(Instant::now()),
        None,
        "a finish for an unobserved attempt cannot use the previous attempt's clock"
    );
}

#[test]
fn invocation_replay_dedupes_without_merging_identical_real_calls() {
    use otto_core::wire::events::{WireEvent, WireToolResult};
    let mut entries = Vec::new();
    let mut event = WireEvent {
        event_type: "tool_call_started".into(),
        operation_id: "op_a".into(),
        tool_call_id: "reused-provider-id".into(),
        tool_name: "bash".into(),
        attempt: Some(1),
        tool_args: otto_core::wire::events::tool_args_raw(r#"{"command":"echo same"}"#),
        ..WireEvent::default()
    };
    reduce_tool_event(&mut entries, &event);
    reduce_tool_event(&mut entries, &event);
    assert_eq!(entries.len(), 1);
    event.event_type = "tool_call_finished".into();
    event.result = Some(WireToolResult {
        content: "done".into(),
        ..WireToolResult::default()
    });
    reduce_tool_event(&mut entries, &event);
    reduce_tool_event(&mut entries, &event);
    event.event_type = "tool_call_started".into();
    reduce_tool_event(&mut entries, &event);
    assert!(
        entries[0].tool_done,
        "replayed start must not regress completion"
    );
    event.operation_id = "op_b".into();
    reduce_tool_event(&mut entries, &event);
    assert_eq!(
        entries.len(),
        2,
        "identical args and reused provider ID are not identity"
    );
    assert_ne!(entries[0].id, entries[1].id);
    event.operation_id = "op_a".into();
    event.attempt = Some(2);
    reduce_tool_event(&mut entries, &event);
    assert_eq!(entries.len(), 2);
    assert!(!entries[0].tool_done);
    event.event_type = "tool_call_finished".into();
    event.attempt = Some(1);
    reduce_tool_event(&mut entries, &event);
    assert!(
        !entries[0].tool_done,
        "stale finish cannot close newer attempt"
    );
}

#[test]
fn running_history_identity_matches_live_replay() {
    let history = vec![Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
            invocation_id: "op_running".into(),
            invocation_attempt: Some(1),
            ..Block::default()
        }],
        ..Message::default()
    }];
    let (mut entries, _) = entries_from_history(&history);
    assert_eq!(entries[0].id, "operation:op_running");
    let event = otto_core::wire::events::WireEvent {
        event_type: "tool_call_started".into(),
        operation_id: "op_running".into(),
        tool_call_id: "c1".into(),
        tool_name: "read".into(),
        attempt: Some(1),
        ..Default::default()
    };
    reduce_tool_event(&mut entries, &event);
    assert_eq!(entries.len(), 1);
}
