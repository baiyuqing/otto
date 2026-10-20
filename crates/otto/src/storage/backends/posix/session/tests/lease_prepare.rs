//! Lease-managed prepare, takeover and archive tests.

use super::*;

// ---------------------------------------------------------------------------
// session failover lease
//
// Test safety: `Lease::acquire` (used by `Prepared::prepare`/`prepare_listed`
// and `Store::enable_failover`) registers with the process-wide
// `failover::lease::Keeper`, whose watchdog calls `libc::_exit(75)` on a
// lease this process holds if it goes more than 5L/6 without renewal or is
// superseded. Every test below that ends up holding such a lease uses
// `lease_seconds` (`L`) >= 6 and releases (`close`) it within milliseconds of
// acquiring, well inside the 5L/6 margin. A "stopped holder" is simulated by
// writing `epoch-<n>` and `heartbeat` by hand, with no live `Lease` of our
// own; a "running holder" is simulated with a thread that rewrites the
// heartbeat file every 200ms, never with a second live lease.
// ---------------------------------------------------------------------------

/// The JSON+padding+newline byte layout `failover::lease` writes to a lease
/// directory's `heartbeat` file, reproduced here to simulate a holder by
/// hand.
pub(super) fn lease_heartbeat_bytes(
    epoch: u64,
    host: &str,
    pid: u32,
    released: bool,
    seq: u64,
) -> Vec<u8> {
    let json = serde_json::json!({
        "epoch": epoch,
        "host": host,
        "pid": pid,
        "released": released,
        "seq": seq,
    })
    .to_string();
    let mut bytes = vec![b' '; 512];
    bytes[..json.len()].copy_from_slice(json.as_bytes());
    bytes[511] = b'\n';
    bytes
}

/// Writes a lease directory by hand -- `lease.json`, one `epoch-<epoch>`
/// marker and an initial `heartbeat` -- without acquiring a live [`Lease`],
/// to simulate a holder another host left behind. Returns the directory.
pub(super) fn write_lease_dir_by_hand(
    session_path: &Path,
    lease_seconds: u64,
    epoch: u64,
    host: &str,
    pid: u32,
    released: bool,
) -> PathBuf {
    let dir = session_path.with_extension("lease");
    fs::create_dir_all(&dir).expect("create lease dir by hand");
    fs::write(
        dir.join("lease.json"),
        serde_json::json!({ "lease_seconds": lease_seconds }).to_string(),
    )
    .expect("write lease.json by hand");
    fs::write(dir.join(format!("epoch-{epoch}")), b"{}").expect("write epoch marker by hand");
    fs::write(
        dir.join("heartbeat"),
        lease_heartbeat_bytes(epoch, host, pid, released, 1),
    )
    .expect("write heartbeat by hand");
    dir
}

/// Reads a lease directory's `heartbeat` file and parses its JSON prefix.
pub(super) fn read_lease_heartbeat(dir: &Path) -> serde_json::Value {
    let bytes = fs::read(dir.join("heartbeat")).expect("read heartbeat");
    let text = String::from_utf8_lossy(&bytes);
    serde_json::from_str(text.trim_end()).expect("parse heartbeat")
}

// R1 (`[failover]` config resolution) is covered by
// `otto_core::config::failover`'s own tests, not here.

// R2: lease-managed open.

#[test]
fn prepare_fails_when_a_lease_managed_sessions_holder_is_still_renewing() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let session_path = Path::new(&path);
    // `L` = 2: this process's own acquire fails (`Held`), so it never holds
    // a live lease and a short `L` is safe here (see the section header).
    let dir = write_lease_dir_by_hand(session_path, 2, 1, "holder-host", 4242, false);

    let heartbeat_path = dir.join("heartbeat");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_stop = std::sync::Arc::clone(&stop);
    let writer = std::thread::spawn(move || {
        let mut seq = 2u64;
        while !writer_stop.load(Ordering::Relaxed) {
            fs::write(
                &heartbeat_path,
                lease_heartbeat_bytes(1, "holder-host", 4242, false, seq),
            )
            .expect("rewrite heartbeat");
            seq += 1;
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });

    let error = Prepared::prepare(session_path, None).expect_err("holder is still live");

    stop.store(true, Ordering::Relaxed);
    writer.join().expect("join heartbeat writer");

    let text = error.to_string();
    assert!(text.contains("holder-host"), "{text}");
    assert!(text.contains("4242"), "{text}");
}

#[test]
fn prepare_with_no_create_lease_and_no_lease_directory_creates_none() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let session_path = Path::new(&path);

    let prepared = Prepared::prepare(session_path, None).expect("prepare");
    let (store, _warnings) = prepared.activate().expect("activate");
    assert!(store.lease().is_none());
    assert!(
        !session_path.with_extension("lease").exists(),
        "no lease directory should be created"
    );
    store.close().expect("close");
}

#[test]
fn prepare_with_create_lease_creates_a_lease_directory_for_an_existing_session() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let session_path = Path::new(&path);
    assert!(!crate::failover::lease::is_lease_managed(session_path).expect("check lease managed"));

    let prepared = Prepared::prepare(session_path, Some(6)).expect("prepare");
    let (store, _warnings) = prepared.activate().expect("activate");

    let lease_dir = session_path.with_extension("lease");
    assert!(lease_dir.join("lease.json").is_file());
    assert!(lease_dir.join("epoch-1").is_file());
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(lease_dir.join("lease.json")).expect("read lease.json"),
    )
    .expect("parse lease.json");
    assert_eq!(meta["lease_seconds"], 6);

    store.close().expect("close");
}

#[test]
fn prepare_honors_lease_seconds_already_recorded_in_an_existing_lease_directory() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let session_path = Path::new(&path);
    crate::failover::lease::create_lease_dir(session_path, 30).expect("create lease dir");

    let prepared = Prepared::prepare(session_path, Some(40)).expect("prepare");
    let (store, _warnings) = prepared.activate().expect("activate");
    assert!(store.lease().is_some(), "lease-managed store");
    let lease_metadata: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(session_path.with_extension("lease").join("lease.json"))
            .expect("read lease metadata"),
    )
    .expect("parse lease metadata");
    assert_eq!(lease_metadata["lease_seconds"], 30);

    store.close().expect("close");
}

// R3: `Prepared` carries the acquired lease; closing without activating
// releases it.

#[test]
fn prepared_close_of_a_lease_managed_session_releases_the_lease_for_the_next_open() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let session_path = Path::new(&path);

    let prepared = Prepared::prepare(session_path, Some(6)).expect("prepare");
    prepared.close().expect("close without activating");

    let lease_dir = session_path.with_extension("lease");
    let heartbeat = read_lease_heartbeat(&lease_dir);
    assert_eq!(heartbeat["released"], true);

    let started = std::time::Instant::now();
    let prepared2 = Prepared::prepare(session_path, None).expect("reopen");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "a released epoch must be taken over without waiting"
    );
    let (store2, _warnings) = prepared2.activate().expect("activate");
    assert!(
        store2.take_recovery().is_none(),
        "a fresh acquire records no takeover"
    );
    store2.close().expect("close");
}

// R4: `Store` holds the lease and checks it before every durable write.

#[test]
fn a_write_after_the_lease_is_marked_lost_fails_and_poisons_the_store_without_growing_the_file() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    let path = store.path();
    let dir = Path::new(&path).with_extension("lease");
    fs::create_dir_all(&dir).expect("create lease dir");
    let lease = crate::failover::lease::Lease::for_test(&dir);
    {
        let mut state = store.lock().expect("lock store");
        state.lease = Some(lease.clone());
        state.owns_lease = true;
    }
    lease.mark_lost_for_test("test induced loss");

    let before = fs::metadata(&path).expect("stat").len();
    let error = store
        .append_message(&user("hello"))
        .expect_err("write must fail once the lease is lost");
    assert!(error.to_string().contains("session lease lost"), "{error}");

    let error2 = store
        .append_message(&user("again"))
        .expect_err("store stays poisoned");
    assert_eq!(
        error.to_string(),
        error2.to_string(),
        "a poisoned store returns the same error on every later write"
    );

    let after = fs::metadata(&path).expect("stat").len();
    assert_eq!(
        before, after,
        "a failed write must not change the file length"
    );
}

#[test]
fn close_of_an_owned_lease_writes_a_released_heartbeat() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    let path = store.path();
    let dir = Path::new(&path).with_extension("lease");
    fs::create_dir_all(&dir).expect("create lease dir");
    let lease = crate::failover::lease::Lease::for_test(&dir);
    {
        let mut state = store.lock().expect("lock store");
        state.lease = Some(lease);
        state.owns_lease = true;
    }

    store.close().expect("close");

    let heartbeat = read_lease_heartbeat(&dir);
    assert_eq!(heartbeat["released"], true);
}

#[test]
fn close_of_a_check_only_lease_does_not_release_it() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    let path = store.path();
    let dir = Path::new(&path).with_extension("lease");
    fs::create_dir_all(&dir).expect("create lease dir");
    let lease = crate::failover::lease::Lease::for_test(&dir);
    store.set_lease_check(lease);

    let before = fs::read(dir.join("heartbeat")).expect("read heartbeat");
    store.close().expect("close");
    let after = fs::read(dir.join("heartbeat")).expect("read heartbeat");
    assert_eq!(
        before, after,
        "close must not write a heartbeat for a lease this store does not own"
    );
}

// R5: a takeover from a stopped holder is recorded on the store and the
// dangling calls it left are repaired.

#[test]
fn takeover_through_prepare_records_the_holder_and_repairs_the_dangling_calls() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    store
        .append_message(&tool_calls(&[("call-1", "read"), ("call-2", "bash")]))
        .expect("append dangling calls");
    let path = store.path();
    store
        .close()
        .expect("close before simulating a stopped holder");
    let original_bytes = fs::read(&path).expect("read original log");

    let session_path = Path::new(&path);
    // `L` = 6, the minimum for a lease this process ends up holding live
    // (see the section header); the 7L/6 takeover wait runs on a virtual
    // clock, since the holder never renews.
    let dir = write_lease_dir_by_hand(session_path, 6, 1, "stopped-host", 9001, false);

    let prepared =
        crate::failover::lease::without_acquire_wait(|| Prepared::prepare(session_path, None))
            .expect("prepare takes over");
    let (activated, _warnings) = prepared.activate().expect("activate");

    let takeover = activated.take_recovery().expect("takeover recorded");
    let crate::session::RecoveryCause::TakenOver(holder) = &takeover.cause else {
        panic!("takeover cause");
    };
    assert_eq!(holder.host, "stopped-host");
    assert_eq!(holder.pid, 9001);
    assert_eq!(takeover.repaired.len(), 2);
    assert_eq!(takeover.repaired[0].name, "read");
    assert!(takeover.repaired[0].may_have_run);
    assert_eq!(takeover.repaired[1].name, "bash");
    assert!(!takeover.repaired[1].may_have_run);

    let (_entries, messages) = Store::read_entries(&path).expect("read repaired log");
    let tool_messages = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .count();
    assert_eq!(
        tool_messages, 2,
        "both dangling calls got synthetic results"
    );

    let fenced = fs::read(dir.join("fenced-1.jsonl")).expect("read fenced copy");
    assert_eq!(
        fenced, original_bytes,
        "the fenced copy holds the original log bytes"
    );

    activated.close().expect("close");
}

// Replay: a takeover leaves calls of replayable tools unanswered, for the
// agent to run again, only when every pending call qualifies.

pub(super) fn read_only_tools(name: &str) -> bool {
    matches!(name, "read" | "grep")
}

/// Takes over a session whose log ends in `calls`, after `prepare` is given
/// `setup` to add operation facts first, and returns the activated store.
pub(super) fn take_over_with(
    calls: &[(&str, &str)],
    attempts: &[(&str, &str, u32)],
    replayable: fn(&str) -> bool,
) -> (TempDir, Store, String) {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    store
        .append_message(&tool_calls(calls))
        .expect("append dangling calls");
    for (index, (call_id, name, count)) in attempts.iter().enumerate() {
        let operation_id =
            otto_core::model::OperationId::new(format!("op-{index}")).expect("operation id");
        for attempt in 1..=*count {
            store
                .append_operation_fact(otto_core::session::operation::OperationFact::attempt(
                    operation_id.clone(),
                    attempt,
                    (*call_id).to_string(),
                    (*name).to_string(),
                ))
                .expect("append attempt");
        }
    }
    let path = store.path();
    store
        .close()
        .expect("close before simulating a stopped holder");
    write_lease_dir_by_hand(Path::new(&path), 6, 1, "stopped-host", 9001, false);
    let prepared =
        crate::failover::lease::without_acquire_wait(|| Prepared::prepare(Path::new(&path), None))
            .expect("prepare takes over")
            .with_replayable(replayable);
    let (activated, _warnings) = prepared.activate().expect("activate");
    (temp, activated, path)
}

pub(super) fn tool_result_count(path: &str) -> usize {
    let (_entries, messages) = Store::read_entries(path).expect("read log");
    messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .count()
}

#[test]
fn takeover_leaves_all_replayable_dangling_calls_unanswered() {
    let (_temp, store, path) = take_over_with(
        &[("call-1", "read"), ("call-2", "grep")],
        &[("call-1", "read", 1)],
        read_only_tools,
    );
    let takeover = store.take_recovery().expect("takeover recorded");
    assert!(takeover.repaired.is_empty());
    assert_eq!(takeover.replayable.len(), 2);
    assert_eq!(takeover.replayable[0].name, "read");
    assert!(takeover.replayable[0].may_have_run);
    assert_eq!(
        tool_result_count(&path),
        0,
        "no synthetic result may be appended for a call the agent will run again"
    );
    let record = store
        .operation_ledger()
        .operation_for_tool_call("call-1")
        .cloned()
        .expect("recorded operation");
    assert!(
        record.terminal.is_none(),
        "the interrupted attempt must stay unsettled so the next attempt can follow it"
    );
    store.close().expect("close");
}

#[test]
fn takeover_repairs_every_call_when_one_is_not_replayable() {
    let (_temp, store, path) = take_over_with(
        &[("call-1", "read"), ("call-2", "bash")],
        &[],
        read_only_tools,
    );
    let takeover = store.take_recovery().expect("takeover recorded");
    assert_eq!(takeover.repaired.len(), 2);
    assert!(takeover.replayable.is_empty());
    assert_eq!(tool_result_count(&path), 2);
    store.close().expect("close");
}

#[test]
fn interrupted_nonidempotent_call_is_settled_once_and_never_replayed() {
    use otto_core::model::{EffectCertainty, OperationDisposition};
    let (_temp, store, path) = take_over_with(
        &[("call-write", "write")],
        &[("call-write", "write", 1)],
        crate::tool::safety::is_replayable,
    );
    let recovery = store.take_recovery().expect("recovery recorded");
    assert!(recovery.replayable.is_empty());
    assert_eq!(recovery.repaired.len(), 1);
    assert_eq!(tool_result_count(&path), 1);
    let messages = store.messages();
    let result = messages
        .iter()
        .flat_map(|m| &m.blocks)
        .find(|b| b.tool_call_id == "call-write" && b.is_error)
        .expect("model-visible result");
    let metadata = result
        .operation_metadata
        .as_ref()
        .expect("operation result metadata");
    assert_eq!(metadata.disposition, OperationDisposition::Interrupted);
    assert_eq!(metadata.effect_certainty, EffectCertainty::Unknown);
    assert!(store.take_recovery().is_none(), "recovery is consumed once");
    assert_eq!(tool_result_count(&path), 1);
    store.close().expect("close");
}

#[test]
fn takeover_repairs_a_call_that_was_already_replayed_once() {
    let (_temp, store, path) = take_over_with(
        &[("call-1", "read")],
        &[("call-1", "read", 2)],
        read_only_tools,
    );
    let takeover = store.take_recovery().expect("takeover recorded");
    assert_eq!(takeover.repaired.len(), 1);
    assert!(takeover.replayable.is_empty());
    assert_eq!(
        tool_result_count(&path),
        1,
        "a second interruption falls back to the interrupted result"
    );
    store.close().expect("close");
}

// Process loss: a plain reopen (no lease) recovers a session the previous
// writer left mid-turn the same way a takeover does.
