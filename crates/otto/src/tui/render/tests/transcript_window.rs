//! The transcript is drawn from a window of cached rows; the frame must equal
//! drawing the full row list with a scroll offset.

use super::*;

/// Reference: the rows `draw_transcript` showed before the cache, a slice of
/// the pure transcript function at the clamped scroll offset.
fn expected_transcript_rows(app: &App, width: u16, height: u16) -> Vec<String> {
    let full = transcript::lines(&app.entries, app.show_details, width as usize);
    let bottom = full.len().saturating_sub(height as usize);
    let top = app.scroll.map_or(bottom, |top| (top as usize).min(bottom));
    full.iter()
        .skip(top)
        .take(height as usize)
        .map(line_text)
        .collect()
}

fn transcript_rows_on_screen(app: &App, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| draw_transcript(frame, app, Rect::new(0, 0, width, height)))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .filter_map(|x| buffer.cell((x, y)))
                .map(|cell| cell.symbol())
                .collect::<String>()
        })
        .collect()
}

fn padded(rows: Vec<String>, width: u16, height: u16) -> Vec<String> {
    (0..height as usize)
        .map(|index| {
            let mut row = rows.get(index).cloned().unwrap_or_default();
            let used = unicode_width::UnicodeWidthStr::width(row.as_str());
            row.push_str(&" ".repeat((width as usize).saturating_sub(used)));
            row
        })
        .collect()
}

#[tokio::test]
async fn windowed_transcript_matches_the_full_list_at_every_scroll() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    for index in 0..8 {
        app.entries.push(Entry {
            kind: Some(EntryKind::User),
            raw: format!("question {index}"),
            ..Entry::default()
        });
        app.entries.push(Entry {
            kind: Some(EntryKind::Assistant),
            raw: format!("answer {index}\n\nwith a second paragraph for turn {index}"),
            ..Entry::default()
        });
    }
    let (width, height) = (50u16, 9u16);
    let total = transcript::lines(&app.entries, false, width as usize).len();
    assert!(total > height as usize * 3);

    for scroll in [
        None,
        Some(0),
        Some(1),
        Some(total as u16 / 2),
        Some(total as u16 - 1),
        Some(u16::MAX),
    ] {
        app.scroll = scroll;
        let drawn = transcript_rows_on_screen(&app, width, height);
        let expected = padded(expected_transcript_rows(&app, width, height), width, height);
        assert_eq!(drawn, expected, "scroll {scroll:?}");
        let bottom = total.saturating_sub(height as usize) as u16;
        assert_eq!(app.max_scroll.get(), bottom, "scroll {scroll:?}");
    }

    // A second frame reuses the cache and still draws the same rows.
    app.scroll = Some(5);
    let first = transcript_rows_on_screen(&app, width, height);
    let second = transcript_rows_on_screen(&app, width, height);
    assert_eq!(first, second);
    assert_eq!(
        first,
        padded(expected_transcript_rows(&app, width, height), width, height)
    );
}

#[tokio::test]
async fn windowed_transcript_scrolls_through_queued_input_rows() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    for index in 0..6 {
        app.push_system(format!("note {index}"));
    }
    app.queued_input = Some("queued text".into());
    let (width, height) = (50u16, 6u16);
    let queued = transcript::lines(
        &[Entry {
            kind: Some(EntryKind::User),
            raw: "[Queued next input] queued text".into(),
            ..Entry::default()
        }],
        false,
        width as usize,
    );
    let mut full = transcript::lines(&app.entries, false, width as usize);
    full.push(Line::default());
    full.extend(queued);
    let bottom = full.len().saturating_sub(height as usize);
    for top in [
        None,
        Some(0u16),
        Some(bottom as u16 - 1),
        Some(bottom as u16),
    ] {
        app.scroll = top;
        let start = top.map_or(bottom, |top| top as usize);
        let expected: Vec<String> = full
            .iter()
            .skip(start)
            .take(height as usize)
            .map(line_text)
            .collect();
        assert_eq!(
            transcript_rows_on_screen(&app, width, height),
            padded(expected, width, height),
            "scroll {top:?}"
        );
    }
}

#[tokio::test]
async fn empty_transcript_draws_the_startup_logo_and_scrolls_it() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    let (width, height) = (40u16, 3u16);
    app.scroll = None;
    let drawn = transcript_rows_on_screen(&app, width, height);
    let logo: Vec<String> = STARTUP_LOGO.lines().map(str::to_string).collect();
    assert_eq!(
        drawn,
        padded(logo[logo.len() - 3..].to_vec(), width, height)
    );
    app.scroll = Some(0);
    let drawn = transcript_rows_on_screen(&app, width, height);
    assert_eq!(drawn, padded(logo[..3].to_vec(), width, height));
}
