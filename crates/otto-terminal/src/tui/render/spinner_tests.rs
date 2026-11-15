//! Deterministic animation frame and redraw cadence regressions.

use std::future::Future;
use std::task::{Context, Waker};

use super::*;

fn tick_ready(interval: &mut tokio::time::Interval) -> bool {
    let mut tick = std::pin::pin!(interval.tick());
    tick.as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_ready()
}

#[test]
fn spinner_visits_every_frame_and_wraps_without_early_advancement() {
    let expected = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    for (index, glyph) in expected.into_iter().enumerate() {
        let start = Duration::from_millis(index as u64 * 100);
        assert_eq!(spinner_frame(start), glyph);
        assert_eq!(spinner_frame(start + Duration::from_millis(99)), glyph);
    }
    assert_eq!(spinner_frame(Duration::from_secs(1)), expected[0]);
}

#[test]
fn task_marker_uses_the_same_cadence_and_handles_missing_or_future_start() {
    let start = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let mut task = Task {
        status: TaskStatus::Running,
        started_at: Some(start),
        ..Task::default()
    };
    for index in 0..=10 {
        let elapsed = Duration::from_millis(index * 100);
        let now = start + chrono::Duration::from_std(elapsed).unwrap();
        assert_eq!(panel_marker(&task, now), spinner_frame(elapsed));
    }
    assert_eq!(
        panel_marker(&task, start - chrono::Duration::seconds(1)),
        "⠋"
    );
    task.started_at = None;
    assert_eq!(panel_marker(&task, start), "⠋");
    task.status = TaskStatus::Queued;
    assert_eq!(panel_marker(&task, start), "○");
}

#[tokio::test(start_paused = true)]
async fn spinner_timer_redraws_each_frame_not_every_fifth_frame() {
    let mut frames = spinner_interval();
    assert!(tick_ready(&mut frames));
    for _ in 0..10 {
        tokio::time::advance(Duration::from_millis(99)).await;
        assert!(!tick_ready(&mut frames), "must hold frame for 100ms");
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(tick_ready(&mut frames), "every frame needs a redraw");
        assert!(!tick_ready(&mut frames), "no duplicate immediate redraw");
    }
}

#[tokio::test(start_paused = true)]
async fn spinner_timer_skips_stale_ticks_after_a_slow_operation() {
    let mut frames = spinner_interval();
    assert!(tick_ready(&mut frames));
    tokio::time::advance(Duration::from_millis(550)).await;
    assert!(tick_ready(&mut frames), "resume with one current frame");
    assert!(
        !tick_ready(&mut frames),
        "must not burst through stale frames"
    );
    tokio::time::advance(Duration::from_millis(49)).await;
    assert!(!tick_ready(&mut frames));
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(tick_ready(&mut frames), "keep the wall-clock cadence");
}

#[tokio::test]
async fn timer_redraws_for_main_turn_and_live_children_but_not_an_idle_screen() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = otto::fixtures::controller(workspace.path(), sessions.path()).await;
    let mut app = App::new(&crate::tui::app::Backend::Local(&controller));
    assert!(!needs_animation(&app));
    app.start_turn();
    assert!(needs_animation(&app));
    app.end_turn();
    assert!(!needs_animation(&app));
    app.tasks.push(Task {
        status: TaskStatus::Running,
        ..Task::default()
    });
    assert!(
        needs_animation(&app),
        "children animate after the main turn ends"
    );
    app.tasks[0].status = TaskStatus::Queued;
    assert!(
        needs_animation(&app),
        "queued task elapsed time still updates"
    );
    app.tasks.clear();
    assert!(!needs_animation(&app));
}
