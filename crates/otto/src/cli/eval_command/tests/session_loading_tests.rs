use std::path::Path;

use otto_core::model::Role;
use otto_core::view_tree::LogKind;

use super::session_fixture::{
    SESSION_ID, assistant_entry, compaction_entry, user_entry, write_session,
};
use crate::cli::eval_command::session_input::{load_session, resolve_session_path};

#[test]
fn history_keeps_messages_before_the_compaction_and_skips_the_other_branch() {
    let directory = tempfile::tempdir().unwrap();
    let entries = [
        user_entry("a0000001", None, "first question"),
        assistant_entry("a0000002", Some("a0000001"), "first answer"),
        user_entry("a0000003", Some("a0000002"), "abandoned branch"),
        user_entry("a0000004", Some("a0000002"), "second question"),
        assistant_entry("a0000005", Some("a0000004"), "second answer"),
        compaction_entry("a0000006", "a0000005", "a0000004", "earlier work"),
        user_entry("a0000007", Some("a0000006"), "third question"),
    ];
    let path = write_session(directory.path(), &entries);

    let session = load_session(&path).unwrap();

    let texts: Vec<&str> = session
        .log
        .iter()
        .map(|message| message.text.as_str())
        .collect();
    assert_eq!(
        texts,
        [
            "first question",
            "first answer",
            "second question",
            "second answer",
            "third question"
        ]
    );
    assert_eq!(session.log[1].kind, LogKind::Assistant);
    assert_eq!(session.log[4].entry_id, "a0000007");
    assert_eq!(session.session_id, SESSION_ID);
    // The baseline is what a turn would send now: the summary, then the messages kept after it.
    let baseline: Vec<String> = session
        .baseline_messages
        .iter()
        .map(|message| message.text())
        .collect();
    assert_eq!(baseline.len(), 4);
    assert!(baseline[0].contains("earlier work"));
    assert_eq!(
        baseline[1..],
        ["second question", "second answer", "third question"]
    );
    assert_eq!(session.baseline_messages[3].role, Role::User);
}

#[test]
fn session_resolves_by_path_or_by_id_and_reports_a_missing_file() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.to_string_lossy().into_owned();
    let root = directory.path().join("sessions");
    let session_directory = crate::session::session_directory(&root, &workspace).unwrap();
    let path = write_session(&session_directory, &[user_entry("a0000001", None, "hello")]);

    assert_eq!(
        resolve_session_path(&path.to_string_lossy(), &root, &workspace).unwrap(),
        path
    );
    assert_eq!(
        resolve_session_path(SESSION_ID, &root, &workspace).unwrap(),
        path
    );
    let missing =
        resolve_session_path("ffffffffffffffffffffffffffffffff", &root, &workspace).unwrap_err();
    assert!(missing.contains("SQLite-routed"), "{missing}");
    let neither =
        resolve_session_path("not-a-session", Path::new("/nonexistent"), &workspace).unwrap_err();
    assert!(neither.contains("neither an existing file"), "{neither}");
}

#[test]
fn a_session_without_entries_is_an_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_session(directory.path(), &[]);
    assert!(load_session(&path).unwrap_err().contains("no entries"));
}
