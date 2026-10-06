//! Portable memory record definitions; no physical schema or SDK dependencies.
use crate::storage::migration::record::RecordSchema;

pub const DOMAIN: &str = "memory";
pub const FAMILY: &str = "otto.memory";
pub const VERSION: u32 = 1;
pub const RECORDS: &[RecordSchema] = &[
    RecordSchema {
        name: "memory_identity_metadata",
        fields: &["key", "value"],
        keys: &["key"],
        revision: None,
    },
    RecordSchema {
        name: "memory_record",
        fields: &[
            "id",
            "scope_namespace",
            "scope_id",
            "kind",
            "semantic_key",
            "text_value",
            "labels_json",
            "metadata_json",
            "source_json",
            "confidence",
            "revision",
            "created_at",
            "updated_at",
            "expires_at",
            "state",
            "forgotten_at",
        ],
        keys: &["id"],
        revision: Some("revision"),
    },
    RecordSchema {
        name: "memory_observation",
        fields: &["id", "candidate_ids_json", "created_at"],
        keys: &["id"],
        revision: None,
    },
    RecordSchema {
        name: "memory_review_candidate",
        fields: &[
            "id",
            "scope_namespace",
            "scope_id",
            "action",
            "target_id",
            "base_revision",
            "observation_id",
            "proposed_json",
            "reason",
            "state",
            "created_at",
            "decided_at",
            "decision_source",
            "result_record_id",
            "result_revision",
        ],
        keys: &["id"],
        revision: Some("result_revision"),
    },
];
