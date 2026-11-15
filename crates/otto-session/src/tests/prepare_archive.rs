//! Prepare, activate, archive and context-record tests.

use super::*;

// ---------------------------------------------------------------------------
// prepared
// ---------------------------------------------------------------------------

#[test]
fn prepare_does_not_mutate_before_activation_and_transfers_ownership() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&tool_call("call-1", "read"))
        .expect("append call");
    let path = store.path();
    store.close().expect("close");
    let before = fs::read(&path).expect("read");

    let prepared = Prepared::prepare(Path::new(&path), None).expect("prepare");
    assert_eq!(prepared.info().id, "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(
        fs::read(&path).expect("read"),
        before,
        "prepare must not write"
    );

    let (activated, warnings) = prepared.activate().expect("activate");
    assert_eq!(
        warnings.len(),
        1,
        "activation performs the documented repair"
    );
    assert_eq!(activated.messages().len(), 2);
    assert_ne!(fs::read(&path).expect("read"), before);
    activated.close().expect("close");
}

#[test]
fn prepared_activate_rejects_path_replacement_without_mutation() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let prepared = Prepared::prepare(Path::new(&path), None).expect("prepare");

    let other = temp.join("other.jsonl");
    fs::copy(&path, &other).expect("copy");
    fs::rename(&other, &path).expect("replace");
    let before = fs::read(&path).expect("read");

    let error = prepared.activate().expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(
        error
            .to_string()
            .contains("prepared session path identity changed before activation"),
        "{error}"
    );
    assert_eq!(fs::read(&path).expect("read"), before);
}

#[test]
fn prepared_close_is_idempotent_and_safe_after_activation() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let prepared = Prepared::prepare(Path::new(&path), None).expect("prepare");
    prepared.close().expect("close");
    prepared.close().expect("close again");
    let error = prepared.activate().expect_err("must reject");
    assert!(
        error
            .to_string()
            .contains("prepared session is no longer available"),
        "{error}"
    );
}

#[test]
fn prepare_rejects_symlink() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let link = temp.join("link.jsonl");
    std::os::unix::fs::symlink(&path, &link).expect("symlink");
    let error = Prepared::prepare(&link, None).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(error.to_string().contains("session file is a symlink"));
}

#[test]
fn prepare_listed_rejects_candidates_outside_the_workspace_directory() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let prepared = Prepared::prepare_listed(
        &root,
        &workspace.to_string_lossy(),
        Path::new(&paths[0]),
        None,
    )
    .expect("prepare listed");
    prepared.close().expect("close");

    let outside = temp.join("outside.jsonl");
    fs::copy(&paths[0], &outside).expect("copy");
    let error = Prepared::prepare_listed(&root, &workspace.to_string_lossy(), &outside, None)
        .expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(
        error
            .to_string()
            .contains("listed session candidate is outside the expected workspace directory"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// archive
// ---------------------------------------------------------------------------

pub(super) const ARCHIVE_LOCK_TEST_ROOT: &str = "OTTO_ARCHIVE_LOCK_TEST_ROOT";
pub(super) const ARCHIVE_LOCK_TEST_WORKSPACE: &str = "OTTO_ARCHIVE_LOCK_TEST_WORKSPACE";
pub(super) const ARCHIVE_LOCK_TEST_PATH: &str = "OTTO_ARCHIVE_LOCK_TEST_PATH";

#[test]
fn archive_rejects_a_session_held_by_another_process() {
    if let (Some(root), Some(workspace), Some(path)) = (
        std::env::var_os(ARCHIVE_LOCK_TEST_ROOT),
        std::env::var_os(ARCHIVE_LOCK_TEST_WORKSPACE),
        std::env::var_os(ARCHIVE_LOCK_TEST_PATH),
    ) {
        let error = archive(
            Path::new(&root),
            &workspace.to_string_lossy(),
            Path::new(&path),
        )
        .expect_err("another process holds the session");
        assert!(error.to_string().contains("already open"), "{error}");
        return;
    }

    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let (store, _) = Store::open(&paths[0]).expect("open active session");
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tests::archive_rejects_a_session_held_by_another_process",
            "--nocapture",
        ])
        .env(ARCHIVE_LOCK_TEST_ROOT, &root)
        .env(ARCHIVE_LOCK_TEST_WORKSPACE, &workspace)
        .env(ARCHIVE_LOCK_TEST_PATH, &paths[0])
        .status()
        .expect("run archive process");
    assert!(status.success(), "archive moved an active session");
    assert!(
        Path::new(&paths[0]).exists(),
        "active source must stay in place"
    );

    store.close().expect("close active session");
    archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0])).expect("archive closed");
}

#[test]
fn archive_moves_active_session_preserving_bytes_and_mode() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let before = fs::read(&paths[0]).expect("read");

    let result =
        archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0])).expect("archive");
    assert_eq!(result.id, "session-0000");
    assert!(result.path.contains("/archive/"));
    assert!(!Path::new(&paths[0]).exists());
    assert_eq!(fs::read(&result.path).expect("read"), before);
    assert_eq!(mode(Path::new(&result.path)), 0o600);
    assert_eq!(
        mode(Path::new(&result.path).parent().expect("parent")),
        0o700
    );
}

/// Writes one child transcript for the parent at `parent` and returns its path.
pub(super) fn seed_child(workspace: &Path, parent: &str) -> PathBuf {
    let mut header = test_header(workspace);
    header.id = "child-0000".into();
    let store = Store::create_child_lazy(parent, "t1-child-0000", header).expect("create child");
    store.append_message(&user("delegated")).expect("append");
    let path = PathBuf::from(store.path());
    store.close().expect("close");
    path
}

#[test]
fn a_child_store_is_written_lazily_beside_its_parent() {
    let temp = TempDir::new();
    let (_, workspace, paths) = seeded_workspace(&temp, 1);
    let mut header = test_header(&workspace);
    header.id = "child-0000".into();
    let want = Path::new(&paths[0])
        .with_extension("")
        .join("t1-child-0000.jsonl");

    let store = Store::create_child_lazy(&paths[0], "t1-child-0000", header).expect("create");
    assert!(!want.exists(), "no file before the first write");
    store.append_message(&user("delegated")).expect("append");
    store.close().expect("close");

    assert_eq!(store.path(), want.to_string_lossy());
    assert_eq!(mode(&want), 0o600);
    assert_eq!(mode(want.parent().expect("parent")), 0o700);
    let text = fs::read_to_string(&want).expect("read");
    let first: serde_json::Value =
        serde_json::from_str(text.lines().next().expect("header")).expect("json");
    assert_eq!(first["parentSession"], paths[0].as_str());
    let (reopened, _) = Store::open(&want).expect("reopen");
    assert_eq!(reopened.messages().len(), 1);
    reopened.close().expect("close");
}

#[test]
fn list_does_not_return_child_transcripts() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    seed_child(&workspace, &paths[0]);

    let listed = list::list(&root, &workspace.to_string_lossy(), "", 10).expect("list");
    let listed: Vec<_> = listed.sessions.iter().map(|s| s.path.clone()).collect();
    assert_eq!(listed, paths);
}

#[test]
fn archive_moves_the_child_transcript_directory() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let child = seed_child(&workspace, &paths[0]);

    let result =
        archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0])).expect("archive");

    assert!(!child.parent().expect("dir").exists());
    let moved = Path::new(&result.path)
        .with_extension("")
        .join(child.file_name().expect("name"));
    assert!(moved.exists(), "{moved:?}");
}

#[test]
fn archive_deletes_the_reminder_sidecar() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let sidecar = Path::new(&paths[0]).with_extension("reminders.json");
    fs::write(&sidecar, b"[]").expect("sidecar");

    let result =
        archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0])).expect("archive");
    assert!(!sidecar.exists());
    let archived = Path::new(&result.path).with_extension("reminders.json");
    assert!(!archived.exists(), "archiving disables outstanding timers");
}

#[test]
fn archive_deletes_the_inbox_sidecar() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let sidecar = Path::new(&paths[0]).with_extension("inbox.json");
    fs::write(&sidecar, b"[]").expect("sidecar");

    let result =
        archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0])).expect("archive");
    assert!(!sidecar.exists());
    let archived = Path::new(&result.path).with_extension("inbox.json");
    assert!(
        !archived.exists(),
        "archiving ends the session, so its queued notifications end with it"
    );
}

#[test]
fn archive_removes_session_from_list_but_keeps_it_resumable() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 2);
    let result =
        archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0])).expect("archive");

    let listed = list::list(&root, &workspace.to_string_lossy(), "", 10).expect("list");
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(listed.sessions[0].path, paths[1]);

    let (store, _) = Store::open(&result.path).expect("open archived");
    assert_eq!(store.messages().len(), 1);
    store.close().expect("close");
}

#[test]
fn archive_refuses_existing_destination() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let key = fsops::workspace_key(&workspace).expect("key");
    let archive_dir = root.join(&key).join("archive");
    fs::create_dir_all(&archive_dir).expect("create archive");
    let basename = Path::new(&paths[0]).file_name().expect("basename");
    fs::write(archive_dir.join(basename), b"existing").expect("write");

    let error = archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0]))
        .expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(
        error
            .to_string()
            .contains("archive destination already exists"),
        "{error}"
    );
    assert!(Path::new(&paths[0]).exists(), "source must stay in place");
}

#[test]
fn archive_reuses_existing_archive_directory() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 2);
    archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0])).expect("archive");
    archive(&root, &workspace.to_string_lossy(), Path::new(&paths[1])).expect("archive");
    let key = fsops::workspace_key(&workspace).expect("key");
    let entries = fs::read_dir(root.join(&key).join("archive"))
        .expect("read archive")
        .count();
    assert_eq!(entries, 2);
}

#[test]
fn archive_rejects_already_archived_path() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let result =
        archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0])).expect("archive");
    let error = archive(&root, &workspace.to_string_lossy(), Path::new(&result.path))
        .expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(
        error
            .to_string()
            .contains("listed session candidate is outside the expected workspace directory"),
        "{error}"
    );
}

#[test]
fn archive_rejects_symlinked_source() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let key = fsops::workspace_key(&workspace).expect("key");
    let real = temp.join("moved.jsonl");
    fs::rename(&paths[0], &real).expect("rename");
    std::os::unix::fs::symlink(&real, &paths[0]).expect("symlink");

    let error = archive(&root, &workspace.to_string_lossy(), Path::new(&paths[0]))
        .expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(
        error.to_string().contains("session file is a symlink"),
        "{error}"
    );
    assert!(!root.join(&key).join("archive").exists());
}

#[test]
fn archive_rejects_recorded_workspace_mismatch() {
    let temp = TempDir::new();
    let (root, workspace, _) = seeded_workspace(&temp, 1);
    let other = temp.join("other-workspace");
    fs::create_dir_all(&other).expect("create other workspace");

    // A session recorded for `other` but stored under `workspace`'s key.
    let key = fsops::workspace_key(&workspace).expect("key");
    let mut header = test_header(&other);
    header.id = "foreign".into();
    let staging = temp.join("staging");
    let store = Store::create(&staging, header).expect("create");
    store.append_message(&user("hello")).expect("append");
    let source = store.path();
    store.close().expect("close");
    let planted = root.join(&key).join("foreign.jsonl");
    fs::copy(&source, &planted).expect("plant");

    let error = archive(&root, &workspace.to_string_lossy(), &planted).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(
        error
            .to_string()
            .contains("session workspace does not match expected workspace"),
        "{error}"
    );
    assert!(planted.exists());
}

#[test]
fn archive_rejects_invalid_pi_session() {
    let temp = TempDir::new();
    let (root, workspace, _) = seeded_workspace(&temp, 1);
    let key = fsops::workspace_key(&workspace).expect("key");
    let broken = root.join(&key).join("broken.jsonl");
    fs::write(&broken, b"not json\n").expect("write");
    let error = archive(&root, &workspace.to_string_lossy(), &broken).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(broken.exists());
}

// ---------------------------------------------------------------------------
// the Session trait
// ---------------------------------------------------------------------------

#[test]
fn store_implements_the_core_session_trait() {
    use otto_core::session::Session;
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let session: &dyn Session = &store;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime
        .block_on(session.append(user("hello")))
        .expect("append through the trait");
    assert_eq!(session.messages().len(), 1);

    let error = runtime
        .block_on(session.append(tool_result("call-1", "read", "orphan")))
        .expect_err("must reject");
    assert!(matches!(
        error,
        otto_core::session::SessionError::Persist(_)
    ));
    store.close().expect("close");
}

// ---------------------------------------------------------------------------
// unsupported formats and role-context messages
// ---------------------------------------------------------------------------

#[test]
fn open_rejects_old_otto_v1_without_mutation() {
    let temp = TempDir::new();
    let path = temp.join("v1.jsonl");
    fs::write(&path, b"{\"type\":\"header\",\"header\":{\"version\":1}}\n").expect("write");
    let before = fs::read(&path).expect("read");
    let text = path.to_string_lossy().into_owned();

    let error = Store::open(&text).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::UnsupportedFormat);
    assert_eq!(fs::read(&path).expect("read"), before);

    let error = Store::read_header(&text).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::UnsupportedFormat);
    assert_eq!(fs::read(&path).expect("read"), before);
}

#[test]
fn store_round_trips_role_context_as_custom_message() {
    for context_type in ["task_notification", "parent_message", "capability_update"] {
        let temp = TempDir::new();
        let (store, _) = new_store(&temp);
        store.append_message(&user("hello")).expect("append");

        let text = "[task-notification] task t1 succeeded\nreport";
        store
            .append_message(&Message {
                role: Role::Context,
                context_type: context_type.into(),
                display: true,
                usage: Some(Usage {
                    input_tokens: 5,
                    ..Usage::default()
                }),
                blocks: vec![Block::text(text)],
                created_at: created_at(),
                ..Message::default()
            })
            .expect("append context");

        let persisted = store.messages();
        assert_eq!(persisted.len(), 2);
        assert_eq!(persisted[1].role, Role::Context);
        assert_eq!(persisted[1].context_type, context_type);
        assert!(persisted[1].display);
        assert_eq!(persisted[1].text(), text);
        assert_eq!(
            persisted[1].usage,
            Some(Usage {
                input_tokens: 5,
                ..Usage::default()
            }),
            "a context message keeps the token usage it carried"
        );

        let path = store.path();
        store.close().expect("close");
        let entry = json_lines(Path::new(&path))
            .into_iter()
            .find(|line| line["type"] == "custom_message")
            .expect("custom_message entry");
        assert_eq!(entry["customType"], context_type);
        assert_eq!(entry["display"], true);
        assert_eq!(entry["content"], text);
        assert_eq!(entry["details"]["otto"]["usage"]["input_tokens"], 5);

        let (reopened, warnings) = Store::open(&path).expect("open");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(reopened.messages(), persisted);
        reopened.close().expect("close");
    }
}

#[test]
fn store_round_trips_context_metadata() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&Message {
            role: Role::Context,
            context_type: "task_notification".into(),
            display: true,
            context_metadata: Some(otto_core::model::ContextMetadata {
                task_id: "t12".into(),
            }),
            blocks: vec![Block::text("report")],
            created_at: created_at(),
            ..Message::default()
        })
        .expect("append");
    let path = store.path();
    store.close().expect("close");

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert!(warnings.is_empty(), "{warnings:?}");
    let messages = reopened.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0]
            .context_metadata
            .as_ref()
            .map(|meta| meta.task_id.as_str()),
        Some("t12")
    );
    reopened.close().expect("close");
}

#[test]
fn store_append_rejects_invalid_role_context() {
    let tool_block = Block {
        block_type: BlockType::ToolCall,
        tool_call_id: "call-1".into(),
        tool_name: "read".into(),
        arguments: Some(serde_json::value::RawValue::from_string("{}".into()).expect("valid JSON")),
        ..Block::default()
    };
    let cases = [
        ("empty context type", "", Block::text("hello")),
        (
            "reserved compaction type",
            "compaction",
            Block::text("hello"),
        ),
        (
            "reserved branch summary type",
            "branch_summary",
            Block::text("hello"),
        ),
        ("tool block", "task_notification", tool_block),
    ];
    for (name, context_type, block) in cases {
        let temp = TempDir::new();
        let (store, _) = new_store(&temp);
        let error = store
            .append_message(&Message {
                role: Role::Context,
                context_type: context_type.into(),
                display: true,
                blocks: vec![block],
                created_at: created_at(),
                ..Message::default()
            })
            .expect_err(name);
        assert_eq!(error.kind(), PiErrorKind::Invalid, "{name}: {error}");
        store.close().expect("close");
    }
}

#[test]
fn store_runtime_update_failure_poisons_further_appends() {
    let temp = TempDir::new();
    let (store, plan) = store_failing_writes_from(&temp, 3);
    let error = store
        .update_runtime(&RuntimeMetadata {
            profile: "local".into(),
            provider: "openai-compatible".into(),
            model: "next-model".into(),
        })
        .expect_err("must fail");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);

    let error = store.append_message(&user("hello")).expect_err("poisoned");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    plan.assert_all_fired();
    store.close().expect("close");
}

#[test]
fn append_rejects_a_record_above_the_entry_cap_before_writing() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let before = fs::read(store.path()).expect("read");
    let mut message = user("x");
    message.blocks[0].text = "x".repeat(otto_core::session::MAX_SESSION_ENTRY_BYTES + 1);

    let error = store.append_message(&message).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::EntryTooLarge);
    assert_eq!(fs::read(store.path()).expect("read"), before);
    assert_eq!(store.messages().len(), 0);
    store.close().expect("close");
}
