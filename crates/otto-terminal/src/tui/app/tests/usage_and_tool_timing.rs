//! Tests for tool timing, provider recovery and usage footers.

use super::*;

#[test]
fn tool_duration_survives_same_session_refresh_but_not_session_replacement() {
    use otto_core::model::{Block, BlockType, Message, Role, ToolResultMetadata};
    let history = vec![Message {
        role: Role::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            tool_call_id: "provider_refresh".into(),
            operation_metadata: Some(ToolResultMetadata {
                operation_id: Some(otto_core::model::OperationId::new("op_refresh").unwrap()),
                disposition: otto_core::model::OperationDisposition::Succeeded,
                effect_certainty: otto_core::model::EffectCertainty::Completed,
                stop_reason: None,
            }),
            ..Default::default()
        }],
        ..Default::default()
    }];
    let remote = Remote::offline(history);
    let mut app = App::new(&Backend::Attach(&remote));
    assert!(app.entries[0].tool_done);
    app.entries[0].tool_elapsed = Some(Duration::from_secs(3));
    app.refresh(&Backend::Attach(&remote));
    assert_eq!(app.entries[0].tool_elapsed, Some(Duration::from_secs(3)));
    app.info.session_id = "other-session".into();
    app.refresh(&Backend::Attach(&remote));
    assert_eq!(app.entries[0].tool_elapsed, None);
}

#[test]
fn attached_tool_frames_cannot_measure_execution_from_replay_delivery() {
    let (mut app, _) = app();
    app.attached = true;
    app.apply_event(&WireEvent {
        event_type: "tool_call_started".into(),
        operation_id: "op_attach".into(),
        ..Default::default()
    });
    assert_eq!(app.entries.last().unwrap().tool_started, None);
}

#[test]
fn provider_recovery_separates_partial_output_in_local_and_attached_tui() {
    for attached in [false, true] {
        let (mut app, _remote) = app();
        app.attached = attached;
        for json in [
            r#"{"type":"text_delta","text":"partial"}"#,
            r#"{"type":"provider_retry","retry":{"attempt":2,"max_attempts":7,"interrupted_output":true}}"#,
            r#"{"type":"text_delta","text":"replacement"}"#,
        ] {
            app.apply_event(&serde_json::from_str(json).unwrap());
        }
        let entries = &app.entries[app.entries.len() - 3..];
        assert_eq!(entries[0].raw, "partial");
        assert_eq!(entries[1].kind, Some(EntryKind::System));
        assert_eq!(entries[1].raw, transcript::INTERRUPTED_OUTPUT_NOTICE);
        assert_eq!(entries[2].raw, "replacement");
    }
}

#[test]
fn provider_usage_updates_the_footer_during_a_turn() {
    let (mut app, _) = app();
    app.usage = Usage {
        input_tokens: 10,
        output_tokens: 2,
        cached_input_tokens: 4,
        ..Default::default()
    };

    app.apply_event(&WireEvent {
        event_type: "provider_usage".into(),
        usage: Some(Usage {
            input_tokens: 7,
            output_tokens: 3,
            cached_input_tokens: 5,
            ..Default::default()
        }),
        usage_present: Some(true),
        ..Default::default()
    });
    app.apply_event(&WireEvent {
        event_type: "provider_usage".into(),
        usage: Some(Usage::default()),
        usage_present: Some(false),
        ..Default::default()
    });

    assert_eq!(
        app.usage,
        Usage {
            input_tokens: 17,
            output_tokens: 5,
            cached_input_tokens: 9,
            ..Default::default()
        }
    );
}

#[test]
fn a_task_notification_decides_whether_the_cache_share_is_shown() {
    let (mut app, _) = app();
    app.cache_breakdown_complete = Some(true);

    // A finished sub-agent that never reported the cached breakdown: the
    // footer must stop presenting the share as measured, because the
    // session totals now include tokens its cache count cannot explain.
    app.apply_event(&WireEvent {
        event_type: "notification".into(),
        task_id: "t1".into(),
        text: "[task-notification] task t1 succeeded".into(),
        usage: Some(Usage {
            input_tokens: 40,
            output_tokens: 5,
            cached_input_tokens: 0,
            ..Default::default()
        }),
        usage_present: Some(true),
        ..Default::default()
    });
    assert_eq!(app.cache_breakdown_complete, Some(false));
}

#[test]
fn a_measured_task_notification_keeps_the_cache_share() {
    let (mut app, _) = app();
    app.cache_breakdown_complete = Some(true);

    app.apply_event(&WireEvent {
        event_type: "notification".into(),
        task_id: "t1".into(),
        text: "[task-notification] task t1 succeeded".into(),
        usage: Some(Usage {
            input_tokens: 40,
            output_tokens: 5,
            cached_input_tokens: 30,
            ..Default::default()
        }),
        usage_present: Some(true),
        cached_input_tokens_present: Some(true),
        ..Default::default()
    });

    assert_eq!(app.cache_breakdown_complete, Some(true));
}
