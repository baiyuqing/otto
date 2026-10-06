//! Portable skill_checks record definitions; no physical schema or SDK dependencies.
use crate::storage::migration::record::RecordSchema;

pub const DOMAIN: &str = "skill_checks";
pub const FAMILY: &str = "otto.skill_checks";
pub const VERSION: u32 = 1;
pub const RECORDS: &[RecordSchema] = &[RecordSchema {
    name: "skill_check_audit",
    fields: &[
        "id",
        "content_sha256",
        "questions_sha256",
        "skill",
        "path",
        "source",
        "model",
        "checked_at",
        "result",
    ],
    keys: &["id"],
    revision: None,
}];
