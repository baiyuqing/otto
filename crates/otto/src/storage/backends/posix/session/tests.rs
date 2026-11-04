//! Native session tests.
//!
//! Every test is offline and deterministic and works inside its own directory
//! under [`std::env::temp_dir`], removed when the guard drops. No `tempfile`
//! dependency: a unique name plus a `Drop` guard is fifteen lines.

// `set_modified` calls `utimes`, the only way to age a file without sleeping.
#![allow(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::session::archive;
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
use super::prepared::Prepared;
use super::store::Store;
use crate::faults::files::{FaultyFileDriver, FileOp};
use crate::faults::{Fault, FaultError, FaultPlan, Filter, Rule, Trigger};
use crate::storage::drivers::posix::PosixDriver;

mod compaction_list;
mod create_append;
mod deferred_custom;
mod fork_cases;
mod inputs;
mod lease_prepare;
mod listing_hardening;
mod open_repair;
mod prepare_archive;
mod recording;
mod turn_recovery;

use compaction_list::*;
use lease_prepare::*;
use open_repair::*;

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

/// A store as `new_store` builds it, over a driver that fails every journal
/// write from write number `from_write` on with `EIO` before writing. The
/// header and runtime record are writes 1 and 2, so `from_write` 3 fails the
/// first durable write after creation.
fn store_failing_writes_from(temp: &TempDir, from_write: u32) -> (Store, Arc<FaultPlan<FileOp>>) {
    let workspace = temp.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let plan = FaultPlan::new(vec![
        Rule::new(
            FileOp::Write,
            Trigger::FromNth(from_write),
            Fault::Before(FaultError::Os(libc::EIO)),
        )
        .filter(Filter::NameContains(".jsonl")),
    ]);
    let driver = FaultyFileDriver::new(Arc::new(PosixDriver), Arc::clone(&plan));
    let store = Store::create_lazy_with_driver(
        temp.join("root"),
        test_header(&workspace),
        Arc::new(driver),
    )
    .expect("create store");
    store
        .lock()
        .expect("lock")
        .ensure_file()
        .expect("create file");
    (store, plan)
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
    assert!(file.is_close_on_exec().expect("check CLOEXEC"));
}
