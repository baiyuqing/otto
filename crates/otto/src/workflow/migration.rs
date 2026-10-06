//! Portable workflows record definitions; no physical schema or SDK dependencies.
use crate::storage::migration::record::RecordSchema;

pub const DOMAIN: &str = "workflows";
pub const FAMILY: &str = "otto.workflows";
pub const VERSION: u32 = 1;
pub const RECORDS: &[RecordSchema] = &[
    RecordSchema {
        name: "workflow_run",
        fields: &[
            "id",
            "workflow",
            "definition_json",
            "definition_hash",
            "workspace",
            "profile",
            "provider",
            "model",
            "input",
            "forked_from_run_id",
            "forked_from_event_seq",
            "forked_from_step_id",
            "status",
            "created_at",
            "updated_at",
        ],
        keys: &["id"],
        revision: Some("updated_at"),
    },
    RecordSchema {
        name: "workflow_step",
        fields: &[
            "run_id",
            "id",
            "position",
            "kind",
            "agent",
            "prompt",
            "needs_json",
            "status",
            "attempt",
            "result",
            "error",
            "transcript_path",
            "source_run_id",
            "source_step_id",
            "source_attempt",
        ],
        keys: &["run_id", "id"],
        revision: Some("attempt"),
    },
    RecordSchema {
        name: "workflow_attempt",
        fields: &[
            "run_id",
            "step_id",
            "attempt",
            "status",
            "transcript_path",
            "result",
            "error",
            "started_at",
            "finished_at",
        ],
        keys: &["run_id", "step_id", "attempt"],
        revision: Some("attempt"),
    },
    RecordSchema {
        name: "workflow_review_request",
        fields: &[
            "id",
            "run_id",
            "step_id",
            "prompt",
            "status",
            "created_at",
            "responded_at",
        ],
        keys: &["id"],
        revision: Some("responded_at"),
    },
    RecordSchema {
        name: "workflow_event",
        fields: &["seq", "run_id", "occurred_at", "kind", "step_id", "status"],
        keys: &["seq"],
        revision: Some("seq"),
    },
];
