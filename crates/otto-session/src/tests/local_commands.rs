//! Human receipt replay is independent of all message/context projections.
use super::*;
use crate::SessionHandle;
use otto_core::session::Session;
use otto_core::session::local_command::{
    LOCAL_COMMAND_CUSTOM_TYPE, LocalCommandReceipt, LocalCommandStatus,
};

fn receipt(status: LocalCommandStatus) -> LocalCommandReceipt {
    LocalCommandReceipt {
        id: "receipt-1".into(),
        command: "/memory show".into(),
        parameters: "id=record-full-id".into(),
        status,
        summary: "record-full-id revision=2".into(),
        output: "human-only-output".into(),
        created_at: "2026-11-07T00:00:00Z".into(),
        after_messages: 2,
    }
}

#[tokio::test]
async fn local_command_receipts_survive_reopen_and_compaction_without_becoming_messages() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("ordinary prompt")).unwrap();
    store.append_message(&assistant("ordinary reply")).unwrap();
    store
        .append_local_command(
            &serde_json::to_string(&receipt(LocalCommandStatus::Pending)).unwrap(),
        )
        .unwrap();
    let receipt = receipt(LocalCommandStatus::Success);
    store
        .append_local_command(&serde_json::to_string(&receipt).unwrap())
        .unwrap();
    assert_eq!(store.local_commands(), vec![receipt.clone()]);
    assert_eq!(store.messages().len(), 2);
    assert_eq!(store.model_messages().len(), 2);
    assert!(!format!("{:?}", store.model_messages()).contains("human-only-output"));
    let first_kept = store.messages()[1].id.clone();
    store
        .append_compaction(&CompactionCheckpoint {
            summary: "ordinary summary".into(),
            first_kept_entry_id: first_kept,
            tokens_before: 100,
            created_at: created_at(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(store.local_commands(), vec![receipt.clone()]);
    assert!(!format!("{:?}", store.messages()).contains("human-only-output"));
    let path = store.path();
    store.close().unwrap();
    let bytes = fs::read_to_string(&path).unwrap();
    assert_eq!(bytes.matches(LOCAL_COMMAND_CUSTOM_TYPE).count(), 2);
    let (reopened, _) = Store::open(&path).unwrap();
    assert_eq!(reopened.local_commands(), vec![receipt]);
    assert!(!format!("{:?}", reopened.model_messages()).contains("human-only-output"));
}

#[test]
fn local_command_receipt_pending_survives_interrupted_execution_reopen() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let pending = receipt(LocalCommandStatus::Pending);
    store
        .append_local_command(&serde_json::to_string(&pending).unwrap())
        .unwrap();
    let path = store.path();
    store.close().unwrap();
    let (reopened, _) = Store::open(&path).unwrap();
    assert_eq!(reopened.local_commands(), vec![pending]);
    assert!(reopened.messages().is_empty());
}

#[test]
fn local_command_receipt_recording_scans_sensitive_query_output() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let field = ["client", "_secret"].concat();
    let mut receipt = receipt(LocalCommandStatus::Success);
    receipt.output = format!("{field}=opaque-test-value");
    store
        .append_local_command(&serde_json::to_string(&receipt).unwrap())
        .unwrap();
    assert!(
        !store.local_commands()[0]
            .output
            .contains("opaque-test-value")
    );
    let raw = fs::read_to_string(store.path()).unwrap();
    assert!(!raw.contains("opaque-test-value"));
}
