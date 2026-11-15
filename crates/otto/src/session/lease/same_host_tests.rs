//! Tests for the same-host takeover shortcut: an unreleased holder that is a
//! dead process of this host is taken over without the liveness wait.

use super::tests::{FakeClock, no_op_keeper};
use super::*;

pub(super) const MACHINE: &str = "0123456789abcdef0123456789abcdef";
pub(super) const HOST: &str = "this-host";
const WINDOW_SLEEPS: u32 = 7; // 7L/6 at L = 6 s

pub(super) fn host(machine: &str) -> LocalHost {
    LocalHost {
        hostname: HOST.into(),
        machine: machine.into(),
        process_matches: crate::process_identity::process_matches,
    }
}

/// A session with a log ending in an incomplete record and an epoch-1 holder
/// described by `heartbeat`, which was never released.
pub(super) fn session_held_by(tmp: &Path, heartbeat: &HeartbeatRecord) -> PathBuf {
    let session_path = tmp.join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    std::fs::write(&session_path, b"{\"a\":1}\n{\"a\":2, incomplete").unwrap();
    let dir = fsops::open_dir_no_follow(&lease_dir(&session_path)).unwrap();
    create_epoch_marker(&dir, 1).unwrap();
    write_heartbeat_record(&dir, heartbeat).unwrap();
    session_path
}

pub(super) fn record(pid: u32, started: &str, machine: &str) -> HeartbeatRecord {
    HeartbeatRecord {
        epoch: 1,
        host: HOST.into(),
        pid,
        released: false,
        seq: 5,
        machine: machine.into(),
        started: started.into(),
    }
}

/// The pid and start time of a process that existed and has been reaped.
pub(super) fn dead_process() -> (u32, String) {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let started = crate::process_identity::os_process_start_time(pid)
        .expect("start time of a live child")
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    child.kill().unwrap();
    child.wait().unwrap();
    (pid, started)
}

pub(super) fn own_process() -> (u32, String) {
    let (pid, started) = crate::process_identity::current_process();
    (pid as u32, started)
}

fn acquire(
    session_path: &Path,
    clock: &FakeClock,
    host: &LocalHost,
    deleting: bool,
) -> Result<(Arc<Lease>, Acquired), AcquireError> {
    acquire_with_mode(session_path, clock, &no_op_keeper(), deleting, host)
}

fn assert_waited_then_took_over(
    result: Result<(Arc<Lease>, Acquired), AcquireError>,
    clock: &FakeClock,
) {
    let (lease, acquired) = result.unwrap();
    assert_eq!(lease.epoch(), 2);
    assert!(matches!(acquired, Acquired::TakenOver(_)), "{acquired:?}");
    assert!(
        clock.sleep_count() >= WINDOW_SLEEPS,
        "expected the 7L/6 wait, slept {} times",
        clock.sleep_count()
    );
}

#[test]
fn dead_same_host_holder_is_taken_over_without_waiting() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, started) = dead_process();
    let session_path = session_held_by(tmp.path(), &record(pid, &started, MACHINE));

    let clock = FakeClock::new();
    let (lease, acquired) = acquire(&session_path, &clock, &host(MACHINE), false).unwrap();

    assert_eq!(lease.epoch(), 2);
    match acquired {
        Acquired::TakenOver(holder) => {
            assert_eq!(holder.epoch, 1);
            assert_eq!(holder.host, HOST);
            assert_eq!(holder.pid, pid);
        }
        other => panic!("expected TakenOver, got {other:?}"),
    }
    assert_eq!(clock.sleep_count(), 0, "no wait for a dead local process");
    let fenced = std::fs::read(lease_dir(&session_path).join("fenced-1.jsonl")).unwrap();
    assert_eq!(fenced, b"{\"a\":1}\n{\"a\":2, incomplete");
    assert_eq!(std::fs::read(&session_path).unwrap(), b"{\"a\":1}\n");
}

#[test]
fn live_same_host_holder_waits_the_full_window() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, started) = own_process();
    let session_path = session_held_by(tmp.path(), &record(pid, &started, MACHINE));

    let clock = FakeClock::new();
    let result = acquire(&session_path, &clock, &host(MACHINE), false);
    assert_waited_then_took_over(result, &clock);
}

#[test]
fn reused_pid_with_a_different_start_time_is_taken_over_without_waiting() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, _) = own_process();
    let session_path = session_held_by(
        tmp.path(),
        &record(pid, "2000-01-01T00:00:00.000000000Z", MACHINE),
    );

    let clock = FakeClock::new();
    let (_, acquired) = acquire(&session_path, &clock, &host(MACHINE), false).unwrap();

    assert!(matches!(acquired, Acquired::TakenOver(_)));
    assert_eq!(clock.sleep_count(), 0);
}

#[test]
fn same_hostname_on_another_machine_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, started) = dead_process();
    let session_path = session_held_by(tmp.path(), &record(pid, &started, "another-machine-id"));

    let clock = FakeClock::new();
    let result = acquire(&session_path, &clock, &host(MACHINE), false);
    assert_waited_then_took_over(result, &clock);
}

#[test]
fn an_empty_machine_id_in_the_record_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, started) = dead_process();
    let session_path = session_held_by(tmp.path(), &record(pid, &started, ""));

    let clock = FakeClock::new();
    // The local id is also empty: two empty ids must not count as equal.
    let result = acquire(&session_path, &clock, &host(""), false);
    assert_waited_then_took_over(result, &clock);
}

#[test]
fn an_empty_local_machine_id_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, started) = dead_process();
    let session_path = session_held_by(tmp.path(), &record(pid, &started, MACHINE));

    let clock = FakeClock::new();
    let result = acquire(&session_path, &clock, &host(""), false);
    assert_waited_then_took_over(result, &clock);
}

#[test]
fn a_different_hostname_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, started) = dead_process();
    let mut heartbeat = record(pid, &started, MACHINE);
    heartbeat.host = "other-host".into();
    let session_path = session_held_by(tmp.path(), &heartbeat);

    let clock = FakeClock::new();
    let result = acquire(&session_path, &clock, &host(MACHINE), false);
    assert_waited_then_took_over(result, &clock);
}

#[test]
fn a_process_that_cannot_be_classified_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, _) = dead_process();
    // An unparseable start time makes `process_matches` return `None`.
    let session_path = session_held_by(tmp.path(), &record(pid, "", MACHINE));

    let clock = FakeClock::new();
    let result = acquire(&session_path, &clock, &host(MACHINE), false);
    assert_waited_then_took_over(result, &clock);
}

#[test]
fn a_heartbeat_behind_the_highest_epoch_marker_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, started) = dead_process();
    let session_path = session_held_by(tmp.path(), &record(pid, &started, MACHINE));
    // A concurrent acquirer created epoch 2 and has not written its
    // heartbeat yet; the epoch-1 heartbeat belongs to a superseded holder.
    let dir = fsops::open_dir_no_follow(&lease_dir(&session_path)).unwrap();
    create_epoch_marker(&dir, 2).unwrap();

    let clock = FakeClock::new();
    let (lease, _) = acquire(&session_path, &clock, &host(MACHINE), false).unwrap();

    assert_eq!(lease.epoch(), 3);
    assert!(clock.sleep_count() >= WINDOW_SLEEPS);
}

#[test]
fn acquire_for_delete_reports_a_dead_same_host_holder_as_held() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, started) = dead_process();
    let session_path = session_held_by(tmp.path(), &record(pid, &started, MACHINE));

    let clock = FakeClock::new();
    let error = acquire(&session_path, &clock, &host(MACHINE), true).unwrap_err();

    match error {
        AcquireError::Held(holder) => {
            assert_eq!(holder.epoch, 1);
            assert_eq!(holder.pid, pid);
        }
        other => panic!("expected Held, got {other:?}"),
    }
    assert_eq!(clock.sleep_count(), 0);
    assert!(!lease_dir(&session_path).join("epoch-2").exists());
}

#[test]
fn heartbeat_with_machine_and_start_time_round_trips_through_the_fixed_record() {
    let tmp = tempfile::tempdir().unwrap();
    let dir_path = tmp.path().join("d");
    std::fs::create_dir(&dir_path).unwrap();
    let dir = fsops::open_dir_no_follow(&dir_path).unwrap();
    let written = record(4242, "2026-11-15T01:02:03.123456789Z", MACHINE);
    write_heartbeat_record(&dir, &written).unwrap();

    let bytes = read_heartbeat_bytes(&dir).unwrap();
    assert_eq!(bytes.len(), HEARTBEAT_SIZE);
    assert_eq!(bytes[HEARTBEAT_SIZE - 1], b'\n');
    let read = read_heartbeat(&dir).unwrap().unwrap();
    assert_eq!(read.machine, MACHINE);
    assert_eq!(read.started, "2026-11-15T01:02:03.123456789Z");
    assert_eq!((read.epoch, read.pid, read.seq), (1, 4242, 5));
}

#[test]
fn a_record_without_the_new_fields_parses_with_empty_values() {
    let old = br#"{"epoch":3,"host":"h","pid":7,"released":false,"seq":9}"#;
    let parsed = parse_heartbeat(old).unwrap();
    assert_eq!(parsed.machine, "");
    assert_eq!(parsed.started, "");
}

#[test]
fn the_largest_record_with_a_255_byte_hostname_fits_the_fixed_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = fsops::open_dir_no_follow(tmp.path()).unwrap();
    let largest = HeartbeatRecord {
        epoch: u64::MAX,
        host: "h".repeat(255),
        pid: u32::MAX,
        released: false,
        seq: u64::MAX,
        // The machine id is at most 64 bytes (see `read_machine_id`).
        machine: "m".repeat(64),
        started: "2026-11-15T01:02:03.123456789Z".into(),
    };
    let json_len = serde_json::to_string(&largest).unwrap().len();
    assert!(json_len < HEARTBEAT_SIZE, "{json_len} bytes");
    write_heartbeat_record(&dir, &largest).unwrap();
}

#[test]
fn a_held_lease_writes_its_machine_id_and_start_time() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    let (lease, _) = acquire_with(&session_path, &FakeClock::new(), &no_op_keeper()).unwrap();

    let dir = fsops::open_dir_no_follow(&lease_dir(&session_path)).unwrap();
    let heartbeat = read_heartbeat(&dir).unwrap().unwrap();
    assert_eq!(heartbeat.machine, local_machine_id());
    assert_eq!(
        heartbeat.started,
        crate::process_identity::current_process().1
    );
    drop(lease);
}
