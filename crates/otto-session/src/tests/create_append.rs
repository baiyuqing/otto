//! Store creation, header validation, append and close tests.

use super::*;

// ---------------------------------------------------------------------------
// create and header
// ---------------------------------------------------------------------------

#[test]
fn create_writes_pi_v3_header_and_otto_runtime_entry() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let lines = json_lines(Path::new(&store.path()));
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["version"], 3);
    assert_eq!(lines[0]["id"], "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(lines[1]["type"], "custom");
    assert_eq!(lines[1]["customType"], "otto.runtime");
    assert_eq!(lines[1]["data"]["provider"], "openai-compatible");
    assert_eq!(lines[1]["data"]["model"], "test-model");
    assert_eq!(lines[1]["data"]["profile"], "local");
    assert!(lines[1]["parentId"].is_null());
    store.close().expect("close");
}

#[test]
fn create_uses_expected_permissions_and_path() {
    let temp = TempDir::new();
    let canonical = temp.join("project");
    fs::create_dir_all(&canonical).expect("create project");
    let link = temp.join("workspace-link");
    std::os::unix::fs::symlink(&canonical, &link).expect("symlink");

    let root = temp.join("root");
    let mut header = test_header(&canonical);
    header.workspace = link.to_string_lossy().into_owned();
    let store = Store::create(&root, header.clone()).expect("create store");

    let resolved = fs::canonicalize(&canonical).expect("canonicalize");
    let key = fsops::workspace_key(&resolved).expect("workspace key");
    let want = root.join(key).join(format!("{}.jsonl", header.id));
    assert_eq!(store.path(), want.to_string_lossy());
    assert_eq!(mode(want.parent().expect("parent")), 0o700);
    assert_eq!(mode(&want), 0o600);
    assert_close_on_exec(&store);
    store.close().expect("close");
}

pub(super) const SESSION_FD_INHERIT_TEST_CHILD: &str = "OTTO_SESSION_FD_INHERIT_TEST_CHILD";

#[test]
fn session_file_descriptors_are_not_inherited_by_spawned_children() {
    if std::env::var_os(SESSION_FD_INHERIT_TEST_CHILD).is_some() {
        std::thread::sleep(std::time::Duration::from_millis(300));
        return;
    }

    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create(&root, test_header(&workspace)).expect("create store");
    store.append_message(&user("hello")).expect("append");
    let path = store.path();

    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tests::session_file_descriptors_are_not_inherited_by_spawned_children",
            "--nocapture",
        ])
        .env(SESSION_FD_INHERIT_TEST_CHILD, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child");
    std::thread::sleep(std::time::Duration::from_millis(100));
    store.close().expect("close parent store");

    archive(&root, &workspace.to_string_lossy(), Path::new(&path))
        .expect("child must not inherit the session lock");
    assert!(child.wait().expect("wait child").success());
}

#[test]
fn reopening_a_session_during_spawn_does_not_look_like_another_process() {
    let hold = fsops::SESSION_LOCK_SPAWN_GRACE / 2;
    assert!(
        hold < fsops::SESSION_LOCK_SPAWN_GRACE,
        "the child must release the descriptor before the lock grace expires"
    );
    let secs = libc::time_t::try_from(hold.as_secs()).expect("hold seconds");
    let nanos = libc::c_long::from(hold.subsec_nanos());

    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create(&root, test_header(&workspace)).expect("create store");
    let path = store.path();

    // The child writes one byte from pre_exec, then sleeps while it still
    // holds the inherited session descriptor. Reopening after that byte
    // arrives is the fork/exec window a concurrent spawn leaves behind.
    let mut pipes = [0; 2];
    // SAFETY: pipe stores two fresh descriptors in `pipes`.
    assert_eq!(unsafe { libc::pipe(pipes.as_mut_ptr()) }, 0, "pipe");
    let read_fd = pipes[0];
    let write_fd = pipes[1];

    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let spawner = std::thread::spawn(move || {
        go_rx.recv().expect("start signal");
        let mut command = std::process::Command::new("/usr/bin/true");
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: runs in the forked child before exec. `write` and
        // `nanosleep` are async-signal-safe. The sleep holds the inherited
        // session descriptor open across the parent's close and reopen.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                let marker = 1u8;
                libc::write(write_fd, std::ptr::from_ref(&marker).cast(), 1);
                let request = libc::timespec {
                    tv_sec: secs,
                    tv_nsec: nanos,
                };
                libc::nanosleep(&request, std::ptr::null_mut());
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn");
        assert!(child.wait().expect("wait child").success());
    });

    go_tx.send(()).expect("signal spawner");
    let mut marker = [0u8; 1];
    // SAFETY: `read_fd` is the read end of the pipe above and outlives this call.
    assert_eq!(
        unsafe { libc::read(read_fd, marker.as_mut_ptr().cast(), 1) },
        1,
        "child entered pre_exec"
    );
    // SAFETY: both ends were opened above and are closed once here.
    unsafe {
        libc::close(read_fd);
        libc::close(write_fd);
    }

    store.close().expect("close");
    let (reopened, _) =
        Store::open(&path).expect("reopen while a pre-exec child still holds the descriptor");
    reopened.close().expect("close reopened");
    spawner.join().expect("spawner thread");
}

#[test]
fn create_rejects_invalid_headers() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    type Case = (&'static str, Box<dyn Fn(&mut Header)>, &'static str);
    let cases: Vec<Case> = vec![
        (
            "blank id",
            Box::new(|header| header.id = "   ".into()),
            "session id is invalid",
        ),
        (
            "id with separator",
            Box::new(|header| header.id = "a/b".into()),
            "session id is invalid",
        ),
        (
            "dot id",
            Box::new(|header| header.id = "..".into()),
            "session id is invalid",
        ),
        (
            "blank workspace",
            Box::new(|header| header.workspace = " ".into()),
            "session workspace is required",
        ),
        (
            "blank provider",
            Box::new(|header| header.provider = String::new()),
            "session provider is required",
        ),
        (
            "blank model",
            Box::new(|header| header.model = String::new()),
            "session model is required",
        ),
        (
            "zero timestamp",
            Box::new(|header| header.created_at = otto_core::model::zero_time()),
            "session timestamp is required",
        ),
    ];
    for (name, mutate, want) in cases {
        let mut header = test_header(&workspace);
        mutate(&mut header);
        let error = Store::create(&root, header).expect_err(name);
        assert_eq!(error.kind(), PiErrorKind::Invalid, "{name}");
        assert!(error.to_string().contains(want), "{name}: {error}");
    }
}

#[test]
fn create_rejects_timestamp_outside_rfc3339_nano_range() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let mut header = test_header(&workspace);
    header.created_at = Utc
        .with_ymd_and_hms(12345, 1, 1, 0, 0, 0)
        .single()
        .expect("valid date");
    let error = Store::create(temp.join("root"), header).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(
        error
            .to_string()
            .contains("session timestamp is outside the RFC3339Nano range"),
        "{error}"
    );
}

#[test]
fn read_header_derives_otto_runtime_metadata() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .update_runtime(&RuntimeMetadata {
            profile: "work".into(),
            provider: "chatgpt".into(),
            model: "gpt-5".into(),
        })
        .expect("update runtime");
    let path = store.path();
    store.close().expect("close");

    let header = Store::read_header(&path).expect("read header");
    assert_eq!(header.version, CURRENT_VERSION);
    assert_eq!(header.provider, "chatgpt");
    assert_eq!(header.model, "gpt-5");
    assert_eq!(header.profile, "work");
    assert_eq!(header.created_at, created_at());
}

// ---------------------------------------------------------------------------
// lazy creation
// ---------------------------------------------------------------------------

#[test]
fn create_lazy_defers_file_until_first_append() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create_lazy(&root, test_header(&workspace)).expect("create lazy");
    assert_eq!(store.path(), "");
    assert!(!root.exists());

    store.append_message(&user("hello")).expect("append");
    let path = store.path();
    assert!(!path.is_empty());
    assert_eq!(json_lines(Path::new(&path)).len(), 3);
    store.append_message(&user("again")).expect("append");
    assert_eq!(
        json_lines(Path::new(&path)).len(),
        4,
        "the published file is the open journal"
    );
    let names: Vec<_> = fs::read_dir(Path::new(&path).parent().expect("session dir"))
        .expect("read session dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(
        names,
        [Path::new(&path).file_name().expect("name")],
        "no temporary name is left"
    );
    store.close().expect("close");
}

#[test]
fn create_lazy_close_without_write_leaves_no_file() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create_lazy(&root, test_header(&workspace)).expect("create lazy");
    store.close().expect("close");
    assert!(!root.exists());
}

#[test]
fn create_lazy_round_trips_through_open() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let store =
        Store::create_lazy(temp.join("root"), test_header(&workspace)).expect("create lazy");
    store.append_message(&user("hello")).expect("append");
    store.append_message(&assistant("hi")).expect("append");
    let path = store.path();
    store.close().expect("close");

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert!(warnings.is_empty(), "{warnings:?}");
    let messages = reopened.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text(), "hello");
    assert_eq!(messages[1].text(), "hi");
    reopened.close().expect("close");
}

// ---------------------------------------------------------------------------
// append and round trip
// ---------------------------------------------------------------------------

#[test]
fn store_round_trips_pi_messages_and_parent_chain() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("hello")).expect("append user");
    store
        .append_message(&tool_call("call-1", "read"))
        .expect("append call");
    store
        .append_message(&tool_result("call-1", "read", "done"))
        .expect("append result");
    let path = store.path();
    store.close().expect("close");

    let lines = json_lines(Path::new(&path));
    assert_eq!(lines.len(), 5);
    let mut parent = lines[1]["id"].as_str().expect("runtime id").to_owned();
    for line in &lines[2..] {
        assert_eq!(line["parentId"], parent, "parent chain");
        parent = line["id"].as_str().expect("entry id").to_owned();
    }

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert!(warnings.is_empty(), "{warnings:?}");
    let messages = reopened.messages();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[1].blocks[0].tool_call_id, "call-1");
    assert_eq!(messages[2].role, Role::Tool);
    reopened.close().expect("close");
}

#[test]
fn append_custom_entry_is_excluded_from_messages() {
    // otto.task_spec and otto.task_result (crates/otto/src/subagent/runner.rs)
    // are custom entries, the same mechanism as otto.runtime. This asserts
    // that mechanism keeps them out of messages() the same way it already
    // keeps otto.runtime out, so a reader building context over a child
    // transcript sees the same messages with or without them.
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("hello")).expect("append user");
    store
        .append_custom_entry("otto.task_spec", "{\"id\":\"t1\"}")
        .expect("append task_spec");
    store
        .append_message(&assistant("done"))
        .expect("append assistant");
    store
        .append_custom_entry("otto.task_result", "{\"status\":\"succeeded\"}")
        .expect("append task_result");
    let path = store.path();
    store.close().expect("close");

    let lines = json_lines(Path::new(&path));
    assert_eq!(lines.len(), 6, "header, otto.runtime, and the four appends");
    assert_eq!(lines[3]["customType"], "otto.task_spec");
    assert_eq!(lines[5]["customType"], "otto.task_result");

    let (reopened, warnings) = Store::open(&path).expect("open");
    assert!(warnings.is_empty(), "{warnings:?}");
    let messages = reopened.messages();
    assert_eq!(messages.len(), 2, "custom entries produce no messages");
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[1].role, Role::Assistant);
    reopened.close().expect("close");
}

#[test]
fn append_empty_tool_result_persists_text_field() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store
        .append_message(&tool_call("call-1", "read"))
        .expect("append call");
    store
        .append_message(&tool_result("call-1", "read", ""))
        .expect("append result");
    let path = store.path();
    store.close().expect("close");

    let (reopened, _) = Store::open(&path).expect("open");
    let messages = reopened.messages();
    assert_eq!(messages[1].blocks.len(), 1);
    assert_eq!(messages[1].blocks[0].text, "");
    reopened.close().expect("close");
}

#[test]
fn append_rejects_invalid_messages_without_mutation() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let before = fs::metadata(store.path()).expect("stat").len();

    let invalid = Message {
        role: Role::User,
        blocks: Vec::new(),
        created_at: created_at(),
        ..Message::default()
    };
    let error = store.append_message(&invalid).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert_eq!(store.messages().len(), 0);
    assert_eq!(fs::metadata(store.path()).expect("stat").len(), before);
    store.close().expect("close");
}

#[test]
fn append_rejects_out_of_order_tool_results_without_mutation() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let before = fs::metadata(store.path()).expect("stat").len();
    let error = store
        .append_message(&tool_result("call-1", "read", "done"))
        .expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert_eq!(store.messages().len(), 0);
    assert_eq!(fs::metadata(store.path()).expect("stat").len(), before);
    store.close().expect("close");
}

#[test]
fn append_rejects_timestamp_outside_rfc3339_nano_range() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let mut message = user("hello");
    message.created_at = Utc
        .with_ymd_and_hms(12345, 1, 1, 0, 0, 0)
        .single()
        .expect("valid date");
    let error = store.append_message(&message).expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(
        error
            .to_string()
            .contains("message timestamp is outside the RFC3339Nano range"),
        "{error}"
    );
    store.close().expect("close");
}

#[test]
fn messages_returns_independent_slices() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("hello")).expect("append");
    let mut taken = store.messages();
    taken[0].blocks[0].text = "mutated".into();
    assert_eq!(store.messages()[0].text(), "hello");
    store.close().expect("close");
}

#[test]
fn store_preserves_explicit_zero_assistant_usage_presence() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("hello")).expect("append");
    store
        .append_message(&assistant_with_usage("hi", Usage::default()))
        .expect("append");
    let (usage, present) = store.aggregate_usage();
    assert_eq!(usage, Usage::default());
    assert!(present, "explicit zero usage must stay present");
    let path = store.path();
    store.close().expect("close");

    let (reopened, _) = Store::open(&path).expect("open");
    let (usage, present) = reopened.aggregate_usage();
    assert_eq!(usage, Usage::default());
    assert!(present, "explicit zero usage must survive a reopen");
    reopened.close().expect("close");
}

#[test]
fn store_aggregates_usage_across_assistant_appends() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("one")).expect("append");
    store
        .append_message(&assistant_with_usage(
            "a",
            Usage {
                input_tokens: 10,
                output_tokens: 4,
                cached_input_tokens: 2,
                ..Default::default()
            },
        ))
        .expect("append");
    store.append_message(&user("two")).expect("append");
    store
        .append_message(&assistant_with_usage(
            "b",
            Usage {
                input_tokens: 5,
                output_tokens: 1,
                cached_input_tokens: 0,
                ..Default::default()
            },
        ))
        .expect("append");
    let (usage, present) = store.aggregate_usage();
    assert!(present);
    assert_eq!(
        usage,
        Usage {
            input_tokens: 15,
            output_tokens: 5,
            cached_input_tokens: 2,
            ..Default::default()
        }
    );
    store.close().expect("close");
}

// ---------------------------------------------------------------------------
// runtime and rename
// ---------------------------------------------------------------------------

#[test]
fn store_update_runtime_persists_change_and_noops_unchanged() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let before = fs::metadata(store.path()).expect("stat").len();
    store
        .update_runtime(&RuntimeMetadata {
            profile: "local".into(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
        })
        .expect("no-op update");
    assert_eq!(fs::metadata(store.path()).expect("stat").len(), before);

    store
        .update_runtime(&RuntimeMetadata {
            profile: "local".into(),
            provider: "openai-compatible".into(),
            model: "other-model".into(),
        })
        .expect("update");
    assert_eq!(store.header().model, "other-model");
    let path = store.path();
    let lines = json_lines(Path::new(&path));
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2]["customType"], "otto.runtime");
    assert_eq!(lines[2]["data"]["model"], "other-model");
    store.close().expect("close");

    assert_eq!(
        Store::read_header(&path).expect("read").model,
        "other-model"
    );
}

#[test]
fn store_update_runtime_rejects_blank_provider_or_model() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    for runtime in [
        RuntimeMetadata {
            profile: String::new(),
            provider: "  ".into(),
            model: "m".into(),
        },
        RuntimeMetadata {
            profile: String::new(),
            provider: "p".into(),
            model: String::new(),
        },
    ] {
        let error = store.update_runtime(&runtime).expect_err("must reject");
        assert_eq!(error.kind(), PiErrorKind::Invalid);
        assert!(
            error
                .to_string()
                .contains("runtime provider and model are required"),
            "{error}"
        );
    }
    store.close().expect("close");
}

#[test]
fn store_rename_session_appends_session_info() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    assert_eq!(store.name(), "");
    store.rename("  Release notes  ").expect("rename");
    assert_eq!(store.name(), "Release notes");
    let path = store.path();
    let lines = json_lines(Path::new(&path));
    assert_eq!(lines[2]["type"], "session_info");
    assert_eq!(lines[2]["name"], "Release notes");
    store.close().expect("close");

    let (reopened, _) = Store::open(&path).expect("open");
    assert_eq!(reopened.name(), "Release notes");
    reopened.close().expect("close");
}

#[test]
fn store_rename_session_rejects_blank_name() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    let before = fs::metadata(store.path()).expect("stat").len();
    let error = store.rename("   ").expect_err("must reject");
    assert_eq!(error.kind(), PiErrorKind::Invalid);
    assert!(error.to_string().contains("session name is required"));
    assert_eq!(fs::metadata(store.path()).expect("stat").len(), before);
    store.close().expect("close");
}

// ---------------------------------------------------------------------------
// close and poisoning
// ---------------------------------------------------------------------------

#[test]
fn store_append_after_close_fails() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.close().expect("close");
    store.close().expect("close is idempotent");
    for error in [
        store.append_message(&user("hello")).expect_err("append"),
        store.rename("name").expect_err("rename"),
        store
            .update_runtime(&RuntimeMetadata {
                profile: String::new(),
                provider: "p".into(),
                model: "m".into(),
            })
            .expect_err("runtime"),
    ] {
        assert_eq!(error.kind(), PiErrorKind::Closed);
        assert_eq!(error.to_string(), "session is closed");
    }
}

#[test]
fn store_poisoned_after_durable_append_failure() {
    let temp = TempDir::new();
    let (store, plan) = store_failing_writes_from(&temp, 4);
    store.append_message(&user("first")).expect("append");

    let error = store
        .append_message(&user("second"))
        .expect_err("must fail");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert_eq!(store.messages().len(), 1);

    let error = store.append_message(&user("third")).expect_err("poisoned");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    let error = store.rename("name").expect_err("poisoned");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert_eq!(store.messages().len(), 1);
    plan.assert_all_fired();
    store.close().expect("close");
}

#[test]
fn write_pi_record_uses_lf_and_sync() {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    store.append_message(&user("hello")).expect("append");
    let contents = fs::read(store.path()).expect("read");
    assert_eq!(contents.last(), Some(&b'\n'));
    assert!(!contents.windows(2).any(|pair| pair == b"\r\n"));
    assert_eq!(contents.iter().filter(|byte| **byte == b'\n').count(), 3);
    store.close().expect("close");
}
