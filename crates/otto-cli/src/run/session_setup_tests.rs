//! Tests for the lease wait notice written before the initial session open.
//!
//! A real wait lasts at least 14 s (`MINIMUM_LEASE_SECONDS` = 12), so these
//! tests open through `prepare_listed` with a session path outside the
//! expected workspace directory. That open fails in path validation, before
//! it acquires the lease, while the notice query reads the real lease
//! directory of the same path. The wait itself is covered by the lease tests
//! in `otto`.

use super::*;
use otto_session::lease::{Lease, create_lease_dir};

/// A session path with a lease held by this process, renewed by its keeper.
fn held_session(dir: &Path, lease_seconds: u64) -> (PathBuf, Arc<Lease>) {
    let path = dir.join("s.jsonl");
    create_lease_dir(&path, lease_seconds).unwrap();
    let (lease, _) = Lease::acquire(&path).unwrap();
    (path, lease)
}

fn open_listed(path: &Path, workspace: &Path) -> (String, std::time::Duration) {
    let storage = otto::runtime::storage::Storage::default();
    let mut stderr = Vec::new();
    let started = Instant::now();
    let result = prepare_session_backend(
        storage.sessions(),
        Path::new("/nonexistent-session-root"),
        &workspace.to_string_lossy(),
        &path.to_string_lossy(),
        Some(12),
        true,
        &mut stderr,
    );
    let error = result.err().expect("the path is outside the session root");
    assert!(error.contains("outside the expected workspace"), "{error}");
    (String::from_utf8(stderr).unwrap(), started.elapsed())
}

#[test]
fn a_session_with_a_live_holder_gets_the_notice_before_the_open() {
    let tmp = tempfile::tempdir().unwrap();
    let (path, lease) = held_session(tmp.path(), 12);
    let (stderr, elapsed) = open_listed(&path, tmp.path());
    let heartbeat =
        std::fs::read_to_string(path.with_extension("lease").join("heartbeat")).unwrap();
    let heartbeat: serde_json::Value =
        serde_json::from_str(heartbeat.trim_end_matches('\0').trim()).unwrap();
    let host = heartbeat["host"].as_str().unwrap();
    assert_eq!(
        stderr,
        format!(
            "waiting up to 14 s for the session lease held by host {host} pid {}\n",
            std::process::id()
        )
    );
    assert!(elapsed.as_secs() < 10, "the open did not wait: {elapsed:?}");
    lease.release().unwrap();
}

#[test]
fn a_released_lease_gets_no_notice() {
    let tmp = tempfile::tempdir().unwrap();
    let (path, lease) = held_session(tmp.path(), 12);
    lease.release().unwrap();
    let (stderr, _) = open_listed(&path, tmp.path());
    assert_eq!(stderr, "");
}

#[test]
fn a_session_without_a_lease_gets_no_notice() {
    let tmp = tempfile::tempdir().unwrap();
    let (stderr, _) = open_listed(&tmp.path().join("s.jsonl"), tmp.path());
    assert_eq!(stderr, "");
}
