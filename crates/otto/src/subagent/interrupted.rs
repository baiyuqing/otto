//! Read-only scanning of a session's child transcripts for interrupted
//! sub-agent tasks, and marking one interrupted.
//!
//! A sub-agent task is interrupted when its transcript has an `otto.task_spec`
//! entry but no `otto.task_result` entry: the process that ran it stopped
//! before the child reached a terminal status. [`scan`] never repairs or
//! writes anything; [`mark_interrupted`] opens the transcript for writing,
//! which repairs any dangling tool call the same way any other open does, and
//! appends the terminal `otto.task_result` entry itself.

use std::path::{Path, PathBuf};

use otto_core::model::{Message, Role};
use otto_core::session::context::pending_tool_calls;

pub use crate::session::UnansweredCall;
use crate::session::unanswered_calls_from;

use super::runner::{
    TASK_RESULT_CUSTOM_TYPE, TASK_SPEC_CUSTOM_TYPE, TaskResultData, TaskSpecData,
    TaskSpecDefinition,
};

/// The `otto.task_result` status [`mark_interrupted`] records.
pub const INTERRUPTED_STATUS: &str = "interrupted";

/// One child transcript's state, read back from its `otto.task_spec` entry
/// and, if present, its `otto.task_result` entry.
#[derive(Debug, Clone)]
pub struct ChildRecord {
    pub path: PathBuf,
    pub task_id: String,
    pub name: String,
    /// The definition name, empty for the default sub-agent.
    pub agent: String,
    pub description: String,
    pub prompt: String,
    pub model: String,
    pub context: String,
    /// `pub(crate)`: [`TaskSpecDefinition`] itself is `pub(crate)`, and
    /// nothing outside the `otto` crate reads this field.
    pub(crate) definition: Option<TaskSpecDefinition>,
    /// Whether a user message with text equal to `prompt` exists in the
    /// transcript: the child agent ran at least once.
    pub started: bool,
    pub unanswered: Vec<UnansweredCall>,
    pub last_assistant_text: String,
    /// The status recorded on the transcript's last entry, when that entry
    /// is an `otto.task_result` custom entry. `None` when the transcript has
    /// no `otto.task_result` yet, or when anything was appended after one
    /// (for example a resumed run's own messages): a task is only ever
    /// eligible for another resume while its transcript still ends exactly
    /// where its last run left it.
    pub final_status: Option<String>,
}

/// Scans every `*.jsonl` file directly in `children_dir` for one with an
/// `otto.task_spec` entry, ordered by task counter. A missing directory
/// yields no records. A file that does not decode, or carries no
/// `otto.task_spec` entry, is skipped. Never repairs or writes.
pub fn scan(children_dir: &Path) -> Vec<ChildRecord> {
    let Ok(entries) = std::fs::read_dir(children_dir) else {
        return Vec::new();
    };
    let mut records: Vec<ChildRecord> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .filter_map(|path| scan_one(&path))
        .collect();
    records.sort_by_key(|record| task_counter(&record.task_id));
    records
}

/// The numeric part of a `t<n>` task id, `0` for anything else, only used to
/// order [`scan`]'s results.
fn task_counter(id: &str) -> u64 {
    id.strip_prefix('t')
        .and_then(|rest| rest.parse().ok())
        .unwrap_or(0)
}

fn scan_one(path: &Path) -> Option<ChildRecord> {
    let (raw_entries, messages) = crate::session::Store::read_entries(path).ok()?;

    let mut spec: Option<TaskSpecData> = None;
    for entry in &raw_entries {
        let Some(custom) = entry.custom.as_ref() else {
            continue;
        };
        if custom.custom_type != TASK_SPEC_CUSTOM_TYPE {
            continue;
        }
        let Some(data) = custom.data.as_ref() else {
            continue;
        };
        spec = serde_json::from_str(data.get()).ok();
    }
    let spec = spec?;

    // Only the transcript's very last entry can set `final_status`: an
    // `otto.task_result` entry followed by anything else (a resumed run's
    // own messages, for example) means the task is running again, not
    // interrupted.
    let final_status = raw_entries.last().and_then(|entry| {
        let custom = entry.custom.as_ref()?;
        if custom.custom_type != TASK_RESULT_CUSTOM_TYPE {
            return None;
        }
        let data = custom.data.as_ref()?;
        serde_json::from_str::<serde_json::Value>(data.get())
            .ok()?
            .get("status")?
            .as_str()
            .map(str::to_string)
    });

    let started = messages
        .iter()
        .any(|message| message.role == Role::User && message.text() == spec.prompt);

    let unanswered = unanswered_calls_from(&pending_tool_calls(&messages).unwrap_or_default());

    let last_assistant_text = messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(Message::text)
        .unwrap_or_default();

    Some(ChildRecord {
        path: path.to_path_buf(),
        task_id: spec.id,
        name: spec.name,
        agent: spec
            .definition
            .as_ref()
            .map_or_else(String::new, |definition| definition.name.clone()),
        description: spec.description,
        prompt: spec.prompt,
        model: spec.model,
        context: spec.context,
        definition: spec.definition,
        started,
        unanswered,
        last_assistant_text,
        final_status,
    })
}

/// Opens `path` for writing, which repairs any dangling tool call by
/// appending a stand-in tool result, and appends an `otto.task_result` entry
/// with status [`INTERRUPTED_STATUS`] and an empty error.
pub fn mark_interrupted(path: &Path) -> Result<(), String> {
    let (store, _warnings) =
        crate::session::Store::open(path).map_err(|error| error.to_string())?;
    let data = serde_json::to_string(&TaskResultData {
        status: INTERRUPTED_STATUS,
        error: "",
    })
    .map_err(|error| error.to_string())?;
    store
        .append_custom_entry(TASK_RESULT_CUSTOM_TYPE, &data)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Store;
    use chrono::Utc;
    use otto_core::model::{Block, BlockType, FinishReason};
    use otto_core::session::Header;

    /// A lazy child transcript at `<parent without .jsonl>/<task_id>-child.jsonl`,
    /// matching the file [`super::super::runner::spawn`] creates.
    fn child_store(parent: &Path, task_id: &str) -> (Store, PathBuf) {
        let name = format!("{task_id}-child");
        let store = Store::create_child_lazy(
            parent,
            &name,
            Header {
                id: "child".into(),
                workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: Utc::now(),
                ..Header::default()
            },
        )
        .expect("child store");
        let path = Store::child_path(parent, &name);
        (store, path)
    }

    fn task_spec_json(id: &str, name: &str, prompt: &str) -> String {
        serde_json::json!({
            "id": id,
            "name": name,
            "description": "",
            "model": "test-model",
            "context": "fresh",
            "prompt": prompt,
            "definition": null,
        })
        .to_string()
    }

    fn task_result_json(status: &str) -> String {
        serde_json::json!({"status": status, "error": ""}).to_string()
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![Block::text(text)],
            created_at: Utc::now(),
            ..Message::default()
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![Block::text(text)],
            finish_reason: Some(FinishReason::Stop),
            created_at: Utc::now(),
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
            finish_reason: Some(FinishReason::ToolCalls),
            created_at: Utc::now(),
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
            created_at: Utc::now(),
            ..Message::default()
        }
    }

    fn children_dir(parent: &Path) -> PathBuf {
        parent.with_extension("")
    }

    #[test]
    fn scan_reports_no_final_status_for_a_spec_only_transcript() {
        let dir = tempfile::tempdir().expect("dir");
        let parent = dir.path().join("parent.jsonl");
        let (store, _path) = child_store(&parent, "t1");
        store
            .append_custom_entry(TASK_SPEC_CUSTOM_TYPE, &task_spec_json("t1", "", "do it"))
            .expect("append task_spec");

        let records = scan(&children_dir(&parent));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].task_id, "t1");
        assert_eq!(records[0].final_status, None);
    }

    #[test]
    fn scan_reports_the_status_a_task_result_entry_carries() {
        let dir = tempfile::tempdir().expect("dir");
        let parent = dir.path().join("parent.jsonl");
        let (store, _path) = child_store(&parent, "t1");
        store
            .append_custom_entry(TASK_SPEC_CUSTOM_TYPE, &task_spec_json("t1", "", "do it"))
            .expect("append task_spec");
        store
            .append_custom_entry(TASK_RESULT_CUSTOM_TYPE, &task_result_json("succeeded"))
            .expect("append task_result");

        let records = scan(&children_dir(&parent));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].final_status, Some("succeeded".to_string()));
    }

    #[test]
    fn scan_skips_a_transcript_with_no_task_spec_entry() {
        let dir = tempfile::tempdir().expect("dir");
        let parent = dir.path().join("parent.jsonl");
        let (store, _path) = child_store(&parent, "t1");
        store
            .append_message(&user("hello"))
            .expect("append message");

        let records = scan(&children_dir(&parent));
        assert!(records.is_empty(), "{records:?}");
    }

    #[test]
    fn scan_reports_started_once_the_prompt_was_sent_as_a_user_message() {
        let dir = tempfile::tempdir().expect("dir");
        let parent = dir.path().join("parent.jsonl");
        let (store, _path) = child_store(&parent, "t1");
        store
            .append_custom_entry(TASK_SPEC_CUSTOM_TYPE, &task_spec_json("t1", "", "do it"))
            .expect("append task_spec");

        assert!(!scan(&children_dir(&parent))[0].started);

        store.append_message(&user("do it")).expect("append user");
        assert!(scan(&children_dir(&parent))[0].started);
    }

    #[test]
    fn scan_marks_only_the_first_unanswered_call_as_possibly_run() {
        let dir = tempfile::tempdir().expect("dir");
        let parent = dir.path().join("parent.jsonl");
        let (store, _path) = child_store(&parent, "t1");
        store
            .append_custom_entry(TASK_SPEC_CUSTOM_TYPE, &task_spec_json("t1", "", "do it"))
            .expect("append task_spec");
        store.append_message(&user("do it")).expect("user");
        store
            .append_message(&tool_calls(&[
                ("call-1", "read"),
                ("call-2", "bash"),
                ("call-3", "write"),
            ]))
            .expect("tool calls");
        store
            .append_message(&tool_result("call-1", "read", "ok"))
            .expect("tool result");

        let records = scan(&children_dir(&parent));
        assert_eq!(records[0].unanswered.len(), 2);
        assert_eq!(records[0].unanswered[0].name, "bash");
        assert!(records[0].unanswered[0].may_have_run);
        assert_eq!(records[0].unanswered[1].name, "write");
        assert!(!records[0].unanswered[1].may_have_run);
    }

    #[test]
    fn scan_reports_the_last_assistant_message_text() {
        let dir = tempfile::tempdir().expect("dir");
        let parent = dir.path().join("parent.jsonl");
        let (store, _path) = child_store(&parent, "t1");
        store
            .append_custom_entry(TASK_SPEC_CUSTOM_TYPE, &task_spec_json("t1", "", "do it"))
            .expect("append task_spec");
        store.append_message(&user("do it")).expect("user");
        store.append_message(&assistant("first")).expect("first");
        store.append_message(&assistant("last")).expect("last");

        let records = scan(&children_dir(&parent));
        assert_eq!(records[0].last_assistant_text, "last");
    }

    #[test]
    fn mark_interrupted_appends_a_task_result_scan_then_finds() {
        let dir = tempfile::tempdir().expect("dir");
        let parent = dir.path().join("parent.jsonl");
        let (store, path) = child_store(&parent, "t1");
        store
            .append_custom_entry(TASK_SPEC_CUSTOM_TYPE, &task_spec_json("t1", "", "do it"))
            .expect("append task_spec");
        store
            .append_message(&tool_calls(&[("call-1", "read")]))
            .expect("dangling call");
        store.close().expect("close");

        mark_interrupted(&path).expect("mark interrupted");

        let records = scan(&children_dir(&parent));
        assert_eq!(
            records[0].final_status,
            Some(INTERRUPTED_STATUS.to_string())
        );

        // The dangling call from before mark_interrupted was repaired on disk,
        // as a side effect of Store::open: a synthetic tool result was
        // appended for it.
        let (_entries, messages) = Store::read_entries(&path).expect("read entries");
        let repaired = messages
            .iter()
            .find(|message| message.role == Role::Tool)
            .expect("a synthetic tool result was written");
        assert_eq!(repaired.blocks[0].tool_call_id, "call-1");
        assert!(repaired.blocks[0].is_error);
    }
}
