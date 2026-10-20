//! Tests for the running-task panel.

use super::*;

/// Layout order top to bottom: transcript, suggestions, composer,
/// sub-agent panel, status line. With one running task the panel takes
/// one row, so the composer's bottom border, the panel row, and the
/// status line are the frame's last three rows, in that order.
#[tokio::test]
async fn the_panel_sits_between_the_composer_and_the_last_row_status_line() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.tasks = vec![running_task("t3", "review-auth", "check the callback")];

    let height = 20;
    let rows = screen_rows(&app, 100, height);
    assert_eq!(rows.len(), height as usize);

    let composer_border_row = &rows[(height - 3) as usize];
    let panel_row = &rows[(height - 2) as usize];
    let status_row = &rows[(height - 1) as usize];

    assert!(composer_border_row.contains('─'), "{composer_border_row:?}");
    assert!(
        !composer_border_row.contains("t3"),
        "{composer_border_row:?}"
    );
    assert!(panel_row.contains("t3"), "{panel_row:?}");
    assert!(panel_row.contains("review-auth"), "{panel_row:?}");
    assert!(status_row.contains("gpt-alpha"), "{status_row:?}");
}

/// The composer's empty-input floor is 1 inner row, so the box (with its
/// two border rows) is 3 rows tall.
#[tokio::test]
async fn empty_composer_is_three_rows_tall() {
    let (_workspace, _sessions, app) = app_fixture().await;
    assert_eq!(composer_height(&app, 80, COMPOSER_MIN_INNER_ROWS), 3);
}

/// With no queued or running task the panel takes no rows at all, and a
/// finished task never appears in it.
///
/// The workspace is pinned to a name that spells the task id: the status
/// line shows the workspace's last path component, and a temporary
/// directory's random name spells a two-character id often enough to have
/// failed this test's absence assertion by chance.
#[tokio::test]
async fn a_finished_task_leaves_no_panel_row() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.info.workspace = "/tmp/t9-workspace".to_string();
    app.tasks = vec![Task {
        id: "t9".into(),
        status: TaskStatus::Succeeded,
        ..Task::default()
    }];

    let height = 20;
    let rows = screen_rows(&app, 100, height);

    // No panel row means the composer's bottom border sits directly
    // above the status line.
    let composer_border_row = &rows[(height - 2) as usize];
    assert!(composer_border_row.contains('─'), "{composer_border_row:?}");
    // The pinned workspace reaches the frame, so the assertion below has
    // to exclude that row rather than pass for want of the id on screen.
    assert!(rows[(height - 1) as usize].contains("t9"), "{rows:?}");
    assert!(!above_status_line(&rows).contains("t9"), "{rows:?}");
}

/// More than 4 queued/running tasks show only the first 3 and a
/// `+N more` row for the rest.
#[tokio::test]
async fn five_running_tasks_show_three_rows_and_a_more_row() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.tasks = (0..5)
        .map(|i| running_task(&format!("t{i}"), &format!("agent{i}"), "working"))
        .collect();

    let rows = screen_rows(&app, 100, 30);
    let screen = above_status_line(&rows);

    for i in 0..3 {
        assert!(screen.contains(&format!("t{i}")), "{screen}");
    }
    for i in 3..5 {
        assert!(!screen.contains(&format!("t{i}")), "{screen}");
    }
    assert!(screen.contains("+2 more"), "{screen}");
}

/// At the minimum terminal size (40x8) with four running tasks, the rows
/// are transcript 1, composer 3, panel 3, status 1: the panel gives up
/// rows before the composer shrinks below its 1-row minimum.
#[tokio::test]
async fn the_minimum_size_keeps_the_transcript_row_and_the_status_line() {
    let (_workspace, _sessions, mut app) = app_fixture().await;
    app.tasks = (1..=4)
        .map(|i| running_task(&format!("t{i}"), "review", "check the diff"))
        .collect();

    let width = MIN_TERMINAL_WIDTH;
    let height = MIN_TERMINAL_HEIGHT;
    let rows = screen_rows(&app, width, height);
    assert_eq!(rows.len(), height as usize);

    // Row 0 is the transcript; the composer's titled top border is row 1
    // and its bottom border row 3, so it kept 1 inner row.
    assert!(rows[1].contains("Otto"), "{rows:?}");
    assert!(
        rows[3].contains('─') && !rows[3].contains("Otto"),
        "{rows:?}"
    );
    assert!(rows[4].contains("t1"), "{rows:?}");
    assert!(rows[5].contains("t2"), "{rows:?}");
    assert!(rows[6].contains("t3"), "{rows:?}");
    assert!(!above_status_line(&rows).contains("t4"), "{rows:?}");
    // The footer text is longer than this width, but it still starts
    // with the model field, confirming the status line landed on the
    // frame's last row rather than being pushed off it.
    assert!(rows[7].contains("gpt-alpha"), "{rows:?}");
}

/// An unnamed task is labeled by its agent, and by `default` when it has
/// no agent either.
#[test]
fn an_unnamed_panel_row_falls_back_to_the_agent() {
    let text = |task: &Task| -> String {
        panel_row_line(task, chrono::Utc::now(), 80, panel_columns(&[task]))
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    };
    let mut task = running_task("t1", "", "check the diff");
    task.agent = "reviewer".into();
    assert!(text(&task).contains("reviewer ?"), "{}", text(&task));
    task.agent.clear();
    assert!(text(&task).contains("default ?"), "{}", text(&task));
}

#[test]
fn the_panel_shows_the_model_and_live_input_output_tokens() {
    let mut task = running_task("t1", "review", "check the diff");
    task.model = "gpt-5.5".into();
    task.usage_present = true;
    task.usage.input_tokens = 12_310;
    task.usage.output_tokens = 842;

    let line = line_text(&panel_row_line(
        &task,
        chrono::Utc::now(),
        100,
        panel_columns(&[&task]),
    ));

    assert!(line.contains("gpt-5.5 in: 12.3k out:   842"), "{line}");
}

#[test]
fn panel_columns_align_across_different_name_lengths() {
    let mut short = running_task("t1", "a", "first description");
    short.model = "gpt-5.5".into();
    short.usage_present = true;
    short.usage.input_tokens = 10;
    short.usage.output_tokens = 2;
    let mut long = running_task("t2", "much-longer-name", "second description");
    long.model = "openai-compatible/model-alpha".into();
    long.usage_present = true;
    long.usage.input_tokens = 12_310;
    long.usage.output_tokens = 842;

    let columns = panel_columns(&[&short, &long]);
    let short = line_text(&panel_row_line(&short, chrono::Utc::now(), 120, columns));
    let long = line_text(&panel_row_line(&long, chrono::Utc::now(), 120, columns));

    assert!(long.contains("much-longer-name"), "{long}");
    assert!(long.contains("openai-compatible/model-alpha"), "{long}");
    assert_eq!(
        short.find("gpt-5.5"),
        long.find("openai-compatible/model-alpha")
    );
    assert_eq!(short.find("in:"), long.find("in:"));
    assert_eq!(short.find("out:"), long.find("out:"));
    assert_eq!(
        short.find("first description"),
        long.find("second description")
    );
}

/// The sub-agent panel needs the idle loop's 1-second redraw timer iff at
/// least one task is queued or running; a registry with nothing pending,
/// or only finished tasks, needs no timer.
#[test]
fn needs_task_clock_only_with_a_queued_or_running_task() {
    assert!(!needs_task_clock(&[]));
    assert!(!needs_task_clock(&[Task {
        status: TaskStatus::Succeeded,
        ..Task::default()
    }]));
    assert!(needs_task_clock(&[Task {
        status: TaskStatus::Queued,
        ..Task::default()
    }]));
    assert!(needs_task_clock(&[Task {
        status: TaskStatus::Running,
        ..Task::default()
    }]));
}
