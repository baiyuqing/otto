//! Portable usage and provider-operation records; no physical schema or SDK dependencies.
use crate::storage::migration::record::RecordSchema;

pub const DOMAIN: &str = "usage";
pub const FAMILY: &str = "otto.usage";
pub const VERSION: u32 = 2;
pub const RECORDS: &[RecordSchema] = &[
    RecordSchema {
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
            "cached_breakdown_present",
        ],
        keys: &["id"],
        revision: None,
    },
    RecordSchema {
        name: "provider_api_operation",
        fields: &[
            "id",
            "occurred_at",
            "workspace",
            "session_id",
            "task_id",
            "provider",
            "profile",
            "model",
            "operation_id",
            "kind",
            "mode",
            "attempts",
            "status",
            "disposition",
            "effect_certainty",
            "stop_reason",
        ],
        keys: &["id"],
        revision: None,
    },
];
