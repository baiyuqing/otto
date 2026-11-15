use std::time::Duration;

use super::*;
use crate::tool::testutil::{run, run_cancelled};
use otto_core::mailbox::Inbox;
use otto_core::model::{EffectCertainty, OperationDisposition, OperationStopReason};
use otto_faults::publish::FaultyPublishOps;
use otto_faults::{Fault, FaultError, FaultPlan, Rule, Trigger};

fn publish_fault(step: PublishStep) -> (Arc<FaultPlan<PublishStep>>, Arc<dyn PublishOps>) {
    let plan = FaultPlan::new(vec![Rule::new(
        step,
        Trigger::Always,
        Fault::Before(FaultError::Os(libc::EIO)),
    )]);
    let ops = Arc::new(FaultyPublishOps::new(plan.clone()));
    (plan, ops)
}

fn outcome(result: &ToolResult) -> &otto_core::model::OperationOutcome {
    result
        .outcome_override
        .as_ref()
        .expect("typed outcome override")
}

fn tool_and_inbox() -> (RemindTool, Arc<Reminders>, Arc<Inbox>) {
    let inbox = Arc::new(Inbox::default());
    let reminders = Arc::new(Reminders::new(inbox.clone()));
    (
        RemindTool {
            reminders: Arc::clone(&reminders),
        },
        reminders,
        inbox,
    )
}

fn persist_setup() -> (tempfile::TempDir, PathBuf, Arc<Inbox>) {
    let directory = tempfile::tempdir().expect("temp dir");
    // `Inner::save` skips writing once `session.jsonl` is gone (the
    // archive-race guard), so a stand-in for it must exist beside the
    // sidecar for these tests to exercise ordinary persistence.
    std::fs::write(directory.path().join("session.jsonl"), b"").expect("stub session file");
    let path = directory.path().join("session.reminders.json");
    (directory, path, Arc::new(Inbox::default()))
}

fn persisted(path: &Path) -> Vec<StoredReminder> {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|_| "[]".to_string());
    serde_json::from_str(&raw).expect("stored reminders")
}

async fn wait_until_fired(inbox: &Inbox) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while inbox.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the timer did not fire");
}

#[tokio::test]
async fn a_call_returns_immediately_and_fires_after_the_delay() {
    let (tool, reminders, inbox) = tool_and_inbox();

    let result = run(&tool, r#"{"seconds":10,"message":"check the tests"}"#).await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.content, "scheduled r1 in 10s: check the tests");
    assert!(inbox.is_empty(), "the timer must not fire before the delay");

    reminders
        .schedule(Duration::ZERO, "check the tests".into())
        .expect("zero-delay timer");
    wait_until_fired(&inbox).await;

    let items = inbox.drain();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].task_id, "timer");
    assert_eq!(items[0].text, "[timer] check the tests");
}

#[tokio::test]
async fn dropping_the_registry_cancels_an_unfired_timer() {
    let inbox = Arc::new(Inbox::default());
    {
        let reminders = Reminders::new(inbox.clone());
        reminders
            .schedule(Duration::ZERO, "gone".into())
            .expect("schedule");
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(inbox.is_empty(), "a dropped registry must not fire");
}

#[tokio::test]
async fn invalid_arguments_are_rejected() {
    let (tool, _reminders, inbox) = tool_and_inbox();
    for (arguments, snippet) in [
        (r#"{"seconds":0,"message":"x"}"#, "seconds must be between"),
        (
            r#"{"seconds":3601,"message":"x"}"#,
            "seconds must be between",
        ),
        (r#"{"seconds":1,"message":"   "}"#, "message is required"),
        (
            &format!(r#"{{"seconds":1,"message":"{}"}}"#, "x".repeat(501)),
            "message is longer than",
        ),
    ] {
        let result = run(&tool, arguments).await;
        assert!(result.is_error, "{arguments} => {result:?}");
        assert!(
            result.content.contains(snippet),
            "{arguments} => {}",
            result.content
        );
        assert_eq!(
            outcome(&result).effect_certainty,
            EffectCertainty::NotStarted,
            "{arguments}"
        );
    }
    assert!(inbox.is_empty());
}

#[tokio::test]
async fn malformed_remind_and_cancel_arguments_are_not_started() {
    let (remind, reminders, _inbox) = tool_and_inbox();
    let cancel = RemindCancelTool { reminders };

    for result in [
        run(&remind, r#"{"seconds":1}"#).await,
        run(&cancel, "{}").await,
    ] {
        assert!(result.is_error, "{result:?}");
        assert_eq!(
            outcome(&result).effect_certainty,
            EffectCertainty::NotStarted
        );
        assert_eq!(outcome(&result).disposition, OperationDisposition::Error);
    }
}

#[tokio::test]
async fn predispatch_cancellation_is_cancelled_not_started() {
    let (remind, reminders, _inbox) = tool_and_inbox();
    let cancel = RemindCancelTool { reminders };

    for result in [
        run_cancelled(&remind, r#"{"seconds":1,"message":"nope"}"#).await,
        run_cancelled(&cancel, r#"{"id":"r1"}"#).await,
    ] {
        assert!(result.is_error, "{result:?}");
        assert_eq!(result.content, CONTEXT_CANCELED);
        assert_eq!(
            outcome(&result).effect_certainty,
            EffectCertainty::NotStarted
        );
        assert_eq!(
            outcome(&result).disposition,
            OperationDisposition::Cancelled
        );
        assert_eq!(
            outcome(&result).stop_reason,
            Some(OperationStopReason::UserCancellation)
        );
    }
}

#[tokio::test]
async fn schedule_directory_sync_failure_is_unknown_and_rolls_back_memory() {
    let (_directory, path, inbox) = persist_setup();
    let (plan, ops) = publish_fault(PublishStep::DirSync);
    let reminders = Arc::new(Reminders::create_with_publish_ops(
        inbox.clone(),
        Some(path.clone()),
        ops,
    ));
    let tool = RemindTool {
        reminders: Arc::clone(&reminders),
    };

    let result = run(&tool, r#"{"seconds":60,"message":"uncertain"}"#).await;

    plan.assert_all_fired();
    assert!(result.is_error, "{result:?}");
    assert_eq!(outcome(&result).disposition, OperationDisposition::Error);
    assert_eq!(outcome(&result).effect_certainty, EffectCertainty::Unknown);
    assert!(
        reminders.list().is_empty(),
        "failed schedule must roll back"
    );
    let stored = persisted(&path);
    assert_eq!(stored.len(), 1, "the rename may already be visible");
    assert_eq!(stored[0].message, "uncertain");
}

#[tokio::test]
async fn schedule_rename_failure_is_not_started_and_keeps_the_previous_file() {
    let (directory, path, inbox) = persist_setup();
    let previous = StoredReminder {
        id: "r1".into(),
        fire_at: Utc::now() + TimeDelta::minutes(1),
        message: "previous".into(),
    };
    std::fs::write(&path, serde_json::to_vec_pretty(&[&previous]).unwrap()).unwrap();
    let (plan, ops) = publish_fault(PublishStep::Rename);
    let reminders = Arc::new(Reminders::create_with_publish_ops(
        inbox,
        Some(path.clone()),
        ops,
    ));
    let tool = RemindTool {
        reminders: Arc::clone(&reminders),
    };

    let result = run(&tool, r#"{"seconds":60,"message":"lost"}"#).await;

    plan.assert_all_fired();
    assert!(result.is_error, "{result:?}");
    assert_eq!(
        outcome(&result).effect_certainty,
        EffectCertainty::NotStarted
    );
    let stored = persisted(&path);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].message, "previous");
    let mut names: Vec<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["session.jsonl", "session.reminders.json"]);
}

#[test]
fn a_leftover_temporary_file_does_not_stop_a_save() {
    let (directory, path, _inbox) = persist_setup();
    let temp = directory.path().join("session.reminders.json.tmp");
    std::fs::write(&temp, "partial").unwrap();

    replace_private_with(&path, b"[]", &PosixPublish).expect("save over a leftover temporary file");

    assert_eq!(std::fs::read(&path).unwrap(), b"[]");
    assert!(!temp.exists());
}

#[test]
fn empty_save_without_a_sidecar_still_syncs_the_directory_before_reporting_failure() {
    let (_directory, path, inbox) = persist_setup();
    let (plan, ops) = publish_fault(PublishStep::DirSync);
    let reminders = Reminders::create_with_publish_ops(inbox.clone(), Some(path), ops);
    let armed = reminders.inner.armed.lock().expect(STATE_MUTEX);

    let error = reminders.inner.save(&armed).expect_err("sync must fail");

    plan.assert_all_fired();
    assert_eq!(error.phase, PersistPhase::BeforeEffect);
    assert!(error.message.contains("sync reminder directory"));
}

#[tokio::test]
async fn cancel_remove_directory_sync_failure_is_unknown_and_restores_timer() {
    let (_directory, path, inbox) = persist_setup();
    let (plan, ops) = publish_fault(PublishStep::DirSync);
    let reminders = Arc::new(Reminders::create_with_publish_ops(
        inbox.clone(),
        Some(path.clone()),
        ops,
    ));
    let item = StoredReminder {
        id: "r1".into(),
        fire_at: Utc::now() + TimeDelta::minutes(1),
        message: "keep armed".into(),
    };
    std::fs::write(&path, serde_json::to_vec_pretty(&[&item]).unwrap()).unwrap();
    reminders.arm(item, false).expect("arm without persistence");
    let cancel = RemindCancelTool {
        reminders: Arc::clone(&reminders),
    };

    let result = run(&cancel, r#"{"id":"r1"}"#).await;

    plan.assert_all_fired();
    assert!(result.is_error, "{result:?}");
    assert_eq!(outcome(&result).disposition, OperationDisposition::Error);
    assert_eq!(outcome(&result).effect_certainty, EffectCertainty::Unknown);
    assert!(!path.exists(), "the remove may already be visible");
    let armed = reminders.inner.armed.lock().expect(STATE_MUTEX);
    assert_eq!(armed.len(), 1, "cancel must restore memory");
    assert!(
        !armed[0].token.is_cancelled(),
        "rollback must keep timer armed"
    );
}

#[tokio::test]
async fn cancel_directory_sync_failure_is_unknown_and_restores_entry_and_token() {
    let (_directory, path, inbox) = persist_setup();
    let (plan, ops) = publish_fault(PublishStep::DirSync);
    let reminders = Arc::new(Reminders::create_with_publish_ops(
        inbox.clone(),
        Some(path.clone()),
        ops,
    ));
    let fire_at = Utc::now() + TimeDelta::minutes(1);
    let items = [
        StoredReminder {
            id: "r1".into(),
            fire_at,
            message: "first".into(),
        },
        StoredReminder {
            id: "r2".into(),
            fire_at,
            message: "keep armed".into(),
        },
        StoredReminder {
            id: "r3".into(),
            fire_at,
            message: "third".into(),
        },
    ];
    std::fs::write(&path, serde_json::to_vec_pretty(&items).unwrap()).unwrap();
    for item in items {
        reminders.arm(item, false).expect("arm without persistence");
    }
    let cancel = RemindCancelTool {
        reminders: Arc::clone(&reminders),
    };

    let result = run(&cancel, r#"{"id":"r2"}"#).await;

    plan.assert_all_fired();
    assert!(result.is_error, "{result:?}");
    assert_eq!(outcome(&result).disposition, OperationDisposition::Error);
    assert_eq!(outcome(&result).effect_certainty, EffectCertainty::Unknown);
    let stored = persisted(&path);
    assert_eq!(
        stored
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["r1", "r3"],
        "the replacement may already be visible"
    );
    let armed = reminders.inner.armed.lock().expect(STATE_MUTEX);
    assert_eq!(
        armed
            .iter()
            .map(|entry| entry.item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["r1", "r2", "r3"],
        "cancel must restore the original index"
    );
    assert!(
        !armed[1].token.is_cancelled(),
        "rollback must keep timer armed"
    );
}

#[tokio::test]
async fn more_than_eight_outstanding_reminders_are_rejected() {
    let (tool, _reminders, _inbox) = tool_and_inbox();
    for index in 0..MAX_OUTSTANDING {
        let arguments = format!(r#"{{"seconds":60,"message":"{index}"}}"#);
        let result = run(&tool, &arguments).await;
        assert!(!result.is_error, "{index}: {result:?}");
    }
    let overflow = run(&tool, r#"{"seconds":60,"message":"too many"}"#).await;
    assert!(overflow.is_error, "{overflow:?}");
    assert!(
        overflow.content.contains("too many reminders"),
        "{}",
        overflow.content
    );
}

#[tokio::test]
async fn a_cancelled_turn_does_not_schedule() {
    let (tool, _reminders, inbox) = tool_and_inbox();
    let result = run_cancelled(&tool, r#"{"seconds":1,"message":"nope"}"#).await;
    assert!(result.is_error, "{result:?}");
    assert_eq!(result.content, CONTEXT_CANCELED);
    assert_eq!(
        outcome(&result).effect_certainty,
        EffectCertainty::NotStarted
    );
    assert_eq!(
        outcome(&result).disposition,
        OperationDisposition::Cancelled
    );
    assert!(inbox.is_empty());
}

#[tokio::test]
async fn a_schedule_is_written_beside_the_session() {
    let (_directory, path, inbox) = persist_setup();
    let reminders = Arc::new(Reminders::with_persist(inbox.clone(), path.clone()));
    let tool = RemindTool {
        reminders: Arc::clone(&reminders),
    };
    let result = run(&tool, r#"{"seconds":60,"message":"later"}"#).await;
    assert!(!result.is_error, "{result:?}");
    let stored = persisted(&path);
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].message, "later");
    assert!(inbox.is_empty());
}

#[tokio::test]
async fn reopening_fires_a_due_reminder_and_clears_it() {
    let (_directory, path, inbox) = persist_setup();
    std::fs::write(
        &path,
        r#"[{"id":"r1","fire_at":"2020-01-01T00:00:00Z","message":"due"}]"#,
    )
    .expect("write");
    let _reminders = Reminders::with_persist(inbox.clone(), path.clone());
    wait_until_fired(&inbox).await;
    let items = inbox.drain();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].text, "[timer] due");
    assert!(persisted(&path).is_empty(), "{:?}", persisted(&path));
}

#[tokio::test]
async fn the_notification_is_queued_before_the_timer_leaves_the_file() {
    let (_directory, path, _unused) = persist_setup();
    // The inbox's change callback runs synchronously inside `push`,
    // before `fire` goes on to remove the timer from the sidecar. If the
    // sidecar still lists the timer at that point, the push happened
    // first.
    let seen_on_disk_at_push = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe = Arc::clone(&seen_on_disk_at_push);
    let probe_path = path.clone();
    let inbox = Arc::new(Inbox::new(Some(Box::new(move || {
        let stored = persisted(&probe_path);
        if stored.iter().any(|item| item.message == "check ordering") {
            probe.store(true, Ordering::SeqCst);
        }
    }))));
    let reminders = Reminders::with_persist(inbox.clone(), path.clone());
    reminders
        .schedule(Duration::ZERO, "check ordering".into())
        .expect("schedule");

    wait_until_fired(&inbox).await;

    assert!(
        seen_on_disk_at_push.load(Ordering::SeqCst),
        "the timer must still be on disk when its notification is pushed"
    );
    assert!(
        persisted(&path).is_empty(),
        "the timer is removed once it has fired"
    );
}

#[tokio::test]
async fn a_fire_after_the_session_file_is_gone_does_not_recreate_the_sidecar() {
    // Mirrors what `session::prepared::archive_open_file` leaves behind:
    // the session's own file gone, and its sidecars already removed. A
    // timer racing that archive on another thread must still notify the
    // in-process inbox (at-least-once delivery), but must not recreate
    // the sidecar file for a session that no longer exists.
    let (directory, path, inbox) = persist_setup();
    let reminders = Reminders::with_persist(inbox.clone(), path.clone());
    let item = reminders
        .schedule(Duration::from_secs(60), "late".into())
        .expect("schedule");
    assert!(
        path.exists(),
        "the sidecar exists while the session is live"
    );

    std::fs::remove_file(directory.path().join("session.jsonl")).expect("remove session file");
    let _ = std::fs::remove_file(&path);

    reminders.inner.fire(&item.id);

    assert!(
        !inbox.is_empty(),
        "the notification still reaches the in-process inbox"
    );
    assert!(
        !path.exists(),
        "a fire that races the archive must not recreate the sidecar"
    );
}

#[test]
fn session_file_exists_checks_the_id_derived_from_the_sidecar_suffix() {
    let directory = tempfile::tempdir().expect("temp dir");
    let sidecar = directory.path().join("s1.reminders.json");

    assert!(
        !session_file_exists(&sidecar, ".reminders.json"),
        "no session file yet"
    );

    std::fs::write(directory.path().join("s1.jsonl"), b"").expect("write session file");
    assert!(
        session_file_exists(&sidecar, ".reminders.json"),
        "the session file now exists"
    );

    std::fs::remove_file(directory.path().join("s1.jsonl")).expect("remove session file");
    assert!(
        !session_file_exists(&sidecar, ".reminders.json"),
        "archiving removed it again"
    );
}

#[tokio::test]
async fn dropping_the_registry_keeps_unfired_reminders_on_disk() {
    let (_directory, path, inbox) = persist_setup();
    {
        let reminders = Arc::new(Reminders::with_persist(inbox.clone(), path.clone()));
        let tool = RemindTool {
            reminders: Arc::clone(&reminders),
        };
        let result = run(&tool, r#"{"seconds":60,"message":"keep"}"#).await;
        assert!(!result.is_error, "{result:?}");
    }
    let stored = persisted(&path);
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].message, "keep");
}

#[tokio::test]
async fn a_restored_session_does_not_reuse_an_outstanding_id() {
    let (_directory, path, inbox) = persist_setup();
    std::fs::write(
        &path,
        r#"[{"id":"r3","fire_at":"2999-01-01T00:00:00Z","message":"restored"}]"#,
    )
    .expect("write");
    let reminders = Reminders::with_persist(inbox.clone(), path.clone());

    let fresh = reminders
        .schedule(Duration::from_secs(60), "new".into())
        .expect("schedule");

    assert_eq!(fresh.id, "r4");
    let ids: Vec<String> = reminders.list().into_iter().map(|item| item.id).collect();
    assert_eq!(ids, vec!["r4".to_string(), "r3".to_string()]);
}

#[tokio::test]
async fn the_list_is_earliest_first() {
    let (_tool, reminders, _inbox) = tool_and_inbox();
    reminders
        .schedule(Duration::from_secs(60), "later".into())
        .expect("schedule");
    reminders
        .schedule(Duration::from_secs(5), "sooner".into())
        .expect("schedule");

    let messages: Vec<String> = reminders
        .list()
        .into_iter()
        .map(|item| item.message)
        .collect();

    assert_eq!(messages, vec!["sooner".to_string(), "later".to_string()]);
}

#[tokio::test]
async fn cancelling_a_timer_stops_it_and_rewrites_the_file() {
    let (_directory, path, inbox) = persist_setup();
    let reminders = Reminders::with_persist(inbox.clone(), path.clone());
    reminders
        .schedule(Duration::from_secs(60), "keep".into())
        .expect("schedule");
    let doomed = reminders
        .schedule(Duration::ZERO, "stop me".into())
        .expect("schedule");

    let canceled = reminders.cancel(&doomed.id).expect("cancel");

    assert_eq!(canceled.message, "stop me");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(inbox.is_empty(), "a canceled timer must not fire");
    let stored = persisted(&path);
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].message, "keep");
    assert_eq!(reminders.list().len(), 1);
}

#[tokio::test]
async fn a_cancel_racing_a_fire_never_lets_both_win() {
    // `Inner::fire` and `Reminders::cancel` both remove the timer under
    // the same `armed` lock, so they must serialize: either the cancel
    // wins outright (no notification) or the fire wins outright (the
    // cancel sees "unknown timer"), never a canceled timer that still
    // notifies. Drive `fire` from a real OS thread against `cancel` on
    // this one, synchronized by a barrier, to force the two onto the
    // lock at the same instant instead of relying on `tokio::select!`'s
    // branch-selection timing.
    let (_directory, path, inbox) = persist_setup();
    let reminders = Reminders::with_persist(inbox.clone(), path.clone());
    let item = reminders
        .schedule(Duration::from_secs(60), "race".into())
        .expect("schedule");
    let id = item.id.clone();

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let fire_inner = Arc::clone(&reminders.inner);
    let fire_id = id.clone();
    let fire_barrier = Arc::clone(&barrier);
    let fire_thread = std::thread::spawn(move || {
        fire_barrier.wait();
        fire_inner.fire(&fire_id);
    });

    barrier.wait();
    let cancel_result = reminders.cancel(&id);
    fire_thread.join().expect("fire thread panicked");

    let notified = !inbox.is_empty();
    match cancel_result {
        Ok(_) => assert!(
            !notified,
            "cancel reported success but the timer also notified"
        ),
        Err(_) => assert!(
            notified,
            "fire won the race but left no notification behind"
        ),
    }
}

#[tokio::test]
async fn cancelling_an_unknown_timer_reports_the_id() {
    let (_tool, reminders, _inbox) = tool_and_inbox();
    let error = reminders.cancel("r9").expect_err("unknown id");
    assert_eq!(error, "unknown timer: r9");
}

#[tokio::test]
async fn clearing_stops_every_timer_and_removes_the_file() {
    let (_directory, path, inbox) = persist_setup();
    let reminders = Reminders::with_persist(inbox.clone(), path.clone());
    reminders
        .schedule(Duration::ZERO, "one".into())
        .expect("schedule");
    reminders
        .schedule(Duration::from_secs(60), "two".into())
        .expect("schedule");

    reminders.clear();

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(inbox.is_empty(), "a cleared timer must not fire");
    assert!(reminders.list().is_empty());
    assert!(!path.exists(), "the sidecar must be gone");
}

#[tokio::test]
async fn the_status_tool_lists_outstanding_timers() {
    let inbox = Arc::new(Inbox::default());
    let reminders = Arc::new(Reminders::new(inbox.clone()));
    let status = RemindStatusTool {
        reminders: Arc::clone(&reminders),
    };

    let empty = run(&status, "{}").await;
    assert!(!empty.is_error, "{empty:?}");
    assert_eq!(empty.content, NO_TIMERS);

    reminders
        .schedule(Duration::from_secs(90), "check the build".into())
        .expect("schedule");
    let listed = run(&status, "{}").await;

    assert!(!listed.is_error, "{listed:?}");
    assert!(listed.content.starts_with("r1  "), "{}", listed.content);
    assert!(listed.content.contains("in 1m"), "{}", listed.content);
    assert!(
        listed.content.ends_with("  check the build"),
        "{}",
        listed.content
    );
}

#[tokio::test]
async fn the_cancel_tool_stops_a_timer_by_id() {
    let inbox = Arc::new(Inbox::default());
    let reminders = Arc::new(Reminders::new(inbox.clone()));
    let cancel = RemindCancelTool {
        reminders: Arc::clone(&reminders),
    };
    reminders
        .schedule(Duration::ZERO, "stop me".into())
        .expect("schedule");

    let result = run(&cancel, r#"{"id":"r1"}"#).await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.content, "canceled r1: stop me");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(inbox.is_empty());

    let unknown = run(&cancel, r#"{"id":"r1"}"#).await;
    assert!(unknown.is_error, "{unknown:?}");
    assert_eq!(unknown.content, "unknown timer: r1");
    assert_eq!(
        outcome(&unknown).effect_certainty,
        EffectCertainty::NotStarted
    );
}

#[test]
fn a_timer_line_shows_the_wait_and_due_once_it_passes() {
    let now = DateTime::parse_from_rfc3339("2026-09-20T12:00:00Z")
        .expect("now")
        .with_timezone(&Utc);
    let item = StoredReminder {
        id: "r1".into(),
        fire_at: now + TimeDelta::seconds(252),
        message: "check the build".into(),
    };

    assert_eq!(timer_line(&item, now), "r1     in 4m12s  check the build");

    let overdue = StoredReminder {
        fire_at: now - TimeDelta::seconds(5),
        ..item
    };
    assert_eq!(
        timer_line(&overdue, now),
        "r1          due  check the build"
    );
}
