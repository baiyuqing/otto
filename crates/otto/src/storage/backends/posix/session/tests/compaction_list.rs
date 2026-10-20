//! Compaction checkpoint, list and inspect tests.

use super::*;

// ---------------------------------------------------------------------------
// compaction
// ---------------------------------------------------------------------------

pub(super) fn checkpoint(first_kept: &str) -> CompactionCheckpoint {
    CompactionCheckpoint {
        summary: "summary".into(),
        first_kept_entry_id: first_kept.into(),
        tokens_before: 100,
        usage: None,
        details: CompactionDetails::default(),
        created_at: created_at(),
    }
}

/// The entry id of the nth message entry, counting from one.
pub(super) fn message_entry_id(path: &str, index: usize) -> String {
    json_lines(Path::new(path))
        .into_iter()
        .filter(|line| line["type"] == "message")
        .nth(index - 1)
        .expect("message entry")
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("entry id")
        .to_owned()
}

#[test]
fn store_append_compaction_writes_checkpoint_and_reopens() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("one")).expect("append");
    store.append_message(&assistant("two")).expect("append");
    store.append_message(&user("three")).expect("append");
    let path = store.path();
    let anchor = message_entry_id(&path, 3);

    let metadata = store
        .append_compaction(&checkpoint(&anchor))
        .expect("append compaction");
    assert_eq!(metadata.first_kept_entry_id, anchor);
    assert_eq!(metadata.summary, "summary");
    assert_eq!(metadata.tokens_before, 100);
    assert!(!metadata.retained_tail_only);
    assert_eq!(store.latest_compaction().expect("latest").id, metadata.id);

    let messages = store.messages();
    assert_eq!(messages[0].role, Role::Context);
    assert_eq!(messages[0].text(), "[Compaction summary]\nsummary");
    assert_eq!(messages.last().expect("last").text(), "three");
    store.close().expect("close");

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(reopened.messages(), messages);
    assert_eq!(
        reopened.latest_compaction().expect("latest").id,
        metadata.id
    );
    reopened.close().expect("close");
}

#[test]
fn store_append_compaction_tracks_first_post_checkpoint_message() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("one")).expect("append");
    let anchor = message_entry_id(&store.path(), 1);
    let metadata = store
        .append_compaction(&checkpoint(&anchor))
        .expect("append compaction");
    assert_eq!(metadata.first_post_checkpoint_message_id, "");

    store.append_message(&assistant("after")).expect("append");
    let latest = store.latest_compaction().expect("latest");
    assert!(!latest.first_post_checkpoint_message_id.is_empty());
    store.close().expect("close");
}

#[test]
fn store_append_compaction_rejects_invalid_checkpoint_without_mutation() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("one")).expect("append");
    let anchor = message_entry_id(&store.path(), 1);
    let before = fs::read(store.path()).expect("read");

    type Case = (&'static str, Box<dyn Fn(&mut CompactionCheckpoint)>);
    let cases: Vec<Case> = vec![
        ("blank summary", Box::new(|c| c.summary = "  ".into())),
        (
            "unknown anchor",
            Box::new(|c| c.first_kept_entry_id = "deadbeef".into()),
        ),
        ("negative tokens", Box::new(|c| c.tokens_before = -1)),
        (
            "zero timestamp",
            Box::new(|c| c.created_at = otto_core::model::zero_time()),
        ),
    ];
    for (name, mutate) in cases {
        let mut candidate = checkpoint(&anchor);
        mutate(&mut candidate);
        let error = store.append_compaction(&candidate).expect_err(name);
        assert_eq!(error.kind(), PiErrorKind::Invalid, "{name}: {error}");
        assert_eq!(fs::read(store.path()).expect("read"), before, "{name}");
        assert!(store.latest_compaction().is_none(), "{name}");
    }
    store.close().expect("close");
}

#[test]
fn store_append_compaction_preserves_lazy_creation_on_rejection() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create_lazy(&root, test_header(&workspace)).expect("create lazy");
    let error = store
        .append_compaction(&checkpoint("deadbeef"))
        .expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert_eq!(store.path(), "");
    assert!(!root.exists(), "rejection must not create the session file");
    store.close().expect("close");
}

#[test]
fn store_append_compaction_after_close_fails() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("one")).expect("append");
    let anchor = message_entry_id(&store.path(), 1);
    store.close().expect("close");
    let error = store
        .append_compaction(&checkpoint(&anchor))
        .expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Closed);
}

#[test]
fn store_append_compaction_durable_failure_poisons_without_advancing_context() {
    let temp = TempDir::new();
    let (store, plan) = store_failing_writes_from(&temp, 4);
    store.append_message(&user("one")).expect("append");
    let anchor = message_entry_id(&store.path(), 1);
    let before = store.messages();

    let error = store
        .append_compaction(&checkpoint(&anchor))
        .expect_err("must fail");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert_eq!(store.messages(), before);
    assert!(store.latest_compaction().is_none());
    plan.assert_all_fired();
    store.close().expect("close");
}

#[test]
fn store_repeated_checkpoints_reopen_exactly() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("one")).expect("append");
    let path = store.path();
    let first_anchor = message_entry_id(&path, 1);
    store
        .append_compaction(&checkpoint(&first_anchor))
        .expect("first checkpoint");
    store.append_message(&assistant("two")).expect("append");
    let second_anchor = message_entry_id(&path, 2);
    let second = store
        .append_compaction(&checkpoint(&second_anchor))
        .expect("second checkpoint");
    let messages = store.messages();
    store.close().expect("close");

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(reopened.messages(), messages);
    assert_eq!(reopened.latest_compaction().expect("latest").id, second.id);
    reopened.close().expect("close");
}

// ---------------------------------------------------------------------------
// snapshot
// ---------------------------------------------------------------------------

#[test]
fn store_snapshot_tracks_context_input_across_compaction() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    assert_eq!(store.snapshot(), otto_core::session::Snapshot::default());

    store.append_message(&user("one")).expect("append");
    store
        .append_message(&assistant_with_usage(
            "two",
            Usage {
                input_tokens: 40,
                output_tokens: 8,
                cached_input_tokens: 10,
            },
        ))
        .expect("append");
    let snapshot = store.snapshot();
    assert!(snapshot.aggregate_usage_present);
    assert_eq!(snapshot.aggregate_usage.input_tokens, 40);
    assert!(snapshot.context_input_tokens_present);
    assert_eq!(snapshot.context_input_tokens, 40);

    let anchor = message_entry_id(&store.path(), 2);
    store
        .append_compaction(&checkpoint(&anchor))
        .expect("compaction");
    let snapshot = store.snapshot();
    assert!(
        snapshot.context_input_tokens_pending,
        "a fresh checkpoint has no post-checkpoint usage yet"
    );
    store.close().expect("close");
}

// ---------------------------------------------------------------------------
// list and inspect
// ---------------------------------------------------------------------------

/// A root holding `count` sessions for one workspace, newest last.
pub(super) fn seeded_workspace(temp: &TempDir, count: usize) -> (PathBuf, PathBuf, Vec<String>) {
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let mut paths = Vec::new();
    for index in 0..count {
        let mut header = test_header(&workspace);
        header.id = format!("session-{index:04}");
        let store = Store::create(&root, header).expect("create store");
        store
            .append_message(&user(&format!("message {index}")))
            .expect("append");
        paths.push(store.path());
        store.close().expect("close");
        set_modified(Path::new(paths.last().expect("path")), 1_000 + index as i64);
    }
    (root, workspace, paths)
}

/// Sets a file's mtime so listing order is deterministic.
pub(super) fn set_modified(path: &Path, seconds: i64) {
    let times = [
        libc::timeval {
            tv_sec: seconds as libc::time_t,
            tv_usec: 0,
        },
        libc::timeval {
            tv_sec: seconds as libc::time_t,
            tv_usec: 0,
        },
    ];
    let raw = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("path");
    // SAFETY: `raw` is a NUL-terminated path and `times` is a two-element
    // array of `timeval`, exactly what `utimes` requires.
    let result = unsafe { libc::utimes(raw.as_ptr(), times.as_ptr()) };
    assert_eq!(result, 0, "utimes failed for {}", path.display());
}

#[test]
fn list_returns_recent_workspace_sessions_newest_first() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 3);
    let result = list::list(&root, &workspace.to_string_lossy(), "", 10).expect("list");
    assert_eq!(result.skipped, 0);
    let listed: Vec<&str> = result
        .sessions
        .iter()
        .map(|info| info.path.as_str())
        .collect();
    assert_eq!(listed, vec![&paths[2], &paths[1], &paths[0]]);
    assert_eq!(result.sessions[0].last_user_text, "message 2");
    assert_eq!(result.sessions[0].message_count, 1);
    assert_eq!(result.sessions[0].provider, "openai-compatible");
    assert!(!result.sessions[0].current);
}

#[test]
fn list_honors_limit_and_marks_current() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 3);
    let result = list::list(&root, &workspace.to_string_lossy(), &paths[0], 2).expect("list");
    assert_eq!(result.sessions.len(), 2);
    assert!(result.sessions.iter().all(|info| !info.current));

    let result = list::list(&root, &workspace.to_string_lossy(), &paths[2], 3).expect("list");
    assert!(result.sessions[0].current);
    assert!(!result.sessions[1].current);
}

#[test]
fn list_orders_mtime_ties_deterministically() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 3);
    for path in &paths {
        set_modified(Path::new(path), 2_000);
    }
    let result = list::list(&root, &workspace.to_string_lossy(), "", 10).expect("list");
    let listed: Vec<&str> = result
        .sessions
        .iter()
        .map(|info| info.path.as_str())
        .collect();
    let mut expected: Vec<&str> = paths.iter().map(String::as_str).collect();
    expected.sort_by(|left, right| right.cmp(left));
    assert_eq!(listed, expected);
}

#[test]
fn list_returns_empty_when_workspace_directory_is_missing() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    fs::create_dir_all(&root).expect("create root");
    let result = list::list(&root, &workspace.to_string_lossy(), "", 10).expect("list");
    assert_eq!(result.sessions.len(), 0);
    assert_eq!(result.skipped, 0);
}

#[test]
fn list_fails_when_session_root_is_missing() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let error = list::list(&temp.join("absent"), &workspace.to_string_lossy(), "", 10)
        .expect_err("must fail");
    assert!(error.to_string().contains("open session root"), "{error}");
}

#[test]
fn list_rejects_symlinked_root_and_workspace_directory() {
    let temp = TempDir::new();
    let (root, workspace, _) = seeded_workspace(&temp, 1);
    let root_link = temp.join("root-link");
    std::os::unix::fs::symlink(&root, &root_link).expect("symlink");
    // macOS reports ENOTDIR rather than ELOOP for `O_NOFOLLOW|O_DIRECTORY` on
    // a symlink to a directory, so only the rejection itself is asserted.
    let error =
        list::list(&root_link, &workspace.to_string_lossy(), "", 10).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);

    let key = fsops::workspace_key(&workspace).expect("key");
    let real = root.join(&key);
    let moved = temp.join("moved-directory");
    fs::rename(&real, &moved).expect("rename");
    std::os::unix::fs::symlink(&moved, &real).expect("symlink");
    let error = list::list(&root, &workspace.to_string_lossy(), "", 10).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
}

#[test]
fn list_skips_other_workspaces_and_unreadable_candidates() {
    let temp = TempDir::new();
    let (root, workspace, _) = seeded_workspace(&temp, 1);
    let key = fsops::workspace_key(&workspace).expect("key");
    fs::write(root.join(&key).join("garbage.jsonl"), b"not json\n").expect("write garbage");
    fs::write(root.join(&key).join("ignored.txt"), b"ignored").expect("write other");

    let result = list::list(&root, &workspace.to_string_lossy(), "", 10).expect("list");
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.skipped, 1);
}

#[test]
fn list_collapses_two_files_that_share_a_session_id() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let key = fsops::workspace_key(&workspace).expect("key");
    let original = &paths[0];
    let duplicate = root.join(&key).join("session-0000-copy.jsonl");
    fs::copy(original, &duplicate).expect("copy duplicate session file");
    set_modified(&duplicate, 2_000);

    let result = list::list(&root, &workspace.to_string_lossy(), "", 10).expect("list");
    assert_eq!(result.sessions.len(), 1, "{:?}", result.sessions);
    assert_eq!(result.sessions[0].id, "session-0000");
    assert_eq!(result.sessions[0].path, duplicate.to_string_lossy());
}

#[test]
fn list_validates_limit() {
    let temp = TempDir::new();
    let (root, workspace, _) = seeded_workspace(&temp, 1);
    for limit in [0, MAX_LIST_SESSIONS + 1] {
        let error =
            list::list(&root, &workspace.to_string_lossy(), "", limit).expect_err("must reject");
        assert!(
            error.to_string().contains("list limit must be between 1"),
            "{error}"
        );
    }
}

#[test]
fn inspect_derives_metadata_and_name_override() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&user("  a very  long   question  "))
        .expect("append");
    let path = store.path();
    let (info, warnings) = list::inspect(Path::new(&path)).expect("inspect");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(info.id, "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(info.message_count, 1);
    assert_eq!(info.last_user_text, "a very long question");
    assert_eq!(info.name, "a very long question");
    assert_eq!(info.created, created_at());
    assert_eq!(info.model, "test-model");

    store.rename("Custom name").expect("rename");
    store.close().expect("close");
    let (info, _) = list::inspect(Path::new(&path)).expect("inspect");
    assert_eq!(info.name, "Custom name");
    assert_eq!(info.last_user_text, "a very long question");
}

#[test]
fn inspect_uses_the_first_prompt_as_the_unnamed_session_label() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&user("first prompt"))
        .expect("append first user message");
    store
        .append_message(&assistant("first reply"))
        .expect("append assistant message");
    store
        .append_message(&user("later prompt"))
        .expect("append later user message");
    let path = store.path();

    let (info, warnings) = list::inspect(Path::new(&path)).expect("inspect");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(info.name, "first prompt");
    assert_eq!(info.last_user_text, "first prompt");

    store.close().expect("close");
}

#[test]
fn inspect_truncates_the_first_prompt_for_the_unnamed_session_label() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&user(&"x".repeat(121)))
        .expect("append user message");
    let path = store.path();

    let (info, warnings) = list::inspect(Path::new(&path)).expect("inspect");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(info.name, format!("{}...", "x".repeat(120)));
    assert_eq!(info.last_user_text, format!("{}...", "x".repeat(120)));

    store.close().expect("close");
}

#[test]
fn inspect_leaves_repairable_files_untouched() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&tool_call("call-1", "read"))
        .expect("append call");
    let path = store.path();
    store.close().expect("close");
    let contents = fs::read(&path).expect("read");
    fs::write(&path, &contents[..contents.len() - 1]).expect("strip delimiter");
    let before = fs::read(&path).expect("read");

    let (info, _) = list::inspect(Path::new(&path)).expect("inspect");
    assert_eq!(info.message_count, 1);
    assert_eq!(fs::read(&path).expect("read"), before);
}

#[test]
fn inspect_rejects_symlink() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let link = temp.join("link.jsonl");
    std::os::unix::fs::symlink(&path, &link).expect("symlink");
    let error = list::inspect(&link).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(error.to_string().contains("session file is a symlink"));
}
