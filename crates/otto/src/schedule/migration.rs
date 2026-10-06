//! Portable schedule record definitions; no physical schema or SDK dependencies.
use crate::storage::migration::record::RecordSchema;

pub const DOMAIN: &str = "schedule";
pub const FAMILY: &str = "otto.schedule";
pub const VERSION: u32 = 1;
pub const RECORDS: &[RecordSchema] = &[
    RecordSchema {
        name: "schedule_occurrence",
        fields: &[
            "id",
            "origin",
            "session_id",
            "workspace",
            "kind",
            "body",
            "repeat",
            "local_time",
            "due_at",
            "state",
            "result",
            "attempts",
            "next_try_at",
            "claimed_until",
            "last_error",
            "created_at",
            "updated_at",
        ],
        keys: &["id"],
        revision: Some("updated_at"),
    },
    RecordSchema {
        name: "schedule_delivery_receipt",
        fields: &[
            "item_id",
            "due_at",
            "origin",
            "session_id",
            "text",
            "delivered_at",
            "appended",
        ],
        keys: &["item_id", "due_at"],
        revision: Some("appended"),
    },
    RecordSchema {
        name: "schedule_delivery_client",
        fields: &["id", "platforms", "destinations", "expires_at"],
        keys: &["id"],
        revision: None,
    },
];
