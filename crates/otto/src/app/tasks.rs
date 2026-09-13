//! The frontend-safe read/control view of a runner's sub-agent tasks.
//!
//! Port of the `TaskLister`/`TaskView` pair in `internal/app/controller.go`
//! and of the `agent.Task` record in `internal/agent/tasks.go` that the HTTP
//! routes serialize.
//!
//! Ownership: phase 7 owns the registry itself. This module owns only the
//! record shape and the read/control contract, because `otto::server` needs
//! both to answer `/v1/sessions/{id}/tasks` before the registry exists.
//! [`task_view`] is the one seam phase 7 replaces: it asks a [`Runner`] for
//! its registry and today always answers `None`, which is exactly what Go's
//! `Controller.Tasks` returns for a runner that tracks no tasks.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use otto_core::model::{Message, Usage};
use serde::Serialize;

use crate::cli::runtime_builder::Runner;

/// Go's `agent.ErrTaskFinished`. The server maps it to 409 `task_done`.
pub const TASK_FINISHED: &str = "task already finished";
/// Go's `agent.ErrTaskNotFound`.
pub const TASK_NOT_FOUND: &str = "task not found";

/// Port of `agent.TaskStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Canceled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    /// Port of `agent.Task.Final`: the statuses no further update can leave.
    pub fn final_status(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Canceled)
    }
}

/// One sub-agent task. Field order matches Go's `taskWire` so the JSON the
/// server writes is byte-compatible.
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub agent: String,
    pub description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    pub steps: i64,
    pub tool_calls: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_tool: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_text: String,
    pub usage: Usage,
    pub usage_present: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub result: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// The read/control contract a frontend gets. Port of `app.TaskView`.
///
/// Concurrency: an implementation is shared with every running task, so
/// every method takes `&self`.
pub trait TaskView: Send + Sync {
    /// Every task in creation order.
    fn list(&self) -> Vec<Task>;

    /// One task by id or by name.
    fn get(&self, reference: &str) -> Option<Task>;

    /// The child session transcript of one task, when it has one.
    fn history(&self, reference: &str) -> Option<Vec<Message>>;

    /// Requests cancellation. [`TASK_FINISHED`] when the task already
    /// reached a final status, [`TASK_NOT_FOUND`] when the reference is
    /// unknown.
    fn cancel(&self, reference: &str) -> Result<(), String>;

    /// Pending notifications waiting to be delivered by a wake turn.
    fn pending(&self) -> usize;
}

/// A registry that holds no tasks. Mirrors
/// [`otto_core::agent::tasks::NoTasks`] on the frontend side of the
/// boundary, so a frontend can be exercised without phase 7's registry.
#[derive(Debug, Default)]
pub struct NoTasks;

impl TaskView for NoTasks {
    fn list(&self) -> Vec<Task> {
        Vec::new()
    }

    fn get(&self, _reference: &str) -> Option<Task> {
        None
    }

    fn history(&self, _reference: &str) -> Option<Vec<Message>> {
        None
    }

    fn cancel(&self, _reference: &str) -> Result<(), String> {
        Err(TASK_NOT_FOUND.to_string())
    }

    fn pending(&self) -> usize {
        0
    }
}

/// The task registry of one runner, or `None` when it tracks no tasks.
///
/// Port of the `taskOwner` type assertion in `internal/app/controller.go`.
/// Phase 7 replaces the body; every caller already handles `None`, which is
/// what Go reports for a runner without a registry.
pub fn task_view(_runner: &Runner) -> Option<Arc<dyn TaskView>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_succeeded_failed_and_canceled_are_final() {
        assert!(!TaskStatus::Queued.final_status());
        assert!(!TaskStatus::Running.final_status());
        assert!(TaskStatus::Succeeded.final_status());
        assert!(TaskStatus::Failed.final_status());
        assert!(TaskStatus::Canceled.final_status());
    }

    #[test]
    fn the_empty_registry_answers_every_read_with_nothing() {
        let tasks = NoTasks;
        assert!(tasks.list().is_empty());
        assert!(tasks.get("a").is_none());
        assert!(tasks.history("a").is_none());
        assert_eq!(tasks.cancel("a").expect_err("cancel"), TASK_NOT_FOUND);
        assert_eq!(tasks.pending(), 0);
    }

    #[test]
    fn a_task_omits_every_empty_optional_field() {
        let task = Task {
            id: "t1".to_string(),
            name: String::new(),
            agent: "reviewer".to_string(),
            description: "check the diff".to_string(),
            model: String::new(),
            status: TaskStatus::Running,
            created_at: DateTime::from_timestamp(0, 0).expect("epoch"),
            started_at: None,
            finished_at: None,
            steps: 2,
            tool_calls: 1,
            last_tool: String::new(),
            last_text: String::new(),
            usage: Usage::default(),
            usage_present: false,
            result: String::new(),
            error: String::new(),
        };
        assert_eq!(
            serde_json::to_string(&task).expect("json"),
            r#"{"id":"t1","agent":"reviewer","description":"check the diff","status":"running","created_at":"1970-01-01T00:00:00Z","steps":2,"tool_calls":1,"usage":{"input_tokens":0,"output_tokens":0},"usage_present":false}"#
        );
    }
}
