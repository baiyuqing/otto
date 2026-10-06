//! Portable reflection record definitions; no physical schema or SDK dependencies.
use crate::storage::migration::record::RecordSchema;

pub const DOMAIN: &str = "reflection";
pub const FAMILY: &str = "otto.reflection";
pub const VERSION: u32 = 1;
pub const RECORDS: &[RecordSchema] = &[
    RecordSchema {
        name: "reflection_run",
        fields: &[
            "id",
            "session_id",
            "trigger",
            "started_at",
            "finished_at",
            "status",
            "from_entry",
            "to_entry",
            "input_bytes",
            "truncated",
            "memories_proposed",
            "memories_dropped",
            "detail",
            "skills_written",
        ],
        keys: &["id"],
        revision: Some("finished_at"),
    },
    RecordSchema {
        name: "reflection_watermark",
        fields: &["session_id", "entry_id", "updated_at"],
        keys: &["session_id"],
        revision: Some("updated_at"),
    },
    RecordSchema {
        name: "generated_skill_ownership",
        fields: &[
            "name",
            "hash",
            "run_id",
            "session_id",
            "reason",
            "created_at",
            "updated_at",
        ],
        keys: &["name"],
        revision: Some("updated_at"),
    },
    RecordSchema {
        name: "generated_skill_version",
        fields: &["id", "name", "hash", "run_id", "saved_at"],
        keys: &["id"],
        revision: Some("id"),
    },
];
