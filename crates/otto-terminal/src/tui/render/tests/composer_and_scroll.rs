//! Tests for manual scroll, suggestions and caret placement.

use super::*;

#[tokio::test]
async fn manual_scroll_moves_up_from_the_bottom() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.push_system(
        (0..20)
            .map(|line| format!("line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n\n"),
    );

    assert!(rendered(&app, MIN_TERMINAL_WIDTH, 12).contains("line 19"));
    app.scroll = Some(3);
    let scrolled = rendered(&app, MIN_TERMINAL_WIDTH, 12);
    assert!(!scrolled.contains("line 19"), "{scrolled}");
}

/// A manual scroll is an absolute top offset, so streamed output
/// appended below it leaves the rows being read exactly where they are
/// instead of pushing them off the top.
#[tokio::test]
async fn a_scrolled_view_stays_put_while_output_streams_in() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.push_system(
        (0..20)
            .map(|line| format!("line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    // Lays out `max_scroll`, which the first scroll key reads.
    let _ = rendered(&app, MIN_TERMINAL_WIDTH, 12);
    app.handle_scroll_key(&KeyCode::PageUp.into());
    let before = screen_rows(&app, MIN_TERMINAL_WIDTH, 12);
    assert!(
        before.iter().any(|row| row.contains("line 05")),
        "{before:?}"
    );
    let previous_bottom = app.max_scroll.get();
    let transcript_rows = |rows: Vec<String>| {
        rows.into_iter()
            .enumerate()
            .filter(|(_, row)| row.contains("line "))
            .collect::<Vec<_>>()
    };

    for index in 0..10 {
        app.apply_event(&wire_frame(&Event::TextDelta {
            text: format!("streamed {index}\n"),
        }));
    }

    let after = screen_rows(&app, MIN_TERMINAL_WIDTH, 12);
    assert!(
        app.max_scroll.get() > previous_bottom,
        "transcript must grow"
    );
    assert_eq!(
        transcript_rows(before),
        transcript_rows(after),
        "visible transcript rows must stay at the same offsets"
    );
}

/// The view half of command completion: a `/` prefix lists the matching
/// commands with their descriptions and leaves the others out.
#[tokio::test]
async fn the_selected_suggestion_row_is_highlighted() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.input = "/s".chars().collect();
    app.cursor = app.input.len();
    app.suggestion = 1;

    let highlighted = highlighted_rows(&app, 100, 20);

    assert_eq!(highlighted.len(), 1, "{highlighted:?}");
    assert!(
        highlighted[0].trim_start().starts_with("/sandbox"),
        "{highlighted:?}"
    );
}

/// A picker holds up to `PICKER_LIST_LIMIT` (50) rows, more than a popup
/// ever shows, so a selection below the fold has to scroll into view.
#[tokio::test]
async fn the_caret_follows_the_display_width_of_wide_characters() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.input = "测试ab".chars().collect();
    app.cursor = app.input.len();

    let (x, y) = cursor(&app, 100, 20);

    // One line of input fills the composer's 1-row minimum, so the box is
    // 3 rows (1 inner + 2 borders) and the caret sits on the
    // first inner row, `composer height` rows above the bottom of the
    // frame (the status line takes the last row; there is no panel row).
    assert_eq!((x, y), (SIDE_MARGIN + 1 + 6, 20 - 3));
}

/// A hard line break moves the caret to the next composer row.
#[tokio::test]
async fn the_caret_follows_an_embedded_newline() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.input = "ab\ncd".chars().collect();
    app.cursor = app.input.len();

    let (x, y) = cursor(&app, 100, 20);

    // Two lines grow the composer to 4 rows including borders; the caret
    // follows to the second inner row.
    assert_eq!((x, y), (SIDE_MARGIN + 1 + 2, 20 - 4 + 1));
}

/// Past [`INPUT_BOX_THRESHOLD`] rows the composer stops growing, so it
/// has to scroll: the tail of the value and the caret both stay inside
/// the box instead of the first rows being pinned and the caret
/// wandering onto the border.
#[tokio::test]
async fn a_value_taller_than_the_composer_scrolls_its_tail_into_view() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.input = format!("{}END", "x".repeat(96 * 13)).chars().collect();
    app.cursor = app.input.len();

    let height = 30;
    let content = rendered(&app, 100, height);
    let (x, y) = cursor(&app, 100, height);

    assert!(content.contains("END"), "{content}");
    // The box is `INPUT_BOX_THRESHOLD` text rows plus two borders. With
    // no panel row, the composer's bottom border sits directly above the
    // status line (the frame's last row), so its last text row is 3 rows
    // above the bottom of the frame.
    assert_eq!((x, y), (SIDE_MARGIN + 1 + 3, height - 3));
}
