//! Tests for the status line, footer and busy indicators.

use super::*;

/// The model id is the longest field of the footer's fixed prefix, so the
/// token and cache totals used to sit past the right edge of any terminal
/// that a long id like `deepseek-v4-flash` did not leave room on, where
/// ratatui clips them.
#[tokio::test]
async fn a_long_model_id_keeps_the_token_and_cache_totals_visible() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.info.model = "deepseek-v4-flash".to_string();
    app.usage = Usage {
        input_tokens: 93_498 + 7_091_584,
        output_tokens: 37_475,
        cached_input_tokens: 7_091_584,
    };
    app.cache_breakdown_complete = Some(true);

    for width in [72u16, 80, 88, 100] {
        let rows = screen_rows(&app, width, MIN_TERMINAL_HEIGHT);
        let footer = rows.last().expect("footer row");
        assert!(
            footer.contains("tokens 7.2M/37.5k") && footer.contains("(cached 7.1M, 98.7%)"),
            "{width} columns: {footer}"
        );
    }
}

#[tokio::test]
async fn footer_omits_the_profile_and_uses_compact_status_labels() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.info.profile = "terra".to_string();
    app.info.model = "gpt-5.6-terra".to_string();
    app.info.thinking.clear();
    app.info.sandbox = testutil::seatbelt_info(crate::runtime::info::SandboxNetwork::Allowed);

    let footer = footer_text(&app);

    assert!(
        footer.starts_with("gpt-5.6-terra | AUTO | tokens 0/0 | "),
        "{footer}"
    );
    assert!(
        footer.contains(" | sb · write · net | default | "),
        "{footer}"
    );
    assert!(!footer.contains("terra/gpt-5.6-terra"), "{footer}");
    assert!(!footer.contains("think default"), "{footer}");
    assert!(!footer.contains("workspace-write"), "{footer}");
    assert!(!footer.contains("network allowed"), "{footer}");
}

#[tokio::test]
async fn empty_startup_transcript_shows_the_otto_logo() {
    let (_workspace, _sessions, app) = app_fixture().await;

    let screen = screen_rows(&app, 80, 20).join("\n");

    assert!(screen.contains("____  __  __"), "{screen}");
    assert!(screen.contains("/ __ \\/ /_/ /____"), "{screen}");
    assert!(screen.contains("/ /_/ / __/ __/ __ \\"), "{screen}");
    assert!(screen.contains("\\____/\\__/\\__/\\____/"), "{screen}");
}

/// The spinner advances, but model phases never expose elapsed duration.
#[test]
fn the_status_line_animates_without_thinking_or_turn_duration() {
    let at = |_phase_ms, turn_ms| {
        line_text(&thinking_line(&TurnStatus {
            phase: "reasoning".into(),
            turn_elapsed: Duration::from_millis(turn_ms),
        }))
    };

    assert_eq!(at(0, 0), "⠋ reasoning");
    assert_eq!(at(0, 100), "⠙ reasoning");
    assert_eq!(at(1_000, 5_000), "⠋ reasoning");
}

/// A turn streams nothing until the model's first token, so without this
/// line the transcript sits unchanged and the terminal looks hung.
#[tokio::test]
async fn a_running_turn_shows_the_thinking_line_under_the_transcript() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries.push(Entry {
        kind: Some(EntryKind::User),
        raw: "hello otto".to_string(),
        ..Default::default()
    });

    let idle = rendered(&app, 50, 10);
    app.start_turn();
    let busy = rendered(&app, 50, 10);
    app.apply_event(&wire_frame(&Event::ToolCallStarted {
        operation_id: otto_core::model::OperationId::new("op_test").expect("operation id"),
        attempt: 1,
        tool_name: "bash".into(),
        tool_call_id: "c1".into(),
        arguments: r#"{"command":"ls"}"#.into(),
    }));
    let running = rendered(&app, 50, 10);
    app.end_turn();

    assert!(!idle.contains("waiting"), "idle transcript:\n{idle}");
    assert!(
        busy.contains("waiting for model"),
        "running transcript:\n{busy}"
    );
    assert!(
        running.contains("running bash") && !running.contains(r#"{"command":"ls"}"#),
        "tool transcript:\n{running}"
    );
    assert!(!rendered(&app, 50, 10).contains("turn"));
}

#[tokio::test]
async fn the_busy_composer_title_says_which_turn_enter_queues_for() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.start_turn();
    assert!(rendered(&app, 80, 12).contains("Enter queues for this turn"));

    app.attached = true;
    assert!(rendered(&app, 80, 12).contains("Enter queues the next turn"));
}

#[tokio::test]
async fn footer_shows_runtime_and_session_status() {
    let (_workspace, _sessions, app) = app_fixture().await;
    let session_id = app.info.session_id.clone();

    let content = rendered(&app, 160, MIN_TERMINAL_HEIGHT);

    assert!(content.contains("gpt-alpha"), "{content}");
    assert!(content.contains("default"), "{content}");
    assert!(
        content.contains("bash disabled · sandbox unavailable"),
        "{content}"
    );
    assert!(content.contains("tokens 0/0"), "{content}");
    assert!(content.contains(&session_id), "{content}");

    let narrow = rendered(&app, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT);
    assert!(narrow.contains("gpt-alpha"), "{narrow}");
    assert!(narrow.contains("tokens 0/0"), "{narrow}");
}
