//! Portable memory_advice record definitions; no physical schema or SDK dependencies.
use crate::storage::migration::record::RecordSchema;

pub const DOMAIN: &str = "memory_advice";
pub const FAMILY: &str = "otto.memory_advice";
pub const VERSION: u32 = 1;
pub const RECORDS: &[RecordSchema] = &[RecordSchema {
    name: "memory_advice_audit",
    fields: &[
        "id",
        "created_at",
        "model",
        "auxiliary",
        "candidate_id",
        "scope",
        "request",
        "response",
        "outcome",
        "error",
        "latency_ms",
    ],
    keys: &["id"],
    revision: None,
}];
