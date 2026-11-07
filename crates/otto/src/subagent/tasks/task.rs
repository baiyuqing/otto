//! The task record, its status and the registry error type.

use super::*;

/// A sub-agent task's position in its lifecycle:
/// queued -> running -> {succeeded, failed, canceled, interrupted}.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TaskStatus {
    #[default]
    Queued,
    Running,
    Succeeded,
    Failed,
    Canceled,
    /// Execution stopped at a durable boundary and may be explicitly resumed.
    Interrupted,
    /// Durable evidence exists, but ownership/spec/repair is not usable.
    /// Non-running and terminal for observation; never execution authority.
    Unavailable,
}

impl TaskStatus {
    /// The status string, as rendered in status output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Interrupted => "interrupted",
            Self::Unavailable => "unavailable",
        }
    }

    /// Whether this is a terminal status.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Canceled | Self::Interrupted | Self::Unavailable
        )
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a task selected its model. Legacy records have no reliable provenance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    Explicit,
    Definition,
    ConfiguredFirst,
    Parent,
    #[default]
    Unknown,
}

impl ModelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Definition => "definition",
            Self::ConfiguredFirst => "configured_first",
            Self::Parent => "parent",
            Self::Unknown => "unknown",
        }
    }
}

/// One sub-agent task record, as shown by `agent_status` and the frontends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Task {
    pub id: String,
    /// Durable execution attempt identity; empty for legacy transcripts.
    pub attempt_id: String,
    pub name: String,
    /// The catalog definition name, or empty for the default sub-agent.
    pub agent: String,
    pub description: String,
    pub prompt: String,
    pub context: String,
    pub model: String,
    pub model_source: ModelSource,
    pub status: TaskStatus,
    pub created_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub steps: i64,
    pub tool_calls: i64,
    pub last_tool: String,
    pub last_text: String,
    pub usage: Usage,
    pub usage_present: bool,
    /// True only when every provider report behind `usage` carried the
    /// cached-input-token breakdown. False while no report exists, and false
    /// for a task read back from `tasks.db`, which does not store it: a reader
    /// then treats the cached count as unknown rather than as a real zero.
    pub cached_input_tokens_present: bool,
    pub result: String,
    pub error: String,
    /// The child transcript file, empty when the child runs in memory.
    pub session_path: String,
}

impl Task {
    /// Whether the task has reached a terminal status.
    pub fn is_final(&self) -> bool {
        self.status.is_final()
    }
}

/// Why a registry call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskError {
    #[error("task registry closed")]
    Closed,
    #[error("task \"{0}\" not found")]
    NotFound(String),
    #[error("task \"{0}\" already finished")]
    Finished(String),
    #[error("{0}")]
    Invalid(String),
    #[error("context canceled")]
    Canceled,
    #[error("message not sent: {0}")]
    NotPersisted(String),
}
