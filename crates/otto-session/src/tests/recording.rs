//! Verify recording protection against real journal bytes and reloads.

use super::*;

#[test]
fn recording_does_not_rewrite_credentials_in_existing_history() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    // Simulate a record written before credential-pattern detection existed.
    let legacy = fs::read_to_string(&path)
        .unwrap()
        .replace("hello", "client_secret=opaque-value-123");
    fs::write(&path, &legacy).unwrap();
    let (store, _) = Store::open(&path).unwrap();
    store.append_message(&assistant("new reply")).unwrap();
    store.close().unwrap();
    assert!(fs::read_to_string(&path).unwrap().starts_with(&legacy));
}

#[test]
fn recording_protects_new_messages_custom_records_and_compaction() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let candidate = "opaque-value-123";
    store
        .append_custom_entry_when_created(
            "otto.task_spec",
            &format!(r#"{{"description":"client_secret={candidate}"}}"#),
        )
        .unwrap();
    let message = user(&format!("postgres://reader:{candidate}@db.internal/app"));
    store.append_message(&message).unwrap();
    store
        .append_message(&assistant(&format!("credentials={candidate}")))
        .unwrap();
    store.append_message(&user("continue")).unwrap();
    store
        .append_custom_entry(
            "otto.task_result",
            &format!(r#"{{"text":"SK={candidate}"}}"#),
        )
        .unwrap();
    let path = store.path();
    let mut summary = checkpoint(&message_entry_id(&path, 3));
    summary.summary = format!("client_secret={candidate}");
    store.append_compaction(&summary).unwrap();
    store.close().unwrap();
    let bytes = fs::read_to_string(&path).unwrap();
    assert!(
        !bytes.contains(candidate),
        "credential reached journal bytes"
    );
    assert!(
        message.text().contains(candidate),
        "caller-owned input changed"
    );
    let (reopened, _) = Store::open(&path).unwrap();
    assert!(
        !serde_json::to_string(&reopened.messages())
            .unwrap()
            .contains(candidate)
    );
    assert!(
        !reopened
            .latest_compaction()
            .unwrap()
            .summary
            .contains(candidate)
    );
    reopened.close().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        bytes,
        "reload rewrote history"
    );
}

#[test]
fn recording_retains_approval_exception_and_leaves_old_bytes_untouched() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let mut call = tool_call("approval-1", "request_approval");
    let candidate = "opaque-value-123";
    call.blocks[0].arguments = Some(
        serde_json::value::RawValue::from_string(format!(
            r#"{{"proposal":"client_secret={candidate}"}}"#
        ))
        .unwrap(),
    );
    store.append_message(&call).unwrap();
    store
        .append_message(&tool_result("approval-1", "request_approval", "declined"))
        .unwrap();
    let path = store.path();
    store.close().unwrap();
    let before = fs::read(&path).unwrap();
    let (reopened, _) = Store::open(&path).unwrap();
    reopened.append_message(&user("new text")).unwrap();
    reopened.close().unwrap();
    let after = fs::read(&path).unwrap();
    assert!(after.starts_with(&before));
    assert!(String::from_utf8(before).unwrap().contains(candidate));
}

#[test]
fn recording_does_not_replay_masked_read_only_arguments() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let mut call = tool_call("read-1", "read");
    call.blocks[0].arguments = Some(
        serde_json::value::RawValue::from_string(
            r#"{"path":"file","client_secret":"opaque-value-123"}"#.into(),
        )
        .unwrap(),
    );
    store.append_message(&call).unwrap();
    let path = store.path();
    store.close().unwrap();
    let reopened = turn_recovery::reopen_plain(&path, |name| name == "read");
    let recovery = reopened.take_recovery().unwrap();
    assert!(recovery.replayable.is_empty());
    assert_eq!(recovery.repaired.len(), 1);
    assert!(
        otto_core::session::context::pending_tool_calls(&reopened.messages())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn recording_protects_child_transcripts_and_queued_input_without_changing_identity() {
    let temp = TempDir::new();
    let (parent, _) = new_store(&temp);
    parent.append_message(&user("start")).unwrap();
    let child = Store::create_child_lazy(parent.path(), "t1-child", test_header(&temp.0)).unwrap();
    let candidate = "opaque-value-123";
    assert!(
        child
            .append_input_if_absent("queued-1", &user(&format!("client_secret={candidate}")))
            .unwrap()
    );
    assert!(
        !child
            .append_input_if_absent("queued-1", &user("duplicate"))
            .unwrap()
    );
    let path = child.path();
    child.close().unwrap();
    assert!(!fs::read_to_string(&path).unwrap().contains(candidate));
    let (reopened, _) = Store::open(path).unwrap();
    assert!(reopened.has_input("queued-1"));
    assert_eq!(reopened.messages().len(), 1);
    reopened.close().unwrap();
}

#[test]
fn recording_protects_hook_provider_request_records() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let candidate = "opaque-value-123";
    store
        .append_custom_entry(
            "otto.hook.provider_request",
            &format!(
                r#"{{"version":1,"operation_id":"op_1","context":"client_secret={candidate}","edits":[{{"message":0,"block":0,"text":"SK={candidate}"}}]}}"#
            ),
        )
        .unwrap();
    let path = store.path();
    store.close().unwrap();
    let bytes = fs::read_to_string(&path).unwrap();
    assert!(bytes.contains("otto.hook.provider_request"));
    assert!(
        !bytes.contains(candidate),
        "credential reached journal bytes"
    );
}
