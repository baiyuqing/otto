use super::*;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::AtomicU32;

pub(super) struct FakeClock {
    now: Mutex<Duration>,
    sleeps: Mutex<u32>,
    on_sleep: Box<dyn Fn() + Send + Sync>,
}

impl FakeClock {
    pub(super) fn new() -> Self {
        FakeClock::with_on_sleep(|| {})
    }

    fn with_on_sleep(on_sleep: impl Fn() + Send + Sync + 'static) -> Self {
        FakeClock {
            now: Mutex::new(Duration::ZERO),
            sleeps: Mutex::new(0),
            on_sleep: Box::new(on_sleep),
        }
    }

    pub(super) fn sleep_count(&self) -> u32 {
        *self.sleeps.lock().unwrap()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap()
    }

    fn sleep(&self, duration: Duration) {
        (self.on_sleep)();
        *self.sleeps.lock().unwrap() += 1;
        *self.now.lock().unwrap() += duration;
    }
}

/// The tick interval test keepers use: fast enough that a small `L`
/// does not have to wait out a full 1s production tick to see renewal
/// or fencing happen, so watchdog tests finish well under 2s.
const TEST_TICK: Duration = Duration::from_millis(50);

pub(super) fn no_op_keeper() -> Arc<Keeper> {
    Keeper::new_for_test(Box::new(|| {}), Arc::new(Children::new()), TEST_TICK)
}

#[test]
fn without_acquire_wait_is_scoped_to_its_closure_even_when_it_panics() {
    let skip = || SKIP_ACQUIRE_WAIT.with(std::cell::Cell::get);
    assert!(!skip());
    without_acquire_wait(|| {
        assert!(skip());
        without_acquire_wait(|| assert!(skip()));
        assert!(skip(), "a nested call restores the outer setting");
    });
    assert!(!skip());
    let panicked = std::panic::catch_unwind(|| without_acquire_wait(|| panic!("inside")));
    assert!(panicked.is_err());
    assert!(!skip(), "a panic inside does not leave the wait skipped");
    without_acquire_wait(|| {
        let other = std::thread::spawn(skip).join().unwrap();
        assert!(!other, "other threads keep the real clock");
    });
}

#[test]
fn acquire_with_creates_epoch_1_when_none_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();

    let clock = FakeClock::new();
    let keeper = no_op_keeper();
    let (lease, acquired) = acquire_with(&session_path, &clock, &keeper).unwrap();

    assert_eq!(acquired, Acquired::Fresh);
    assert_eq!(lease.epoch(), 1);
    assert_eq!(clock.sleep_count(), 0, "fresh acquire does not wait");
}

#[test]
fn acquire_with_reports_a_running_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();

    let keeper1 = no_op_keeper();
    let (_lease1, _) = acquire_with(&session_path, &FakeClock::new(), &keeper1).unwrap();

    let dir_path = lease_dir(&session_path);
    let renew_dir = fsops::open_dir_no_follow(&dir_path).unwrap();
    let seq = AtomicU32::new(1);
    let clock2 = FakeClock::with_on_sleep(move || {
        let record = HeartbeatRecord {
            epoch: 1,
            host: "holder-host".into(),
            pid: 4242,
            released: false,
            seq: seq.fetch_add(1, Ordering::Relaxed) as u64,
            ..Default::default()
        };
        write_heartbeat_record(&renew_dir, &record).unwrap();
    });
    let keeper2 = no_op_keeper();
    let err = acquire_with(&session_path, &clock2, &keeper2).unwrap_err();

    match err {
        AcquireError::Held(holder) => {
            assert_eq!(holder.epoch, 1);
            assert_eq!(holder.host, "holder-host");
            assert_eq!(holder.pid, 4242);
        }
        other => panic!("expected Held, got {other:?}"),
    }
}

#[test]
fn acquire_with_takes_over_a_stopped_holder_after_the_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap(); // 7L/6 = 7s

    let keeper1 = no_op_keeper();
    let (_lease1, _) = acquire_with(&session_path, &FakeClock::new(), &keeper1).unwrap();

    let clock2 = FakeClock::new();
    let keeper2 = no_op_keeper();
    let (lease2, acquired) = acquire_with(&session_path, &clock2, &keeper2).unwrap();

    assert_eq!(lease2.epoch(), 2);
    match acquired {
        Acquired::TakenOver(holder) => assert_eq!(holder.epoch, 1),
        other => panic!("expected TakenOver, got {other:?}"),
    }
    assert!(clock2.sleep_count() >= 7, "expected to wait out 7L/6");
}

#[test]
fn acquire_with_takes_over_a_released_holder_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    std::fs::write(&session_path, b"{\"line\":1}\n").unwrap();

    use std::os::unix::fs::MetadataExt;
    let inode_before = std::fs::metadata(&session_path).unwrap().ino();

    let keeper1 = no_op_keeper();
    let (lease1, _) = acquire_with(&session_path, &FakeClock::new(), &keeper1).unwrap();
    lease1.release().unwrap();

    let clock2 = FakeClock::new();
    let keeper2 = no_op_keeper();
    let (lease2, acquired) = acquire_with(&session_path, &clock2, &keeper2).unwrap();

    assert_eq!(lease2.epoch(), 2);
    assert_eq!(
        acquired,
        Acquired::Fresh,
        "a released holder's epoch is taken over as Fresh, not TakenOver"
    );
    assert_eq!(
        clock2.sleep_count(),
        0,
        "a released holder is taken over without waiting"
    );
    assert_eq!(
        std::fs::metadata(&session_path).unwrap().ino(),
        inode_before,
        "the log keeps its inode when taking over a released epoch"
    );
    assert_eq!(
        std::fs::read(&session_path).unwrap(),
        b"{\"line\":1}\n",
        "the log is not moved aside when taking over a released epoch"
    );
    let dir_path = lease_dir(&session_path);
    assert!(
        !dir_path.join("fenced-1.jsonl").exists(),
        "no fenced copy is created for a released epoch"
    );
}

#[test]
fn concurrent_acquire_with_races_exactly_one_winner() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    // Use a real, minimum lease window: a loser must wait long enough to
    // observe the winning thread's first heartbeat. Advancing a fake clock
    // instantly made this test depend on the winner receiving a scheduler
    // slice during a fixed 35 ms wall-clock delay, which CI can exceed.
    create_lease_dir(&session_path, 1).unwrap();
    let session_path = Arc::new(session_path);

    let mut handles = Vec::new();
    for _ in 0..4 {
        let session_path = Arc::clone(&session_path);
        handles.push(std::thread::spawn(move || {
            acquire_with(&session_path, &RealClock, no_op_keeper2())
        }));
    }
    fn no_op_keeper2() -> &'static Arc<Keeper> {
        // a fresh test keeper per call would race registering threads
        // pointlessly; a leaked static is fine for this one test.
        static KEEPER: OnceLock<Arc<Keeper>> = OnceLock::new();
        KEEPER.get_or_init(|| {
            Keeper::new_for_test(Box::new(|| {}), Arc::new(Children::new()), TEST_TICK)
        })
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let successes = results.iter().filter(|r| r.is_ok()).count();
    // A losing thread reports either `Raced(1)`, if it lost the
    // exclusive-create race for the epoch-1 marker outright, or
    // `Held`, if it observed the marker already existing and then
    // waited out the liveness window and saw the winner's heartbeat: a
    // thread that arrives after the marker exists must not treat the
    // still-forming epoch as abandoned and race a takeover against it.
    let lost = results
        .iter()
        .filter(|r| {
            matches!(r, Err(AcquireError::Raced(1)))
                || matches!(r, Err(AcquireError::Held(h)) if h.epoch == 1)
        })
        .count();
    assert_eq!(successes, 1, "exactly one acquirer should win epoch 1");
    assert_eq!(lost, 3, "the rest should observe the race or the winner");
}

#[test]
fn concurrent_create_lease_dir_has_exactly_one_winner() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = Arc::new(tmp.path().join("s.jsonl"));

    let mut handles = Vec::new();
    for _ in 0..4 {
        let session_path = Arc::clone(&session_path);
        handles.push(std::thread::spawn(move || {
            create_lease_dir(&session_path, 6)
        }));
    }
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    // Every caller observes the lease directory in place by the time it
    // returns, win or lose: a loser removes its own temp directory and
    // reports success rather than an error.
    assert!(results.iter().all(|r| r.is_ok()), "{results:?}");

    let dir_path = lease_dir(&session_path);
    let lease_json_count = std::fs::read_dir(&dir_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() == "lease.json")
        .count();
    assert_eq!(lease_json_count, 1, "exactly one lease.json must exist");

    let leftover_tmp_dirs = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .count();
    assert_eq!(leftover_tmp_dirs, 0, "no *.tmp-* directories may remain");
}

#[test]
fn heartbeat_reads_are_never_torn_under_concurrent_renewal() {
    let tmp = tempfile::tempdir().unwrap();
    let dir_path = tmp.path().to_path_buf();
    let writer_dir = fsops::open_dir_no_follow(&dir_path).unwrap();
    let record = HeartbeatRecord {
        epoch: 1,
        host: "h".into(),
        pid: 1,
        released: false,
        seq: 0,
        ..Default::default()
    };
    write_heartbeat_record(&writer_dir, &record).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = Arc::clone(&stop);
    let writer = std::thread::spawn(move || {
        let mut seq = 1u64;
        while !writer_stop.load(Ordering::Relaxed) {
            let record = HeartbeatRecord {
                epoch: 1,
                host: "h".into(),
                pid: 1,
                released: false,
                seq,
                ..Default::default()
            };
            write_heartbeat_record(&writer_dir, &record).unwrap();
            seq += 1;
        }
    });

    let reader_dir = fsops::open_dir_no_follow(&dir_path).unwrap();
    for _ in 0..2000 {
        let bytes = read_heartbeat_bytes(&reader_dir).unwrap();
        assert_eq!(bytes.len(), HEARTBEAT_SIZE);
        assert_eq!(bytes[HEARTBEAT_SIZE - 1], b'\n');
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}

#[test]
fn independent_directory_opens_see_each_others_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir_path = tmp.path().to_path_buf();

    let lease_a = Lease::for_test(&dir_path);
    let lease_b = Lease::for_test(&dir_path);

    lease_b.write_heartbeat(false).unwrap();

    let bytes = read_heartbeat_bytes(&lease_a.dir).unwrap();
    let record = parse_heartbeat(&bytes).unwrap();
    assert_eq!(record.host, "test-host");
    assert!(record.seq >= 1);
}

#[test]
fn watchdog_fences_and_kills_a_real_child_process_group() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 1).unwrap(); // L=1s, 5L/6 ~= 0.83s

    let lease = Lease::for_test_with_duration(&lease_dir(&session_path), 1);
    // Renewal must never succeed, so fencing cannot race a renewal
    // thread that keeps the lease alive: without this, the test only
    // passes because the renewal and watchdog threads' first ticks
    // happen to race in the watchdog's favor.
    lease.fail_renewal_for_test();

    // A private registry: kill_all must never reach a process group a
    // concurrently running sandbox::process test registered on the
    // global one.
    let children = Arc::new(Children::new());
    let mut child = Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let _registration = Children::register(&children, pid, true);

    let fenced = Arc::new(AtomicBool::new(false));
    let fenced_flag = Arc::clone(&fenced);
    let keeper = Keeper::new_for_test(
        Box::new(move || fenced_flag.store(true, Ordering::Release)),
        Arc::clone(&children),
        TEST_TICK,
    );
    Keeper::register(&keeper, &lease);

    let deadline = Instant::now() + Duration::from_millis(1900);
    loop {
        if fenced.load(Ordering::Acquire) {
            break;
        }
        assert!(Instant::now() < deadline, "watchdog did not fence in time");
        std::thread::sleep(Duration::from_millis(10));
    }

    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        assert!(Instant::now() < deadline, "child process was not killed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(lease.check().is_err());
}

#[test]
fn watchdog_fences_when_a_successor_epoch_appears() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 1).unwrap();
    let children = Arc::new(Children::new());
    let fenced = Arc::new(AtomicBool::new(false));
    let fenced_flag = Arc::clone(&fenced);
    let keeper = Keeper::new_for_test(
        Box::new(move || fenced_flag.store(true, Ordering::Release)),
        children,
        TEST_TICK,
    );
    let (lease, _) = acquire_with(&session_path, &FakeClock::new(), &keeper).unwrap();

    let dir_path = lease_dir(&session_path);
    let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
    create_epoch_marker(&dir, 2).unwrap();

    let deadline = Instant::now() + Duration::from_millis(1900);
    loop {
        if fenced.load(Ordering::Acquire) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watchdog did not react to a successor epoch"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(lease.check().unwrap_err().starts_with("session lease lost"));
}

#[test]
fn release_after_rename_still_updates_the_heartbeat_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();

    let (lease, _) = acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();

    let old_dir = lease_dir(&session_path);
    let new_dir = tmp.path().join("archived.lease");
    std::fs::rename(&old_dir, &new_dir).unwrap();

    lease.release().unwrap();

    let reopened = fsops::open_dir_no_follow(&new_dir).unwrap();
    let bytes = read_heartbeat_bytes(&reopened).unwrap();
    let record = parse_heartbeat(&bytes).unwrap();
    assert!(record.released);
}

#[test]
fn release_does_not_write_once_superseded() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();

    let (lease, _) = acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();

    let dir_path = lease_dir(&session_path);
    let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
    create_epoch_marker(&dir, 2).unwrap();

    // One renewal tick observes the successor epoch and marks this
    // lease superseded, without writing another heartbeat record.
    lease.maybe_renew();
    assert!(
        lease.check().unwrap_err().contains("successor epoch"),
        "expected the successor-epoch check to mark this lease superseded"
    );

    let before = read_heartbeat_bytes(&dir).unwrap();
    lease.release().unwrap();
    let after = read_heartbeat_bytes(&dir).unwrap();

    assert_eq!(after, before, "release must not write once superseded");
    let record = parse_heartbeat(&after).unwrap();
    assert!(
        !record.released,
        "release is a no-op once superseded, so the record stays unreleased"
    );
}

#[test]
fn maybe_renew_does_not_write_once_a_successor_epoch_appears() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();

    let (lease, _) = acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();

    let dir_path = lease_dir(&session_path);
    let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
    let before = read_heartbeat_bytes(&dir).unwrap();

    create_epoch_marker(&dir, 2).unwrap();
    // Force the tick to consider renewal due, so the test exercises the
    // successor check racing an otherwise-due renewal, not just a tick
    // that would have skipped writing anyway.
    lease.set_last_renewal_for_test(Instant::now() - lease.duration());

    lease.maybe_renew();

    let after = read_heartbeat_bytes(&dir).unwrap();
    assert_eq!(
        after, before,
        "a renewal tick after a successor epoch appears must not write another record"
    );
}

#[test]
fn check_fails_when_the_last_renewal_is_older_than_5l_over_6() {
    let tmp = tempfile::tempdir().unwrap();
    let lease = Lease::for_test(tmp.path());
    // DEFAULT_LEASE_SECONDS is 30s; 5L/6 = 25s. No Keeper is ever
    // registered, so this exercises `check()`'s own evaluation, not the
    // watchdog thread.
    lease.set_last_renewal_for_test(Instant::now() - Duration::from_secs(26));

    let err = lease.check().unwrap_err();
    assert!(err.starts_with("session lease lost"), "got: {err}");
}

#[test]
fn acquire_with_takes_over_an_unparseable_heartbeat_after_the_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap(); // 7L/6 = 7s

    let dir_path = lease_dir(&session_path);
    let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
    create_epoch_marker(&dir, 1).unwrap();
    // Fixed-size garbage bytes, not valid JSON: acquisition must never
    // hard-fail on this, and identical garbage on every poll must read
    // as "unchanged" rather than "different holder each time".
    let mut garbage = vec![b'x'; HEARTBEAT_SIZE];
    garbage[HEARTBEAT_SIZE - 1] = b'\n';
    let heartbeat_file =
        fsops::create_at_no_follow(&dir, "heartbeat", libc::O_WRONLY, 0o600).unwrap();
    pwrite_all(&heartbeat_file, &garbage, 0).unwrap();
    heartbeat_file.sync_all().unwrap();

    let clock = FakeClock::new();
    let keeper = no_op_keeper();
    let (lease, acquired) = acquire_with(&session_path, &clock, &keeper).unwrap();

    assert_eq!(lease.epoch(), 2);
    match acquired {
        Acquired::TakenOver(holder) => {
            assert_eq!(holder.host, "unknown");
            assert_eq!(holder.pid, 0);
        }
        other => panic!("expected TakenOver, got {other:?}"),
    }
    assert!(clock.sleep_count() >= 7, "expected to wait out 7L/6");
}

#[test]
fn move_aside_log_keeps_only_complete_lines_and_fences_the_old_epoch() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    let dir_path = lease_dir(&session_path);
    std::fs::write(&session_path, b"{\"a\":1}\n{\"a\":2}\n{\"a\":3, incomplete").unwrap();

    move_aside_log(&session_path, &dir_path, 1).unwrap();

    let fenced = std::fs::read(dir_path.join("fenced-1.jsonl")).unwrap();
    assert_eq!(fenced, b"{\"a\":1}\n{\"a\":2}\n{\"a\":3, incomplete");

    let new_log = std::fs::read(&session_path).unwrap();
    assert_eq!(new_log, b"{\"a\":1}\n{\"a\":2}\n");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&session_path)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "the restored log is owner-only");
}

#[test]
fn move_aside_log_does_not_affect_a_stale_writer_descriptor_to_the_old_file() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    let dir_path = lease_dir(&session_path);
    std::fs::write(&session_path, b"{\"a\":1}\n").unwrap();

    // A writer holding the old path open before the takeover.
    use std::io::Write;
    let mut stale = std::fs::OpenOptions::new()
        .append(true)
        .open(&session_path)
        .unwrap();

    move_aside_log(&session_path, &dir_path, 1).unwrap();

    // The stale descriptor still refers to the renamed (fenced) inode,
    // not the new file at `session_path`.
    stale.write_all(b"{\"a\":2}\n").unwrap();
    stale.sync_all().unwrap();

    assert_eq!(std::fs::read(&session_path).unwrap(), b"{\"a\":1}\n");
    assert_eq!(
        std::fs::read(dir_path.join("fenced-1.jsonl")).unwrap(),
        b"{\"a\":1}\n{\"a\":2}\n"
    );
}

#[test]
fn move_aside_log_restores_from_an_existing_fenced_copy_when_the_log_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    let dir_path = lease_dir(&session_path);
    // Simulates a crash after the first rename of an earlier,
    // interrupted attempt: the fenced copy exists, `session_path` does
    // not.
    std::fs::write(dir_path.join("fenced-1.jsonl"), b"{\"a\":1}\n").unwrap();

    move_aside_log(&session_path, &dir_path, 1).unwrap();

    assert_eq!(std::fs::read(&session_path).unwrap(), b"{\"a\":1}\n");
}

#[test]
fn move_aside_log_restores_from_the_highest_fenced_copy_at_a_different_epoch() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    let dir_path = lease_dir(&session_path);
    // Simulates: acquirer X created epoch 2, renamed `s.jsonl` to
    // `fenced-1.jsonl` (fencing epoch 1, the epoch it took over), then
    // crashed before restoring `s.jsonl`. Acquirer Y now retries the
    // takeover of epoch 2; `fenced-2.jsonl` does not exist.
    std::fs::write(dir_path.join("fenced-1.jsonl"), b"{\"a\":1}\n").unwrap();
    assert!(!session_path.exists());

    move_aside_log(&session_path, &dir_path, 2).unwrap();

    assert_eq!(std::fs::read(&session_path).unwrap(), b"{\"a\":1}\n");
}

#[test]
fn move_aside_log_rejects_an_oversized_source_without_truncating_it() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    let dir_path = lease_dir(&session_path);
    let oversized = otto_core::session::MAX_SESSION_FILE_BYTES as u64 + 1;
    std::fs::File::create(&session_path)
        .unwrap()
        .set_len(oversized)
        .unwrap();

    let err = move_aside_log(&session_path, &dir_path, 1).unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    let msg = err.to_string();
    assert!(msg.contains(&oversized.to_string()), "{msg}");
    assert!(
        msg.contains(&otto_core::session::MAX_SESSION_FILE_BYTES.to_string()),
        "{msg}"
    );

    let fenced = dir_path.join("fenced-1.jsonl");
    assert_eq!(
        std::fs::metadata(&fenced).unwrap().len(),
        oversized,
        "the fenced copy is left intact, not truncated"
    );
}

#[test]
fn acquire_with_waits_out_a_released_heartbeat_behind_a_newer_epoch_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap(); // 7L/6 = 7s

    let keeper1 = no_op_keeper();
    let (lease1, _) = acquire_with(&session_path, &FakeClock::new(), &keeper1).unwrap();
    lease1.release().unwrap();

    // A concurrent acquirer created epoch 2 but has not written its
    // heartbeat yet: the released epoch-1 heartbeat is no longer the
    // newest marker.
    let dir_path = lease_dir(&session_path);
    let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
    create_epoch_marker(&dir, 2).unwrap();

    let clock2 = FakeClock::new();
    let keeper2 = no_op_keeper();
    let (lease2, acquired) = acquire_with(&session_path, &clock2, &keeper2).unwrap();

    assert_eq!(lease2.epoch(), 3);
    assert!(
        matches!(acquired, Acquired::TakenOver(_)),
        "expected TakenOver, got {acquired:?}"
    );
    assert!(clock2.sleep_count() >= 7, "expected to wait out 7L/6");
}

#[test]
fn acquire_with_takes_over_a_stopped_holder_through_the_public_entry_point() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap(); // 7L/6 = 7s

    let keeper1 = no_op_keeper();
    let (_lease1, _) = acquire_with(&session_path, &FakeClock::new(), &keeper1).unwrap();
    let original = b"{\"a\":1}\n{\"a\":2}\n{\"a\":3, incomplete";
    std::fs::write(&session_path, original).unwrap();

    let clock2 = FakeClock::new();
    let keeper2 = no_op_keeper();
    let (lease2, acquired) = acquire_with(&session_path, &clock2, &keeper2).unwrap();

    assert_eq!(lease2.epoch(), 2);
    assert!(
        matches!(acquired, Acquired::TakenOver(_)),
        "expected TakenOver, got {acquired:?}"
    );

    let dir_path = lease_dir(&session_path);
    assert_eq!(
        std::fs::read(dir_path.join("fenced-1.jsonl")).unwrap(),
        original,
        "the fenced copy keeps the original, untruncated bytes"
    );
    assert_eq!(
        std::fs::read(&session_path).unwrap(),
        b"{\"a\":1}\n{\"a\":2}\n",
        "the new log holds only the complete lines"
    );
}

#[test]
fn a_crash_between_the_two_renames_of_a_takeover_is_completed_by_the_next_acquire() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    let (_lease1, _) = acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();

    // The state a process killed in `move_aside_log` leaves: it created
    // epoch 2, then renamed the log to `fenced-1.jsonl` and ended before
    // writing the truncated copy back. The lease has no fault seam, so
    // the state is built directly.
    let original = b"{\"a\":1}\n{\"a\":2, incomplete";
    let dir_path = lease_dir(&session_path);
    create_epoch_marker(&fsops::open_dir_no_follow(&dir_path).unwrap(), 2).unwrap();
    std::fs::write(dir_path.join("fenced-1.jsonl"), original).unwrap();
    assert!(!session_path.exists());

    let (lease3, acquired) =
        acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();

    assert_eq!(lease3.epoch(), 3);
    assert!(
        matches!(acquired, Acquired::TakenOver(_)),
        "expected TakenOver, got {acquired:?}"
    );
    assert_eq!(
        std::fs::read(&session_path).expect("the next acquire restores the log"),
        b"{\"a\":1}\n",
        "the log is restored from the earlier fenced copy, complete lines only"
    );
    assert_eq!(
        std::fs::read(dir_path.join("fenced-1.jsonl")).unwrap(),
        original
    );
    assert!(
        !dir_path.join("fenced-2.jsonl").exists(),
        "the earlier fenced copy is reused, not replaced"
    );
}

#[test]
fn holds_any_is_false_with_nothing_registered() {
    let keeper = no_op_keeper();
    assert!(!keeper.holds_any());
}

#[test]
fn holds_any_is_true_after_acquire() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    let keeper = no_op_keeper();
    let (_lease, _acquired) = acquire_with(&session_path, &FakeClock::new(), &keeper).unwrap();

    assert!(keeper.holds_any());
}

#[test]
fn holds_any_is_false_after_release() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    let keeper = no_op_keeper();
    let (lease, _acquired) = acquire_with(&session_path, &FakeClock::new(), &keeper).unwrap();

    lease.release().unwrap();
    assert!(!keeper.holds_any());
}

#[test]
fn holds_any_is_false_once_the_lease_arc_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    let keeper = no_op_keeper();
    let (lease, _acquired) = acquire_with(&session_path, &FakeClock::new(), &keeper).unwrap();

    drop(lease);
    assert!(!keeper.holds_any());
}
