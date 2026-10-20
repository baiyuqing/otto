
use crate::schedule::time::format_local;
use crate::schedule::{delivery_text, run_text};
use chrono::TimeZone;
use std::sync::Arc;

use super::*;

fn local(text: &str) -> DateTime<Utc> {
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
    Local
        .from_local_datetime(&naive)
        .earliest()
        .unwrap()
        .with_timezone(&Utc)
}

/// The wall clock at the store's millisecond resolution.
fn now_ms() -> DateTime<Utc> {
    from_ms(ms(Utc::now()))
}

fn origin() -> Origin {
    Origin::chat("telegram", "42").unwrap()
}

fn new_item(kind: Kind, body: &str, due_at: DateTime<Utc>) -> NewItem {
    NewItem {
        origin: origin(),
        session_id: "sess-1".into(),
        workspace: "/work".into(),
        kind,
        body: body.into(),
        repeat: None,
        local_time: None,
        due_at,
    }
}

fn recurring(repeat: Repeat, time: &str, due_at: DateTime<Utc>) -> NewItem {
    NewItem {
        repeat: Some(repeat),
        local_time: Some(NaiveTime::parse_from_str(time, "%H:%M").unwrap()),
        ..new_item(Kind::Text, "daily", due_at)
    }
}

fn store() -> Store {
    Store::open_in_memory().unwrap()
}

fn item(store: &Store, id: i64) -> Item {
    store
        .read(|connection| get(connection, id))
        .unwrap()
        .unwrap()
}

fn count(store: &Store, table: &str) -> i64 {
    store
        .read(|connection| {
            connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), (), |row| {
                row.get(0)
            })
        })
        .unwrap()
}

const ANY_LEASE: Duration = Duration::from_secs(180);

#[test]
fn create_limits_active_items_per_origin() {
    let store = store();
    let now = now_ms();
    for _ in 0..32 {
        store.create(new_item(Kind::Text, "x", now), now).unwrap();
    }
    assert_eq!(
        store
            .create(new_item(Kind::Text, "x", now), now)
            .unwrap_err(),
        CreateError::Limit
    );
    let other = NewItem {
        origin: Origin::web(),
        ..new_item(Kind::Text, "x", now)
    };
    store.create(other, now).unwrap();
    store.cancel(&origin(), 1, now).unwrap().unwrap();
    store.create(new_item(Kind::Text, "x", now), now).unwrap();
}

#[test]
fn concurrent_store_handles_claim_an_occurrence_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("schedule.db");
    let first = Arc::new(Store::open(&path).unwrap());
    let second = Arc::new(Store::open(&path).unwrap());
    let now = now_ms();
    let item = first
        .create(new_item(Kind::Text, "once", now), now)
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers = [Arc::clone(&first), Arc::clone(&second)].map(|store| {
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            store
                .claim_deliveries(&Scope::Any, ANY_LEASE, 1, now)
                .unwrap()
        })
    });
    let claimed: Vec<_> = workers
        .into_iter()
        .flat_map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(
        claimed.iter().map(|claimed| claimed.id).collect::<Vec<_>>(),
        [item.id]
    );
}

#[test]
fn a_delivery_claim_holds_until_its_lease_expires() {
    let store = store();
    let now = now_ms();
    store.create(new_item(Kind::Text, "hi", now), now).unwrap();
    let first = store
        .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].result, "⏰ hi");
    assert!(
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
            .unwrap()
            .is_empty()
    );
    let later = now + TimeDelta::seconds(181);
    assert_eq!(
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 10, later)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn an_item_is_not_claimable_before_it_is_due() {
    let store = store();
    let now = now_ms();
    store
        .create(
            new_item(Kind::Text, "later", now + TimeDelta::seconds(60)),
            now,
        )
        .unwrap();
    assert!(
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
            .unwrap()
            .is_empty()
    );
    let due = now + TimeDelta::seconds(60);
    assert_eq!(
        store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 10, due)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn delivery_completes_a_one_shot_item_once() {
    let store = store();
    let now = now_ms();
    let created = store.create(new_item(Kind::Text, "hi", now), now).unwrap();
    store
        .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
        .unwrap();
    assert!(store.delivered(created.id, created.due_at, now).unwrap());
    assert!(!store.delivered(created.id, created.due_at, now).unwrap());
    assert_eq!(item(&store, created.id).state, State::Done);
    assert_eq!(count(&store, "deliveries"), 1);
    let pending = store
        .pending_writebacks(&origin(), "other-session", 10)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].text, "⏰ hi");
    store.mark_appended(created.id, created.due_at).unwrap();
    assert!(
        store
            .pending_writebacks(&origin(), "other-session", 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn cancelled_items_ignore_late_results() {
    let store = store();
    let now = now_ms();
    let prompt = store
        .create(new_item(Kind::Prompt, "check", now), now)
        .unwrap();
    store.claim_run(&Scope::Any, None, now).unwrap().unwrap();
    store.cancel(&origin(), prompt.id, now).unwrap().unwrap();
    let outcome = RunOutcome::Reply("late".into());
    assert!(
        !store
            .finish_run(prompt.id, prompt.due_at, outcome, now)
            .unwrap()
    );
    assert_eq!(item(&store, prompt.id).state, State::Cancelled);

    let text = store.create(new_item(Kind::Text, "hi", now), now).unwrap();
    store
        .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
        .unwrap();
    store.cancel(&origin(), text.id, now).unwrap().unwrap();
    assert!(!store.delivered(text.id, text.due_at, now).unwrap());
    store
        .delivery_failed(text.id, text.due_at, "e", now)
        .unwrap();
    let after = item(&store, text.id);
    assert_eq!((after.state, after.attempts), (State::Cancelled, 0));
    assert_eq!(count(&store, "deliveries"), 0);
    assert!(
        store.cancel(&origin(), text.id, now).unwrap().is_none(),
        "already cancelled"
    );
    assert!(
        store
            .cancel(&Origin::web(), prompt.id, now)
            .unwrap()
            .is_none(),
        "other origin"
    );
}

#[test]
fn results_for_an_old_occurrence_are_ignored() {
    let store = store();
    let now = now_ms();
    let created = store
        .create(new_item(Kind::Prompt, "check", now), now)
        .unwrap();
    store.claim_run(&Scope::Any, None, now).unwrap().unwrap();
    let stale = now - TimeDelta::seconds(5);
    assert!(
        !store
            .finish_run(created.id, stale, RunOutcome::Reply("x".into()), now)
            .unwrap()
    );
    assert_eq!(item(&store, created.id).state, State::Waiting);
}

#[test]
fn run_claims_respect_scope_workspace_kind_and_lease() {
    let store = store();
    let now = now_ms();
    store
        .create(new_item(Kind::Text, "text", now), now)
        .unwrap();
    assert!(
        store.claim_run(&Scope::Any, None, now).unwrap().is_none(),
        "text is not run"
    );
    let prompt = store.create(new_item(Kind::Prompt, "p", now), now).unwrap();
    assert!(store.claim_run(&Scope::Web, None, now).unwrap().is_none());
    assert!(
        store
            .claim_run(&Scope::Any, Some("/other"), now)
            .unwrap()
            .is_none()
    );
    let chats = Scope::Chats(vec!["telegram".into()]);
    let claimed = store
        .claim_run(&chats, Some("/work"), now)
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, prompt.id);
    assert!(
        store.claim_run(&Scope::Any, None, now).unwrap().is_none(),
        "leased"
    );
    assert!(
        store
            .claim_run(&Scope::Any, None, now + TimeDelta::minutes(14))
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .claim_run(&Scope::Any, None, now + TimeDelta::minutes(16))
            .unwrap()
            .is_some()
    );
}

#[test]
fn run_results_become_delivery_text() {
    let store = store();
    let now = now_ms();
    let cases = [
        (RunOutcome::Reply("  sunny \n".into()), "⏰ sunny"),
        (
            RunOutcome::Reply("  ".into()),
            "⏰ Scheduled prompt s1 finished with no reply.",
        ),
        (
            RunOutcome::Failed("boom".into()),
            "⏰ Scheduled prompt s1 failed: boom",
        ),
    ];
    for (outcome, expected) in cases {
        let store = &store;
        store.write(|c| c.execute("DELETE FROM items", ())).unwrap();
        // ids keep counting; compute the label from the created item.
        let created = store.create(new_item(Kind::Prompt, "p", now), now).unwrap();
        store.claim_run(&Scope::Any, None, now).unwrap().unwrap();
        assert!(
            store
                .finish_run(created.id, created.due_at, outcome, now)
                .unwrap()
        );
        let done = item(store, created.id);
        assert_eq!(done.state, State::Ready);
        assert_eq!(done.result, expected.replace("s1", &created.label()));
        assert_eq!(done.claimed_until, DateTime::<Utc>::default());
    }
}

#[test]
fn a_failure_before_tools_is_retried_with_backoff_until_24_hours() {
    let store = store();
    let now = now_ms();
    let created = store.create(new_item(Kind::Prompt, "p", now), now).unwrap();
    let mut at = now;
    for expected in [30, 60, 120, 240, 480, 960, 1800, 1800] {
        store.claim_run(&Scope::Any, None, at).unwrap().unwrap();
        let outcome = RunOutcome::FailedBeforeTools("provider down".into());
        assert!(
            store
                .finish_run(created.id, created.due_at, outcome, at)
                .unwrap()
        );
        let row = item(&store, created.id);
        assert_eq!(row.state, State::Waiting);
        assert_eq!(row.last_error, "provider down");
        assert_eq!(row.next_try_at - at, TimeDelta::seconds(expected));
        assert!(store.claim_run(&Scope::Any, None, at).unwrap().is_none());
        at = row.next_try_at;
    }
    let late = now + TimeDelta::hours(24);
    store.claim_run(&Scope::Any, None, late).unwrap().unwrap();
    let outcome = RunOutcome::FailedBeforeTools("provider down".into());
    store
        .finish_run(created.id, created.due_at, outcome, late)
        .unwrap();
    let row = item(&store, created.id);
    assert_eq!(row.state, State::Ready);
    assert_eq!(row.result, "⏰ Scheduled prompt s1 failed: provider down");
}

#[test]
fn a_failure_text_is_cut_to_300_characters() {
    let store = store();
    let now = now_ms();
    let created = store.create(new_item(Kind::Prompt, "p", now), now).unwrap();
    store.claim_run(&Scope::Any, None, now).unwrap();
    let outcome = RunOutcome::Failed("e".repeat(1000));
    store
        .finish_run(created.id, created.due_at, outcome, now)
        .unwrap();
    let row = item(&store, created.id);
    assert_eq!(
        row.result.chars().count(),
        "⏰ Scheduled prompt s1 failed: ".chars().count() + 303
    );
}

#[test]
fn delivery_failures_back_off_to_30_minutes_and_clear_on_success() {
    let store = store();
    let now = now_ms();
    let created = store.create(new_item(Kind::Text, "hi", now), now).unwrap();
    let mut at = now;
    for expected in [30, 60, 120, 240, 480, 960, 1800, 1800] {
        let claimed = store
            .claim_deliveries(&Scope::Any, ANY_LEASE, 1, at)
            .unwrap();
        assert_eq!(claimed.len(), 1, "claimable at {at}");
        store
            .delivery_failed(created.id, created.due_at, "send failed", at)
            .unwrap();
        let row = item(&store, created.id);
        assert_eq!(row.last_error, "send failed");
        assert_eq!(row.next_try_at - at, TimeDelta::seconds(expected));
        assert!(
            store
                .claim_deliveries(&Scope::Any, ANY_LEASE, 1, at)
                .unwrap()
                .is_empty()
        );
        at = row.next_try_at;
    }
    store
        .claim_deliveries(&Scope::Any, ANY_LEASE, 1, at)
        .unwrap();
    store.delivered(created.id, created.due_at, at).unwrap();
    assert_eq!(item(&store, created.id).last_error, "");
}

#[test]
fn delivery_clients_claim_only_their_exact_destinations_across_store_handles() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schedule.db");
    let first = Store::open(&path).unwrap();
    let second = Store::open(&path).unwrap();
    let now = now_ms();
    let a = Origin::chat("telegram", "a").unwrap();
    let b = Origin::chat("telegram", "b").unwrap();
    first
        .register_delivery_client(
            "a",
            &["telegram".into()],
            Some(std::slice::from_ref(&a)),
            now,
        )
        .unwrap();
    second
        .register_delivery_client(
            "b",
            &["telegram".into()],
            Some(std::slice::from_ref(&b)),
            now,
        )
        .unwrap();
    for index in 0..201 {
        let mut unrelated = new_item(Kind::Text, "unrelated", now);
        unrelated.origin = Origin::chat("telegram", &format!("other-{index}")).unwrap();
        first.create(unrelated, now).unwrap();
    }
    let mut left = new_item(Kind::Text, "only a", now);
    left.origin = a;
    let mut right = new_item(Kind::Text, "only b", now);
    right.origin = b;
    first.create(left, now).unwrap();
    let created = second.create(right, now).unwrap();
    let claimed = first
        .claim_deliveries(&Scope::DeliveryClient("a".into()), ANY_LEASE, 10, now)
        .unwrap();
    assert_eq!(
        claimed
            .iter()
            .map(|row| row.body.as_str())
            .collect::<Vec<_>>(),
        ["only a"]
    );
    assert_eq!(
        item(&second, created.id).claimed_until,
        DateTime::<Utc>::default()
    );
    let claimed = second
        .claim_deliveries(&Scope::DeliveryClient("b".into()), ANY_LEASE, 10, now)
        .unwrap();
    assert_eq!(
        claimed
            .iter()
            .map(|row| row.body.as_str())
            .collect::<Vec<_>>(),
        ["only b"]
    );
}

#[test]
fn legacy_delivery_suspends_for_overlap_and_recovers_after_disconnect_or_expiry() {
    let store = Store::open_in_memory().unwrap();
    let now = now_ms();
    store
        .register_delivery_client("legacy", &["telegram".into()], None, now)
        .unwrap();
    store
        .register_delivery_client("other", &["telegram".into()], None, now)
        .unwrap();
    store
        .create(new_item(Kind::Text, "private", now), now)
        .unwrap();
    let scope = Scope::DeliveryClient("legacy".into());
    assert!(
        store
            .claim_deliveries(&scope, ANY_LEASE, 10, now)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.refresh_delivery_client("legacy", now).unwrap(),
        ["telegram"]
    );
    store.unregister_delivery_client("other").unwrap();
    assert_eq!(
        store
            .claim_deliveries(&scope, ANY_LEASE, 10, now)
            .unwrap()
            .len(),
        1
    );
    let exact = Origin::chat("telegram", "elsewhere").unwrap();
    store
        .register_delivery_client("exact", &["telegram".into()], Some(&[exact]), now)
        .unwrap();
    store
        .create(new_item(Kind::Text, "next", now), now)
        .unwrap();
    assert!(
        store
            .claim_deliveries(&scope, ANY_LEASE, 10, now)
            .unwrap()
            .is_empty()
    );
    let later = now + TimeDelta::seconds(91);
    assert!(
        store
            .refresh_delivery_client("legacy", later)
            .unwrap()
            .is_empty()
    );
    assert!(
        !store
            .claim_deliveries(&scope, ANY_LEASE, 10, later)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn version_one_schedule_migrates_without_losing_items() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("schedule.db");
    let store = Store::open(&path).unwrap();
    let now = now_ms();
    let created = store
        .create(new_item(Kind::Text, "keep", now), now)
        .unwrap();
    drop(store);
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("DROP TABLE delivery_clients").unwrap();
    raw.pragma_update("user_version", 1).unwrap();
    drop(raw);
    let store = Store::open(&path).unwrap();
    assert_eq!(item(&store, created.id).body, "keep");
    store
        .register_delivery_client("client", &["telegram".into()], None, now)
        .unwrap();
    assert_eq!(
        store
            .claim_deliveries(&Scope::DeliveryClient("client".into()), ANY_LEASE, 10, now)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn claim_deliveries_honours_scope_and_limit() {
    let store = store();
    let now = now_ms();
    for _ in 0..3 {
        store
            .create(new_item(Kind::Text, "chat", now), now)
            .unwrap();
    }
    let web = NewItem {
        origin: Origin::web(),
        ..new_item(Kind::Text, "web", now)
    };
    store.create(web, now).unwrap();
    let claimed = store
        .claim_deliveries(&Scope::Web, ANY_LEASE, 10, now)
        .unwrap();
    assert_eq!(
        claimed.iter().map(|i| i.body.as_str()).collect::<Vec<_>>(),
        ["web"]
    );
    let chats = Scope::Chats(vec!["telegram".into()]);
    assert_eq!(
        store
            .claim_deliveries(&chats, ANY_LEASE, 2, now)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_one_shot_occurrence_fails_after_24_hours() {
    let store = store();
    let now = now_ms();
    let created = store.create(new_item(Kind::Text, "old", now), now).unwrap();
    store.sweep(now + TimeDelta::hours(23)).unwrap();
    assert_eq!(item(&store, created.id).state, State::Waiting);
    store.sweep(now + TimeDelta::hours(25)).unwrap();
    let row = item(&store, created.id);
    assert_eq!(row.state, State::Failed);
    assert_eq!(row.last_error, EXPIRED);
}

#[test]
fn sweep_leaves_claimed_rows_alone() {
    let store = store();
    let now = now_ms();
    let created = store.create(new_item(Kind::Prompt, "p", now), now).unwrap();
    let claim_time = now + TimeDelta::hours(24);
    store
        .claim_run(&Scope::Any, None, claim_time)
        .unwrap()
        .unwrap();
    store.sweep(claim_time + TimeDelta::minutes(1)).unwrap();
    assert_eq!(item(&store, created.id).state, State::Waiting);
    store.sweep(claim_time + TimeDelta::minutes(16)).unwrap();
    assert_eq!(item(&store, created.id).state, State::Failed);
}

#[test]
fn a_missed_recurring_occurrence_catches_up_within_24_hours() {
    let store = store();
    let due = local("2026-10-02 08:00");
    let now = local("2026-10-04 12:00");
    let created = store
        .create(recurring(Repeat::Daily, "08:00", due), due)
        .unwrap();
    store.sweep(now).unwrap();
    let row = item(&store, created.id);
    assert_eq!(row.state, State::Waiting);
    assert_eq!(row.due_at, local("2026-10-04 08:00"));
    assert_eq!(row.next_try_at, row.due_at);
    assert_eq!(row.last_error, "");
}

#[test]
fn a_recurring_occurrence_older_than_24_hours_is_skipped() {
    let store = store();
    // 2026-10-02 is a Friday; on Sunday noon the latest weekday occurrence is 52 h old.
    let due = local("2026-10-02 08:00");
    let now = local("2026-10-04 12:00");
    let created = store
        .create(recurring(Repeat::Weekdays, "08:00", due), due)
        .unwrap();
    store.sweep(now).unwrap();
    let row = item(&store, created.id);
    assert_eq!(row.state, State::Waiting);
    assert_eq!(row.due_at, local("2026-10-05 08:00"));
    assert_eq!(row.last_error, EXPIRED);
}

#[test]
fn an_undelivered_ready_recurring_occurrence_advances_after_24_hours() {
    let store = store();
    let due = local("2026-10-01 08:00");
    let created = store
        .create(recurring(Repeat::Daily, "08:00", due), due)
        .unwrap();
    store
        .claim_deliveries(&Scope::Any, ANY_LEASE, 1, due)
        .unwrap();
    let now = local("2026-10-02 09:00");
    store.sweep(now).unwrap();
    let row = item(&store, created.id);
    assert_eq!(
        (row.state, row.last_error.as_str()),
        (State::Waiting, EXPIRED)
    );
    assert_eq!(row.due_at, local("2026-10-03 08:00"));
    assert_eq!(row.result, "");
}

#[test]
fn delivering_a_recurring_item_advances_it() {
    let store = store();
    let due = local("2026-10-01 08:00");
    let created = store
        .create(recurring(Repeat::Daily, "08:00", due), due)
        .unwrap();
    store
        .claim_deliveries(&Scope::Any, ANY_LEASE, 1, due)
        .unwrap();
    store.delivery_failed(created.id, due, "x", due).unwrap();
    let retry = due + TimeDelta::seconds(30);
    store
        .claim_deliveries(&Scope::Any, ANY_LEASE, 1, retry)
        .unwrap();
    assert!(store.delivered(created.id, due, retry).unwrap());
    let row = item(&store, created.id);
    assert_eq!((row.state, row.attempts), (State::Waiting, 0));
    assert_eq!(row.due_at, local("2026-10-02 08:00"));
    assert_eq!((row.last_error.as_str(), row.result.as_str()), ("", ""));
}

#[test]
fn sweep_deletes_ended_rows_and_deliveries_after_7_days() {
    let store = store();
    let now = now_ms();
    let created = store.create(new_item(Kind::Text, "hi", now), now).unwrap();
    store
        .claim_deliveries(&Scope::Any, ANY_LEASE, 1, now)
        .unwrap();
    store.delivered(created.id, created.due_at, now).unwrap();
    store.sweep(now + TimeDelta::days(6)).unwrap();
    assert_eq!(
        (count(&store, "items"), count(&store, "deliveries")),
        (1, 1)
    );
    store.sweep(now + TimeDelta::days(8)).unwrap();
    assert_eq!(
        (count(&store, "items"), count(&store, "deliveries")),
        (0, 0)
    );
}

#[test]
fn list_shows_active_then_recently_ended_items() {
    let store = store();
    let now = now_ms();
    let ended = store
        .create(new_item(Kind::Text, "ended", now), now)
        .unwrap();
    store.cancel(&origin(), ended.id, now).unwrap();
    store
        .create(
            new_item(Kind::Text, "active", now + TimeDelta::hours(1)),
            now,
        )
        .unwrap();
    store
        .create(
            NewItem {
                origin: Origin::web(),
                ..new_item(Kind::Text, "other", now)
            },
            now,
        )
        .unwrap();
    let bodies = |at| {
        store
            .list(&origin(), at)
            .unwrap()
            .into_iter()
            .map(|i| i.body)
            .collect::<Vec<_>>()
    };
    assert_eq!(bodies(now), ["active", "ended"]);
    assert_eq!(bodies(now + TimeDelta::days(8)), ["active"]);
}

#[test]
fn writebacks_match_chat_by_origin_and_terminal_by_session() {
    let store = store();
    let now = now_ms();
    let mut ids = Vec::new();
    let specs = [
        ("chat-a", origin(), "sess-1"),
        ("chat-b", origin(), "sess-2"),
        (
            "other-chat",
            Origin::chat("telegram", "7").unwrap(),
            "sess-1",
        ),
        ("term-1", Origin::terminal(), "sess-1"),
        ("web-1", Origin::web(), "sess-1"),
        ("term-2", Origin::terminal(), "sess-2"),
    ];
    for (body, origin, session) in specs {
        let new = NewItem {
            origin,
            session_id: session.into(),
            ..new_item(Kind::Text, body, now)
        };
        let created = store.create(new, now).unwrap();
        ids.push((created.id, created.due_at));
    }
    store
        .claim_deliveries(&Scope::Any, ANY_LEASE, 10, now)
        .unwrap();
    for (offset, (id, due)) in ids.iter().enumerate() {
        store
            .delivered(*id, *due, now + TimeDelta::seconds(offset as i64))
            .unwrap();
    }
    let texts = |origin: &Origin, session: &str, limit| {
        store
            .pending_writebacks(origin, session, limit)
            .unwrap()
            .into_iter()
            .map(|d| d.text)
            .collect::<Vec<_>>()
    };
    assert_eq!(texts(&origin(), "sess-9", 10), ["⏰ chat-a", "⏰ chat-b"]);
    assert_eq!(texts(&origin(), "sess-9", 1), ["⏰ chat-a"]);
    assert_eq!(
        texts(&Origin::terminal(), "sess-1", 10),
        ["⏰ term-1", "⏰ web-1"]
    );
    assert_eq!(texts(&Origin::web(), "sess-2", 10), ["⏰ term-2"]);
}

#[test]
fn next_due_is_the_earliest_claimable_instant() {
    let store = store();
    let now = now_ms();
    assert_eq!(store.next_due(now).unwrap(), None);
    store
        .create(
            new_item(Kind::Text, "later", now + TimeDelta::seconds(90)),
            now,
        )
        .unwrap();
    store
        .create(
            new_item(Kind::Text, "soon", now + TimeDelta::seconds(30)),
            now,
        )
        .unwrap();
    let soon = (now + TimeDelta::seconds(30)).timestamp_millis();
    assert_eq!(
        store.next_due(now).unwrap().unwrap().timestamp_millis(),
        soon
    );
}

#[test]
fn delivery_text_adds_a_late_line_after_two_minutes() {
    let store = store();
    let now = now_ms();
    store.create(new_item(Kind::Text, "hi", now), now).unwrap();
    let claimed = |at| {
        store
            .claim_deliveries(&Scope::Any, Duration::ZERO, 1, at)
            .unwrap()
            .remove(0)
    };
    let on_time = claimed(now + TimeDelta::seconds(120));
    assert_eq!(
        delivery_text(&on_time, now + TimeDelta::seconds(120)),
        "⏰ hi"
    );
    let at = now + TimeDelta::seconds(2 * 3600 + 3 * 60);
    let late = claimed(at);
    let expected = format!(
        "⏰ hi\n(scheduled for {}, delivered 2h3m late)",
        format_local(now)
    );
    assert_eq!(delivery_text(&late, at), expected);
}

#[test]
fn run_and_context_text_carry_the_schedule_facts() {
    let now = now_ms();
    let created = store()
        .create(new_item(Kind::Prompt, "check rain", now), now)
        .unwrap();
    let text = run_text(&created, now);
    assert!(
        text.starts_with(&format!("[scheduled run s{}: created ", created.id)),
        "{text}"
    );
    assert!(text.contains("from a telegram chat;"), "{text}");
    assert!(
        text.ends_with("No user is present; approval requests are denied.]\n\ncheck rain"),
        "{text}"
    );
    let delivery = Delivery {
        item_id: 7,
        due_at: now,
        origin: origin(),
        session_id: "s".into(),
        text: "⏰ x".into(),
        delivered_at: now,
    };
    assert_eq!(
        delivery.context_text(),
        format!("[scheduled s7, due {}]\n⏰ x", format_local(now))
    );
}

#[test]
fn the_file_is_private_and_an_unknown_schema_version_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sub").join("schedule.db");
    drop(Store::open(&path).unwrap());
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    let raw = Connection::open(&path).unwrap();
    raw.pragma_update("user_version", 99).unwrap();
    drop(raw);
    assert!(Store::open(&path).is_err());
}
