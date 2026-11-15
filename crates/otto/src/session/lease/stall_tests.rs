//! Tests for lease revalidation after a stop of the process (see `stall.rs`).
//!
//! A test cannot stop its own process, so it moves the keeper's recorded
//! watchdog tick and the lease's last renewal into the past, as a stop of
//! that length leaves them. Keepers tick every 50 ms, so the stall threshold
//! is 250 ms and the grace period 150 ms; leases use `L` = 1 s, so 5`L`/6 is
//! about 833 ms.

use super::*;

const TICK: Duration = Duration::from_millis(50);
const GRACE: Duration = Duration::from_millis(150);
const STOP: Duration = Duration::from_secs(2);
const WAIT: Duration = Duration::from_secs(3);

struct Held {
    _tmp: tempfile::TempDir,
    session_path: PathBuf,
    keeper: Arc<Keeper>,
    lease: Arc<Lease>,
    fenced_at: Arc<Mutex<Option<Instant>>>,
}

/// A lease acquired through a keeper whose threads run, and which records
/// when its fence action ran.
fn held() -> Held {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 1).unwrap();
    let fenced_at = Arc::new(Mutex::new(None));
    let record = Arc::clone(&fenced_at);
    let keeper = Keeper::new_for_test(
        Box::new(move || *record.lock().unwrap() = Some(Instant::now())),
        Arc::new(Children::new()),
        TICK,
    );
    let (lease, _) = acquire_with(&session_path, &super::tests::FakeClock::new(), &keeper).unwrap();
    Held {
        _tmp: tmp,
        session_path,
        keeper,
        lease,
        fenced_at,
    }
}

/// Leaves the keeper's tick and the lease's last renewal `age` old. The tick
/// is set first: a watchdog tick in between sees a current renewal.
fn stop(keeper: &Keeper, lease: &Lease, age: Duration) {
    keeper.age_watchdog_tick_for_test(age);
    lease.set_last_renewal_for_test(Instant::now().checked_sub(age).unwrap());
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn heartbeat_seq(lease: &Lease) -> u64 {
    parse_heartbeat(&read_heartbeat_bytes(&lease.dir).unwrap())
        .unwrap()
        .seq
}

/// A lease with no keeper threads running, for calling `check` first.
fn idle_lease(tmp: &Path) -> Arc<Lease> {
    let session_path = tmp.join("s.jsonl");
    create_lease_dir(&session_path, 1).unwrap();
    Lease::for_test_with_duration(&lease_dir(&session_path), 1)
}

fn add_successor(dir: &Path) {
    let dir = fsops::open_dir_no_follow(dir).unwrap();
    create_epoch_marker(&dir, 2).unwrap();
}

#[test]
fn stall_without_successor_renews_and_does_not_fence() {
    let held = held();
    let seq_before = heartbeat_seq(&held.lease);
    stop(&held.keeper, &held.lease, STOP);
    wait_until("the heartbeat to be rewritten", || {
        heartbeat_seq(&held.lease) > seq_before
    });
    // Outlast the grace period: the renewal reset `last_renewal`.
    std::thread::sleep(GRACE * 3);
    assert!(held.fenced_at.lock().unwrap().is_none());
    assert_eq!(held.lease.check(), Ok(()));
}

#[test]
fn check_before_the_watchdog_noticed_the_stall_returns_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let lease = idle_lease(tmp.path());
    stop(&lease.keeper, &lease, STOP);
    // The keeper has no threads: the tick stays stale, no grace period.
    assert_eq!(lease.check(), Ok(()));
    assert!(!lease.superseded.load(Ordering::Acquire));
}

#[test]
fn stall_with_successor_fences_with_the_superseded_reason() {
    let held = held();
    add_successor(&lease_dir(&held.session_path));
    stop(&held.keeper, &held.lease, STOP);
    wait_until("the fence action", || {
        held.fenced_at.lock().unwrap().is_some()
    });
    let error = held.lease.check().unwrap_err();
    assert!(error.starts_with("session lease lost"), "{error}");
    assert!(error.contains("successor epoch"), "{error}");
}

#[test]
fn check_first_after_a_stall_with_successor_returns_err() {
    let tmp = tempfile::tempdir().unwrap();
    let lease = idle_lease(tmp.path());
    add_successor(&lease_dir(&tmp.path().join("s.jsonl")));
    stop(&lease.keeper, &lease, STOP);
    let error = lease.check().unwrap_err();
    assert!(error.contains("successor epoch"), "{error}");
    assert!(lease.superseded.load(Ordering::Acquire));
}

#[test]
fn stall_with_failing_renewal_fences_after_the_grace_period() {
    let held = held();
    held.lease.fail_renewal_for_test();
    let started = Instant::now();
    stop(&held.keeper, &held.lease, STOP);
    wait_until("the fence action", || {
        held.fenced_at.lock().unwrap().is_some()
    });
    let fenced_at = held.fenced_at.lock().unwrap().unwrap();
    assert!(
        fenced_at.duration_since(started) >= GRACE,
        "fenced {:?} after the stall, before the {GRACE:?} grace period ended",
        fenced_at.duration_since(started)
    );
    let error = held.lease.check().unwrap_err();
    assert!(error.contains("no successful heartbeat renewal"), "{error}");
}

#[test]
fn renewal_blocked_without_a_stall_is_fenced_by_the_time_rule() {
    let held = held();
    let started = Instant::now();
    // The renewal thread blocks on `write_lock` when renewal is due, as it
    // would in a write to an unreachable file system.
    let blocked = held.lease.write_lock.lock().unwrap();
    wait_until("the fence action", || {
        held.fenced_at.lock().unwrap().is_some()
    });
    let fenced_at = held.fenced_at.lock().unwrap().unwrap();
    assert!(fenced_at.duration_since(started) >= Duration::from_millis(800));
    assert!(!held.keeper.grace_was_opened_for_test());
    drop(blocked);
}

#[test]
fn the_watchdog_rule_reads_no_file_while_the_time_rule_is_deferred() {
    let tmp = tempfile::tempdir().unwrap();
    let lease = idle_lease(tmp.path());
    add_successor(&lease_dir(&tmp.path().join("s.jsonl")));
    stop(&lease.keeper, &lease, STOP);
    // Without `probe` the successor marker on disk is not read.
    assert_eq!(lease.should_fence(false), None);
    assert!(!lease.superseded.load(Ordering::Acquire));
    let reason = lease.should_fence(true).unwrap();
    assert!(reason.contains("successor epoch"), "{reason}");
}

#[test]
fn released_lease_is_not_fenced_after_a_stall() {
    let held = held();
    held.lease.release().unwrap();
    stop(&held.keeper, &held.lease, STOP);
    std::thread::sleep(GRACE * 4);
    assert!(held.fenced_at.lock().unwrap().is_none());
    assert_eq!(held.lease.should_fence(true), None);
    assert!(!held.lease.fenced.load(Ordering::Acquire));
}
