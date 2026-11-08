//! Startup recovery of delivery receipts and tasks owned by other processes.

use super::tests::binding;
use super::*;

const WORKTREE: &str = "/target/.otto/delivery-worktrees/delivery-1";

/// A pid that belonged to a process that has exited, with a start time.
fn exited_process() -> (i64, String) {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = i64::from(child.id());
    child.wait().unwrap();
    (pid, "2020-01-01T00:00:00.000000000Z".into())
}

fn applying_receipt_with_task(store: &ReceiptStore) {
    store.prepare(binding("approval-1")).unwrap();
    store.begin("approval-1").unwrap();
    store
        .begin_task("approval-1", "source", "target", Path::new(WORKTREE))
        .unwrap();
}

#[test]
fn rows_owned_by_this_live_process_survive_startup_recovery() {
    let store = ReceiptStore::in_memory();
    applying_receipt_with_task(&store);
    assert_eq!(store.mark_interrupted_unknown().unwrap(), 0);
    assert_eq!(store.mark_running_tasks_unknown().unwrap(), 0);
    assert_eq!(
        store.get("approval-1").unwrap().unwrap().state,
        ReceiptState::Applying
    );
    assert_eq!(
        store.task("approval-1").unwrap().unwrap().state,
        TaskState::Running
    );
    // The owner still finishes its own rows.
    assert_eq!(
        store.started("approval-1").unwrap().state,
        ReceiptState::Started
    );
    let finished = store
        .finish_task("approval-1", TaskState::Succeeded, "ok", "")
        .unwrap();
    assert_eq!(finished.state, TaskState::Succeeded);
}

#[test]
fn rows_owned_by_an_exited_process_become_unknown() {
    let store = ReceiptStore::in_memory();
    applying_receipt_with_task(&store);
    let (pid, started) = exited_process();
    store.set_owner_for_test("approval-1", pid, &started);
    assert_eq!(store.mark_interrupted_unknown().unwrap(), 1);
    assert_eq!(store.mark_running_tasks_unknown().unwrap(), 1);
    assert_eq!(
        store.get("approval-1").unwrap().unwrap().state,
        ReceiptState::Unknown
    );
    let task = store.task("approval-1").unwrap().unwrap();
    assert_eq!(task.state, TaskState::Unknown);
    assert!(task.error.contains("interrupted"));
    assert!(store.begin("approval-1").unwrap_err().contains("unknown"));
    assert!(store.started("approval-1").unwrap_err().contains("unknown"));
}

#[test]
fn rows_without_a_recorded_owner_become_unknown() {
    let store = ReceiptStore::in_memory();
    applying_receipt_with_task(&store);
    store.set_owner_for_test("approval-1", 0, "");
    assert_eq!(store.mark_interrupted_unknown().unwrap(), 1);
    assert_eq!(store.mark_running_tasks_unknown().unwrap(), 1);
}

#[test]
fn a_database_without_owner_columns_opens_and_its_rows_recover() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("delivery-receipts.db");
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE delivery_receipts (approval_id TEXT PRIMARY KEY,
                 source_workspace TEXT NOT NULL, target_repository TEXT NOT NULL,
                 target_common_dir TEXT NOT NULL, worktree TEXT NOT NULL, branch TEXT NOT NULL,
                 base TEXT NOT NULL, handoff_digest TEXT NOT NULL, state TEXT NOT NULL) STRICT;
                 INSERT INTO delivery_receipts VALUES
                 ('old','/s','/t','/c','/w','agent/x','','','applying');",
            )
            .unwrap();
    }
    let store = ReceiptStore::open(&path).unwrap();
    assert_eq!(store.mark_interrupted_unknown().unwrap(), 1);
    assert_eq!(
        store.get("old").unwrap().unwrap().state,
        ReceiptState::Unknown
    );
    drop(store);
    ReceiptStore::open(&path).unwrap();
}
