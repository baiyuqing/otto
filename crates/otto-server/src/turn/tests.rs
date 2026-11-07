use super::*;
use otto_core::agent::events::ApiStatus;
use otto_core::tool::ToolResult;
use std::sync::Arc;

fn turn() -> (Arc<Turn>, Arc<Metrics>) {
    (
        Arc::new(Turn::new(
            "t1".to_string(),
            TRIGGER_USER,
            CancellationToken::new(),
        )),
        Arc::new(Metrics::new()),
    )
}

fn usage(input: i64, output: i64, cached: i64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
        ..Default::default()
    }
}

#[test]
fn provider_recovery_summary_keeps_completed_steps_and_only_the_replacement() {
    let (turn, metrics) = turn();
    let mut emit = turn.emitter(&metrics);
    emit(Event::TextDelta {
        text: "earlier ✓".into(),
    });
    emit(Event::ProviderUsage {
        usage: Default::default(),
        present: false,
        cached_input_tokens_present: false,
    });
    emit(Event::TextDelta {
        text: "partial 一".into(),
    });
    emit(Event::ProviderRetry {
        operation_id: otto_core::model::OperationId::new("retry-test").unwrap(),
        attempt: 2,
        max_attempts: 7,
        delay: Duration::ZERO,
        reason: "connection interrupted".into(),
        interrupted_output: true,
    });
    emit(Event::TextDelta {
        text: "final".into(),
    });
    assert_eq!(turn.summary().text, "earlier ✓final");
    let (events, _) = turn.snapshot(0);
    assert!(
        events
            .iter()
            .any(|event| event.retry.as_ref().is_some_and(|r| r.interrupted_output))
    );
}

#[test]
fn emit_accumulates_text_and_usage() {
    let (turn, metrics) = turn();
    let mut emit = turn.emitter(&metrics);
    emit(Event::AgentStarted);
    emit(Event::TextDelta {
        text: "hel".to_string(),
    });
    emit(Event::TextDelta {
        text: "lo".to_string(),
    });
    emit(Event::ProviderUsage {
        usage: usage(1, 2, 3),
        present: true,
        cached_input_tokens_present: false,
    });
    emit(Event::ProviderUsage {
        usage: usage(10, 20, 0),
        present: true,
        cached_input_tokens_present: false,
    });
    drop(emit);

    let (events, done) = turn.snapshot(0);
    assert!(!done);
    assert_eq!(events.len(), 5);
    assert_eq!(events[0].turn_id, "t1");
    assert_eq!(events[1].turn_id, "");

    let summary = turn.summary();
    assert_eq!(summary.text, "hello");
    assert_eq!(summary.usage, usage(11, 22, 3));
    assert!(summary.usage_present);
    assert_eq!(summary.status, TURN_RUNNING);
    assert!(summary.finished_at.is_none());
    assert!(!turn.is_done());
}

#[test]
fn delivered_guidance_remains_a_user_message_in_live_and_replayed_frames() {
    let (turn, metrics) = turn();
    let text = "我发的 queued input 又看不到了";
    let mut emit = turn.emitter(&metrics);
    emit(Event::Notification {
        input_id: String::new(),
        input_provenance: None,
        kind: Some(otto_core::agent::inbox::NotificationKind::UserMessage),
        task_id: String::new(),
        text: text.into(),
        usage: Usage::default(),
        present: false,
        cached_input_tokens_present: false,
    });
    drop(emit);
    let (live, _) = turn.snapshot(0);
    assert_eq!(live, vec![WireEvent::user_message(text, false)]);
    turn.finish(None, false);
    let (replayed, terminal) = turn.snapshot(0);
    assert!(terminal);
    assert_eq!(replayed[0], WireEvent::user_message(text, false));
    assert_eq!(
        replayed
            .iter()
            .filter(|event| event.event_type == "user_message")
            .count(),
        1
    );
    assert!(
        turn.snapshot(1)
            .0
            .iter()
            .all(|event| event.event_type != "user_message")
    );
}

#[test]
fn emit_accumulates_notification_usage() {
    let (turn, metrics) = turn();
    let mut emit = turn.emitter(&metrics);
    emit(Event::ProviderUsage {
        usage: usage(1, 2, 3),
        present: true,
        cached_input_tokens_present: false,
    });
    emit(Event::Notification {
        input_id: String::new(),
        input_provenance: None,
        kind: None,
        task_id: "t1".to_string(),
        text: "[task-notification] task t1 succeeded".to_string(),
        usage: usage(10, 20, 1),
        present: true,
        cached_input_tokens_present: true,
    });
    drop(emit);
    assert_eq!(turn.summary().usage, usage(11, 22, 4));
}

#[test]
fn usage_presence_distinguishes_missing_from_explicit_zero() {
    let (turn, metrics) = turn();
    let mut emit = turn.emitter(&metrics);
    emit(Event::ProviderUsage {
        usage: Usage::default(),
        present: false,
        cached_input_tokens_present: false,
    });
    assert!(!turn.summary().usage_present);
    emit(Event::ProviderUsage {
        usage: Usage::default(),
        present: true,
        cached_input_tokens_present: false,
    });
    drop(emit);
    let summary = turn.summary();
    assert!(summary.usage_present);
    assert_eq!(summary.usage, Usage::default());
}

#[test]
fn a_provider_api_call_is_measured_but_never_buffered() {
    let (turn, metrics) = turn();
    let mut emit = turn.emitter(&metrics);
    emit(Event::ProviderApiCall {
        operation_id: otto_core::model::OperationId::new("provider_op").expect("operation id"),
        kind: otto_core::agent::ProviderOperationKind::Ordinary,
        provider: "chatgpt".to_string(),
        model: "gpt-5".to_string(),
        duration: std::time::Duration::from_millis(1500),
        attempts: 1,
        status: ApiStatus::Ok,
        outcome: otto_core::model::OperationOutcome {
            disposition: otto_core::model::OperationDisposition::Succeeded,
            effect_certainty: otto_core::model::EffectCertainty::Completed,
            stop_reason: None,
        },
    });
    drop(emit);
    assert!(turn.snapshot(0).0.is_empty());
    assert!(metrics.render().contains(
        r#"otto_provider_api_requests_total{provider="chatgpt",model="gpt-5",status="ok"} 1"#
    ));
    assert!(
        metrics
            .render()
            .contains(r#"otto_provider_attempts_total{provider="chatgpt",model="gpt-5"} 1"#)
    );
    assert!(metrics.render().contains(
        r#"otto_provider_operations_total{provider="chatgpt",model="gpt-5",disposition="succeeded",certainty="completed",stop_reason="none"} 1"#
    ));
}

#[test]
fn a_finished_tool_call_is_measured_and_buffered() {
    let (turn, metrics) = turn();
    let mut emit = turn.emitter(&metrics);
    let operation_id = otto_core::model::OperationId::new("op_test").unwrap();
    emit(Event::ToolCallStarted {
        operation_id: operation_id.clone(),
        attempt: 1,
        tool_name: "bash".to_string(),
        tool_call_id: "c1".to_string(),
        arguments: String::new(),
    });
    emit(Event::ToolCallFinished {
        operation_id,
        attempt: 1,
        tool_name: "bash".to_string(),
        tool_call_id: "c1".to_string(),
        result: ToolResult {
            content: "ok".to_string(),
            ..ToolResult::default()
        },
        outcome: otto_core::model::OperationOutcome {
            disposition: otto_core::model::OperationDisposition::Succeeded,
            effect_certainty: otto_core::model::EffectCertainty::Completed,
            stop_reason: None,
        },
    });
    drop(emit);
    assert_eq!(turn.snapshot(0).0.len(), 2);
    assert!(
        metrics
            .render()
            .contains(r#"otto_tool_calls_total{tool="bash",status="ok"} 1"#)
    );
    assert!(
        metrics
            .render()
            .contains(r#"otto_operation_attempts_total{kind="tool_call",tool="bash"} 1"#)
    );
    assert!(!metrics.render().contains("op_test"));
    assert!(!metrics.render().contains("content=\"ok\""));
}

#[test]
fn an_unknown_effect_is_counted_without_operation_content() {
    let (turn, metrics) = turn();
    let mut emit = turn.emitter(&metrics);
    let operation_id = otto_core::model::OperationId::new("op_secretish").unwrap();
    emit(Event::ToolCallStarted {
        operation_id: operation_id.clone(),
        attempt: 1,
        tool_name: "mcp_call_tool".to_string(),
        tool_call_id: "c1".to_string(),
        arguments: r#"{"secret":"do-not-store"}"#.to_string(),
    });
    emit(Event::ToolCallFinished {
        operation_id,
        attempt: 1,
        tool_name: "mcp_call_tool".to_string(),
        tool_call_id: "c1".to_string(),
        result: ToolResult::error("raw upstream body"),
        outcome: otto_core::model::OperationOutcome {
            disposition: otto_core::model::OperationDisposition::Interrupted,
            effect_certainty: otto_core::model::EffectCertainty::Unknown,
            stop_reason: Some(otto_core::model::OperationStopReason::TransportLost),
        },
    });
    drop(emit);

    let rendered = metrics.render();
    assert!(rendered.contains(
        r#"otto_operation_unknown_effects_total{kind="tool_call",tool="mcp_call_tool",disposition="interrupted"} 1"#
    ));
    for sensitive in ["op_secretish", "do-not-store", "raw upstream body"] {
        assert!(!rendered.contains(sensitive), "metrics leaked {sensitive}");
    }
}

#[test]
fn provider_tokens_are_counted_by_kind() {
    let (turn, metrics) = turn();
    let mut emit = turn.emitter(&metrics);
    emit(Event::ProviderUsage {
        usage: usage(5, 7, 0),
        present: true,
        cached_input_tokens_present: false,
    });
    drop(emit);
    let body = metrics.render();
    assert!(body.contains(r#"otto_provider_tokens_total{kind="input"} 5"#));
    assert!(body.contains(r#"otto_provider_tokens_total{kind="output"} 7"#));
}

#[test]
fn finish_sets_the_terminal_status() {
    for (error, canceled, want, want_error) in [
        (None, false, TURN_OK, false),
        (Some("canceled".to_string()), true, TURN_CANCELED, false),
        (Some("boom".to_string()), false, TURN_ERROR, true),
    ] {
        let (turn, _) = turn();
        turn.finish(error, canceled);
        assert!(turn.is_done());
        let summary = turn.summary();
        assert_eq!(summary.status, want);
        assert!(summary.finished_at.is_some());
        assert_eq!(!summary.error.is_empty(), want_error);
    }
}

#[test]
fn a_snapshot_starts_at_an_arbitrary_sequence() {
    let (turn, metrics) = turn();
    let mut emit = turn.emitter(&metrics);
    for index in 0..5 {
        emit(Event::TextDelta {
            text: index.to_string(),
        });
    }
    drop(emit);
    let (events, done) = turn.snapshot(3);
    assert!(!done);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].text, "3");
    assert_eq!(events[1].text, "4");
    assert!(turn.snapshot(5).0.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_observe_every_event() {
    const COUNT: usize = 50;
    let (turn, metrics) = turn();

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let turn = Arc::clone(&turn);
            tokio::spawn(async move {
                let mut changed = turn.subscribe();
                let mut seen = Vec::new();
                loop {
                    let (events, done) = turn.snapshot(seen.len());
                    seen.extend(events);
                    if done && seen.len() > COUNT {
                        return seen.len();
                    }
                    if changed.changed().await.is_err() {
                        return seen.len();
                    }
                }
            })
        })
        .collect();

    // Let every reader subscribe before the first event lands.
    tokio::task::yield_now().await;
    let mut emit = turn.emitter(&metrics);
    for index in 0..COUNT {
        emit(Event::TextDelta {
            text: index.to_string(),
        });
    }
    drop(emit);
    turn.finish(None, false);

    for reader in readers {
        // The deltas plus the closing `turn_end` frame.
        assert_eq!(reader.await.expect("reader"), COUNT + 1);
    }
}

mod cache_usage_tests;
