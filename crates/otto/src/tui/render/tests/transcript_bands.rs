//! Tests for prompts, replies and tool calls drawn in the transcript.

use super::*;

/// The drawn frame, not the row builder: a prompt, the reply after it,
/// and a tool call each reach the screen under their own marker, so a
/// turn boundary is visible in the rendering the reader actually sees.
/// `transcript`'s own tests cover the row rules.
#[tokio::test]
async fn a_drawn_turn_shows_a_prompt_a_reply_and_a_tool_call_apart() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = vec![
        Entry {
            kind: Some(EntryKind::User),
            raw: "hello otto".to_string(),
            ..Default::default()
        },
        Entry {
            kind: Some(EntryKind::Tool),
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            tool_args: r#"{"command":"ls"}"#.to_string(),
            tool_output: "a.txt".to_string(),
            tool_done: true,
            ..Default::default()
        },
        Entry {
            kind: Some(EntryKind::Assistant),
            raw: "the reply".to_string(),
            ..Default::default()
        },
    ];

    let screen = rendered(&app, 60, 12);

    assert!(screen.contains("❯ hello otto"), "{screen}");
    assert!(screen.contains("\u{23fa} bash"), "{screen}");
    assert!(screen.contains("\u{23bf} completed · 1 lines"), "{screen}");
    assert!(
        !screen.contains("a.txt"),
        "raw output must stay folded: {screen}"
    );
    assert!(screen.contains("\u{23fa} the reply"), "{screen}");
}

/// A prompt too wide for the terminal keeps one marker and an aligned
/// indent on the rows the wrap produced, which is what `Paragraph::wrap`
/// used to lose entirely.
#[tokio::test]
async fn a_wrapped_prompt_is_marked_once_and_stays_aligned() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = vec![Entry {
        kind: Some(EntryKind::User),
        raw: "alpha bravo charlie delta echo foxtrot golf".to_string(),
        ..Default::default()
    }];

    let rows = screen_rows(&app, 44, 10);
    let prompt: Vec<&String> = rows.iter().filter(|row| !row.is_empty()).take(2).collect();

    assert_eq!(prompt.len(), 2, "{rows:?}");
    assert!(prompt[0].starts_with("❯ "), "{rows:?}");
    assert!(prompt[1].starts_with("  "), "{rows:?}");
    assert!(!prompt[1].trim_start().starts_with('\u{276f}'), "{rows:?}");
}

/// The band is the thing a reader scrolls back to find, so it has to
/// survive the drawn frame: `Paragraph` colours only the cells a span
/// covers, and it stops at the transcript's own width, never running
/// into the side margin.
#[tokio::test]
async fn a_drawn_prompt_is_banded_across_the_content_width() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = vec![Entry {
        kind: Some(EntryKind::User),
        raw: "hi".to_string(),
        ..Default::default()
    }];
    let (width, height) = (40u16, 10u16);

    let buffer = drawn(&app, width, height);
    let row = banded_row(&buffer, width, height).expect("the prompt row is on screen");
    for x in SIDE_MARGIN..width - SIDE_MARGIN {
        let cell = buffer.cell((x, row)).expect("cell in bounds");
        assert_eq!(
            cell.bg,
            transcript::PROMPT_BACKGROUND,
            "column {x} is not banded"
        );
    }
    for x in [0, width - 1] {
        let cell = buffer.cell((x, row)).expect("cell in bounds");
        assert_ne!(
            cell.bg,
            transcript::PROMPT_BACKGROUND,
            "the band ran into the margin"
        );
    }
}

/// The same band under wide glyphs.
///
/// The walk steps by display width, as [`super::super::super::selection`] does:
/// `ratatui` writes a wide character into the first of the cells it
/// covers and its frame diff never sends the rest, so `TestBackend` holds
/// a default-coloured cell where a terminal paints the glyph's own
/// background. Reading those cells back would assert on an artifact of
/// the test backend rather than on what the reader sees.
#[tokio::test]
async fn a_drawn_cjk_prompt_is_banded_too() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.entries = vec![Entry {
        kind: Some(EntryKind::User),
        raw: "集群数量极多".to_string(),
        ..Default::default()
    }];
    let (width, height) = (44u16, 12u16);

    let buffer = drawn(&app, width, height);
    let row = banded_row(&buffer, width, height).expect("the prompt row is on screen");
    let mut x = SIDE_MARGIN;
    while x < width - SIDE_MARGIN {
        let cell = buffer.cell((x, row)).expect("cell in bounds");
        assert_eq!(
            cell.bg,
            transcript::PROMPT_BACKGROUND,
            "column {x} breaks the band"
        );
        x += cell.symbol().width().max(1) as u16;
    }
}
