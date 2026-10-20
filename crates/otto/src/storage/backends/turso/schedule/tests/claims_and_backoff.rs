//! Run and delivery claims, results and retry backoff tests.

use super::*;

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
