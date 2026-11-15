use super::*;
use crate::memory::files::testsupport::{open_temp, sample_record};
use crate::memory::{RecordKey, UpsertRequest};

#[test]
fn legacy_memory_remains_reachable_after_a_move_and_is_separate_after_fork() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("workspace");
    std::fs::create_dir(&original).unwrap();
    let original = std::fs::canonicalize(original).unwrap();
    let config = MemoryRuntime::default();
    let scope = workspace_memory_scope(&config, original.to_str().unwrap()).unwrap();
    let (_store_dir, store) = open_temp();
    let record = store
        .upsert(&UpsertRequest {
            record: sample_record("legacy-memory", &scope, "style", "concise"),
            expected_revision: None,
        })
        .unwrap();
    otto_session::workspace::initialize(&original, "").unwrap();
    let moved = dir.path().join("moved");
    std::fs::rename(&original, &moved).unwrap();
    otto_session::workspace::initialize(&moved, "").unwrap();
    let moved_scope = workspace_memory_scope(&config, moved.to_str().unwrap()).unwrap();
    assert_eq!(moved_scope, scope);
    assert_eq!(
        store
            .get_by_key(&RecordKey {
                scope: moved_scope,
                kind: "preference".into(),
                key: "style".into()
            })
            .unwrap(),
        record
    );
    crate::runtime::builder::validate_session_workspace(
        original.to_str().unwrap(),
        moved.to_str().unwrap(),
    )
    .unwrap();
    otto_session::workspace::fork(&moved).unwrap();
    let fork_scope = workspace_memory_scope(&config, moved.to_str().unwrap()).unwrap();
    assert_ne!(fork_scope, scope);
    assert!(
        store
            .get_by_key(&RecordKey {
                scope: fork_scope,
                kind: "preference".into(),
                key: "style".into()
            })
            .is_err()
    );
    assert!(
        crate::runtime::builder::validate_session_workspace(
            original.to_str().unwrap(),
            moved.to_str().unwrap()
        )
        .is_err()
    );
}
