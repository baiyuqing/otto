//! Tests for the status line, footer and busy indicators.

use super::*;

#[tokio::test]
async fn observer_hint_uses_the_configured_entry_key_not_the_back_key() {
    use crate::tui::observer::bindings::Bindings;
    use otto_core::config::ui::ObserverBindings;

    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.tasks = vec![running_task("t1", "review", "check the diff")];
    for (toggle, open, back, expected) in [
        ("F6", "", "Esc", "F6"),
        (" Alt+o ", "F7", "Esc", "Alt+o"),
        ("", "Ctrl+Left", "Ctrl+Right", "Ctrl+Left"),
    ] {
        app.observer.bindings = Bindings::parse(&ObserverBindings {
            toggle: toggle.into(),
            open: open.into(),
            back: back.into(),
        })
        .expect("valid observer bindings");
        let footer = footer_text(&app);
        assert!(
            footer.starts_with(&format!("Press {expected} to view sub-agent observer | ")),
            "{footer}"
        );
        assert!(!footer.contains(back), "{footer}");
    }
}

#[tokio::test]
async fn observer_hint_follows_task_lifecycle_and_is_absent_in_the_observer() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    assert!(!footer_text(&app).contains("sub-agent observer"));
    for status in [TaskStatus::Queued, TaskStatus::Running] {
        app.tasks = vec![Task {
            status,
            ..Task::default()
        }];
        assert!(footer_text(&app).contains("Press F6 to view sub-agent observer"));
    }
    app.observer.active = true;
    assert!(!footer_text(&app).contains("sub-agent observer"));
    assert!(!rendered(&app, 160, 20).contains("Press F6 to view sub-agent observer"));
    app.observer.active = false;
    app.tasks = vec![
        Task {
            status: TaskStatus::Succeeded,
            ..Task::default()
        },
        Task {
            status: TaskStatus::Failed,
            ..Task::default()
        },
        Task {
            status: TaskStatus::Canceled,
            ..Task::default()
        },
    ];
    assert!(!footer_text(&app).contains("sub-agent observer"));
}

#[tokio::test]
async fn many_tasks_show_one_footer_hint_without_transcript_messages() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.tasks = (0..5)
        .map(|index| running_task(&format!("t{index}"), "review", "check the diff"))
        .collect();
    let entries_before = app.entries.len();
    for _ in 0..2 {
        let rows = screen_rows(&app, 180, 20);
        let hint = "Press F6 to view sub-agent observer";
        assert_eq!(rows.join("\n").matches(hint).count(), 1);
        assert!(rows.last().expect("footer").contains(hint));
        assert!(!above_status_line(&rows).contains(hint));
    }
    assert_eq!(app.entries.len(), entries_before);
}

#[tokio::test]
async fn observer_hint_keeps_status_first_and_clips_without_adding_rows() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.tasks = vec![running_task("t1", "review", "check the diff")];
    app.status = Some("Disconnected".into());
    let footer = footer_text(&app);
    assert!(footer.starts_with("Disconnected | Press F6 to view sub-agent observer | "));
    assert!(footer.contains("gpt-alpha"));
    assert!(footer.contains("tokens 0/0"));
    for width in [MIN_TERMINAL_WIDTH, 80, 180] {
        let rows = screen_rows(&app, width, MIN_TERMINAL_HEIGHT);
        assert_eq!(rows.len(), MIN_TERMINAL_HEIGHT as usize);
        let status_row = rows.last().expect("footer");
        assert!(
            status_row.starts_with("Disconnected | Press F6"),
            "{status_row}"
        );
        assert!(rows[rows.len() - 2].contains("t1"), "{rows:?}");
        assert!(rows[rows.len() - 3].contains('─'), "{rows:?}");
    }
}

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
async fn footer_omits_the_provider_and_uses_compact_status_labels() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.info.model_selector = "chatgpt/gpt-5.6-terra".to_string();
    app.info.model = "gpt-5.6-terra".to_string();
    app.info.thinking.clear();
    app.info.sandbox = fixtures::seatbelt_info(crate::runtime::info::SandboxNetwork::Allowed);

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
