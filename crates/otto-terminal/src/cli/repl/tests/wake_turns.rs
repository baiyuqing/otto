//! Sub-agent wake turn tests.

use super::*;

#[tokio::test]
async fn a_notification_renders_during_a_one_shot_turn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    tasks.notifications().push(Notification {
        task_id: "t1".to_string(),
        text: "[task-notification] task t1 (explorer) succeeded · 1s · 1 tool call\nall good"
            .to_string(),
        ..Notification::default()
    });
    let provider = ScriptedProvider::new(|_| Ok("ok".to_string()));
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::clone(&tasks),
    );
    let (mut repl, stdout, _stderr) = repl_buffers(&controller);

    repl.run_once("go", &CancellationToken::new())
        .await
        .expect("run once");

    assert!(
        stdout.text().contains(
            "\n[task-notification] task t1 (explorer) succeeded · 1s · 1 tool call\nall good\n"
        ),
        "{}",
        stdout.text()
    );
}

#[tokio::test]
async fn the_loop_wakes_only_when_a_notification_is_pending() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    let provider = ScriptedProvider::new(|_| Ok("woke up".to_string()));
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::clone(&tasks),
    );
    let (mut repl, stdout, _stderr) = repl_buffers(&controller);
    let (sender, receiver) = std::sync::mpsc::channel::<()>();
    let cancel = CancellationToken::new();

    let driver = async {
        // A registry signal with nothing pending must not wake a turn.
        tasks
            .add(otto::subagent::tasks::Task::default(), None, None)
            .expect("add");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            provider.calls(),
            0,
            "a wake turn ran before any notification was pending"
        );
        tasks.notifications().push(Notification {
            task_id: "t1".to_string(),
            text: "[task-notification] task t1 succeeded".to_string(),
            ..Notification::default()
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), provider.wait_calls(1))
            .await
            .expect("the pending notification did not trigger a wake turn");
        drop(sender);
    };
    let (result, ()) = tokio::join!(
        repl.run(std::io::BufReader::new(Pipe(receiver)), &cancel),
        driver
    );

    result.expect("run");
    assert_eq!(
        provider.roles(),
        vec![Role::Context],
        "want exactly one wake turn, whose last request message is the notification"
    );
    assert!(stdout.text().contains("\nwoke up"), "{}", stdout.text());
}

#[tokio::test]
async fn a_one_shot_run_waits_for_a_running_task_and_then_wakes() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    let added = tasks
        .add(otto::subagent::tasks::Task::default(), None, None)
        .expect("add");
    tasks.mark_running(&added.id, chrono::Utc::now());
    let child = Arc::clone(&tasks);
    let id = added.id.clone();
    let provider = ScriptedProvider::new(move |call| {
        if call > 1 {
            return Ok("reported".to_string());
        }
        let child = Arc::clone(&child);
        let id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            // Pushed before the final update, as the sub-agent runner
            // does: the wait unblocks on the update and the drain must
            // then find the notification already pending.
            child.notifications().push(Notification {
                task_id: id.clone(),
                text: format!("[task-notification] task {id} succeeded\nchild done"),
                ..Notification::default()
            });
            child.finish(
                &id,
                otto::subagent::tasks::TaskStatus::Succeeded,
                chrono::Utc::now(),
                "child done",
                "",
            );
        });
        Ok("started".to_string())
    });
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::clone(&tasks),
    );
    let (mut repl, stdout, _stderr) = repl_buffers(&controller);

    repl.run_once("go", &CancellationToken::new())
        .await
        .expect("run once");

    assert_eq!(
        provider.roles(),
        vec![Role::User, Role::Context],
        "want the initial turn and one wake turn"
    );
    assert!(stdout.text().contains("reported"), "{}", stdout.text());
}

#[tokio::test]
async fn a_one_shot_run_returns_the_wake_turns_error() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    let pushed = Arc::clone(&tasks);
    let provider = ScriptedProvider::new(move |call| {
        if call > 1 {
            return Err("wake provider failed".to_string());
        }
        // Pushed inside the first turn, after the agent drained the
        // inbox, so the notification is still pending when it returns.
        pushed.notifications().push(Notification {
            task_id: "t1".to_string(),
            kind: Some(otto_core::agent::inbox::NotificationKind::TaskReport),
            text: "progress".to_string(),
            ..Notification::default()
        });
        Ok("ok".to_string())
    });
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::clone(&tasks),
    );
    let (mut repl, _stdout, _stderr) = repl_buffers(&controller);

    let error = repl
        .run_once("inspect", &CancellationToken::new())
        .await
        .expect_err("the wake turn's error");

    assert!(
        error.to_string().contains("wake provider failed"),
        "{error}"
    );
}

/// More than the default `max_tokens` of 4,200,000, so one response uses the
/// root's tokens up.
const OVER_THE_DEFAULT_TOKEN_LIMIT: i64 = 5_000_000;

#[tokio::test]
async fn a_one_shot_run_ends_with_an_error_when_the_token_budget_withholds_a_wake() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    let pushed = Arc::clone(&tasks);
    let provider = ScriptedProvider::with_usage(
        move |_| {
            // Pushed inside the turn, so it is still pending when the turn
            // ends and the one-shot run looks for a wake to run.
            pushed.notifications().push(Notification {
                task_id: "t1".to_string(),
                text: "[task-notification] task t1 succeeded\nchild done".to_string(),
                ..Notification::default()
            });
            Ok("ok".to_string())
        },
        OVER_THE_DEFAULT_TOKEN_LIMIT,
    );
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::clone(&tasks),
    );
    let (mut repl, _stdout, stderr) = repl_buffers(&controller);

    // A wake that never runs and is waited for would hang here.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        repl.run_once("go", &CancellationToken::new()),
    )
    .await
    .expect("the one-shot run did not return");

    let error = result.expect_err("undelivered notifications are an error");
    assert!(error.to_string().contains("not delivered"), "{error}");
    assert!(
        error.to_string().contains("[agent.budget].max_tokens"),
        "{error}"
    );
    assert!(stderr.text().contains("not delivered"), "{}", stderr.text());
    assert_eq!(provider.calls(), 1, "no wake turn ran");
    assert_eq!(
        tasks.notifications().len(),
        1,
        "the notification stays queued"
    );
}

#[tokio::test]
async fn registry_updates_do_not_start_a_wake_while_the_token_budget_withholds_it() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let tasks = Arc::new(Tasks::new());
    let provider = ScriptedProvider::new(|_| Ok("ok".to_string()));
    let controller = scripted_controller(
        workspace.path(),
        sessions.path(),
        Arc::clone(&provider),
        Arc::clone(&tasks),
    );
    controller
        .current_runner()
        .expect("runner")
        .exhaust_root_budget();
    tasks.notifications().push(Notification {
        task_id: "t1".to_string(),
        text: "[task-notification] task t1 succeeded".to_string(),
        ..Notification::default()
    });
    let (mut repl, stdout, _stderr) = repl_buffers(&controller);
    let (sender, receiver) = std::sync::mpsc::channel::<()>();
    let cancel = CancellationToken::new();

    let driver = async {
        // Each new notification reaches the loop, which asks for a wake and is
        // refused; the loop then waits for the next signal.
        for index in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            tasks.notifications().push(Notification {
                task_id: format!("t{}", index + 2),
                text: format!("[task-notification] task t{} succeeded", index + 2),
                ..Notification::default()
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert_eq!(provider.calls(), 0, "a wake ran with the tokens used up");
        assert_eq!(tasks.notifications().len(), 4);
        drop(sender);
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(
            repl.run(std::io::BufReader::new(Pipe(receiver)), &cancel),
            driver
        )
    })
    .await
    .expect("the loop did not return");

    result.expect("run");
    // The user is told once, however often the wake was refused, with the
    // reason and without the text of the notification.
    let shown = stdout.text();
    assert_eq!(
        shown.matches(" held: the token budget").count(),
        1,
        "{shown}"
    );
    assert!(shown.contains("[agent.budget].max_tokens"), "{shown}");
    assert!(shown.contains("delivered with the next input"), "{shown}");
    assert!(!shown.contains("succeeded"), "{shown}");
}
