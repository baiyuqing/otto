//! Stable workflow records shared by services and repository adapters.
use crate::subagent::WritePolicy;
use serde::{Deserialize, Serialize};
use std::time::Duration;
pub(crate) const MAX_TEXT_BYTES: usize = 64 << 10;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    pub description: String,
    /// Construction-time definition location; old snapshots have no source.
    #[serde(default)]
    pub source: String,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub agents: Vec<AgentSnapshot>,
    pub hash: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: String,
    pub context: String,
    #[serde(default)]
    pub write_policy: SnapshotWritePolicy,
    #[serde(default)]
    pub write_paths: Vec<String>,
    pub body: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotWritePolicy {
    ReadOnly,
    ProposeOnly,
    #[default]
    SingleWriter,
    OwnedPaths,
}

impl SnapshotWritePolicy {
    pub(crate) fn from_subagent(policy: WritePolicy) -> Self {
        match policy {
            WritePolicy::ReadOnly => Self::ReadOnly,
            WritePolicy::ProposeOnly => Self::ProposeOnly,
            WritePolicy::SingleWriter => Self::SingleWriter,
            WritePolicy::OwnedPaths => Self::OwnedPaths,
        }
    }

    pub(crate) fn as_subagent(self) -> WritePolicy {
        match self {
            Self::ReadOnly => WritePolicy::ReadOnly,
            Self::ProposeOnly => WritePolicy::ProposeOnly,
            Self::SingleWriter => WritePolicy::SingleWriter,
            Self::OwnedPaths => WritePolicy::OwnedPaths,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub kind: StepKind,
    pub agent: String,
    pub prompt: String,
    pub needs: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    #[default]
    Agent,
    Handoff,
    Approval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Waiting,
    Paused,
    Succeeded,
    Failed,
    Canceled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Ready,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Canceled,
    Interrupted,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Handoff => "handoff",
            Self::Approval => "approval",
        }
    }
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Run {
    pub id: String,
    pub workflow: String,
    pub workspace: String,
    pub profile: String,
    pub provider: String,
    pub model: String,
    pub input: String,
    pub forked_from_run_id: Option<String>,
    pub forked_from_event_seq: Option<i64>,
    pub forked_from_step_id: Option<String>,
    pub status: RunStatus,
    pub steps: Vec<StepRecord>,
    #[serde(skip_serializing)]
    pub definition: Definition,
    /// Creation timestamp; also the cursor for [`ListQuery::before`].
    pub created_at: String,
}

/// Default page size for a run listing.
pub const DEFAULT_PAGE_LIMIT: usize = 50;
/// Upper bound on a caller-requested page size.
pub const MAX_PAGE_LIMIT: usize = 200;

/// Bounds one read of a workspace's runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// Cap on returned runs; `None` returns every run in the workspace.
    pub limit: Option<usize>,
    /// Return runs created strictly before this `created_at` cursor.
    pub before: Option<String>,
}

impl ListQuery {
    pub fn page(limit: usize, before: Option<String>) -> Self {
        Self {
            limit: Some(limit),
            before,
        }
    }

    pub fn all() -> Self {
        Self::default()
    }
}

/// One page of runs plus the cursor for the next page. `next_before` is empty
/// on the last page, matching the task listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunPage {
    pub runs: Vec<Run>,
    pub next_before: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StepRecord {
    pub id: String,
    pub kind: StepKind,
    pub agent: String,
    pub prompt: String,
    pub needs: Vec<String>,
    pub status: StepStatus,
    pub attempt: u32,
    pub result: String,
    pub error: String,
    pub transcript_path: String,
    pub source_run_id: Option<String>,
    pub source_step_id: Option<String>,
    pub source_attempt: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub run_id: String,
    pub step_id: String,
    pub prompt: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EventRecord {
    pub seq: i64,
    pub run_id: String,
    pub occurred_at: String,
    pub kind: String,
    pub step_id: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub profile: String,
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub run_id: String,
    pub step_id: String,
    pub agent: String,
    pub agent_definition: AgentSnapshot,
    pub prompt: String,
    pub attempt: u32,
    pub session_id: String,
    pub transcript_root: String,
    pub transcript_path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeadlinePolicy {
    pub step_timeout: Option<Duration>,
    pub cancellation_grace: Duration,
}

impl Default for DeadlinePolicy {
    fn default() -> Self {
        Self {
            step_timeout: None,
            cancellation_grace: Duration::from_secs(5),
        }
    }
}
