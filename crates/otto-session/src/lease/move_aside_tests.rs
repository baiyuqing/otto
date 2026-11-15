//! Tests for `move_aside_log`: the takeover step that fences the log of the old epoch.

use super::*;

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
