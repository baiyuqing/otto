//! List, inspect, prepare and archive rejection tests.

use super::*;

// ---------------------------------------------------------------------------
// listing edge cases
// ---------------------------------------------------------------------------

/// Writes raw JSONL records into the workspace-key directory under `root`.
pub(super) fn plant_records(
    root: &Path,
    workspace: &Path,
    name: &str,
    records: &[&str],
) -> PathBuf {
    let key = fsops::workspace_key(workspace).expect("key");
    let directory = root.join(&key);
    fs::create_dir_all(&directory).expect("create workspace directory");
    let path = directory.join(format!("{name}.jsonl"));
    let mut contents = String::new();
    for record in records {
        contents.push_str(record);
        contents.push('\n');
    }
    fs::write(&path, contents).expect("write records");
    path
}

#[test]
fn list_stops_inspecting_after_limit_and_ignores_older_poison_candidates() {
    let temp = TempDir::new();
    let (root, workspace, _) = seeded_workspace(&temp, MAX_LIST_SESSIONS);
    let key = fsops::workspace_key(&workspace).expect("key");

    let corrupt = root.join(&key).join("older-corrupt-poison.jsonl");
    fs::write(&corrupt, b"not json\n").expect("write corrupt");
    set_modified(&corrupt, 1);

    let oversized = root.join(&key).join("oldest-oversize-poison.jsonl");
    let file = fs::File::create(&oversized).expect("create oversized");
    file.set_len(otto_core::session::MAX_SESSION_FILE_BYTES as u64 + 1)
        .expect("grow");
    drop(file);
    set_modified(&oversized, 2);

    let result =
        list::list(&root, &workspace.to_string_lossy(), "", MAX_LIST_SESSIONS).expect("list");
    assert_eq!(result.sessions.len(), MAX_LIST_SESSIONS);
    assert_eq!(
        result.skipped, 0,
        "older poison candidates must never be inspected"
    );
}

#[test]
fn list_matches_canonical_workspace_and_current_path() {
    let temp = TempDir::new();
    let canonical = temp.join("workspace");
    fs::create_dir_all(&canonical).expect("create workspace");
    let link = temp.join("workspace-link");
    std::os::unix::fs::symlink(&canonical, &link).expect("symlink");

    let root = temp.join("root");
    let mut header = test_header(&canonical);
    header.workspace = link.to_string_lossy().into_owned();
    let store = Store::create(&root, header).expect("create store");
    store.append_message(&user("hello")).expect("append");
    let path = store.path();
    store.close().expect("close");

    let current = temp.join("current.jsonl");
    std::os::unix::fs::symlink(&path, &current).expect("symlink");

    let result = list::list(
        &root,
        &canonical.to_string_lossy(),
        &current.to_string_lossy(),
        10,
    )
    .expect("list");
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.sessions[0].path, path);
    assert!(
        result.sessions[0].current,
        "a symlinked current path still matches"
    );
    assert_eq!(result.sessions[0].cwd, link.to_string_lossy());
}

#[test]
fn list_rejects_symlinks_and_other_workspaces() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let key = fsops::workspace_key(&workspace).expect("key");
    std::os::unix::fs::symlink(&paths[0], root.join(&key).join("linked.jsonl")).expect("symlink");

    let other = temp.join("other-workspace");
    fs::create_dir_all(&other).expect("create other workspace");
    let staging = temp.join("staging");
    let mut header = test_header(&other);
    header.id = "other".into();
    let store = Store::create(&staging, header).expect("create");
    store.append_message(&user("hello")).expect("append");
    let source = store.path();
    store.close().expect("close");
    fs::copy(&source, root.join(&key).join("other.jsonl")).expect("plant");

    let result = list::list(&root, &workspace.to_string_lossy(), "", 10).expect("list");
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.sessions[0].path, paths[0]);
    assert_eq!(result.skipped, 2);
}

#[test]
fn list_fails_when_session_directory_cannot_be_read() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("sessions-root");
    fs::write(&root, b"not a directory").expect("write file");
    let error = list::list(&root, &workspace.to_string_lossy(), "", 10).expect_err("must fail");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(
        error
            .to_string()
            .contains("session root is not a directory"),
        "{error}"
    );
}

#[test]
fn inspect_sanitizes_picker_metadata() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let message = concat!(
        "{\"type\":\"message\",\"id\":\"71000002\",\"parentId\":\"71000001\",",
        "\"timestamp\":\"2026-08-27T12:00:02Z\",\"message\":{\"role\":\"user\",",
        "\"content\":\"hello\\n\\u001b[31msecret\\tworld\",\"timestamp\":1}}"
    );
    let name = concat!(
        "{\"type\":\"session_info\",\"id\":\"71000003\",\"parentId\":\"71000002\",",
        "\"timestamp\":\"2026-08-27T12:00:03Z\",\"name\":\"picked\\nname\\u001b[31m\"}"
    );
    let path = plant_records(
        &temp.join("root"),
        &workspace,
        "sanitized",
        &[
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"sanitized\",",
                "\"timestamp\":\"2026-08-27T12:00:00Z\",\"cwd\":\"/workspace\"}"
            ),
            concat!(
                "{\"type\":\"custom\",\"id\":\"71000001\",\"parentId\":null,",
                "\"timestamp\":\"2026-08-27T12:00:01Z\",\"customType\":\"otto.runtime\",",
                "\"data\":{\"profile\":\"default\",\"provider\":\"openai-compatible\",",
                "\"model\":\"test-model\"}}"
            ),
            message,
            name,
        ],
    );
    let (info, _) = list::inspect(&path).expect("inspect");
    assert_eq!(info.name, r"picked name\x1b[31m");
    assert_eq!(info.last_user_text, r"hello \x1b[31msecret world");
}

// ---------------------------------------------------------------------------
// prepared and archive security
// ---------------------------------------------------------------------------

#[test]
fn prepared_activate_rejects_metadata_identity_mismatch_without_mutation() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let prepared = Prepared::prepare(Path::new(&path), None).expect("prepare");

    // Same inode, different session: only the metadata check catches this.
    let replacement = TempDir::new();
    let other = seeded_session(&replacement);
    let changed = fs::read(&other).expect("read replacement");
    fs::write(&path, &changed).expect("overwrite in place");

    let error = prepared.activate().expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(error.to_string().contains("metadata changed"), "{error}");
    assert_eq!(fs::read(&path).expect("read"), changed);
}

#[test]
fn prepare_listed_rejects_workspace_directory_symlink_replacement() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let key = fsops::workspace_key(&workspace).expect("key");
    let directory = root.join(&key);

    let outside_root = temp.join("outside-root");
    let mut header = test_header(&workspace);
    header.id = Path::new(&paths[0])
        .file_stem()
        .expect("stem")
        .to_string_lossy()
        .into_owned();
    let outside = Store::create(&outside_root, header).expect("create outside");
    outside.append_message(&user("outside")).expect("append");
    let outside_path = outside.path();
    outside.close().expect("close");
    let outside_before = fs::read(&outside_path).expect("read");

    fs::rename(&directory, directory.with_extension("moved")).expect("move directory");
    std::os::unix::fs::symlink(
        Path::new(&outside_path).parent().expect("parent"),
        &directory,
    )
    .expect("symlink");

    let error = Prepared::prepare_listed(
        &root,
        &workspace.to_string_lossy(),
        Path::new(&paths[0]),
        None,
    )
    .expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert_eq!(fs::read(&outside_path).expect("read"), outside_before);
}

#[test]
fn archive_rejects_session_from_another_workspace() {
    let temp = TempDir::new();
    let (root, _, paths) = seeded_workspace(&temp, 1);
    let other = temp.join("other-workspace");
    fs::create_dir_all(&other).expect("create other workspace");
    let error =
        archive(&root, &other.to_string_lossy(), Path::new(&paths[0])).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
}

#[test]
fn archive_rejects_non_regular_source() {
    let temp = TempDir::new();
    let (root, workspace, _) = seeded_workspace(&temp, 1);
    let key = fsops::workspace_key(&workspace).expect("key");
    let directory = root.join(&key).join("dir.jsonl");
    fs::create_dir_all(&directory).expect("create directory");
    let error = archive(&root, &workspace.to_string_lossy(), &directory).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
}

#[test]
fn archive_rejects_symlinked_archive_directory() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let key = fsops::workspace_key(&workspace).expect("key");
    let elsewhere = temp.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("create elsewhere");
    std::os::unix::fs::symlink(&elsewhere, root.join(&key).join("archive")).expect("symlink");

    let error = archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0]))
        .expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(
        Path::new(&paths[0]).exists(),
        "the source must stay in place"
    );
}
