//! Plain reopen recovery, failover and workspace identity tests.

use super::*;

pub(super) fn turn_record(record: &otto_core::session::TurnRecord) -> String {
    otto_core::session::encode_turn_record(record)
}

/// Reopens `path` without a lease, as the next process would after a crash.
pub(super) fn reopen_plain(path: &str, replayable: fn(&str) -> bool) -> Store {
    let prepared = Prepared::prepare(Path::new(path), None)
        .expect("prepare")
        .with_replayable(replayable);
    prepared.activate().expect("activate").0
}

#[test]
fn a_plain_reopen_replays_read_only_calls_and_records_process_loss() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    store
        .append_custom_entry(
            otto_core::session::TURN_CUSTOM_TYPE,
            &turn_record(&otto_core::session::TurnRecord::started(
                "op_turn",
                otto_core::session::TurnTrigger::User,
                1,
                None,
            )),
        )
        .expect("turn start");
    store
        .append_message(&tool_calls(&[("call-1", "read")]))
        .expect("append dangling call");
    let path = store.path();
    store.close().expect("close without ending the turn");

    let reopened = reopen_plain(&path, read_only_tools);
    let recovery = reopened.take_recovery().expect("process loss recorded");
    assert_eq!(recovery.cause, crate::RecoveryCause::ProcessLost);
    assert!(recovery.repaired.is_empty());
    assert_eq!(recovery.replayable.len(), 1);
    assert_eq!(
        recovery.interrupted_turn,
        Some(otto_core::session::InterruptedTurn {
            turn_id: "op_turn".into(),
            attempt: 1
        })
    );
    assert!(!recovery.resume_exhausted());
    assert_eq!(tool_result_count(&path), 0);
    reopened.close().expect("close");
}

#[test]
fn a_plain_reopen_of_a_turn_interrupted_mid_stream_records_it() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    store
        .append_custom_entry(
            otto_core::session::TURN_CUSTOM_TYPE,
            &turn_record(&otto_core::session::TurnRecord::started(
                "op_turn",
                otto_core::session::TurnTrigger::User,
                1,
                None,
            )),
        )
        .expect("turn start");
    store
        .append_message(&user("do it"))
        .expect("append user message");
    let path = store.path();
    store.close().expect("close");

    let reopened = reopen_plain(&path, read_only_tools);
    let recovery = reopened.take_recovery().expect("process loss recorded");
    assert_eq!(recovery.cause, crate::RecoveryCause::ProcessLost);
    assert!(recovery.repaired.is_empty() && recovery.replayable.is_empty());
    assert_eq!(
        otto_core::session::Session::interrupted_turn(&reopened).map(|turn| turn.turn_id),
        Some("op_turn".to_string())
    );
    reopened.close().expect("close");
}

#[test]
fn a_plain_reopen_after_an_ended_turn_records_no_recovery() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    for record in [
        otto_core::session::TurnRecord::started(
            "op_turn",
            otto_core::session::TurnTrigger::User,
            1,
            None,
        ),
        otto_core::session::TurnRecord::ended("op_turn", otto_core::session::TurnOutcome::Failed),
    ] {
        store
            .append_custom_entry(otto_core::session::TURN_CUSTOM_TYPE, &turn_record(&record))
            .expect("turn record");
    }
    store
        .append_message(&user("do it"))
        .expect("append user message");
    let path = store.path();
    store.close().expect("close");

    let reopened = reopen_plain(&path, read_only_tools);
    assert_eq!(reopened.take_recovery(), None);
    reopened.close().expect("close");
}

#[test]
fn a_resumed_turn_interrupted_again_is_repaired_and_not_resumed() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    store
        .append_custom_entry(
            otto_core::session::TURN_CUSTOM_TYPE,
            &turn_record(&otto_core::session::TurnRecord::started(
                "op_second",
                otto_core::session::TurnTrigger::Recovery,
                2,
                Some("op_first".into()),
            )),
        )
        .expect("turn start");
    store
        .append_message(&tool_calls(&[("call-1", "read")]))
        .expect("append dangling call");
    let path = store.path();
    store.close().expect("close");

    let reopened = reopen_plain(&path, read_only_tools);
    let recovery = reopened.take_recovery().expect("process loss recorded");
    assert!(recovery.resume_exhausted());
    assert_eq!(recovery.repaired.len(), 1);
    assert!(recovery.replayable.is_empty());
    assert_eq!(tool_result_count(&path), 1);
    reopened.close().expect("close");
}

#[test]
fn a_call_attempted_under_another_safety_is_not_replayed() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    store
        .append_message(&tool_calls(&[("call-1", "read")]))
        .expect("append dangling call");
    store
        .append_operation_fact(
            otto_core::session::operation::OperationFact::attempt(
                otto_core::model::OperationId::new("op-1").expect("operation id"),
                1,
                "call-1".to_string(),
                "read".to_string(),
            )
            .with_retry_safety(otto_core::model::RetrySafety::NonIdempotent),
        )
        .expect("append attempt");
    let path = store.path();
    store.close().expect("close");

    let reopened = reopen_plain(&path, read_only_tools);
    let recovery = reopened.take_recovery().expect("process loss recorded");
    assert_eq!(recovery.repaired.len(), 1);
    assert!(recovery.replayable.is_empty());
    assert_eq!(
        recovery.interrupted_turn, None,
        "a pre-change session has no turn records"
    );
    reopened.close().expect("close");
}

// R6: new sessions.

#[test]
fn enable_failover_creates_the_lease_directory_only_at_the_first_append() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create_lazy(&root, test_header(&workspace)).expect("create lazy");
    store.enable_failover(12).expect("enable failover");

    assert!(
        !root.exists(),
        "a lazy store must not touch disk before the first append"
    );

    store.append_message(&user("hello")).expect("first append");

    let path = store.path();
    let dir = Path::new(&path).with_extension("lease");
    assert!(dir.join("lease.json").is_file());
    assert!(dir.join("epoch-1").is_file());
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("lease.json")).expect("read lease.json"))
            .expect("parse lease.json");
    assert_eq!(meta["lease_seconds"], 12);

    store.close().expect("close");
}

#[test]
fn a_lazy_store_without_enable_failover_creates_no_lease_directory() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create_lazy(&root, test_header(&workspace)).expect("create lazy");

    store.append_message(&user("hello")).expect("first append");

    let path = store.path();
    let dir = Path::new(&path).with_extension("lease");
    assert!(!dir.exists());
    store.close().expect("close");
}

#[test]
fn two_stores_racing_enable_failover_for_the_same_session_id_leave_exactly_one_lease_directory_and_one_writer()
 {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");

    let store_a = std::sync::Arc::new(
        Store::create_lazy(&root, test_header(&workspace)).expect("create lazy a"),
    );
    let store_b = std::sync::Arc::new(
        Store::create_lazy(&root, test_header(&workspace)).expect("create lazy b"),
    );
    store_a.enable_failover(12).expect("enable failover a");
    store_b.enable_failover(12).expect("enable failover b");

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    let (ba, sa) = (
        std::sync::Arc::clone(&barrier),
        std::sync::Arc::clone(&store_a),
    );
    let thread_a = std::thread::spawn(move || {
        ba.wait();
        sa.append_message(&user("from a"))
    });
    let (bb, sb) = (
        std::sync::Arc::clone(&barrier),
        std::sync::Arc::clone(&store_b),
    );
    let thread_b = std::thread::spawn(move || {
        bb.wait();
        sb.append_message(&user("from b"))
    });

    let result_a = thread_a.join().expect("join a");
    let result_b = thread_b.join().expect("join b");

    let ok_count = [&result_a, &result_b]
        .into_iter()
        .filter(|result| result.is_ok())
        .count();
    assert_eq!(ok_count, 1, "exactly one racing writer must win");

    let winner = if result_a.is_ok() { &store_a } else { &store_b };
    let path = winner.path();
    assert!(!path.is_empty(), "the winner must have created the file");
    // Every line must be complete, LF-terminated JSON: the loser's failed
    // attempt must not have left a half-written record behind.
    let lines = json_lines(Path::new(&path));
    assert_eq!(
        lines.len(),
        3,
        "header, the runtime entry, and the one appended message, nothing else"
    );
    let dir = Path::new(&path).with_extension("lease");
    assert!(dir.join("epoch-1").is_file());

    store_a.close().ok();
    store_b.close().ok();
}

// R7: archive.

/// Creates a lease-managed session with one message and closes it, returning
/// (root, workspace, session path).
pub(super) fn seeded_lease_managed_session(temp: &TempDir) -> (PathBuf, PathBuf, String) {
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create_lazy(&root, test_header(&workspace)).expect("create lazy");
    store.enable_failover(12).expect("enable failover");
    store.append_message(&user("hello")).expect("append");
    let path = store.path();
    store.close().expect("close");
    (root, workspace, path)
}

#[test]
fn archive_of_a_lease_managed_session_moves_the_lease_directory_and_releases_it() {
    let temp = TempDir::new();
    let (root, workspace, path) = seeded_lease_managed_session(&temp);
    let source_lease_dir = Path::new(&path).with_extension("lease");
    assert!(source_lease_dir.is_dir());

    let result = archive(&root, &workspace.to_string_lossy(), Path::new(&path)).expect("archive");

    assert!(
        !source_lease_dir.exists(),
        "the lease directory must move with the session file"
    );
    let archived_lease_dir = Path::new(&result.path).with_extension("lease");
    assert!(archived_lease_dir.join("lease.json").is_file());
    assert!(archived_lease_dir.join("epoch-1").is_file());
    let heartbeat = read_lease_heartbeat(&archived_lease_dir);
    assert_eq!(
        heartbeat["released"], true,
        "archiving releases the lease after the move"
    );
}

#[test]
fn archive_fails_and_moves_nothing_while_another_holder_is_still_renewing() {
    let temp = TempDir::new();
    let (root, workspace, path) = seeded_lease_managed_session(&temp);
    let session_path = Path::new(&path);
    let dir = session_path.with_extension("lease");
    // Overwrite the epoch our own store had released with a live-looking
    // holder: `L` = 2, since our own acquire attempt here always fails
    // (`Held`) and never registers a lease with the real Keeper.
    fs::write(
        dir.join("lease.json"),
        serde_json::json!({ "lease_seconds": 2 }).to_string(),
    )
    .expect("rewrite lease.json");
    fs::write(dir.join("epoch-2"), b"{}").expect("write epoch-2 marker");
    fs::write(
        dir.join("heartbeat"),
        lease_heartbeat_bytes(2, "other-host", 5150, false, 1),
    )
    .expect("write heartbeat");

    let heartbeat_path = dir.join("heartbeat");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_stop = std::sync::Arc::clone(&stop);
    let writer = std::thread::spawn(move || {
        let mut seq = 2u64;
        while !writer_stop.load(Ordering::Relaxed) {
            fs::write(
                &heartbeat_path,
                lease_heartbeat_bytes(2, "other-host", 5150, false, seq),
            )
            .expect("rewrite heartbeat");
            seq += 1;
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });

    let before = fs::read(&path).expect("read session file before archive attempt");
    let error =
        archive(&root, &workspace.to_string_lossy(), session_path).expect_err("holder is live");

    stop.store(true, Ordering::Relaxed);
    writer.join().expect("join heartbeat writer");

    assert!(error.to_string().contains("other-host"), "{error}");
    assert!(session_path.exists(), "the session file must stay in place");
    assert_eq!(
        fs::read(&path).expect("read session file after failed archive"),
        before,
        "a failed archive must not modify the session file"
    );
    assert!(dir.is_dir(), "the lease directory must stay in place");
}

#[test]
fn prepare_of_an_archived_lease_managed_session_acquires_without_waiting() {
    let temp = TempDir::new();
    let (root, workspace, path) = seeded_lease_managed_session(&temp);

    let result = archive(&root, &workspace.to_string_lossy(), Path::new(&path)).expect("archive");
    let archived_path = Path::new(&result.path);

    let started = std::time::Instant::now();
    let prepared = Prepared::prepare(archived_path, None).expect("prepare archived session");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "a released lease must be acquired without waiting"
    );
    prepared.close().expect("close");
}

#[test]
fn workspace_identity_preserves_listing_resume_and_archive_after_moves() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("sessions");
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let store = Store::create(&root, test_header(&workspace)).unwrap();
    let session_path = store.path();
    store.close().unwrap();
    let before = fs::read(&session_path).unwrap();
    let identity = crate::workspace::initialize(&workspace, "").unwrap();
    assert_eq!(
        list::list(&root, workspace.to_str().unwrap(), "", 20)
            .unwrap()
            .sessions
            .len(),
        1
    );

    let moved = temp.path().join("moved");
    fs::rename(&workspace, &moved).unwrap();
    crate::workspace::initialize(&moved, "").unwrap();
    let listed = list::list(&root, moved.to_str().unwrap(), "", 20).unwrap();
    assert_eq!(listed.sessions[0].path, session_path);
    let prepared = Prepared::prepare_listed(
        &root,
        moved.to_str().unwrap(),
        Path::new(&session_path),
        None,
    )
    .unwrap();
    let (reopened, _) = prepared.activate().unwrap();
    reopened.close().unwrap();
    assert_eq!(fs::read(&session_path).unwrap(), before);

    // New sessions belong to the same directory but retain their actual cwd.
    let mut header = test_header(&moved);
    header.id = "550e8400-e29b-41d4-a716-446655440001".into();
    let second = Store::create(&root, header).unwrap();
    let second_path = second.path();
    second.close().unwrap();
    assert_eq!(
        Path::new(&second_path).parent().unwrap(),
        root.join(&identity.session_key)
    );
    let twice = temp.path().join("twice");
    fs::rename(&moved, &twice).unwrap();
    crate::workspace::initialize(&twice, "").unwrap();
    assert_eq!(
        list::list(&root, twice.to_str().unwrap(), "", 20)
            .unwrap()
            .sessions
            .len(),
        2
    );
    let archived = archive(&root, twice.to_str().unwrap(), Path::new(&session_path)).unwrap();
    assert_eq!(fs::read(archived.path).unwrap(), before);

    crate::workspace::fork(&twice).unwrap();
    assert!(
        list::list(&root, twice.to_str().unwrap(), "", 20)
            .unwrap()
            .sessions
            .is_empty()
    );
    assert!(
        Prepared::prepare_listed(
            &root,
            twice.to_str().unwrap(),
            Path::new(&second_path),
            None
        )
        .is_err()
    );
}
