//! Tests for minimum size, margins and extreme terminal sizes.

use super::*;

#[tokio::test]
async fn narrower_or_shorter_than_the_minimum_shows_the_resize_message() {
    let (_workspace, _sessions, app) = app_fixture().await;

    let narrow = rendered(&app, MIN_TERMINAL_WIDTH - 1, MIN_TERMINAL_HEIGHT);
    let short = rendered(&app, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT - 1);

    assert!(narrow.contains("Terminal too small"));
    assert!(short.contains("Terminal too small"));
}

#[tokio::test]
async fn exactly_the_minimum_size_shows_the_normal_layout() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.input = "visible draft".chars().collect();
    app.cursor = app.input.len();

    let content = rendered(&app, MIN_TERMINAL_WIDTH, MIN_TERMINAL_HEIGHT);

    assert!(!content.contains("Terminal too small"));
    assert!(
        content.contains("visible draft"),
        "normal layout must render the draft: {content}"
    );
}

#[tokio::test]
async fn normal_layout_leaves_side_margins() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.push_system("hello from otto");
    app.input = "draft".chars().collect();
    app.cursor = app.input.len();

    let width = 80;
    let height = 20;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, &app)).expect("draw");
    let buffer = terminal.backend().buffer();

    for y in 0..height {
        assert_eq!(
            buffer.cell((0, y)).expect("left margin").symbol(),
            " ",
            "row {y} should leave the left edge empty"
        );
        assert_eq!(
            buffer.cell((width - 1, y)).expect("right margin").symbol(),
            " ",
            "row {y} should leave the right edge empty"
        );
    }
}

/// No-panic smoke test at extreme terminal sizes. Ratatui's `Buffer` makes
/// staying within bounds structurally true (see this test module's doc
/// comment above), so what is left to check is that drawing at these sizes
/// does not panic.
#[tokio::test]
async fn draw_does_not_panic_at_extreme_terminal_sizes() {
    let (_workspace, _sessions, app) = app_fixture().await;

    for (width, height) in [(1u16, 1u16), (2, 1), (10, 3), (39, 7)] {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw at extreme size");
    }
}

/// Confirms Otto's own transcript text — a run of wide (CJK/emoji)
/// characters and a long unbroken ASCII token with no wrap points — feeds
/// into `Paragraph::wrap` without panicking at a narrow width. This does
/// not re-test ratatui's own wrapping algorithm, only that Otto's text
/// reaches it intact.
#[tokio::test]
async fn wide_characters_and_unbroken_tokens_wrap_without_panicking() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.push_system("测试测试测试测试测试测试测试测试测试测试😀😀😀😀😀😀😀😀😀😀😀😀");
    app.push_system("x".repeat(200));

    let backend = TestBackend::new(MIN_TERMINAL_WIDTH, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| draw(frame, &app))
        .expect("draw with wide/unbroken content");
}
