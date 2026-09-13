//! Pi v3 session wire types.
//!
//! Port of `internal/session/pi_types.go` and `internal/session/pi_details.go`.
//! These structs mirror the on-disk JSONL records field for field, including
//! the serde rename and omit-empty behavior, so that a record written by
//! either implementation decodes in the other.
//!
//! Ownership: every value here is owned and `Clone`. Fields that carry
//! provider or forward-compatible JSON are kept as raw bytes
//! (`Option<Box<RawValue>>` for serialized fields, `Vec<u8>` for the
//! non-serialized `raw` capture) so unknown fields survive a decode/encode
//! round trip unchanged.
//!
//! Concurrency: plain data, no interior mutability.
//!
//! Errors: these types perform no validation. [`crate::session::codec`] owns
//! every rule and every error string.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::model::ContextMetadata;

use super::PiError;

/// The only session format version this implementation reads or writes.
pub const PI_SESSION_VERSION: i64 = 3;
/// Largest single JSONL record, in bytes.
pub const MAX_SESSION_ENTRY_BYTES: usize = 16 << 20;
/// Largest whole session file, in bytes.
pub const MAX_SESSION_FILE_BYTES: usize = 256 << 20;

/// A decoded session file: one header followed by its entries in file order.
#[derive(Debug, Clone, Default)]
pub struct PiFile {
    pub header: PiHeader,
    pub entries: Vec<PiEntry>,
}

/// Line 1 of a session file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PiHeader {
    #[serde(rename = "type")]
    pub type_name: String,
    pub version: i64,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(
        rename = "parentSession",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_session: Option<String>,
    /// The exact bytes this header was decoded from, empty when it was built
    /// in memory. Re-encoding prefers these bytes so unknown fields survive.
    #[serde(skip)]
    pub raw: Vec<u8>,
}

/// One entry line. Exactly one payload field is set, chosen by `type_name`;
/// an unrecognized type leaves all of them unset and is carried by `raw`.
#[derive(Debug, Clone, Default)]
pub struct PiEntry {
    pub type_name: String,
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    /// The exact bytes this entry was decoded from, empty when it was built in
    /// memory.
    pub raw: Vec<u8>,
    pub message: Option<Box<PiMessage>>,
    pub model_change: Option<PiModelChange>,
    pub thinking_level_change: Option<PiThinkingLevelChange>,
    pub compaction: Option<Box<PiCompaction>>,
    pub branch_summary: Option<Box<PiBranchSummary>>,
    pub custom: Option<PiCustom>,
    pub custom_message: Option<Box<PiCustomMessage>>,
    pub label: Option<PiLabel>,
    pub session_info: Option<PiSessionInfo>,
}

impl PiEntry {
    /// A bare entry with only the shared header fields set.
    pub fn new(type_name: &str, id: &str, parent_id: Option<String>, timestamp: &str) -> Self {
        Self {
            type_name: type_name.to_owned(),
            id: id.to_owned(),
            parent_id,
            timestamp: timestamp.to_owned(),
            ..Self::default()
        }
    }
}

/// The `message` payload of a `message` entry, and the element type of a
/// compaction's `retainedTail`.
///
/// The field order matches the Go struct because both implementations encode
/// in declaration order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PiMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub api: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(rename = "responseModel", skip_serializing_if = "String::is_empty")]
    pub response_model: String,
    #[serde(rename = "responseId", skip_serializing_if = "String::is_empty")]
    pub response_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<PiUsage>,
    #[serde(rename = "stopReason", skip_serializing_if = "String::is_empty")]
    pub stop_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<Box<RawValue>>,
    #[serde(rename = "errorMessage", skip_serializing_if = "String::is_empty")]
    pub error_message: String,
    #[serde(rename = "rawStopReason", skip_serializing_if = "String::is_empty")]
    pub raw_stop_reason: String,
    #[serde(rename = "endTurn", skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    #[serde(rename = "toolCallId", skip_serializing_if = "String::is_empty")]
    pub tool_call_id: String,
    #[serde(rename = "toolName", skip_serializing_if = "String::is_empty")]
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<RawValue>>,
    #[serde(rename = "addedToolNames", skip_serializing_if = "Vec::is_empty")]
    pub added_tool_names: Vec<String>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub output: String,
    #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(rename = "fullOutputPath", skip_serializing_if = "String::is_empty")]
    pub full_output_path: String,
    #[serde(rename = "excludeFromContext", skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
    #[serde(rename = "customType", skip_serializing_if = "String::is_empty")]
    pub custom_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(rename = "fromId", skip_serializing_if = "String::is_empty")]
    pub from_id: String,
    #[serde(rename = "tokensBefore", skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<i64>,
    pub timestamp: i64,

    /// Decoded string form of `content`, set only when the wire value was a
    /// JSON string.
    #[serde(skip)]
    pub content_text: Option<String>,
    /// Decoded array form of `content`.
    #[serde(skip)]
    pub content_blocks: Vec<PiContentBlock>,
}

/// One element of a message's `content` array.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PiContentBlock {
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(rename = "textSignature", skip_serializing_if = "String::is_empty")]
    pub text_signature: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub data: String,
    #[serde(rename = "mimeType", skip_serializing_if = "String::is_empty")]
    pub mime_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    #[serde(rename = "thinkingSignature", skip_serializing_if = "String::is_empty")]
    pub thinking_signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Box<RawValue>>,
    #[serde(rename = "thoughtSignature", skip_serializing_if = "String::is_empty")]
    pub thought_signature: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    #[serde(skip)]
    pub raw: Vec<u8>,
}

/// Provider-reported token counts on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PiUsage {
    pub input: i64,
    pub output: i64,
    #[serde(rename = "cacheRead")]
    pub cache_read: i64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: i64,
    #[serde(rename = "cacheWrite1h", skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<i64>,
    #[serde(rename = "totalTokens")]
    pub total_tokens: i64,
    pub cost: PiCost,
}

/// Provider-reported cost on the wire. Otto never computes these values; they
/// are read and written unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PiCost {
    pub input: f64,
    pub output: f64,
    #[serde(rename = "cacheRead")]
    pub cache_read: f64,
    #[serde(rename = "cacheWrite")]
    pub cache_write: f64,
    pub total: f64,
}

/// The payload of a `model_change` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiModelChange {
    pub provider: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
}

/// The payload of a `thinking_level_change` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiThinkingLevelChange {
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
}

/// The payload of a `compaction` entry.
///
/// A checkpoint anchors the visible history either with `first_kept_entry_id`
/// (a real entry that follows it) or with `retained_tail` (synthetic messages
/// stored in the entry itself). At least one must be present.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PiCompaction {
    pub summary: String,
    #[serde(
        rename = "firstKeptEntryId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub first_kept_entry_id: Option<String>,
    #[serde(rename = "tokensBefore")]
    pub tokens_before: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<RawValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<PiUsage>,
    #[serde(rename = "fromHook", default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
    /// `None` when the record carried no `retainedTail` key at all; `Some`
    /// (possibly empty) when it did. Go distinguishes a nil slice from an
    /// empty one here, and the checkpoint form depends on that difference.
    #[serde(
        rename = "retainedTail",
        default,
        skip_serializing_if = "is_empty_retained_tail"
    )]
    pub retained_tail: Option<Vec<PiMessage>>,
}

/// Mirrors Go's `omitempty` on a slice: a nil or empty tail is not written.
fn is_empty_retained_tail(tail: &Option<Vec<PiMessage>>) -> bool {
    tail.as_ref().is_none_or(Vec::is_empty)
}

/// The payload of a `branch_summary` entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PiBranchSummary {
    #[serde(rename = "fromId")]
    pub from_id: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<RawValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<PiUsage>,
    #[serde(rename = "fromHook", default, skip_serializing_if = "Option::is_none")]
    pub from_hook: Option<bool>,
}

/// The payload of a `custom` entry. Otto writes one of these with custom type
/// `otto.runtime` to record the active provider, model, and profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PiCustom {
    #[serde(rename = "customType")]
    pub custom_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Box<RawValue>>,
}

/// The payload of a `custom_message` entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PiCustomMessage {
    #[serde(rename = "customType")]
    pub custom_type: String,
    pub content: Option<Box<RawValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<RawValue>>,
    pub display: bool,
    #[serde(skip)]
    pub content_text: Option<String>,
    #[serde(skip)]
    pub content_blocks: Vec<PiContentBlock>,
}

/// The payload of a `label` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiLabel {
    #[serde(rename = "targetId")]
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The payload of a `session_info` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiSessionInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Otto's private extension block, nested under a record's `details.otto`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiOttoDetails {
    #[serde(rename = "taskId", default, skip_serializing_if = "String::is_empty")]
    pub task_id: String,
    #[serde(
        rename = "usagePresent",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub usage_present: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PiDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    otto: Option<PiOttoDetails>,
}

/// Encodes `details` as the `{"otto":{...}}` wrapper Otto writes.
pub fn encode_pi_otto_details(details: &PiOttoDetails) -> Result<Box<RawValue>, PiError> {
    serde_json::value::to_raw_value(&PiDetails {
        otto: Some(details.clone()),
    })
    .map_err(|error| PiError::other(format!("encode Otto Pi details: {error}")))
}

/// Reads Otto's extension block out of a record's `details`.
///
/// Returns `Ok(None)` when the field is absent, `null`, not an object, or has
/// no `otto` key: another writer's details are not an error. A present but
/// invalid task id is rejected, because Otto wrote it and a bad value would
/// silently reattach a message to the wrong sub-agent.
pub fn decode_pi_otto_details(raw: Option<&RawValue>) -> Result<Option<PiOttoDetails>, PiError> {
    let Some(raw) = raw else { return Ok(None) };
    if raw.get().trim() == "null" {
        return Ok(None);
    }
    let Ok(details) = serde_json::from_str::<PiDetails>(raw.get()) else {
        return Ok(None);
    };
    let Some(otto) = details.otto else {
        return Ok(None);
    };
    if !otto.task_id.is_empty()
        && let Err(error) = (ContextMetadata {
            task_id: otto.task_id.clone(),
        })
        .validate()
    {
        return Err(PiError::invalid(format!(
            "invalid context metadata: {error}"
        )));
    }
    Ok(Some(otto))
}
