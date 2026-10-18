//! Portable tasks record definitions; no physical schema or SDK dependencies.
use crate::storage::migration::record::RecordSchema;

pub const DOMAIN: &str = "tasks";
pub const FAMILY: &str = "otto.tasks";
pub const VERSION: u32 = 2;
pub const RECORDS: &[RecordSchema] = &[RecordSchema {
    name: "subagent_task",
    fields: &[
        "parent_session",
        "task_id",
        "workspace",
        "parent_session_path",
        "name",
        "agent",
        "description",
        "model",
        "model_source",
        "context",
        "prompt",
        "status",
        "created_at",
        "started_at",
        "finished_at",
        "steps",
        "tool_calls",
        "last_tool",
        "input_tokens",
        "output_tokens",
        "cached_tokens",
        "result",
        "error",
        "session_path",
        "pid",
        "process_started_at",
    ],
    keys: &["parent_session", "task_id"],
    revision: Some("finished_at"),
}];
