//! The Rust half of the two-way session interoperability gate.
//!
//! Running `cargo test -p otto --test interop` produces `target/interop/`:
//!
//! * `session/` holds a session written entirely by the Rust store,
//! * `expectation.json` states what Go must read back from it,
//! * `fixtures.json` states what Rust decoded from every Go fixture under
//!   `internal/session/testdata/pi-v3`.
//!
//! `go test -tags rustinterop ./internal/session -run TestRustInterop` then
//! checks both files with Go's own codec. The Makefile target `rust-interop`
//! runs the pair.

use std::fs;
use std::path::{Path, PathBuf};

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
