//! Tests for custom entries deferred until the session file is created.

use super::*;

/// A store whose session file does not exist until the first durable write.
fn new_lazy_store(temp: &TempDir) -> Store {
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    Store::create_lazy_with_driver(
        temp.join("root"),
        test_header(&workspace),
        Arc::new(PosixDriver),
    )
    .expect("create lazy store")
}

#[tokio::test]
async fn goal_custom_record_survives_reopen_and_rejects_invalid_data() {
    use otto_core::session::Session;
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let goal = otto_core::goal::Goal::draft("A meaningful goal".into());
    let data = serde_json::to_string(&goal).unwrap();
    store
        .append_custom_entry(otto_core::goal::GOAL_CUSTOM_TYPE, &data)
        .unwrap();
    assert_eq!(store.goal(), Some(goal.clone()));
    assert!(
        store
            .append_custom_entry(otto_core::goal::GOAL_CUSTOM_TYPE, "{}")
            .is_err()
    );
    assert_eq!(store.goal(), Some(goal.clone()));
    let mut updated = goal.clone();
    updated.note = "newer state".into();
    assert!(
        store
            .compare_and_set_goal(Some(&goal), &updated)
            .await
            .unwrap()
    );
    assert!(
        !store
            .compare_and_set_goal(Some(&goal), &goal)
            .await
            .unwrap()
    );
    assert_eq!(store.goal(), Some(updated.clone()));
    let goal = updated;
    let path = store.path();
    store.close().unwrap();
    let (reopened, _) = Store::open(&path).unwrap();
    assert_eq!(reopened.goal(), Some(goal));
    reopened.close().unwrap();
}

fn session_files(directory: &Path) -> Vec<PathBuf> {
    match fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn jsonl_files_under(root: &Path) -> Vec<PathBuf> {
    let Ok(workspaces) = fs::read_dir(root) else {
        return Vec::new();
    };
    workspaces
        .flat_map(|workspace| session_files(&workspace.expect("workspace dir").path()))
        .collect()
}

#[test]
fn deferred_custom_entry_is_written_after_runtime_and_before_the_first_message() {
    let temp = TempDir::new();
    let store = new_lazy_store(&temp);
    store
        .append_custom_entry_when_created("otto.plugins", r#"{"packages":["demo"]}"#)
        .expect("defer");
    assert_eq!(store.path(), "");
    assert!(jsonl_files_under(&temp.join("root")).is_empty());

    store.append_message(&user("hello")).expect("append");
    let lines = json_lines(Path::new(&store.path()));
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[1]["customType"], "otto.runtime");
    assert_eq!(lines[2]["customType"], "otto.plugins");
    assert_eq!(lines[2]["data"]["packages"][0], "demo");
    assert_eq!(lines[2]["parentId"], lines[1]["id"]);
    assert_eq!(lines[3]["type"], "message");
    assert_eq!(lines[3]["parentId"], lines[2]["id"]);
    store.close().expect("close");
}

#[test]
fn deferred_custom_entries_keep_call_order() {
    let temp = TempDir::new();
    let store = new_lazy_store(&temp);
    store
        .append_custom_entry_when_created("test.first", "1")
        .expect("first");
    store
        .append_custom_entry_when_created("test.second", "2")
        .expect("second");
    store.rename("named").expect("rename");
    let lines = json_lines(Path::new(&store.path()));
    let kinds: Vec<_> = lines
        .iter()
        .map(|l| {
            l["customType"]
                .as_str()
                .unwrap_or(l["type"].as_str().unwrap())
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "session",
            "otto.runtime",
            "test.first",
            "test.second",
            "session_info"
        ]
    );
    assert_eq!(lines[4]["parentId"], lines[3]["id"]);
    assert_eq!(lines[3]["parentId"], lines[2]["id"]);
    store.close().expect("close");
}

#[test]
fn lazy_store_closed_after_only_a_deferred_entry_writes_no_file() {
    let temp = TempDir::new();
    let store = new_lazy_store(&temp);
    store
        .append_custom_entry_when_created("otto.plugins", r#"{"packages":[]}"#)
        .expect("defer");
    store.close().expect("close");
    assert!(jsonl_files_under(&temp.join("root")).is_empty());
}

#[test]
fn deferred_custom_entry_on_an_existing_file_is_written_immediately() {
    let temp = TempDir::new();
    let store = new_lazy_store(&temp);
    store.append_message(&user("hello")).expect("append");
    let path = store.path();
    let before = json_lines(Path::new(&path)).len();
    store
        .append_custom_entry_when_created("otto.plugins", r#"{"packages":[]}"#)
        .expect("append");
    let lines = json_lines(Path::new(&path));
    assert_eq!(lines.len(), before + 1);
    assert_eq!(lines.last().expect("last")["customType"], "otto.plugins");
    store.close().expect("close");
}

#[test]
fn deferred_custom_entry_is_rejected_for_side_effect_types_and_malformed_data() {
    let temp = TempDir::new();
    let store = new_lazy_store(&temp);
    for custom_type in [OPERATION_CUSTOM_TYPE, otto_core::session::PLAN_CUSTOM_TYPE] {
        let error = store
            .append_custom_entry_when_created(custom_type, "{}")
            .expect_err("rejected");
        assert_eq!(error.kind(), PiErrorKind::Invalid, "{error}");
    }
    store
        .append_custom_entry_when_created("test.bad", "{not json")
        .expect_err("malformed data fails at the call");
    store.append_message(&user("hello")).expect("append");
    assert_eq!(json_lines(Path::new(&store.path())).len(), 3);
    store.close().expect("close");
}
