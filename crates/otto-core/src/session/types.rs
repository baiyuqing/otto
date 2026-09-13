//! Session domain types.
//!
//! Port of `internal/session/types.go`. These are Otto's own view of a
//! session, independent of the Pi v3 wire format in [`super::pi`].
//!
//! Ownership: every value is owned and `Clone`; nothing here is shared or
//! mutated behind a handle. Concurrency: plain data. Errors: none of these
//! types validate; the rules live in [`super::context`] and the native store.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{Usage, zero_time};

/// The session format version this build reads and writes.
pub const CURRENT_VERSION: i64 = super::pi::PI_SESSION_VERSION;

/// Provider, model and profile in force at a point in the session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeMetadata {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    pub provider: String,
    pub model: String,
}

/// File paths a compaction checkpoint carried forward.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionDetails {
    #[serde(rename = "readFiles", skip_serializing_if = "Vec::is_empty")]
    pub read_files: Vec<String>,
    #[serde(rename = "modifiedFiles", skip_serializing_if = "Vec::is_empty")]
    pub modified_files: Vec<String>,
    #[serde(rename = "omittedReadFiles", skip_serializing_if = "is_zero")]
    pub omitted_read_files: i64,
    #[serde(rename = "omittedModifiedFiles", skip_serializing_if = "is_zero")]
    pub omitted_modified_files: i64,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// A compaction a caller asks the session to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionCheckpoint {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: i64,
    pub usage: Option<Usage>,
    pub details: CompactionDetails,
    pub created_at: DateTime<Utc>,
}

impl Default for CompactionCheckpoint {
    fn default() -> Self {
        Self {
            summary: String::new(),
            first_kept_entry_id: String::new(),
            tokens_before: 0,
            usage: None,
            details: CompactionDetails::default(),
            created_at: zero_time(),
        }
    }
}

/// The compaction that is currently in force on the active path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionMetadata {
    pub id: String,
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: i64,
    pub usage: Option<Usage>,
    pub details: CompactionDetails,
    /// True when the checkpoint carries a synthetic retained tail instead of
    /// anchoring to an entry that is still on the active path.
    pub retained_tail_only: bool,
    pub first_post_checkpoint_message_id: String,
}

/// Otto's header view of a session file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: i64,
    pub id: String,
    pub workspace: String,
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub created_at: DateTime<Utc>,
}

impl Default for Header {
    fn default() -> Self {
        Self {
            version: 0,
            id: String::new(),
            workspace: String::new(),
            provider: String::new(),
            profile: String::new(),
            model: String::new(),
            created_at: zero_time(),
        }
    }
}

/// A non-fatal problem found while reading a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub message: String,
}

impl Warning {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// One row of a session listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub path: String,
    pub id: String,
    pub cwd: String,
    pub name: String,
    pub created: DateTime<Utc>,
    pub modified: DateTime<Utc>,
    pub message_count: i64,
    pub last_user_text: String,
    pub profile: String,
    pub provider: String,
    pub model: String,
    pub current: bool,
}

impl Default for SessionInfo {
    fn default() -> Self {
        Self {
            path: String::new(),
            id: String::new(),
            cwd: String::new(),
            name: String::new(),
            created: zero_time(),
            modified: zero_time(),
            message_count: 0,
            last_user_text: String::new(),
            profile: String::new(),
            provider: String::new(),
            model: String::new(),
            current: false,
        }
    }
}

/// The result of listing a workspace's sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListResult {
    pub sessions: Vec<SessionInfo>,
    /// Candidates that could not be read; they are counted, never surfaced.
    pub skipped: i64,
}

/// Token accounting a frontend can display without reading the whole session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub aggregate_usage: Usage,
    pub aggregate_usage_present: bool,
    pub context_input_tokens: i64,
    pub context_input_tokens_present: bool,
    pub context_input_tokens_pending: bool,
}
