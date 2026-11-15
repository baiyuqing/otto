//! Open, tail repair and operation-fact repair tests.

use super::*;

// ---------------------------------------------------------------------------
// open-time repair
// ---------------------------------------------------------------------------

/// Appends raw bytes to a closed session file.
pub(super) fn append_raw(path: &str, bytes: &[u8]) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open for append");
    file.write_all(bytes).expect("append raw bytes");
}

pub(super) fn seeded_session(temp: &TempDir) -> String {
    let (store, _) = new_store(temp);
    store.append_message(&user("hello")).expect("append");
    let path = store.path();
    store.close().expect("close");
    path
}

pub(super) const SESSION_LOCK_TEST_PATH: &str = "OTTO_SESSION_LOCK_TEST_PATH";

#[test]
fn open_rejects_a_session_held_by_another_process() {
    if let Some(path) = std::env::var_os(SESSION_LOCK_TEST_PATH) {
        let error = Store::open(path).expect_err("another process holds the session");
        assert!(error.to_string().contains("already open"), "{error}");
        return;
    }

    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let (store, _) = Store::open(&path).expect("open first store");
    let before = fs::read(&path).expect("read before second open");
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tests::open_rejects_a_session_held_by_another_process",
            "--nocapture",
        ])
        .env(SESSION_LOCK_TEST_PATH, &path)
        .status()
        .expect("run second process");
    assert!(
        status.success(),
        "second process unexpectedly opened session"
    );
    assert_eq!(fs::read(&path).expect("read after second open"), before);

    store.close().expect("close first store");
    Store::open(&path)
        .expect("open after first store closes")
        .0
        .close()
        .expect("close");
}

#[test]
fn open_repairs_missing_final_lf_before_append() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let contents = fs::read(&path).expect("read");
    fs::write(&path, &contents[..contents.len() - 1]).expect("strip delimiter");

    let (store, warnings) = Store::open(&path).expect("open");
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0].message,
        format!("repaired missing final session delimiter at {path}")
    );
    store.append_message(&assistant("hi")).expect("append");
    store.close().expect("close");
    assert_eq!(json_lines(Path::new(&path)).len(), 4);
}

#[test]
fn open_truncates_incomplete_final_line_with_warning() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    append_raw(&path, br#"{"type":"message","id":"aabbccdd""#);

    let (store, warnings) = Store::open(&path).expect("open");
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0].message,
        format!("truncated incomplete final session line at {path}")
    );
    assert_eq!(store.messages().len(), 1);
    store.close().expect("close");
    assert_eq!(json_lines(Path::new(&path)).len(), 3);
}

#[test]
fn open_rejects_complete_invalid_final_json_without_mutation() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    append_raw(&path, b"{\"type\":\"message\"}\n");
    let before = fs::read(&path).expect("read");

    let error = Store::open(&path).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert_eq!(fs::read(&path).expect("read"), before);
}

#[test]
fn open_rejects_malformed_non_final_line_without_mutation() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    append_raw(&path, b"not json\n");
    append_raw(&path, b"{\"type\":\"custom\"}\n");
    let before = fs::read(&path).expect("read");

    Store::open(&path).expect_err("must reject");
    assert_eq!(fs::read(&path).expect("read"), before);
}

#[test]
fn operation_attempt_repair_writes_terminal_before_unavailable_result_and_reopens_idempotently() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&tool_call("call-1", "bash"))
        .expect("append call");
    store
        .append_operation_fact(OperationFact::attempt(
            operation_id("op-1"),
            1,
            "call-1",
            "bash",
        ))
        .expect("append attempt");
    let path = store.path();
    store.close().expect("close");

    let (reopened, warnings) = Store::open(&path).expect("repair");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains("call-1"))
    );
    let lines = json_lines(Path::new(&path));
    let terminal = lines
        .iter()
        .position(|line| {
            line["customType"] == OPERATION_CUSTOM_TYPE && line["data"]["event"] == "terminal"
        })
        .expect("terminal fact");
    let result = lines
        .iter()
        .position(|line| line["message"]["role"] == "toolResult")
        .expect("synthetic result");
    assert!(terminal < result, "terminal must be durable before result");
    assert_eq!(lines[terminal]["data"]["disposition"], "interrupted");
    assert_eq!(lines[terminal]["data"]["effectCertainty"], "unknown");
    assert_eq!(lines[terminal]["data"]["stopReason"], "process_lost");
    assert_eq!(lines[result]["message"]["isError"], true);
    assert_eq!(
        lines[result]["message"]["details"]["otto"]["operationId"],
        "op-1"
    );
    assert_eq!(
        lines[result]["message"]["details"]["otto"]["effectCertainty"],
        "unknown"
    );
    assert_eq!(
        reopened.messages().last().expect("result").blocks[0].operation_metadata,
        Some(result_metadata(Some("op-1"), interrupted_outcome()))
    );
    assert_eq!(
        reopened
            .operation_ledger()
            .operation(&operation_id("op-1"))
            .expect("operation")
            .terminal,
        Some(interrupted_outcome())
    );
    reopened.close().expect("close repaired");

    let before = fs::read(&path).expect("read repaired");
    let (again, warnings) = Store::open(&path).expect("reopen");
    assert!(
        warnings.is_empty(),
        "repair must be idempotent: {warnings:?}"
    );
    again.close().expect("close again");
    assert_eq!(fs::read(&path).expect("read again"), before);
}

#[test]
fn operation_terminal_without_result_gets_only_unavailable_result() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&tool_call("call-1", "read"))
        .expect("append call");
    store
        .append_operation_fact(OperationFact::attempt(
            operation_id("op-1"),
            1,
            "call-1",
            "read",
        ))
        .expect("append attempt");
    store
        .append_operation_fact(OperationFact::terminal(
            operation_id("op-1"),
            1,
            "call-1",
            "read",
            known_no_effect_error_outcome(),
        ))
        .expect("append terminal");
    let path = store.path();
    store.close().expect("close");

    let terminal_count_before = json_lines(Path::new(&path))
        .iter()
        .filter(|line| line["data"]["event"] == "terminal")
        .count();
    let (reopened, _) = Store::open(&path).expect("repair");
    let messages = reopened.messages();
    assert!(messages.last().expect("result").blocks[0].is_error);
    assert_eq!(
        messages.last().expect("result").blocks[0].operation_metadata,
        Some(result_metadata(
            Some("op-1"),
            known_no_effect_error_outcome(),
        ))
    );
    reopened.close().expect("close repaired");
    let lines = json_lines(Path::new(&path));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line["data"]["event"] == "terminal")
            .count(),
        terminal_count_before,
        "an existing terminal must not be synthesized again"
    );
}

#[test]
fn corrupt_terminal_history_repairs_tail_as_unknown() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&tool_call("call-1", "write"))
        .expect("append call");
    store
        .append_operation_fact(OperationFact::attempt(
            operation_id("op-1"),
            1,
            "call-1",
            "write",
        ))
        .expect("append attempt");
    store
        .append_operation_fact(OperationFact::terminal(
            operation_id("op-1"),
            1,
            "call-1",
            "write",
            OperationOutcome {
                disposition: OperationDisposition::Succeeded,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
        ))
        .expect("append terminal");
    let path = store.path();
    store.close().expect("close");

    let parent = json_lines(Path::new(&path))
        .last()
        .and_then(|line| line["id"].as_str())
        .expect("leaf id")
        .to_string();
    let conflicting = serde_json::json!({
        "type": "custom",
        "id": "deadbeef",
        "parentId": parent,
        "timestamp": "2026-09-28T00:00:00Z",
        "customType": OPERATION_CUSTOM_TYPE,
        "data": {
            "event": "terminal",
            "schemaVersion": 1,
            "operationId": "op-1",
            "attempt": 1,
            "kind": "tool_call",
            "toolCallId": "call-1",
            "toolName": "write",
            "disposition": "interrupted",
            "effectCertainty": "unknown",
            "stopReason": "process_lost"
        }
    });
    append_raw(&path, format!("{}\n", conflicting).as_bytes());

    let (reopened, warnings) = Store::open(&path).expect("repair corrupt history");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains("effects are unknown")),
        "{warnings:?}"
    );
    let metadata = reopened.messages().last().expect("synthetic result").blocks[0]
        .operation_metadata
        .clone()
        .expect("typed metadata");
    assert_eq!(metadata.operation_id, Some(operation_id("op-1")));
    assert_eq!(metadata.disposition, OperationDisposition::Interrupted);
    assert_eq!(metadata.effect_certainty, EffectCertainty::Unknown);
    assert_eq!(metadata.stop_reason, Some(OperationStopReason::ProcessLost));
    reopened.close().expect("close");
}

#[test]
fn legacy_multi_call_repair_never_fabricates_operation_facts() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&tool_calls(&[("call-1", "read"), ("call-2", "bash")]))
        .expect("append calls");
    let path = store.path();
    store.close().expect("close");

    let (reopened, _) = Store::open(&path).expect("repair");
    let messages = reopened.messages();
    assert_eq!(messages[1].blocks[0].text, MAY_HAVE_RUN_TOOL_RESULT_TEXT);
    assert_eq!(
        messages[1].blocks[0].operation_metadata,
        Some(result_metadata(None, interrupted_outcome()))
    );
    assert_eq!(messages[2].blocks[0].text, NOT_EXECUTED_TOOL_RESULT_TEXT);
    assert_eq!(
        messages[2].blocks[0].operation_metadata,
        Some(result_metadata(
            None,
            OperationOutcome {
                disposition: OperationDisposition::Interrupted,
                effect_certainty: EffectCertainty::NotStarted,
                stop_reason: Some(OperationStopReason::ProcessLost),
            },
        ))
    );
    assert!(reopened.operation_ledger().operations().next().is_none());
    reopened.close().expect("close");
    assert!(
        json_lines(Path::new(&path))
            .iter()
            .all(|line| line["customType"] != OPERATION_CUSTOM_TYPE)
    );
}

#[test]
fn generic_operation_custom_uses_strict_typed_path_and_malformed_fails_closed() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let malformed_before = fs::read(store.path()).expect("read before malformed");
    store
        .append_custom_entry(
            OPERATION_CUSTOM_TYPE,
            r#"{"schemaVersion":1,"event":"attempt"}"#,
        )
        .expect_err("known malformed fact must fail");
    assert_eq!(
        fs::read(store.path()).expect("read after malformed"),
        malformed_before
    );

    let fact = OperationFact::attempt(operation_id("op-1"), 1, "call-1", "read");
    let encoded = otto_core::session::encode_operation_fact(&fact).expect("encode");
    store
        .append_custom_entry(OPERATION_CUSTOM_TYPE, &encoded)
        .expect("generic typed append");
    let before_conflict = fs::read(store.path()).expect("read before conflict");
    store
        .append_custom_entry(OPERATION_CUSTOM_TYPE, &encoded)
        .expect_err("duplicate attempt must fail strict validation");
    assert_eq!(
        fs::read(store.path()).expect("read after conflict"),
        before_conflict
    );
    assert_eq!(
        store
            .operation_ledger()
            .operation(&operation_id("op-1"))
            .expect("operation")
            .attempts,
        1
    );
    store.close().expect("close");
}

#[test]
fn failed_operation_fact_write_does_not_commit_candidate_ledger() {
    let temp = TempDir::new();
    let (store, plan) = store_failing_writes_from(&temp, 3);
    let before = fs::read(store.path()).expect("read before");
    let error = store
        .append_operation_fact(OperationFact::attempt(
            operation_id("op-1"),
            1,
            "call-1",
            "bash",
        ))
        .expect_err("injected failure");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert!(store.operation_ledger().operations().next().is_none());
    assert_eq!(fs::read(store.path()).expect("read after"), before);
    plan.assert_all_fired();
}

#[test]
fn operation_ledger_survives_reopen_with_multiple_calls() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    for (operation, call, tool) in [("op-1", "call-1", "read"), ("op-2", "call-2", "bash")] {
        store
            .append_operation_fact(OperationFact::attempt(
                operation_id(operation),
                1,
                call,
                tool,
            ))
            .expect("append attempt");
    }
    let path = store.path();
    store.close().expect("close");
    let (reopened, warnings) = Store::open(&path).expect("reopen");
    assert!(warnings.is_empty());
    assert_eq!(reopened.operation_ledger().operations().count(), 2);
    reopened.close().expect("close reopened");
}

#[test]
fn open_repairs_dangling_tool_call_durably() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&tool_call("call-1", "read"))
        .expect("append call");
    let path = store.path();
    store.close().expect("close");

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].message, "repaired dangling tool call call-1");
    let messages = reopened.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].role, Role::Tool);
    assert!(messages[1].blocks[0].is_error);
    assert_eq!(messages[1].blocks[0].text, MAY_HAVE_RUN_TOOL_RESULT_TEXT);
    reopened.close().expect("close");

    let (again, warnings) = Store::open(&path).expect("reopen");
    assert!(warnings.is_empty(), "repair must be durable: {warnings:?}");
    assert_eq!(again.messages().len(), 2);
    again.close().expect("close");
}

#[test]
fn open_marks_only_the_first_dangling_call_as_possibly_run_durably() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&tool_calls(&[
            ("call-1", "read"),
            ("call-2", "bash"),
            ("call-3", "write"),
        ]))
        .expect("append calls");
    store
        .append_message(&tool_result("call-1", "read", "ok"))
        .expect("append result");
    let path = store.path();
    store.close().expect("close");

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert_eq!(warnings.len(), 2);
    let messages = reopened.messages();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[2].blocks[0].tool_call_id, "call-2");
    assert_eq!(messages[2].blocks[0].text, MAY_HAVE_RUN_TOOL_RESULT_TEXT);
    assert!(messages[2].blocks[0].is_error);
    assert_eq!(messages[3].blocks[0].tool_call_id, "call-3");
    assert_eq!(messages[3].blocks[0].text, NOT_EXECUTED_TOOL_RESULT_TEXT);
    assert!(messages[3].blocks[0].is_error);
    reopened.close().expect("close");

    let (again, warnings) = Store::open(&path).expect("reopen");
    assert!(warnings.is_empty(), "repair must be durable: {warnings:?}");
    again.close().expect("close");
}

#[test]
fn open_repairs_a_dangling_tool_call_left_mid_history() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("hello")).expect("append user");
    store
        .append_message(&tool_call("call-1", "read"))
        .expect("append call");
    let path = store.path();
    store.close().expect("close");

    // A user message written straight after the call, which `append_message`
    // refuses to write and only another Pi writer can produce.
    let lines = json_lines(Path::new(&path));
    let leaf = lines.last().expect("leaf")["id"]
        .as_str()
        .expect("leaf id")
        .to_owned();
    let mut orphaning = lines
        .iter()
        .find(|line| line["message"]["role"] == "user")
        .expect("user entry")
        .clone();
    orphaning["id"] = serde_json::json!("deadbeef");
    orphaning["parentId"] = serde_json::json!(leaf);
    append_raw(&path, format!("{orphaning}\n").as_bytes());
    let before = json_lines(Path::new(&path)).len();

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains("call-1")),
        "the repair must be reported: {warnings:?}"
    );
    let roles: Vec<Role> = reopened
        .messages()
        .iter()
        .map(|message| message.role.clone())
        .collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::Tool, Role::User],
        "the stand-in result belongs next to its call"
    );
    // The session takes writes again, which is what the dangling call broke.
    reopened.append_message(&assistant("hi")).expect("append");
    reopened.close().expect("close");

    let lines = json_lines(Path::new(&path));
    assert_eq!(
        lines.len(),
        before + 1,
        "an append-only file cannot hold the repair: {lines:?}"
    );
    let (again, warnings) = Store::open(&path).expect("reopen");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains("call-1")),
        "the repair is redone on every open: {warnings:?}"
    );
    assert_eq!(again.messages().len(), 5);
    again.close().expect("close");
}

#[test]
fn open_preserves_unknown_pi_entries_and_appends_beneath_leaf() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let leaf = json_lines(Path::new(&path))
        .last()
        .expect("leaf")
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("leaf id")
        .to_owned();
    let unknown = format!(
        "{{\"type\":\"future_thing\",\"id\":\"deadbeef\",\"parentId\":\"{leaf}\",\"timestamp\":\"1970-01-01T00:00:01Z\",\"extra\":[1,2,3]}}\n"
    );
    append_raw(&path, unknown.as_bytes());

    let (store, _) = Store::open(&path).expect("open");
    store.append_message(&assistant("hi")).expect("append");
    store.close().expect("close");

    let lines = json_lines(Path::new(&path));
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[3]["type"], "future_thing");
    assert_eq!(lines[3]["extra"], serde_json::json!([1, 2, 3]));
    assert_eq!(lines[4]["parentId"], "deadbeef");
}

#[test]
fn read_header_leaves_incomplete_tail_for_open_recovery() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    append_raw(&path, br#"{"type":"message""#);
    let before = fs::read(&path).expect("read");

    let header = Store::read_header(&path).expect("read header");
    assert_eq!(header.model, "test-model");
    assert_eq!(fs::read(&path).expect("read"), before);
}
