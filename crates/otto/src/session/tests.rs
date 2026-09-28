//! Native session tests.
//!
//! Every test is offline and deterministic and works inside its own directory
//! under [`std::env::temp_dir`], removed when the guard drops. No `tempfile`
//! dependency: a unique name plus a `Drop` guard is fifteen lines.

// `set_modified` calls `utimes`, the only way to age a file without sleeping.
#![allow(unsafe_code)]

use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, TimeZone, Utc};
use otto_core::model::{
    Block, BlockType, EffectCertainty, FinishReason, Message, OperationDisposition, OperationId,
    OperationOutcome, OperationStopReason, Role, ToolResultMetadata, Usage,
};
use otto_core::session::context::{MAY_HAVE_RUN_TOOL_RESULT_TEXT, NOT_EXECUTED_TOOL_RESULT_TEXT};
use otto_core::session::{
    CURRENT_VERSION, CompactionCheckpoint, CompactionDetails, Header, OPERATION_CUSTOM_TYPE,
    OperationFact, PiErrorKind, RuntimeMetadata,
};

use super::fsops;
use super::list::{self, MAX_LIST_SESSIONS};
use super::prepared::{Prepared, archive};
use super::store::Store;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A directory under the system temp directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "otto-session-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create temp directory");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn created_at() -> DateTime<Utc> {
    Utc.timestamp_opt(1, 0).single().expect("valid timestamp")
}

fn test_header(workspace: &Path) -> Header {
    Header {
        version: CURRENT_VERSION,
        id: "550e8400-e29b-41d4-a716-446655440000".into(),
        workspace: workspace.to_string_lossy().into_owned(),
        provider: "openai-compatible".into(),
        profile: "local".into(),
        model: "test-model".into(),
        created_at: created_at(),
    }
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![Block::text(text)],
        created_at: created_at(),
        ..Message::default()
    }
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block::text(text)],
        created_at: created_at(),
        finish_reason: Some(FinishReason::Stop),
        ..Message::default()
    }
}

fn assistant_with_usage(text: &str, usage: Usage) -> Message {
    Message {
        usage: Some(usage),
        ..assistant(text)
    }
}

fn tool_call(id: &str, name: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: id.into(),
            tool_name: name.into(),
            arguments: Some(
                serde_json::value::RawValue::from_string("{}".into()).expect("valid JSON"),
            ),
            ..Block::default()
        }],
        created_at: created_at(),
        finish_reason: Some(FinishReason::ToolCalls),
        ..Message::default()
    }
}

fn tool_calls(calls: &[(&str, &str)]) -> Message {
    Message {
        role: Role::Assistant,
        blocks: calls
            .iter()
            .map(|(id, name)| Block {
                block_type: BlockType::ToolCall,
                tool_call_id: (*id).into(),
                tool_name: (*name).into(),
                arguments: Some(
                    serde_json::value::RawValue::from_string("{}".into()).expect("valid JSON"),
                ),
                ..Block::default()
            })
            .collect(),
        created_at: created_at(),
        finish_reason: Some(FinishReason::ToolCalls),
        ..Message::default()
    }
}

fn tool_result(id: &str, name: &str, text: &str) -> Message {
    Message {
        role: Role::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            text: text.into(),
            tool_call_id: id.into(),
            tool_name: name.into(),
            ..Block::default()
        }],
        created_at: created_at(),
        ..Message::default()
    }
}

fn interrupted_outcome() -> OperationOutcome {
    OperationOutcome {
        disposition: OperationDisposition::Interrupted,
        effect_certainty: EffectCertainty::Unknown,
        stop_reason: Some(OperationStopReason::ProcessLost),
    }
}

fn known_no_effect_error_outcome() -> OperationOutcome {
    OperationOutcome {
        disposition: OperationDisposition::Error,
        effect_certainty: EffectCertainty::KnownNoEffect,
        stop_reason: None,
    }
}

fn operation_id(value: &str) -> OperationId {
    OperationId::new(value).expect("valid operation id")
}

fn result_metadata(operation_id: Option<&str>, outcome: OperationOutcome) -> ToolResultMetadata {
    ToolResultMetadata {
        operation_id: operation_id
            .map(|value| OperationId::new(value).expect("valid operation id")),
        disposition: outcome.disposition,
        effect_certainty: outcome.effect_certainty,
        stop_reason: outcome.stop_reason,
    }
}

/// A store over its own root, with the workspace inside the same temp tree.
fn new_store(temp: &TempDir) -> (Store, PathBuf) {
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create(&root, test_header(&workspace)).expect("create store");
    (store, root)
}

fn json_lines(path: &Path) -> Vec<serde_json::Value> {
    let contents = fs::read_to_string(path).expect("read session file");
    assert!(contents.ends_with('\n'), "file is not LF terminated");
    contents
        .trim_end_matches('\n')
        .split('\n')
        .map(|line| serde_json::from_str(line).expect("valid JSON line"))
        .collect()
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

fn assert_close_on_exec(store: &Store) {
    let guard = store.lock().expect("lock store");
    let file = guard.file.as_ref().expect("open session file");
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0, "fcntl(F_GETFD) failed");
    assert_ne!(flags & libc::FD_CLOEXEC, 0, "session fd must be CLOEXEC");
}

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
    let key = fsops::workspace_key_of_canonical(&resolved.to_string_lossy());
    let want = root.join(key).join(format!("{}.jsonl", header.id));
    assert_eq!(store.path(), want.to_string_lossy());
    assert_eq!(mode(want.parent().expect("parent")), 0o700);
    assert_eq!(mode(&want), 0o600);
    assert_close_on_exec(&store);
    store.close().expect("close");
}

const SESSION_FD_INHERIT_TEST_CHILD: &str = "OTTO_SESSION_FD_INHERIT_TEST_CHILD";

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
            "session::tests::session_file_descriptors_are_not_inherited_by_spawned_children",
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
            cached_input_tokens: 2
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
    let (store, _) = new_store(&temp);
    store.append_message(&user("first")).expect("append");
    store.lock().expect("lock").fail_writes = true;

    let error = store
        .append_message(&user("second"))
        .expect_err("must fail");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert_eq!(store.messages().len(), 1);

    store.lock().expect("lock").fail_writes = false;
    let error = store.append_message(&user("third")).expect_err("poisoned");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    let error = store.rename("name").expect_err("poisoned");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert_eq!(store.messages().len(), 1);
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

// ---------------------------------------------------------------------------
// open-time repair
// ---------------------------------------------------------------------------

/// Appends raw bytes to a closed session file.
fn append_raw(path: &str, bytes: &[u8]) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open for append");
    file.write_all(bytes).expect("append raw bytes");
}

fn seeded_session(temp: &TempDir) -> String {
    let (store, _) = new_store(temp);
    store.append_message(&user("hello")).expect("append");
    let path = store.path();
    store.close().expect("close");
    path
}

const SESSION_LOCK_TEST_PATH: &str = "OTTO_SESSION_LOCK_TEST_PATH";

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
            "session::tests::open_rejects_a_session_held_by_another_process",
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
    let (store, _) = new_store(&temp);
    let before = fs::read(store.path()).expect("read before");
    store.lock().expect("lock").fail_writes = true;
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

// ---------------------------------------------------------------------------
// compaction
// ---------------------------------------------------------------------------

fn checkpoint(first_kept: &str) -> CompactionCheckpoint {
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
fn message_entry_id(path: &str, index: usize) -> String {
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
    let (store, _) = new_store(&temp);
    store.append_message(&user("one")).expect("append");
    let anchor = message_entry_id(&store.path(), 1);
    let before = store.messages();
    store.lock().expect("lock").fail_writes = true;

    let error = store
        .append_compaction(&checkpoint(&anchor))
        .expect_err("must fail");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert_eq!(store.messages(), before);
    assert!(store.latest_compaction().is_none());
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
fn seeded_workspace(temp: &TempDir, count: usize) -> (PathBuf, PathBuf, Vec<String>) {
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
fn set_modified(path: &Path, seconds: i64) {
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

const ARCHIVE_LOCK_TEST_ROOT: &str = "OTTO_ARCHIVE_LOCK_TEST_ROOT";
const ARCHIVE_LOCK_TEST_WORKSPACE: &str = "OTTO_ARCHIVE_LOCK_TEST_WORKSPACE";
const ARCHIVE_LOCK_TEST_PATH: &str = "OTTO_ARCHIVE_LOCK_TEST_PATH";

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
            "session::tests::archive_rejects_a_session_held_by_another_process",
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
fn seed_child(workspace: &Path, parent: &str) -> PathBuf {
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
    for context_type in ["task_notification", "parent_message"] {
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
        assert_eq!(persisted[1].usage, None, "context usage is dropped");

        let path = store.path();
        store.close().expect("close");
        let entry = json_lines(Path::new(&path))
            .into_iter()
            .find(|line| line["type"] == "custom_message")
            .expect("custom_message entry");
        assert_eq!(entry["customType"], context_type);
        assert_eq!(entry["display"], true);
        assert_eq!(entry["content"], text);

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
    let (store, _) = new_store(&temp);
    store.lock().expect("lock").fail_writes = true;
    let error = store
        .update_runtime(&RuntimeMetadata {
            profile: "local".into(),
            provider: "openai-compatible".into(),
            model: "next-model".into(),
        })
        .expect_err("must fail");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);

    store.lock().expect("lock").fail_writes = false;
    let error = store.append_message(&user("hello")).expect_err("poisoned");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
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

// ---------------------------------------------------------------------------
// listing edge cases
// ---------------------------------------------------------------------------

/// Writes raw JSONL records into the workspace-key directory under `root`.
fn plant_records(root: &Path, workspace: &Path, name: &str, records: &[&str]) -> PathBuf {
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

// ---------------------------------------------------------------------------
// session failover lease
//
// Test safety: `Lease::acquire` (used by `Prepared::prepare`/`prepare_listed`
// and `Store::enable_failover`) registers with the process-wide
// `failover::lease::Keeper`, whose watchdog calls `libc::_exit(75)` on a
// lease this process holds if it goes more than 5L/6 without renewal or is
// superseded. Every test below that ends up holding such a lease uses
// `lease_seconds` (`L`) >= 6 and releases (`close`) it within milliseconds of
// acquiring, well inside the 5L/6 margin. A "stopped holder" is simulated by
// writing `epoch-<n>` and `heartbeat` by hand, with no live `Lease` of our
// own; a "running holder" is simulated with a thread that rewrites the
// heartbeat file every 200ms, never with a second live lease.
// ---------------------------------------------------------------------------

/// The JSON+padding+newline byte layout `failover::lease` writes to a lease
/// directory's `heartbeat` file, reproduced here to simulate a holder by
/// hand.
fn lease_heartbeat_bytes(epoch: u64, host: &str, pid: u32, released: bool, seq: u64) -> Vec<u8> {
    let json = serde_json::json!({
        "epoch": epoch,
        "host": host,
        "pid": pid,
        "released": released,
        "seq": seq,
    })
    .to_string();
    let mut bytes = vec![b' '; 512];
    bytes[..json.len()].copy_from_slice(json.as_bytes());
    bytes[511] = b'\n';
    bytes
}

/// Writes a lease directory by hand -- `lease.json`, one `epoch-<epoch>`
/// marker and an initial `heartbeat` -- without acquiring a live [`Lease`],
/// to simulate a holder another host left behind. Returns the directory.
fn write_lease_dir_by_hand(
    session_path: &Path,
    lease_seconds: u64,
    epoch: u64,
    host: &str,
    pid: u32,
    released: bool,
) -> PathBuf {
    let dir = session_path.with_extension("lease");
    fs::create_dir_all(&dir).expect("create lease dir by hand");
    fs::write(
        dir.join("lease.json"),
        serde_json::json!({ "lease_seconds": lease_seconds }).to_string(),
    )
    .expect("write lease.json by hand");
    fs::write(dir.join(format!("epoch-{epoch}")), b"{}").expect("write epoch marker by hand");
    fs::write(
        dir.join("heartbeat"),
        lease_heartbeat_bytes(epoch, host, pid, released, 1),
    )
    .expect("write heartbeat by hand");
    dir
}

/// Reads a lease directory's `heartbeat` file and parses its JSON prefix.
fn read_lease_heartbeat(dir: &Path) -> serde_json::Value {
    let bytes = fs::read(dir.join("heartbeat")).expect("read heartbeat");
    let text = String::from_utf8_lossy(&bytes);
    serde_json::from_str(text.trim_end()).expect("parse heartbeat")
}

// R1 (`[failover]` config resolution) is covered by
// `otto_core::config::failover`'s own tests, not here.

// R2: lease-managed open.

#[test]
fn prepare_fails_when_a_lease_managed_sessions_holder_is_still_renewing() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let session_path = Path::new(&path);
    // `L` = 2: this process's own acquire fails (`Held`), so it never holds
    // a live lease and a short `L` is safe here (see the section header).
    let dir = write_lease_dir_by_hand(session_path, 2, 1, "holder-host", 4242, false);

    let heartbeat_path = dir.join("heartbeat");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_stop = std::sync::Arc::clone(&stop);
    let writer = std::thread::spawn(move || {
        let mut seq = 2u64;
        while !writer_stop.load(Ordering::Relaxed) {
            fs::write(
                &heartbeat_path,
                lease_heartbeat_bytes(1, "holder-host", 4242, false, seq),
            )
            .expect("rewrite heartbeat");
            seq += 1;
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });

    let error = Prepared::prepare(session_path, None).expect_err("holder is still live");

    stop.store(true, Ordering::Relaxed);
    writer.join().expect("join heartbeat writer");

    let text = error.to_string();
    assert!(text.contains("holder-host"), "{text}");
    assert!(text.contains("4242"), "{text}");
}

#[test]
fn prepare_with_no_create_lease_and_no_lease_directory_creates_none() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let session_path = Path::new(&path);

    let prepared = Prepared::prepare(session_path, None).expect("prepare");
    let (store, _warnings) = prepared.activate().expect("activate");
    assert!(store.lease().is_none());
    assert!(
        !session_path.with_extension("lease").exists(),
        "no lease directory should be created"
    );
    store.close().expect("close");
}

#[test]
fn prepare_with_create_lease_creates_a_lease_directory_for_an_existing_session() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let session_path = Path::new(&path);
    assert!(!crate::failover::lease::is_lease_managed(session_path).expect("check lease managed"));

    let prepared = Prepared::prepare(session_path, Some(6)).expect("prepare");
    let (store, _warnings) = prepared.activate().expect("activate");

    let lease_dir = session_path.with_extension("lease");
    assert!(lease_dir.join("lease.json").is_file());
    assert!(lease_dir.join("epoch-1").is_file());
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(lease_dir.join("lease.json")).expect("read lease.json"),
    )
    .expect("parse lease.json");
    assert_eq!(meta["lease_seconds"], 6);

    store.close().expect("close");
}

#[test]
fn prepare_honors_lease_seconds_already_recorded_in_an_existing_lease_directory() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let session_path = Path::new(&path);
    crate::failover::lease::create_lease_dir(session_path, 30).expect("create lease dir");

    let prepared = Prepared::prepare(session_path, Some(40)).expect("prepare");
    let (store, _warnings) = prepared.activate().expect("activate");
    let lease = store.lease().expect("lease-managed store");
    assert_eq!(lease.duration(), std::time::Duration::from_secs(30));

    store.close().expect("close");
}

// R3: `Prepared` carries the acquired lease; closing without activating
// releases it.

#[test]
fn prepared_close_of_a_lease_managed_session_releases_the_lease_for_the_next_open() {
    let temp = TempDir::new();
    let path = seeded_session(&temp);
    let session_path = Path::new(&path);

    let prepared = Prepared::prepare(session_path, Some(6)).expect("prepare");
    prepared.close().expect("close without activating");

    let lease_dir = session_path.with_extension("lease");
    let heartbeat = read_lease_heartbeat(&lease_dir);
    assert_eq!(heartbeat["released"], true);

    let started = std::time::Instant::now();
    let prepared2 = Prepared::prepare(session_path, None).expect("reopen");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "a released epoch must be taken over without waiting"
    );
    let (store2, _warnings) = prepared2.activate().expect("activate");
    assert!(
        store2.take_takeover().is_none(),
        "a fresh acquire records no takeover"
    );
    store2.close().expect("close");
}

// R4: `Store` holds the lease and checks it before every durable write.

#[test]
fn a_write_after_the_lease_is_marked_lost_fails_and_poisons_the_store_without_growing_the_file() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    let path = store.path();
    let dir = Path::new(&path).with_extension("lease");
    fs::create_dir_all(&dir).expect("create lease dir");
    let lease = crate::failover::lease::Lease::for_test(&dir);
    {
        let mut state = store.lock().expect("lock store");
        state.lease = Some(lease.clone());
        state.owns_lease = true;
    }
    lease.mark_lost_for_test("test induced loss");

    let before = fs::metadata(&path).expect("stat").len();
    let error = store
        .append_message(&user("hello"))
        .expect_err("write must fail once the lease is lost");
    assert!(error.to_string().contains("session lease lost"), "{error}");

    let error2 = store
        .append_message(&user("again"))
        .expect_err("store stays poisoned");
    assert_eq!(
        error.to_string(),
        error2.to_string(),
        "a poisoned store returns the same error on every later write"
    );

    let after = fs::metadata(&path).expect("stat").len();
    assert_eq!(
        before, after,
        "a failed write must not change the file length"
    );
}

#[test]
fn close_of_an_owned_lease_writes_a_released_heartbeat() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    let path = store.path();
    let dir = Path::new(&path).with_extension("lease");
    fs::create_dir_all(&dir).expect("create lease dir");
    let lease = crate::failover::lease::Lease::for_test(&dir);
    {
        let mut state = store.lock().expect("lock store");
        state.lease = Some(lease);
        state.owns_lease = true;
    }

    store.close().expect("close");

    let heartbeat = read_lease_heartbeat(&dir);
    assert_eq!(heartbeat["released"], true);
}

#[test]
fn close_of_a_check_only_lease_does_not_release_it() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    let path = store.path();
    let dir = Path::new(&path).with_extension("lease");
    fs::create_dir_all(&dir).expect("create lease dir");
    let lease = crate::failover::lease::Lease::for_test(&dir);
    store.set_lease_check(lease);

    let before = fs::read(dir.join("heartbeat")).expect("read heartbeat");
    store.close().expect("close");
    let after = fs::read(dir.join("heartbeat")).expect("read heartbeat");
    assert_eq!(
        before, after,
        "close must not write a heartbeat for a lease this store does not own"
    );
}

// R5: a takeover from a stopped holder is recorded on the store and the
// dangling calls it left are repaired.

#[test]
fn takeover_through_prepare_records_the_holder_and_repairs_the_dangling_calls() {
    let temp = TempDir::new();
    let (store, _root) = new_store(&temp);
    store
        .append_message(&tool_calls(&[("call-1", "read"), ("call-2", "bash")]))
        .expect("append dangling calls");
    let path = store.path();
    store
        .close()
        .expect("close before simulating a stopped holder");
    let original_bytes = fs::read(&path).expect("read original log");

    let session_path = Path::new(&path);
    // `L` = 6, the minimum for a lease this process ends up holding live
    // (see the section header); the takeover wait is 7L/6 = 7s.
    let dir = write_lease_dir_by_hand(session_path, 6, 1, "stopped-host", 9001, false);

    let prepared = Prepared::prepare(session_path, None).expect("prepare takes over");
    let (activated, _warnings) = prepared.activate().expect("activate");

    let takeover = activated.take_takeover().expect("takeover recorded");
    assert_eq!(takeover.holder.host, "stopped-host");
    assert_eq!(takeover.holder.pid, 9001);
    assert_eq!(takeover.repaired.len(), 2);
    assert_eq!(takeover.repaired[0].name, "read");
    assert!(takeover.repaired[0].may_have_run);
    assert_eq!(takeover.repaired[1].name, "bash");
    assert!(!takeover.repaired[1].may_have_run);

    let (_entries, messages) = Store::read_entries(&path).expect("read repaired log");
    let tool_messages = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .count();
    assert_eq!(
        tool_messages, 2,
        "both dangling calls got synthetic results"
    );

    let fenced = fs::read(dir.join("fenced-1.jsonl")).expect("read fenced copy");
    assert_eq!(
        fenced, original_bytes,
        "the fenced copy holds the original log bytes"
    );

    activated.close().expect("close");
}

// R6: new sessions.

#[test]
fn enable_failover_creates_the_lease_directory_only_at_the_first_append() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create_lazy(&root, test_header(&workspace)).expect("create lazy");
    store.enable_failover(12).expect("enable failover");

    assert!(
        !root.exists(),
        "a lazy store must not touch disk before the first append"
    );

    store.append_message(&user("hello")).expect("first append");

    let path = store.path();
    let dir = Path::new(&path).with_extension("lease");
    assert!(dir.join("lease.json").is_file());
    assert!(dir.join("epoch-1").is_file());
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.join("lease.json")).expect("read lease.json"))
            .expect("parse lease.json");
    assert_eq!(meta["lease_seconds"], 12);

    store.close().expect("close");
}

#[test]
fn a_lazy_store_without_enable_failover_creates_no_lease_directory() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create_lazy(&root, test_header(&workspace)).expect("create lazy");

    store.append_message(&user("hello")).expect("first append");

    let path = store.path();
    let dir = Path::new(&path).with_extension("lease");
    assert!(!dir.exists());
    store.close().expect("close");
}

#[test]
fn two_stores_racing_enable_failover_for_the_same_session_id_leave_exactly_one_lease_directory_and_one_writer()
 {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");

    let store_a = std::sync::Arc::new(
        Store::create_lazy(&root, test_header(&workspace)).expect("create lazy a"),
    );
    let store_b = std::sync::Arc::new(
        Store::create_lazy(&root, test_header(&workspace)).expect("create lazy b"),
    );
    store_a.enable_failover(12).expect("enable failover a");
    store_b.enable_failover(12).expect("enable failover b");

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    let (ba, sa) = (
        std::sync::Arc::clone(&barrier),
        std::sync::Arc::clone(&store_a),
    );
    let thread_a = std::thread::spawn(move || {
        ba.wait();
        sa.append_message(&user("from a"))
    });
    let (bb, sb) = (
        std::sync::Arc::clone(&barrier),
        std::sync::Arc::clone(&store_b),
    );
    let thread_b = std::thread::spawn(move || {
        bb.wait();
        sb.append_message(&user("from b"))
    });

    let result_a = thread_a.join().expect("join a");
    let result_b = thread_b.join().expect("join b");

    let ok_count = [&result_a, &result_b]
        .into_iter()
        .filter(|result| result.is_ok())
        .count();
    assert_eq!(ok_count, 1, "exactly one racing writer must win");

    let winner = if result_a.is_ok() { &store_a } else { &store_b };
    let path = winner.path();
    assert!(!path.is_empty(), "the winner must have created the file");
    // Every line must be complete, LF-terminated JSON: the loser's failed
    // attempt must not have left a half-written record behind.
    let lines = json_lines(Path::new(&path));
    assert_eq!(
        lines.len(),
        3,
        "header, the runtime entry, and the one appended message, nothing else"
    );
    let dir = Path::new(&path).with_extension("lease");
    assert!(dir.join("epoch-1").is_file());

    store_a.close().ok();
    store_b.close().ok();
}

// R7: archive.

/// Creates a lease-managed session with one message and closes it, returning
/// (root, workspace, session path).
fn seeded_lease_managed_session(temp: &TempDir) -> (PathBuf, PathBuf, String) {
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let root = temp.join("root");
    let store = Store::create_lazy(&root, test_header(&workspace)).expect("create lazy");
    store.enable_failover(12).expect("enable failover");
    store.append_message(&user("hello")).expect("append");
    let path = store.path();
    store.close().expect("close");
    (root, workspace, path)
}

#[test]
fn archive_of_a_lease_managed_session_moves_the_lease_directory_and_releases_it() {
    let temp = TempDir::new();
    let (root, workspace, path) = seeded_lease_managed_session(&temp);
    let source_lease_dir = Path::new(&path).with_extension("lease");
    assert!(source_lease_dir.is_dir());

    let result = archive(&root, &workspace.to_string_lossy(), Path::new(&path)).expect("archive");

    assert!(
        !source_lease_dir.exists(),
        "the lease directory must move with the session file"
    );
    let archived_lease_dir = Path::new(&result.path).with_extension("lease");
    assert!(archived_lease_dir.join("lease.json").is_file());
    assert!(archived_lease_dir.join("epoch-1").is_file());
    let heartbeat = read_lease_heartbeat(&archived_lease_dir);
    assert_eq!(
        heartbeat["released"], true,
        "archiving releases the lease after the move"
    );
}

#[test]
fn archive_fails_and_moves_nothing_while_another_holder_is_still_renewing() {
    let temp = TempDir::new();
    let (root, workspace, path) = seeded_lease_managed_session(&temp);
    let session_path = Path::new(&path);
    let dir = session_path.with_extension("lease");
    // Overwrite the epoch our own store had released with a live-looking
    // holder: `L` = 2, since our own acquire attempt here always fails
    // (`Held`) and never registers a lease with the real Keeper.
    fs::write(
        dir.join("lease.json"),
        serde_json::json!({ "lease_seconds": 2 }).to_string(),
    )
    .expect("rewrite lease.json");
    fs::write(dir.join("epoch-2"), b"{}").expect("write epoch-2 marker");
    fs::write(
        dir.join("heartbeat"),
        lease_heartbeat_bytes(2, "other-host", 5150, false, 1),
    )
    .expect("write heartbeat");

    let heartbeat_path = dir.join("heartbeat");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_stop = std::sync::Arc::clone(&stop);
    let writer = std::thread::spawn(move || {
        let mut seq = 2u64;
        while !writer_stop.load(Ordering::Relaxed) {
            fs::write(
                &heartbeat_path,
                lease_heartbeat_bytes(2, "other-host", 5150, false, seq),
            )
            .expect("rewrite heartbeat");
            seq += 1;
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });

    let before = fs::read(&path).expect("read session file before archive attempt");
    let error =
        archive(&root, &workspace.to_string_lossy(), session_path).expect_err("holder is live");

    stop.store(true, Ordering::Relaxed);
    writer.join().expect("join heartbeat writer");

    assert!(error.to_string().contains("other-host"), "{error}");
    assert!(session_path.exists(), "the session file must stay in place");
    assert_eq!(
        fs::read(&path).expect("read session file after failed archive"),
        before,
        "a failed archive must not modify the session file"
    );
    assert!(dir.is_dir(), "the lease directory must stay in place");
}

#[test]
fn prepare_of_an_archived_lease_managed_session_acquires_without_waiting() {
    let temp = TempDir::new();
    let (root, workspace, path) = seeded_lease_managed_session(&temp);

    let result = archive(&root, &workspace.to_string_lossy(), Path::new(&path)).expect("archive");
    let archived_path = Path::new(&result.path);

    let started = std::time::Instant::now();
    let prepared = Prepared::prepare(archived_path, None).expect("prepare archived session");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "a released lease must be acquired without waiting"
    );
    prepared.close().expect("close");
}
