//! The frontend-safe read/control view of a runner's sub-agent tasks.
//!
//! Port of the `TaskLister`/`TaskView` pair in `internal/app/controller.go`
//! and of the `agent.Task` record in `internal/agent/tasks.go` that the HTTP
//! routes serialize.
//!
//! Ownership: [`crate::subagent::tasks::Tasks`] owns the registry itself.
//! This module owns the wire record and the read/control contract, and
//! [`task_view`] adapts one to the other, the way Go's `taskView` wraps
//! `*agent.Tasks`. A runner without a registry answers `None`, which is what
//! Go's `Controller.Tasks` returns for a runner that tracks no tasks.
//!
//! Divergence from Go: Go's `agent.Task` is one record shared by the
//! registry, the frontends and `internal/subagent`'s formatters, and
//! `internal/server` converts it to `taskWire` at the edge. Here [`Task`] is
//! that wire record, so it omits `prompt` and `context`. The REPL therefore
//! reads the concrete registry rather than this view: `subagent::format`
//! falls back to the prompt when a task has no description.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use otto_core::model::{Message, Usage};
use serde::Serialize;

use crate::cli::runtime_builder::Runner;
use crate::subagent::tasks::{TaskError, TaskStatus as SubagentStatus, Tasks as Registry};

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

/// The wire record for one registry task. Port of `server.toTaskWire`.
///
/// `created_at` is set by the sub-agent runner on every real task; a record
/// that never got one serializes the Unix epoch, where Go writes its zero
/// `time.Time`.
fn wire(task: &crate::subagent::tasks::Task) -> Task {
    Task {
        id: task.id.clone(),
        name: task.name.clone(),
        agent: task.agent.clone(),
        description: task.description.clone(),
        model: task.model.clone(),
        status: match task.status {
            SubagentStatus::Queued => TaskStatus::Queued,
            SubagentStatus::Running => TaskStatus::Running,
            SubagentStatus::Succeeded => TaskStatus::Succeeded,
            SubagentStatus::Failed => TaskStatus::Failed,
            SubagentStatus::Canceled => TaskStatus::Canceled,
        },
        created_at: task.created_at.unwrap_or_default(),
        started_at: task.started_at,
        finished_at: task.finished_at,
        steps: task.steps,
        tool_calls: task.tool_calls,
        last_tool: task.last_tool.clone(),
        last_text: task.last_text.clone(),
        usage: task.usage,
        usage_present: task.usage_present,
        result: task.result.clone(),
        error: task.error.clone(),
    }
}

/// Port of Go's `taskView`, the adapter `Controller.Tasks` hands a frontend.
impl TaskView for Registry {
    fn list(&self) -> Vec<Task> {
        Registry::list(self).iter().map(wire).collect()
    }

    fn get(&self, reference: &str) -> Option<Task> {
        Registry::get(self, reference).as_ref().map(wire)
    }

    fn history(&self, reference: &str) -> Option<Vec<Message>> {
        Registry::history(self, reference)
    }

    fn cancel(&self, reference: &str) -> Result<(), String> {
        Registry::cancel(self, reference).map_err(|error| match error {
            TaskError::Finished(_) => TASK_FINISHED.to_string(),
            TaskError::NotFound(_) => TASK_NOT_FOUND.to_string(),
            other => other.to_string(),
        })
    }

    fn pending(&self) -> usize {
        Registry::pending(self)
    }
}

/// The task registry of one runner, or `None` when it tracks no tasks.
///
/// Port of the `taskOwner` type assertion in `internal/app/controller.go`.
pub fn task_view(runner: &Runner) -> Option<Arc<dyn TaskView>> {
    let tasks = runner.tasks.clone()?;
    Some(tasks as Arc<dyn TaskView>)
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

    /// The adapter over the real registry, Go's `taskView`.
    #[test]
    fn the_view_maps_every_registry_read_onto_the_wire_record() {
        let registry = Registry::new();
        let added = registry
            .add(
                crate::subagent::tasks::Task {
                    name: "lint".to_string(),
                    agent: "reviewer".to_string(),
                    description: "check the diff".to_string(),
                    prompt: "the prompt the wire record omits".to_string(),
                    model: "gpt-5".to_string(),
                    created_at: DateTime::from_timestamp(0, 0),
                    ..crate::subagent::tasks::Task::default()
                },
                None,
                None,
            )
            .expect("add");
        let view: &dyn TaskView = &registry;

        let listed = view.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, added.id);
        assert_eq!(listed[0].status, TaskStatus::Queued);
        // A name resolves the same way an id does.
        assert_eq!(view.get("lint").expect("by name").id, added.id);
        // A task with no history hook has an empty transcript, not none.
        assert!(view.history(&added.id).expect("known task").is_empty());
        assert!(view.history("missing").is_none());
        assert_eq!(view.pending(), 0);
        assert_eq!(view.cancel("missing").expect_err("unknown"), TASK_NOT_FOUND);

        registry.finish(
            &added.id,
            SubagentStatus::Succeeded,
            DateTime::from_timestamp(1, 0).expect("epoch"),
            "done",
            "",
        );
        assert_eq!(view.cancel(&added.id).expect_err("final"), TASK_FINISHED);
        assert_eq!(
            view.get(&added.id).expect("task").status,
            TaskStatus::Succeeded
        );
    }

    #[test]
    fn a_pushed_notification_is_pending_on_the_view() {
        let registry = Registry::new();
        registry
            .notifications()
            .push(otto_core::agent::inbox::Notification {
                text: "[task-notification] task t1 succeeded".to_string(),
                ..otto_core::agent::inbox::Notification::default()
            });
        let view: &dyn TaskView = &registry;
        assert_eq!(view.pending(), 1);
    }
}
