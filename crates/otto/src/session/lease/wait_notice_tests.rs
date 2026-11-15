//! Tests for [`pending_wait`]: it reports the wait `acquire_with_mode` would
//! enter, without touching the lease directory.

use super::same_host_tests::{
    HOST, MACHINE, dead_process, host, own_process, record, session_held_by,
};
use super::tests::{FakeClock, no_op_keeper};
use super::*;
use std::collections::BTreeMap;

fn pending(session_path: &Path) -> Option<LeaseWait> {
    pending_wait_with_host(session_path, &host(MACHINE)).unwrap()
}

/// A lease directory with L = 6 s and no epoch marker.
fn session_without_epoch(tmp: &Path) -> PathBuf {
    let session_path = tmp.join("s.jsonl");
    create_lease_dir(&session_path, 6).unwrap();
    session_path
}

/// A lease directory with an epoch-1 marker and the heartbeat bytes `raw`,
/// or no heartbeat file when `raw` is `None`.
fn session_with_raw_heartbeat(tmp: &Path, raw: Option<&[u8]>) -> PathBuf {
    let session_path = session_without_epoch(tmp);
    let dir = fsops::open_dir_no_follow(&lease_dir(&session_path)).unwrap();
    create_epoch_marker(&dir, 1).unwrap();
    if let Some(raw) = raw {
        std::fs::write(lease_dir(&session_path).join("heartbeat"), raw).unwrap();
    }
    session_path
}

fn session_released(tmp: &Path) -> PathBuf {
    let (pid, started) = own_process();
    let mut released = record(pid, &started, MACHINE);
    released.released = true;
    session_held_by(tmp, &released)
}

fn session_dead_local(tmp: &Path) -> PathBuf {
    let (pid, started) = dead_process();
    session_held_by(tmp, &record(pid, &started, MACHINE))
}

fn session_live(tmp: &Path) -> PathBuf {
    let (pid, started) = own_process();
    session_held_by(tmp, &record(pid, &started, MACHINE))
}

fn session_live_other_machine(tmp: &Path) -> PathBuf {
    let (pid, started) = own_process();
    session_held_by(
        tmp,
        &record(pid, &started, "fedcba9876543210fedcba9876543210"),
    )
}

#[test]
fn no_lease_directory_is_no_wait() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(pending(&tmp.path().join("s.jsonl")), None);
}

#[test]
fn a_lease_directory_without_an_epoch_marker_is_no_wait() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(pending(&session_without_epoch(tmp.path())), None);
}

#[test]
fn a_released_holder_is_no_wait() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(pending(&session_released(tmp.path())), None);
}

#[test]
fn a_dead_same_host_holder_is_no_wait() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(pending(&session_dead_local(tmp.path())), None);
}

#[test]
fn a_live_holder_waits_7_l_over_6_and_names_the_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let (pid, _) = own_process();
    let wait = pending(&session_live(tmp.path())).expect("a live holder waits");
    assert_eq!(
        wait,
        LeaseWait {
            seconds: 7,
            host: HOST.into(),
            pid
        }
    );
}

#[test]
fn the_wait_scales_with_the_lease_duration() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = tmp.path().join("s.jsonl");
    create_lease_dir(&session_path, 30).unwrap();
    let dir = fsops::open_dir_no_follow(&lease_dir(&session_path)).unwrap();
    create_epoch_marker(&dir, 1).unwrap();
    assert_eq!(pending(&session_path).unwrap().seconds, 35);
}

#[test]
fn a_missing_heartbeat_waits_and_names_an_unknown_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let wait = pending(&session_with_raw_heartbeat(tmp.path(), None)).unwrap();
    assert_eq!(
        (wait.seconds, wait.host.as_str(), wait.pid),
        (7, "unknown", 0)
    );
}

#[test]
fn an_unparsable_heartbeat_waits_and_names_an_unknown_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let wait = pending(&session_with_raw_heartbeat(tmp.path(), Some(b"not json"))).unwrap();
    assert_eq!(
        (wait.seconds, wait.host.as_str(), wait.pid),
        (7, "unknown", 0)
    );
}

/// Every file of `dir` with its bytes, by name.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn pending_wait_leaves_the_lease_directory_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let cases: [fn(&Path) -> PathBuf; 5] = [
        session_live,
        session_dead_local,
        session_released,
        session_without_epoch,
        |tmp| session_with_raw_heartbeat(tmp, None),
    ];
    for (index, case) in cases.into_iter().enumerate() {
        let root = tmp.path().join(index.to_string());
        std::fs::create_dir(&root).unwrap();
        let session_path = case(&root);
        let lease = lease_dir(&session_path);
        let before = snapshot(&lease);
        let log_before = std::fs::read(&session_path).ok();
        let _ = pending(&session_path);
        assert_eq!(snapshot(&lease), before, "case {index}");
        assert_eq!(
            std::fs::read(&session_path).ok(),
            log_before,
            "case {index}"
        );
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            if log_before.is_some() { 2 } else { 1 },
            "case {index}: nothing beside the session and its lease directory"
        );
    }
}

/// A state name and the function that builds a session in that state under a directory.
type Case = (&'static str, fn(&Path) -> PathBuf);

#[test]
fn pending_wait_agrees_with_whether_acquire_sleeps() {
    let cases: [Case; 7] = [
        ("no epoch marker", session_without_epoch),
        ("released", session_released),
        ("dead local holder", session_dead_local),
        ("live holder", session_live),
        ("live holder of another machine", session_live_other_machine),
        ("missing heartbeat", |tmp| {
            session_with_raw_heartbeat(tmp, None)
        }),
        ("unparsable heartbeat", |tmp| {
            session_with_raw_heartbeat(tmp, Some(b"{"))
        }),
    ];
    let mut waiting = 0;
    for (name, case) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = case(tmp.path());
        let predicted = pending(&session_path).is_some();
        let clock = FakeClock::new();
        acquire_with_mode(
            &session_path,
            &clock,
            &no_op_keeper(),
            false,
            &host(MACHINE),
        )
        .unwrap();
        assert_eq!(predicted, clock.sleep_count() > 0, "{name}");
        waiting += usize::from(predicted);
    }
    assert_eq!(waiting, 4, "the live and unreadable-heartbeat cases wait");
}
