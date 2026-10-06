//! Portable usage record definitions; no physical schema or SDK dependencies.
use crate::storage::migration::record::RecordSchema;

pub const DOMAIN: &str = "usage";
pub const FAMILY: &str = "otto.usage";
pub const VERSION: u32 = 1;
pub const RECORDS: &[RecordSchema] = &[RecordSchema {
    name: "usage_event",
    fields: &[
        "id",
        "occurred_at",
        "workspace",
        "session_id",
        "task_id",
        "provider",
        "profile",
        "model",
        "kind",
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "usage_present",
    ],
    keys: &["id"],
    revision: None,
}];
