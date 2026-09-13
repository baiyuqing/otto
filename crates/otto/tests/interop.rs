//! The Rust half of the two-way session interoperability gate.
//!
//! Running `cargo test -p otto --test interop` produces `target/interop/`:
//!
//! * `session/` holds a session written entirely by the Rust store,
//! * `expectation.json` states what Go must read back from it,
//! * `binary-expectation.json` states the same for a session the built `otto`
//!   binary produced by running one `--approve` turn,
//! * `fixtures.json` states what Rust decoded from every Go fixture under
//!   `internal/session/testdata/pi-v3`.
//!
//! `go test -tags rustinterop ./internal/session -run TestRustInterop` then
//! checks both files with Go's own codec. The Makefile target `rust-interop`
//! runs the pair.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::{Script, serve, text_reply, tool_call_reply};

use chrono::{TimeZone, Utc};
use otto::session::Store;
use otto_core::model::{Block, BlockType, FinishReason, Message, Role, Usage};
use otto_core::session::{CURRENT_VERSION, CompactionCheckpoint, Header, RuntimeMetadata};
use serde_json::{Value, json};

/// The repository root, two levels above this crate.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/otto has a workspace root")
        .to_path_buf()
}

/// `target/interop`, emptied so a rerun never sees a stale session.
fn interop_directory() -> PathBuf {
    let directory = repository_root().join("target").join("interop");
    if directory.exists() {
        fs::remove_dir_all(&directory).expect("clear the interop directory");
    }
    fs::create_dir_all(&directory).expect("create the interop directory");
    directory
}

fn role_name(role: &Role) -> String {
    String::from(role.clone())
}

/// The message shape both sides compare: role, text and tool identity.
fn message_json(message: &Message) -> Value {
    let tool = message.blocks.iter().find(|block| {
        matches!(
            block.block_type,
            BlockType::ToolCall | BlockType::ToolResult
        )
    });
    json!({
        "role": role_name(&message.role),
        "contextType": message.context_type,
        "text": message.text(),
        "toolCallId": tool.map(|block| block.tool_call_id.clone()).unwrap_or_default(),
        "toolName": tool.map(|block| block.tool_name.clone()).unwrap_or_default(),
        "toolText": tool.map(|block| block.text.clone()).unwrap_or_default(),
    })
}

fn text(role: Role, value: &str, seconds: i64) -> Message {
    Message {
        role,
        blocks: vec![Block::text(value)],
        created_at: Utc.timestamp_opt(seconds, 0).single().expect("timestamp"),
        ..Message::default()
    }
}

/// Both halves run in one test: they share `target/interop`, and the first
/// step empties it.
#[test]
fn writes_the_interop_bundle() {
    let directory = interop_directory();
    let workspace = directory.join("workspace");
    fs::create_dir_all(&workspace).expect("create the workspace");
    let root = directory.join("sessions");

    let header = Header {
        version: CURRENT_VERSION,
        id: "6f9619ff-8b86-d011-b42d-00c04fc964ff".into(),
        workspace: workspace.to_string_lossy().into_owned(),
        provider: "openai-compatible".into(),
        profile: "local".into(),
        model: "interop-model".into(),
        created_at: Utc.timestamp_opt(1, 0).single().expect("timestamp"),
    };
    let store = Store::create(&root, header.clone()).expect("create the session");

    store
        .append_message(&text(Role::User, "first question", 2))
        .expect("append the first question");
    store
        .append_message(&Message {
            usage: Some(Usage {
                input_tokens: 30,
                output_tokens: 7,
                cached_input_tokens: 5,
            }),
            finish_reason: Some(FinishReason::Stop),
            ..text(Role::Assistant, "first answer", 3)
        })
        .expect("append the first answer");

    // A runtime custom entry, written as its own `otto.runtime` record.
    store
        .update_runtime(&RuntimeMetadata {
            profile: "local".into(),
            provider: "openai-compatible".into(),
            model: "interop-model-2".into(),
        })
        .expect("update the runtime");

    store
        .append_message(&Message {
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                arguments: Some(
                    serde_json::value::RawValue::from_string(r#"{"path":"README.md"}"#.into())
                        .expect("valid JSON"),
                ),
                ..Block::default()
            }],
            created_at: Utc.timestamp_opt(4, 0).single().expect("timestamp"),
            finish_reason: Some(FinishReason::ToolCalls),
            ..Message::default()
        })
        .expect("append the tool call");
    store
        .append_message(&Message {
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                text: "file contents".into(),
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                ..Block::default()
            }],
            created_at: Utc.timestamp_opt(5, 0).single().expect("timestamp"),
            ..Message::default()
        })
        .expect("append the tool result");

    store.rename("Interop session").expect("name the session");

    // Anchor the checkpoint on the tool call, so the retained tail spans the
    // tool call and its result.
    let path = store.path();
    let anchor = fs::read_to_string(&path)
        .expect("read the session")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == "message")
        .nth(2)
        .and_then(|entry| entry["id"].as_str().map(str::to_owned))
        .expect("the tool call entry id");
    let compaction = store
        .append_compaction(&CompactionCheckpoint {
            summary: "compacted the earlier exchange".into(),
            first_kept_entry_id: anchor.clone(),
            tokens_before: 512,
            usage: Some(Usage {
                input_tokens: 11,
                output_tokens: 2,
                cached_input_tokens: 1,
            }),
            details: Default::default(),
            created_at: Utc.timestamp_opt(6, 0).single().expect("timestamp"),
        })
        .expect("append the compaction");

    store
        .append_message(&text(Role::User, "question after compaction", 7))
        .expect("append after the compaction");

    let messages: Vec<Value> = store.messages().iter().map(message_json).collect();
    let (usage, usage_present) = store.aggregate_usage();
    let snapshot = store.snapshot();
    store.close().expect("close the session");

    let expectation = json!({
        "sessionPath": path,
        "workspace": header.workspace,
        "header": {
            "version": header.version,
            "id": header.id,
            "provider": "openai-compatible",
            "profile": "local",
            "model": "interop-model-2",
            "createdAt": "1970-01-01T00:00:01Z",
        },
        "name": "Interop session",
        "messages": messages,
        "aggregateUsage": {
            "inputTokens": usage.input_tokens,
            "outputTokens": usage.output_tokens,
            "cachedInputTokens": usage.cached_input_tokens,
        },
        "aggregateUsagePresent": usage_present,
        "contextInputTokens": snapshot.context_input_tokens,
        "contextInputTokensPresent": snapshot.context_input_tokens_present,
        "contextInputTokensPending": snapshot.context_input_tokens_pending,
        "compaction": {
            "summary": compaction.summary,
            "firstKeptEntryId": compaction.first_kept_entry_id,
            "tokensBefore": compaction.tokens_before,
            "retainedTailOnly": compaction.retained_tail_only,
        },
    });
    fs::write(
        directory.join("expectation.json"),
        serde_json::to_vec_pretty(&expectation).expect("encode the expectation"),
    )
    .expect("write the expectation");

    // The Rust store must also read its own output back unchanged.
    let (reopened, warnings) = Store::open(&path).expect("reopen the session");
    assert!(warnings.is_empty(), "reopen warnings: {warnings:?}");
    let reread: Vec<Value> = reopened.messages().iter().map(message_json).collect();
    assert_eq!(reread, messages, "the reopened session differs");
    reopened.close().expect("close the reopened session");

    decode_every_go_fixture(&directory);
    write_binary_session(&directory);
}

/// Runs the built binary for one `--approve` turn and records the session it
/// wrote, so the Go gate also reads a file produced by the whole process and
/// not only by the store API.
///
/// The shape matches `expectation.json` except that a one-turn run has no
/// compaction checkpoint, which `compactionPresent` states.
fn write_binary_session(directory: &Path) {
    let home = directory.join("binary-home");
    let workspace = directory.join("binary-workspace");
    fs::create_dir_all(workspace.join("sub")).expect("create the binary workspace");
    fs::write(workspace.join("README.md"), "interop fixture\n").expect("seed README");
    // Seatbelt keeps its private state under `$HOME/Library/Caches`, which both
    // implementations require to exist already.
    fs::create_dir_all(home.join("Library/Caches")).expect("create the cache base");
    let config_directory = home.join(".config/otto");
    fs::create_dir_all(&config_directory).expect("create the config directory");

    let served = Arc::new(AtomicUsize::new(0));
    let base_url = serve(Script {
        replies: vec![
            tool_call_reply("call-1", "read", r#"{"path":"README.md"}"#),
            text_reply("the interop answer"),
        ],
        served: Arc::clone(&served),
    });
    fs::write(
        config_directory.join("config.toml"),
        format!(
            "default_profile = \"interop\"\n\n[profiles.interop]\nprovider = \"openai-compatible\"\nbase_url = \"{base_url}\"\nmodel = \"interop-binary-model\"\napi_key_env = \"OTTO_API_KEY\"\n"
        ),
    )
    .expect("write the config");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_otto"));
    command
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("OTTO_API_KEY", "sk-interop-not-a-real-key")
        .arg("--cwd")
        .arg(&workspace)
        .arg("--approve")
        .arg("read the readme");
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    let output = command.output().expect("run the otto binary");
    assert!(
        output.status.success(),
        "otto --approve exit = {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(served.load(Ordering::SeqCst), 2, "model turns served");

    let path = only_session_file(&home);
    let (store, warnings) = Store::open(&path).expect("open the binary session");
    assert!(warnings.is_empty(), "reopen warnings: {warnings:?}");
    let header = store.header().clone();
    let messages: Vec<Value> = store.messages().iter().map(message_json).collect();
    let (usage, usage_present) = store.aggregate_usage();
    let snapshot = store.snapshot();
    let name = store.name();
    let compaction_present = store.latest_compaction().is_some();
    store.close().expect("close the binary session");

    let expectation = json!({
        "sessionPath": path,
        "workspace": header.workspace,
        "header": {
            "version": header.version,
            "id": header.id,
            "provider": header.provider,
            "profile": header.profile,
            "model": header.model,
        },
        "name": name,
        "messages": messages,
        "aggregateUsage": {
            "inputTokens": usage.input_tokens,
            "outputTokens": usage.output_tokens,
            "cachedInputTokens": usage.cached_input_tokens,
        },
        "aggregateUsagePresent": usage_present,
        "contextInputTokens": snapshot.context_input_tokens,
        "contextInputTokensPresent": snapshot.context_input_tokens_present,
        "contextInputTokensPending": snapshot.context_input_tokens_pending,
        "compactionPresent": compaction_present,
    });
    fs::write(
        directory.join("binary-expectation.json"),
        serde_json::to_vec_pretty(&expectation).expect("encode the binary expectation"),
    )
    .expect("write the binary expectation");
}

/// The one session file the binary run left under `$HOME/.otto/sessions`.
fn only_session_file(home: &Path) -> PathBuf {
    let mut found = Vec::new();
    for workspace in fs::read_dir(home.join(".otto/sessions")).expect("session root") {
        for session in
            fs::read_dir(workspace.expect("workspace entry").path()).expect("workspace sessions")
        {
            found.push(session.expect("session entry").path());
        }
    }
    assert_eq!(found.len(), 1, "one session per run: {found:?}");
    found.remove(0)
}

/// Decodes every Go fixture from a copy and records what Rust read.
fn decode_every_go_fixture(directory: &Path) {
    let fixtures = repository_root()
        .join("internal")
        .join("session")
        .join("testdata")
        .join("pi-v3");
    let scratch = repository_root().join("target").join("interop-fixtures");
    if scratch.exists() {
        fs::remove_dir_all(&scratch).expect("clear the fixture scratch directory");
    }
    fs::create_dir_all(&scratch).expect("create the fixture scratch directory");

    let mut names: Vec<String> = fs::read_dir(&fixtures)
        .expect("read the fixture directory")
        .map(|entry| {
            entry
                .expect("fixture entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no fixtures found in {fixtures:?}");

    let mut decoded = serde_json::Map::new();
    for name in &names {
        // Open repairs in place, so every fixture is decoded from a copy.
        let copy = scratch.join(name);
        fs::copy(fixtures.join(name), &copy).expect("copy the fixture");
        let (store, warnings) =
            Store::open(&copy).unwrap_or_else(|error| panic!("open {name}: {error}"));
        let messages: Vec<Value> = store.messages().iter().map(message_json).collect();
        let (usage, usage_present) = store.aggregate_usage();
        store.close().expect("close the fixture");
        decoded.insert(
            name.clone(),
            json!({
                "messages": messages,
                "warnings": warnings.iter().map(|warning| warning.message.clone()).collect::<Vec<_>>(),
                "aggregateUsage": {
                    "inputTokens": usage.input_tokens,
                    "outputTokens": usage.output_tokens,
                    "cachedInputTokens": usage.cached_input_tokens,
                },
                "aggregateUsagePresent": usage_present,
            }),
        );
    }

    fs::write(
        directory.join("fixtures.json"),
        serde_json::to_vec_pretty(&Value::Object(decoded)).expect("encode the fixture report"),
    )
    .expect("write the fixture report");
}
