//! Capability record, input id and plan tests.

use super::*;

// ---------------------------------------------------------------------------
// otto.capabilities
// ---------------------------------------------------------------------------

pub(super) fn raw_capabilities_entry(id: &str, parent: &str, data: &str) -> String {
    format!(
        "{{\"type\":\"custom\",\"id\":\"{id}\",\"parentId\":\"{parent}\",\"timestamp\":\"1970-01-01T00:00:01Z\",\"customType\":\"otto.capabilities\",\"data\":{data}}}\n"
    )
}

pub(super) fn last_entry_id(path: &str) -> String {
    json_lines(Path::new(path))
        .last()
        .and_then(|line| line["id"].as_str())
        .expect("leaf id")
        .to_owned()
}

#[test]
fn capabilities_record_survives_reopen_and_the_latest_applies() {
    use otto_core::session::Session;
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    assert_eq!(Session::observed_capabilities(&store), None);
    store.append_message(&user("hello")).expect("append");
    store
        .append_custom_entry("otto.capabilities", r#"{"tools":["a","b"]}"#)
        .expect("first record");
    store
        .append_custom_entry("otto.capabilities", r#"{"tools":["c","b","c"]}"#)
        .expect("second record");
    let path = store.path();
    store.close().expect("close");

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        Session::observed_capabilities(&reopened).map(|c| c.tools),
        Some(vec!["b".to_owned(), "c".to_owned()])
    );
    assert_eq!(reopened.messages().len(), 1, "the record is not a message");
    reopened.close().expect("close");
}

#[test]
fn capabilities_record_with_all_fields_survives_reopen() {
    use otto_core::session::{Capabilities, Session, encode_capabilities};
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let record = Capabilities {
        tools: vec!["a".to_owned()],
        definitions: Some([("a".to_owned(), "d1".to_owned())].into()),
        version: Some("0.2.1".to_owned()),
    };
    store
        .append_custom_entry("otto.capabilities", &encode_capabilities(&record))
        .expect("record");
    let path = store.path();
    store.close().expect("close");

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(Session::observed_capabilities(&reopened), Some(record));
    reopened.close().expect("close");
}

#[test]
fn capabilities_record_on_another_branch_does_not_apply() {
    use otto_core::session::Session;
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let user_id = last_entry_id(&path);
    append_raw(
        &path,
        raw_capabilities_entry("deadbee1", &user_id, r#"{"tools":["side"]}"#).as_bytes(),
    );
    append_raw(
        &path,
        raw_capabilities_entry("deadbee2", &user_id, r#"{"tools":["main"]}"#).as_bytes(),
    );

    let (store, warnings) = Store::open(&path).expect("open");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        Session::observed_capabilities(&store).map(|c| c.tools),
        Some(vec!["main".to_owned()])
    );
    store.close().expect("close");
}

#[test]
fn malformed_capabilities_record_is_ignored_with_a_warning() {
    use otto_core::session::Session;
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let user_id = last_entry_id(&path);
    append_raw(
        &path,
        raw_capabilities_entry("deadbee1", &user_id, r#"{"tools":"read"}"#).as_bytes(),
    );

    let (store, warnings) = Store::open(&path).expect("a malformed record does not block open");
    assert_eq!(Session::observed_capabilities(&store), None);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.message.contains("malformed otto.capabilities")),
        "{warnings:?}"
    );
    store.close().expect("close");
}

// Input ids: delivery of a mailbox entry or a client request is recorded on
// the message and survives a reopen, so it is never appended twice.

#[test]
fn append_input_if_absent_survives_a_reopen() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    assert!(
        store
            .append_input_if_absent("req:1", &user("do it"))
            .expect("append")
    );
    let context = Message {
        role: Role::Context,
        context_type: "task_notification".into(),
        display: true,
        blocks: vec![Block::text("done")],
        created_at: created_at(),
        ..Message::default()
    };
    assert!(
        store
            .append_input_if_absent("in_2", &context)
            .expect("append")
    );
    assert!(
        !store
            .append_input_if_absent("req:1", &user("do it"))
            .expect("check")
    );
    let path = store.path();
    store.close().expect("close");

    let (reopened, _warnings) = Store::open(Path::new(&path)).expect("reopen");
    assert!(reopened.has_input("req:1"));
    assert!(reopened.has_input("in_2"));
    assert!(
        !reopened
            .append_input_if_absent("in_2", &context)
            .expect("check")
    );
    let (_entries, messages) = Store::read_entries(&path).expect("read");
    assert_eq!(
        messages
            .iter()
            .filter(|message| !message.input_id.is_empty())
            .count(),
        2
    );
    reopened.close().expect("close");
}

#[test]
fn an_input_id_is_rejected_on_an_assistant_message() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    let mut reply = assistant("hi");
    reply.input_id = "in_1".into();
    assert!(store.append_message(&reply).is_err());
    store.close().expect("close");
}

// An input delivered before a compaction checkpoint stays delivered: the
// checkpoint summarizes its message away from the context, but its entry is
// still on the active path, in memory and after a reopen.
#[test]
fn an_input_before_a_compaction_checkpoint_stays_delivered() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    assert!(
        store
            .append_input_if_absent("in_old", &user("old"))
            .expect("append")
    );
    store.append_message(&assistant("reply")).expect("append");
    store.append_message(&user("new")).expect("append");
    let path = store.path();
    let anchor = message_entry_id(&path, 3);
    store
        .append_compaction(&checkpoint(&anchor))
        .expect("append compaction");
    assert!(
        store
            .messages()
            .iter()
            .all(|message| message.input_id != "in_old")
    );
    assert!(store.has_input("in_old"));
    assert!(
        !store
            .append_input_if_absent("in_old", &user("old"))
            .expect("check")
    );
    store.close().expect("close");

    let (reopened, _warnings) = Store::open(Path::new(&path)).expect("reopen");
    assert!(reopened.has_input("in_old"));
    assert!(
        !reopened
            .append_input_if_absent("in_old", &user("old"))
            .expect("check")
    );
    reopened.close().expect("close");
}

#[test]
fn a_plan_set_before_a_compaction_survives_it_and_a_reopen() {
    use otto_core::session::{PLAN_CUSTOM_TYPE, PlanStatus, PlanStep, Session, encode_plan};
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let plan = vec![PlanStep {
        text: "ship".into(),
        status: PlanStatus::InProgress,
    }];
    store.append_message(&user("one")).expect("append");
    store
        .append_custom_entry(PLAN_CUSTOM_TYPE, &encode_plan(&plan))
        .expect("plan");
    store.append_message(&assistant("two")).expect("append");
    store.append_message(&user("three")).expect("append");
    let anchor = message_entry_id(&store.path(), 3);
    store
        .append_compaction(&checkpoint(&anchor))
        .expect("compact");
    assert_eq!(store.plan(), plan);
    // A malformed record is refused and leaves the plan in force.
    assert!(
        store
            .append_custom_entry(PLAN_CUSTOM_TYPE, r#"{"schemaVersion":1}"#)
            .is_err()
    );
    assert_eq!(store.plan(), plan);
    let path = store.path();
    store.close().expect("close");

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(reopened.plan(), plan);
    reopened.close().expect("close");
}
