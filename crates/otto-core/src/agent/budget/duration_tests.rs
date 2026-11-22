//! The active-time limit: the clock, the holders and the user pauses, with an
//! injected clock so no test sleeps.

use super::clock::{ActiveTime, Change};
use super::*;

const SECOND: Duration = Duration::from_secs(1);

fn seconds(count: u64) -> Duration {
    Duration::from_secs(count)
}

fn limited(max_seconds: u64, clock: &ManualClock) -> RootBudget {
    RootBudget::new(
        "root-1",
        BudgetPolicy {
            max_duration: Limit::from_count(max_seconds),
            ..BudgetPolicy::unlimited()
        },
        clock.shared(),
    )
}

#[test]
fn the_default_is_forty_minutes() {
    assert_eq!(
        BudgetPolicy::default().max_duration,
        Limit::from_count(2_400)
    );
}

#[test]
fn time_with_no_holder_adds_nothing() {
    let clock = ManualClock::new();
    let root = limited(100, &clock);
    clock.advance(seconds(3 * 3600));
    assert_eq!(root.active_time(), Duration::ZERO);
    assert_eq!(root.admit_shared(), Ok(()));
}

#[test]
fn a_holder_counts_from_entry_to_drop_and_the_gap_after_it_adds_nothing() {
    let clock = ManualClock::new();
    let root = limited(100, &clock);
    clock.advance(seconds(50));
    let holder = root.enter();
    clock.advance(seconds(30));
    assert_eq!(root.active_time(), seconds(30));
    drop(holder);
    clock.advance(seconds(10_000));
    assert_eq!(root.active_time(), seconds(30));
    // A later holder (a timer wake turn) adds only the time it runs.
    let wake = root.enter();
    clock.advance(seconds(20));
    drop(wake);
    assert_eq!(root.active_time(), seconds(50));
    assert_eq!(root.admit_shared(), Ok(()));
}

#[test]
fn two_concurrent_holders_are_counted_once() {
    let clock = ManualClock::new();
    let root = limited(1_000, &clock);
    let first = root.enter();
    clock.advance(seconds(10));
    let second = root.enter();
    clock.advance(seconds(10));
    drop(first);
    clock.advance(seconds(10));
    drop(second);
    assert_eq!(root.active_time(), seconds(30));
}

#[test]
fn a_pause_of_every_running_holder_stops_the_clock() {
    let clock = ManualClock::new();
    let root = limited(1_000, &clock);
    let holder = root.enter();
    clock.advance(seconds(10));
    let pause = holder.handle().pause_for_user();
    clock.advance(seconds(500));
    assert_eq!(root.active_time(), seconds(10));
    drop(pause);
    clock.advance(seconds(5));
    assert_eq!(root.active_time(), seconds(15));
}

#[test]
fn a_waiting_holder_does_not_stop_the_clock_while_another_holder_runs() {
    let clock = ManualClock::new();
    let root = limited(1_000, &clock);
    let waiting = root.enter();
    let running = root.enter();
    let pause = waiting.handle().pause_for_user();
    clock.advance(seconds(40));
    assert_eq!(root.active_time(), seconds(40));
    // The running holder ends: every remaining holder waits.
    drop(running);
    clock.advance(seconds(500));
    assert_eq!(root.active_time(), seconds(40));
    // A new holder starts while the other waits: the clock runs again.
    let third = root.enter();
    clock.advance(seconds(7));
    assert_eq!(root.active_time(), seconds(47));
    drop(third);
    drop(pause);
    drop(waiting);
    clock.advance(seconds(500));
    assert_eq!(root.active_time(), seconds(47));
}

#[test]
fn several_pauses_of_one_holder_count_as_one_waiting_holder() {
    let clock = ManualClock::new();
    let root = limited(1_000, &clock);
    let waiting = root.enter();
    let running = root.enter();
    let handle = waiting.handle();
    let first = handle.pause_for_user();
    let second = handle.pause_for_user();
    clock.advance(seconds(10));
    // Two pauses of one holder must not make the running holder look paused.
    assert_eq!(root.active_time(), seconds(10));
    drop(first);
    clock.advance(seconds(10));
    assert_eq!(root.active_time(), seconds(20));
    drop(second);
    drop(running);
    drop(waiting);
}

#[test]
fn dropping_the_guards_restores_the_counts() {
    let clock = ManualClock::new();
    let root = limited(1_000, &clock);
    {
        let holder = root.enter();
        let _pause = holder.handle().pause_for_user();
        let _second = root.enter();
    }
    // Everything was released: time passing adds nothing and a new holder
    // starts from a clean state.
    clock.advance(seconds(100));
    assert_eq!(root.active_time(), Duration::ZERO);
    let holder = root.enter();
    clock.advance(seconds(1));
    assert_eq!(root.active_time(), seconds(1));
    drop(holder);
}

#[test]
fn a_panic_in_a_holder_releases_it() {
    let clock = ManualClock::new();
    let root = limited(1_000, &clock);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let holder = root.enter();
        let _pause = holder.handle().pause_for_user();
        panic!("holder failed");
    }));
    assert!(result.is_err());
    clock.advance(seconds(100));
    assert_eq!(root.active_time(), Duration::ZERO);
}

#[test]
fn a_pause_that_outlives_its_holder_changes_nothing() {
    let clock = ManualClock::new();
    let root = limited(1_000, &clock);
    let holder = root.enter();
    let pause = holder.handle().pause_for_user();
    drop(holder);
    clock.advance(seconds(10));
    let other = root.enter();
    clock.advance(seconds(3));
    drop(pause);
    clock.advance(seconds(2));
    assert_eq!(root.active_time(), seconds(5));
    drop(other);
}

#[test]
fn counts_follow_each_change() {
    let mut time = ActiveTime::new();
    assert_eq!(time.counts(), (0, 0));
    time.apply(Duration::ZERO, Change::Enter);
    time.apply(SECOND, Change::Enter);
    assert_eq!(time.counts(), (2, 0));
    time.apply(SECOND, Change::Wait);
    assert_eq!(time.counts(), (2, 1));
    time.apply(SECOND, Change::Wait);
    assert_eq!(time.counts(), (2, 2));
    time.apply(SECOND, Change::Resume);
    time.apply(SECOND, Change::Leave { waiting: false });
    time.apply(SECOND, Change::Leave { waiting: true });
    assert_eq!(time.counts(), (0, 0));
}

#[test]
fn the_limit_is_reached_at_the_active_time_and_reported_in_seconds() {
    let clock = ManualClock::new();
    let root = limited(2_400, &clock);
    let _holder = root.enter();
    clock.advance(seconds(2_399));
    assert_eq!(root.admit_shared(), Ok(()));
    clock.advance(seconds(2));
    let exceeded = root.admit_shared().unwrap_err();
    assert_eq!(exceeded.kind, BudgetKind::Duration);
    assert_eq!((exceeded.used, exceeded.limit.get()), (2_401, 2_400));
    assert_eq!(
        exceeded.to_string(),
        "2401 of 2400 seconds of active time used by this root turn ([agent.budget].max_duration)"
    );
}

#[test]
fn an_exhausted_root_keeps_reporting_its_first_amount() {
    let clock = ManualClock::new();
    let root = limited(10, &clock);
    let holder = root.enter();
    clock.advance(seconds(11));
    let first = root.admit_shared().unwrap_err();
    clock.advance(seconds(100));
    assert_eq!(root.admit_shared().unwrap_err(), first);
    assert_eq!(root.active_time(), seconds(11));
    drop(holder);
    // A later holder cannot make it admit again.
    let _again = root.enter();
    assert_eq!(root.admit_shared().unwrap_err(), first);
}

#[test]
fn tokens_are_reported_before_the_duration_when_both_are_reached() {
    let clock = ManualClock::new();
    let root = RootBudget::new(
        "root-1",
        BudgetPolicy {
            max_tokens: Limit::from_count(10),
            max_duration: Limit::from_count(10),
            ..BudgetPolicy::unlimited()
        },
        clock.shared(),
    );
    let _holder = root.enter();
    clock.advance(seconds(20));
    root.charge(Some(Usage {
        input_tokens: 10,
        ..Usage::default()
    }));
    assert_eq!(root.admit_shared().unwrap_err().kind, BudgetKind::Tokens);
}

#[test]
fn an_unlimited_duration_never_refuses() {
    let clock = ManualClock::new();
    let root = RootBudget::new("root-1", BudgetPolicy::unlimited(), clock.shared());
    let _holder = root.enter();
    clock.advance(seconds(10_000_000));
    assert_eq!(root.admit_shared(), Ok(()));
}

#[test]
fn a_fresh_scope_measures_no_time() {
    let turn = BudgetScope::Fresh(BudgetPolicy {
        max_duration: Limit::from_count(1),
        ..BudgetPolicy::unlimited()
    })
    .into_turn();
    let _holder = turn.root().enter();
    assert_eq!(turn.admit_shared(), Ok(()));
}

mod session {
    use super::*;

    fn policy(max_seconds: u64) -> BudgetPolicy {
        BudgetPolicy {
            max_duration: Limit::from_count(max_seconds),
            ..BudgetPolicy::unlimited()
        }
    }

    fn ids() -> impl FnOnce() -> String {
        || "generated".to_string()
    }

    #[test]
    fn an_exhausted_duration_withholds_a_wake_turn_and_reports_once() {
        let clock = ManualClock::new();
        let cell = SessionBudgetCell::new(clock.shared());
        let turn = cell.begin_root("a".into(), policy(100));
        let holder = turn.root().enter();
        clock.advance(seconds(101));
        drop(holder);

        let WakeGate::Withheld(first) = cell.wake_gate(ids(), policy(100)) else {
            panic!("the used-up active time withholds the wake turn");
        };
        assert_eq!(first.exceeded.kind, BudgetKind::Duration);
        assert_eq!(first.report, SkipReport::Due);
        let WakeGate::Withheld(second) = cell.wake_gate(ids(), policy(100)) else {
            panic!("still withheld");
        };
        assert_eq!(second.report, SkipReport::Written);
        assert!(cell.exhausted().is_some());
    }

    #[test]
    fn a_long_idle_gap_before_a_timer_wake_does_not_withhold_it() {
        let clock = ManualClock::new();
        let cell = SessionBudgetCell::new(clock.shared());
        let turn = cell.begin_root("a".into(), policy(100));
        let holder = turn.root().enter();
        clock.advance(seconds(60));
        drop(holder);
        // Three hours with no holder running: nothing counts.
        clock.advance(seconds(3 * 3600));
        let WakeGate::Admit(wake) = cell.wake_gate(ids(), policy(100)) else {
            panic!("idle time does not withhold a wake turn");
        };
        // The wake turn adds only the time it runs, to the same root.
        let holder = wake.root().enter();
        clock.advance(seconds(30));
        drop(holder);
        assert_eq!(wake.root().active_time(), seconds(90));
        assert!(matches!(
            cell.wake_gate(ids(), policy(100)),
            WakeGate::Admit(_)
        ));
    }

    #[test]
    fn a_new_user_turn_starts_a_root_with_no_active_time() {
        let clock = ManualClock::new();
        let cell = SessionBudgetCell::new(clock.shared());
        let first = cell.begin_root("a".into(), policy(10));
        let holder = first.root().enter();
        clock.advance(seconds(11));
        assert!(cell.exhausted().is_some());
        drop(holder);
        let second = cell.begin_root("b".into(), policy(10));
        assert!(cell.exhausted().is_none());
        assert_eq!(second.root().active_time(), Duration::ZERO);
    }
}
