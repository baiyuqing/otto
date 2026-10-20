
use std::time::Duration;

use otto_core::model::{Block, BlockType, Role};

use crate::subagent::record;

use super::*;

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("a valid timestamp")
}

fn user_message(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![Block {
            block_type: BlockType::Text,
            text: text.to_string(),
            ..Block::default()
        }],
        ..Message::default()
    }
}

#[test]
fn loading_a_notification_signals_before_any_receiver_subscribes() {
    // `Tasks::updates()` clones a stored receiver that this registry
    // never reads itself, so a signal sent before a frontend calls
    // `updates()` still shows up as `has_changed()` on the frontend's
    // first check. This is what lets a reopened session's already-queued
    // notifications start a wake turn instead of waiting for the next
    // user prompt.
    let tasks = Tasks::new();
    tasks
        .notifications()
        .load(vec![otto_core::agent::inbox::Entry {
            seq: 0,
            notification: otto_core::agent::inbox::Notification {
                task_id: "t1".into(),
                kind: Some(NotificationKind::Message),
                text: "queued while the session was closed".into(),
                usage: None,
                ..Default::default()
            },
        }]);

    let receiver = tasks.updates();
    assert!(
        receiver.has_changed().expect("the channel is open"),
        "a load that seeded a notification must signal"
    );
    assert_eq!(tasks.pending(), 1);
}

#[tokio::test]
async fn a_task_moves_from_queued_through_running_to_canceled() {
    let tasks = Arc::new(Tasks::new());
    let cancel = CancellationToken::new();
    let history = vec![user_message("child prompt")];
    let hook = {
        let history = history.clone();
        Arc::new(move || history.clone()) as Arc<dyn Fn() -> Vec<Message> + Send + Sync>
    };
    let updates = tasks.updates();
    let task = tasks
        .add(
            Task {
                agent: "explorer".into(),
                prompt: "find sessions".into(),
                ..Task::default()
            },
            Some(cancel.clone()),
            Some(hook),
        )
        .expect("the first task is valid");
    assert_eq!(task.id, "t1");
    assert_eq!(task.status, TaskStatus::Queued);
    assert!(
        updates.has_changed().expect("the channel is open"),
        "add must signal updates"
    );

    assert_eq!(tasks.get("t1").expect("t1 exists").prompt, "find sessions");
    let stored = tasks.history("t1").expect("t1 exists");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].text(), "child prompt");

    tasks.mark_running("t1", at(1));
    tasks.record_provider_step("t1", Usage::default(), "", false, false);
    tasks.record_provider_step("t1", Usage::default(), "", true, false);

    let waiter = {
        let tasks = Arc::clone(&tasks);
        tokio::spawn(async move { tasks.wait("t1", &CancellationToken::new()).await })
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(20), async {})
            .await
            .is_ok()
    );
    assert!(!waiter.is_finished(), "wait returned before the task ended");

    tasks.cancel("t1").expect("a running task can be canceled");
    assert!(cancel.is_cancelled(), "cancel must reach the token");
    tasks.finish("t1", TaskStatus::Canceled, at(2), "", "");
    let waited = waiter
        .await
        .expect("the waiter task ran")
        .expect("the wait succeeded");
    assert_eq!(waited.status, TaskStatus::Canceled);

    let list = tasks.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, TaskStatus::Canceled);
    assert_eq!(list[0].steps, 2);
    assert!(list[0].usage_present, "a present-usage step sets the flag");
    assert_eq!(list[0].usage, Usage::default());

    assert_eq!(
        tasks.cancel("t1"),
        Err(TaskError::Finished("t1".into())),
        "a finished task cannot be canceled"
    );
    assert_eq!(
        tasks.cancel("missing"),
        Err(TaskError::NotFound("missing".into()))
    );

    tasks
        .notifications()
        .push(otto_core::agent::inbox::Notification {
            task_id: "t1".into(),
            kind: Some(otto_core::agent::inbox::NotificationKind::TaskFinished),
            text: "x".into(),
            usage: None,
            ..Default::default()
        });
    assert_eq!(tasks.pending(), 1);

    let second_cancel = CancellationToken::new();
    let second = tasks
        .add(
            Task {
                prompt: "second".into(),
                ..Task::default()
            },
            Some(second_cancel.clone()),
            None,
        )
        .expect("a second task is valid");
    assert_eq!(second.id, "t2");

    tasks.close();
    assert!(
        second_cancel.is_cancelled(),
        "close must cancel the non-final task"
    );
    assert!(
        tasks.updates().has_changed().is_err(),
        "close must close the update channel"
    );
    assert_eq!(
        tasks.add(Task::default(), None, None),
        Err(TaskError::Closed)
    );
    tasks.close();
}

#[tokio::test]
async fn begin_migration_cancels_every_non_final_task_and_returns_their_ids() {
    let tasks = Tasks::new();
    assert!(!tasks.is_migrating());

    let running_cancel = CancellationToken::new();
    let running = tasks
        .add(
            Task {
                prompt: "running".into(),
                ..Task::default()
            },
            Some(running_cancel.clone()),
            None,
        )
        .expect("running task is valid");
    tasks.mark_running(&running.id, at(1));

    let queued_cancel = CancellationToken::new();
    let queued = tasks
        .add(
            Task {
                prompt: "queued".into(),
                ..Task::default()
            },
            Some(queued_cancel.clone()),
            None,
        )
        .expect("queued task is valid");

    let finished = tasks
        .add(
            Task {
                prompt: "finished".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("finished task is valid");
    tasks.finish(&finished.id, TaskStatus::Succeeded, at(2), "done", "");

    let mut ids = tasks.begin_migration();
    ids.sort();
    assert_eq!(ids, vec![running.id.clone(), queued.id.clone()]);
    assert!(tasks.is_migrating());
    assert!(tasks.is_closed(), "migration also closes the registry");
    assert!(running_cancel.is_cancelled());
    assert!(queued_cancel.is_cancelled());

    // Idempotent: a second call finds nothing new to migrate.
    assert_eq!(tasks.begin_migration(), Vec::<String>::new());

    tasks.finish(&running.id, TaskStatus::Canceled, at(3), "", "");
    tasks.finish(&queued.id, TaskStatus::Canceled, at(3), "", "");
    tasks.wait_final(&ids).await;
    assert!(tasks.get(&running.id).expect("still present").is_final());
    assert!(tasks.get(&queued.id).expect("still present").is_final());
}

#[test]
fn is_migrating_is_false_before_begin_migration_and_true_after() {
    let tasks = Tasks::new();
    assert!(!tasks.is_migrating());
    tasks.begin_migration();
    assert!(tasks.is_migrating());
}

#[test]
fn add_with_id_rejects_an_id_already_registered_and_leaves_the_registry_unchanged() {
    let tasks = Tasks::new();
    tasks
        .add_with_id(
            "t5".to_string(),
            Task {
                prompt: "first".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("the first registration under t5 is valid");

    let error = tasks
        .add_with_id(
            "t5".to_string(),
            Task {
                prompt: "second".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect_err("a duplicate id must be rejected");
    assert_eq!(
        error,
        TaskError::Invalid("task \"t5\" already exists".to_string())
    );

    let list = tasks.list();
    assert_eq!(list.len(), 1, "the rejected call must not add an entry");
    assert_eq!(list[0].prompt, "first");

    // The id counter only advances on the auto-id path, so it must still
    // be at zero: the next plain `add` gets "t1", not "t6".
    let next = tasks
        .add(Task::default(), None, None)
        .expect("a plain add still works");
    assert_eq!(
        next.id, "t1",
        "add_with_id must never advance the auto-id counter"
    );
}

#[test]
fn add_clears_runtime_fields_and_only_valid_transitions_apply() {
    let tasks = Tasks::new();
    let created = tasks
        .add(
            Task {
                prompt: "inspect".into(),
                status: TaskStatus::Succeeded,
                started_at: Some(at(1)),
                finished_at: Some(at(2)),
                steps: 9,
                tool_calls: 8,
                last_tool: "stale".into(),
                last_text: "stale".into(),
                usage: Usage {
                    input_tokens: 10,
                    ..Usage::default()
                },
                result: "stale".into(),
                error: "stale".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("the task is valid");
    assert_eq!(
        created,
        Task {
            id: "t1".into(),
            prompt: "inspect".into(),
            status: TaskStatus::Queued,
            ..Task::default()
        },
        "add must drop every stale runtime field"
    );

    tasks.mark_running("t1", at(10));
    tasks.mark_running("t1", at(11));
    tasks.record_tool_call("t1", "grep \"session\"");
    tasks.record_provider_step(
        "t1",
        Usage {
            input_tokens: 3,
            output_tokens: 2,
            ..Usage::default()
        },
        "found it",
        true,
        true,
    );

    let started = tasks.get("t1").expect("t1 exists");
    assert_eq!(started.status, TaskStatus::Running);
    assert_eq!(
        started.started_at,
        Some(at(10)),
        "a repeat start is ignored"
    );
    assert_eq!(started.tool_calls, 1);
    assert_eq!(started.steps, 1);
    assert_eq!(started.last_tool, "grep \"session\"");
    assert_eq!(started.last_text, "found it");

    tasks.finish("t1", TaskStatus::Succeeded, at(20), "done", "");
    tasks.finish("t1", TaskStatus::Failed, at(30), "overwritten", "bad");
    tasks.record_tool_call("t1", "late");
    tasks.record_provider_step(
        "t1",
        Usage {
            input_tokens: 1,
            ..Usage::default()
        },
        "late",
        true,
        true,
    );
    let finished = tasks.get("t1").expect("t1 exists");
    assert_eq!(finished.status, TaskStatus::Succeeded);
    assert_eq!(finished.result, "done");
    assert_eq!(finished.error, "");
    assert_eq!(finished.finished_at, Some(at(20)));
    assert_eq!(finished.tool_calls, 1, "updates after finish are no-ops");
}

#[tokio::test]
async fn a_task_keeps_its_cache_breakdown_only_while_every_report_has_one() {
    let tasks = Arc::new(Tasks::new());
    let task = tasks
        .add(
            Task {
                prompt: "inspect".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("the task is admitted");
    tasks.mark_running(&task.id, at(1));
    let usage = Usage {
        input_tokens: 10,
        output_tokens: 2,
        cached_input_tokens: 4,
    };

    tasks.record_provider_step(&task.id, usage, "", true, true);
    let reported = tasks.get(&task.id).expect("t1 exists");
    assert!(
        reported.cached_input_tokens_present,
        "a report that carried the breakdown makes the cached count measured"
    );

    tasks.record_provider_step(&task.id, usage, "", true, false);
    let mixed = tasks.get(&task.id).expect("t1 exists");
    assert!(
        !mixed.cached_input_tokens_present,
        "one report without the breakdown leaves the cached total unmeasured"
    );
    assert_eq!(mixed.usage.cached_input_tokens, 8);
}

#[tokio::test]
async fn a_name_resolves_to_the_same_task_as_its_id() {
    let tasks = Arc::new(Tasks::new());
    let cancel = CancellationToken::new();
    let hook = Arc::new(|| vec![user_message("child prompt")])
        as Arc<dyn Fn() -> Vec<Message> + Send + Sync>;
    let task = tasks
        .add(
            Task {
                name: "lint-check".into(),
                prompt: "lint".into(),
                ..Task::default()
            },
            Some(cancel.clone()),
            Some(hook),
        )
        .expect("the name is valid");
    assert_eq!(task.id, "t1");
    assert_eq!(task.name, "lint-check");
    assert_eq!(tasks.get("lint-check"), tasks.get("t1"));
    let history = tasks.history("lint-check").expect("the name resolves");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].text(), "child prompt");

    let waiter = {
        let tasks = Arc::clone(&tasks);
        tokio::spawn(async move { tasks.wait("lint-check", &CancellationToken::new()).await })
    };
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished(), "wait returned before the task ended");

    tasks.cancel("lint-check").expect("the name resolves");
    assert!(cancel.is_cancelled());
    tasks.finish("t1", TaskStatus::Canceled, at(2), "", "");
    waiter
        .await
        .expect("the waiter task ran")
        .expect("the wait succeeded");
}

#[test]
fn names_are_validated_and_reserved() {
    let tasks = Tasks::new();
    tasks
        .add(
            Task {
                name: "lint-check".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("the first name is free");
    let duplicate = tasks
        .add(
            Task {
                name: "lint-check".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect_err("a duplicate name is rejected");
    assert_eq!(
        duplicate.to_string(),
        "task \"lint-check\" already used by t1"
    );

    let reserved = Tasks::new();
    let error = reserved
        .add(
            Task {
                name: "t7".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect_err("a task-id shape is reserved");
    assert_eq!(error.to_string(), "task \"t7\" is reserved for task ids");
    assert!(
        reserved.get("t7").is_none(),
        "a rejected add must create nothing"
    );

    for name in ["bad name", &"a".repeat(65)] {
        let invalid = Tasks::new();
        let error = invalid
            .add(
                Task {
                    name: name.to_string(),
                    ..Task::default()
                },
                None,
                None,
            )
            .expect_err("the name is invalid");
        assert!(
            error.to_string().contains("is invalid"),
            "{name:?} error = {error}"
        );
        let after = invalid
            .add(Task::default(), None, None)
            .expect("a rejected add consumes no id");
        assert_eq!(after.id, "t1");
    }

    let unnamed = Tasks::new();
    let first = unnamed.add(Task::default(), None, None).expect("valid");
    let second = unnamed.add(Task::default(), None, None).expect("valid");
    assert_ne!(first.id, second.id);
}

#[tokio::test]
async fn wait_stops_on_cancellation_and_rejects_an_unknown_reference() {
    let tasks = Tasks::new();
    tasks.add(Task::default(), None, None).expect("valid");

    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        tasks.wait("t1", &cancel).await,
        Err(TaskError::Canceled),
        "a cancelled token ends the wait"
    );
    assert_eq!(
        tasks.wait("missing", &CancellationToken::new()).await,
        Err(TaskError::NotFound("missing".into()))
    );
}

fn recorder_context(parent_session: &str) -> record::TaskContext {
    record::TaskContext {
        parent_session: parent_session.to_string(),
        parent_session_path: format!("/home/me/.otto/sessions/{parent_session}.jsonl"),
        workspace: "/work".into(),
        pid: 4_294_967_294, // a pid that cannot exist; not what these tests check
        process_started_at: "2026-09-25T10:00:00Z".into(),
    }
}

#[test]
fn two_registries_share_one_recorder_and_both_rows_are_listed() {
    // Standing in for two otto processes writing the same `tasks.db`.
    let store = Arc::new(record::Store::open_in_memory().expect("store"));
    let first = Tasks::with_recorder(
        Arc::clone(&store) as Arc<dyn Recorder>,
        recorder_context("s1"),
    );
    let second = Tasks::with_recorder(
        Arc::clone(&store) as Arc<dyn Recorder>,
        recorder_context("s2"),
    );

    first
        .add(
            Task {
                prompt: "first".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("valid");
    second
        .add(
            Task {
                prompt: "second".into(),
                ..Task::default()
            },
            None,
            None,
        )
        .expect("valid");

    let listed = store.list(&record::ListQuery::default()).expect("list");
    let mut sessions: Vec<&str> = listed
        .tasks
        .iter()
        .map(|row| row.parent_session.as_str())
        .collect();
    sessions.sort_unstable();
    assert_eq!(sessions, ["s1", "s2"]);
}

#[test]
fn a_recorder_write_error_is_silent_and_the_task_still_finishes() {
    let store = record::Store::open_in_memory().expect("store");
    store
        .list(&record::ListQuery::default())
        .expect("the schema exists before it is broken");
    // Break the schema so every subsequent write fails.
    store.break_schema_for_test();
    let broken: Arc<dyn Recorder> = Arc::new(store);
    let tasks = Tasks::with_recorder(broken, recorder_context("s1"));

    let task = tasks.add(Task::default(), None, None).expect("valid");
    tasks.mark_running(&task.id, at(1));
    tasks.finish(&task.id, TaskStatus::Succeeded, at(2), "done", "");

    let finished = tasks.get(&task.id).expect("the task still exists");
    assert_eq!(finished.status, TaskStatus::Succeeded);
    assert_eq!(finished.result, "done");
}

#[test]
fn highest_task_counter_is_the_largest_t_prefixed_file_name() {
    let temp = tempfile::tempdir().expect("temp dir");
    for name in ["t3-abcd.jsonl", "t7-wxyz.jsonl", "t1-aaaa.jsonl"] {
        std::fs::write(temp.path().join(name), b"").expect("write fixture file");
    }
    // Names that do not match `t<digits>-...` must be ignored, not parsed
    // as a higher counter.
    for name in [
        "parent.jsonl",
        "t-nodigits.jsonl",
        "t9nodash.jsonl",
        "task10.jsonl",
    ] {
        std::fs::write(temp.path().join(name), b"").expect("write fixture file");
    }

    assert_eq!(highest_task_counter(temp.path()), 7);
}

#[test]
fn highest_task_counter_is_zero_for_a_missing_directory() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("does-not-exist");
    assert_eq!(highest_task_counter(&missing), 0);
}

#[test]
fn a_resumed_registry_continues_the_task_id_counter() {
    let tasks = Tasks::new_from(7);
    let task = tasks.add(Task::default(), None, None).expect("valid");
    assert_eq!(task.id, "t8");
}

#[test]
fn replacing_a_child_mailbox_keeps_restored_messages_first() {
    let tasks = Tasks::new();
    let task = tasks.add(Task::default(), None, None).expect("add");
    tasks.send_message(&task.id, "queued early").expect("send");
    let durable = Arc::new(Inbox::default());
    durable.push(otto_core::agent::inbox::Notification {
        text: "[parent-message] restored".into(),
        ..Default::default()
    });
    tasks.replace_child_inbox(&task.id, durable.clone());
    let texts: Vec<String> = durable
        .snapshot()
        .into_iter()
        .map(|notification| notification.text)
        .collect();
    assert_eq!(
        texts,
        ["[parent-message] restored", "[parent-message] queued early"]
    );
    assert!(tasks.child_inbox(&task.id).expect("mailbox").len() == 2);
}

#[test]
fn a_parent_message_that_cannot_be_saved_is_not_sent() {
    let tasks = Tasks::new();
    let task = tasks.add(Task::default(), None, None).expect("add");
    let failing = Arc::new(Inbox::default());
    failing.set_persist(Box::new(|_| Err("disk full".into())));
    tasks.replace_child_inbox(&task.id, failing.clone());
    let error = tasks
        .send_message(&task.id, "hello")
        .expect_err("not persisted");
    assert!(matches!(error, TaskError::NotPersisted(_)), "{error}");
    assert!(failing.is_empty());
}
