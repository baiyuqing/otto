//! The per-session sub-agent task registry.
//!
//! A [`Tasks`] registry tracks one record per delegated task, the cancellation
//! token and history hook that belong to it, and the parent's notification
//! inbox. It is the real implementation behind
//! [`otto_core::agent::tasks::TaskRegistry`], whose only method the turn loop
//! calls is `close`.
//!
//! Ownership: the registry owns its records. Every accessor returns a clone, so
//! a caller never holds a reference into the locked state.
//!
//! Concurrency: one mutex guards everything. `history` and `cancel` run their
//! stored hooks after releasing it, because a hook may call back into the
//! registry. Waiters park on a [`Notify`] that every state change wakes.
//!
//! Errors: the lifecycle transitions (`mark_running`, `record_provider_step`,
//! `record_tool_call`, `finish`) are no-ops for an unknown id or an invalid
//! transition. Only `add`, `cancel` and `wait` report errors.
//!
//! The cancel hook is the child's [`CancellationToken`], which is what the
//! runner has and what a test can observe.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use otto_core::agent::inbox::Inbox;
use otto_core::agent::tasks::TaskRegistry;
use otto_core::model::{Message, Usage};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

/// A sub-agent task's position in its lifecycle:
/// queued -> running -> {succeeded, failed, canceled}.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TaskStatus {
    #[default]
    Queued,
    Running,
    Succeeded,
    Failed,
    Canceled,
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
        }
    }

    /// Whether this is a terminal status.
    pub fn is_final(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Canceled)
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One sub-agent task record, as shown by `agent_status` and the frontends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Task {
    pub id: String,
    pub name: String,
    /// The catalog definition name, or empty for the default sub-agent.
    pub agent: String,
    pub description: String,
    pub prompt: String,
    pub context: String,
    pub model: String,
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
    pub result: String,
    pub error: String,
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
}

/// What the registry stores beside the record.
struct Entry {
    task: Task,
    cancel: Option<CancellationToken>,
    #[allow(clippy::type_complexity)]
    history: Option<Arc<dyn Fn() -> Vec<Message> + Send + Sync>>,
    done: Arc<Notify>,
}

#[derive(Default)]
struct State {
    closed: bool,
    counter: u64,
    order: Vec<String>,
    entries: HashMap<String, Entry>,
    names: HashMap<String, String>,
}

/// The coalescing change signal, shared by the registry and its inbox
/// callback. Closing the registry drops the sender, which closes the channel.
#[derive(Debug)]
struct Updates {
    sender: Mutex<Option<watch::Sender<u64>>>,
}

impl Updates {
    fn signal(&self) {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sender) = sender.as_ref() {
            sender.send_modify(|counter| *counter += 1);
        }
    }
}

/// The per-session sub-agent task registry.
pub struct Tasks {
    state: Mutex<State>,
    inbox: Arc<Inbox>,
    updates: Arc<Updates>,
    updates_receiver: watch::Receiver<u64>,
}

impl std::fmt::Debug for Tasks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Tasks")
            .field("len", &self.list().len())
            .finish_non_exhaustive()
    }
}

impl Default for Tasks {
    fn default() -> Self {
        Self::new()
    }
}

impl Tasks {
    /// Creates an empty registry.
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(0);
        let updates = Arc::new(Updates {
            sender: Mutex::new(Some(sender)),
        });
        // The inbox change callback signals the same update channel the
        // registry's own mutations do, so a frontend watches one source.
        let inbox_updates = Arc::clone(&updates);
        let inbox = Arc::new(Inbox::new(Some(Box::new(move || {
            inbox_updates.signal();
        }))));
        Self {
            state: Mutex::new(State::default()),
            inbox,
            updates,
            updates_receiver: receiver,
        }
    }

    /// Registers a new task, assigning it the next `tN` id and
    /// [`TaskStatus::Queued`] regardless of the status passed in.
    ///
    /// `task.name`, after trimming, is validated and reserved for the life of
    /// the registry (finished tasks included): empty means no name; otherwise
    /// it must be 1 to 64 letters, digits, `_` or `-` starting with a letter
    /// or digit, must not look like a task id, and must not already be in use.
    /// Any validation error leaves the registry unchanged: no id is consumed.
    pub fn add(
        &self,
        mut task: Task,
        cancel: Option<CancellationToken>,
        history: Option<Arc<dyn Fn() -> Vec<Message> + Send + Sync>>,
    ) -> Result<Task, TaskError> {
        let created = {
            let mut state = self.lock();
            if state.closed {
                return Err(TaskError::Closed);
            }
            let name = task.name.trim().to_string();
            if !name.is_empty() {
                if name.len() > 64 || !is_task_name(&name) {
                    return Err(TaskError::Invalid(format!(
                        "task \"{name}\" is invalid: 1 to 64 letters, digits, '_' or '-', starting with a letter or digit"
                    )));
                }
                if is_reserved_task_name(&name) {
                    return Err(TaskError::Invalid(format!(
                        "task \"{name}\" is reserved for task ids"
                    )));
                }
                if let Some(existing) = state.names.get(&name) {
                    return Err(TaskError::Invalid(format!(
                        "task \"{name}\" already used by {existing}"
                    )));
                }
            }
            state.counter += 1;
            task.id = format!("t{}", state.counter);
            task.name = name.clone();
            task.status = TaskStatus::Queued;
            task.started_at = None;
            task.finished_at = None;
            task.steps = 0;
            task.tool_calls = 0;
            task.last_tool = String::new();
            task.last_text = String::new();
            task.usage = Usage::default();
            task.usage_present = false;
            task.result = String::new();
            task.error = String::new();
            let id = task.id.clone();
            let created = task.clone();
            state.entries.insert(
                id.clone(),
                Entry {
                    task,
                    cancel,
                    history,
                    done: Arc::new(Notify::new()),
                },
            );
            state.order.push(id.clone());
            if !name.is_empty() {
                state.names.insert(name, id);
            }
            created
        };
        self.signal();
        Ok(created)
    }

    /// Moves a queued task to running. Invalid or repeated transitions,
    /// including unknown ids, are no-ops.
    pub fn mark_running(&self, id: &str, started_at: DateTime<Utc>) {
        let changed = {
            let mut state = self.lock();
            match state.entries.get_mut(id) {
                Some(entry) if entry.task.status == TaskStatus::Queued => {
                    entry.task.status = TaskStatus::Running;
                    entry.task.started_at = Some(started_at);
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.signal();
        }
    }

    /// Records one completed provider round trip. It only applies while the
    /// task is running; invalid or unknown updates are no-ops.
    pub fn record_provider_step(&self, id: &str, usage: Usage, text: &str, usage_present: bool) {
        if usage.validate().is_err() {
            return;
        }
        let changed = {
            let mut state = self.lock();
            match state.entries.get_mut(id) {
                Some(entry) if entry.task.status == TaskStatus::Running => {
                    entry.task.steps += 1;
                    if usage_present {
                        entry.task.usage_present = true;
                        entry.task.usage.input_tokens += usage.input_tokens;
                        entry.task.usage.output_tokens += usage.output_tokens;
                        entry.task.usage.cached_input_tokens += usage.cached_input_tokens;
                    }
                    entry.task.last_text = text.to_string();
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.signal();
        }
    }

    /// Records the latest tool call while the task is running. Invalid or
    /// unknown updates are no-ops.
    pub fn record_tool_call(&self, id: &str, last_tool: &str) {
        let changed = {
            let mut state = self.lock();
            match state.entries.get_mut(id) {
                Some(entry) if entry.task.status == TaskStatus::Running => {
                    entry.task.tool_calls += 1;
                    entry.task.last_tool = last_tool.to_string();
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.signal();
        }
    }

    /// Moves a queued or running task to a terminal status, records its final
    /// fields, and releases [`Tasks::wait`] callers. Repeated or invalid
    /// transitions, including unknown ids, are no-ops.
    pub fn finish(
        &self,
        id: &str,
        status: TaskStatus,
        finished_at: DateTime<Utc>,
        result: &str,
        task_error: &str,
    ) {
        if !status.is_final() {
            return;
        }
        let done = {
            let mut state = self.lock();
            match state.entries.get_mut(id) {
                Some(entry) if !entry.task.is_final() => {
                    entry.task.status = status;
                    entry.task.finished_at = Some(finished_at);
                    entry.task.result = result.to_string();
                    entry.task.error = task_error.to_string();
                    Some(Arc::clone(&entry.done))
                }
                _ => None,
            }
        };
        if let Some(done) = done {
            done.notify_waiters();
            self.signal();
        }
    }

    /// A copy of every task, in creation order.
    pub fn list(&self) -> Vec<Task> {
        let state = self.lock();
        state
            .order
            .iter()
            .filter_map(|id| state.entries.get(id).map(|entry| entry.task.clone()))
            .collect()
    }

    /// A copy of one task, resolving `reference` as a task id or a task name.
    pub fn get(&self, reference: &str) -> Option<Task> {
        let state = self.lock();
        Self::entry(&state, reference).map(|entry| entry.task.clone())
    }

    /// The task's child session messages, calling the stored history hook
    /// outside the registry lock.
    ///
    /// Returns `Some(vec![])` when the task exists but carries no hook, and
    /// `None` when `reference` names no task.
    pub fn history(&self, reference: &str) -> Option<Vec<Message>> {
        let hook = {
            let state = self.lock();
            let entry = Self::entry(&state, reference)?;
            entry.history.clone()
        };
        Some(match hook {
            Some(hook) => hook(),
            None => Vec::new(),
        })
    }

    /// Cancels the task outside the registry lock. It errors for an unknown
    /// reference or an already-final task; it does not itself change the
    /// task's status.
    pub fn cancel(&self, reference: &str) -> Result<(), TaskError> {
        let token = {
            let state = self.lock();
            let Some(entry) = Self::entry(&state, reference) else {
                return Err(TaskError::NotFound(reference.to_string()));
            };
            if entry.task.is_final() {
                return Err(TaskError::Finished(reference.to_string()));
            }
            entry.cancel.clone()
        };
        if let Some(token) = token {
            token.cancel();
        }
        Ok(())
    }

    /// Blocks until the task reaches a final status or `cancel` fires.
    pub async fn wait(
        &self,
        reference: &str,
        cancel: &CancellationToken,
    ) -> Result<Task, TaskError> {
        let (id, done) = {
            let state = self.lock();
            let Some(entry) = Self::entry(&state, reference) else {
                return Err(TaskError::NotFound(reference.to_string()));
            };
            (entry.task.id.clone(), Arc::clone(&entry.done))
        };
        loop {
            // enable() registers the waiter before the status is re-read, so a
            // finish between the two wakes this call instead of being missed.
            let notified = done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(task) = self.get(&id)
                && task.is_final()
            {
                return Ok(task);
            }
            tokio::select! {
                () = notified => {}
                () = cancel.cancelled() => return Err(TaskError::Canceled),
            }
        }
    }

    /// The number of notifications waiting for the parent.
    pub fn pending(&self) -> usize {
        self.inbox.len()
    }

    /// The parent's notification inbox.
    pub fn notifications(&self) -> &Arc<Inbox> {
        &self.inbox
    }

    /// A receiver signaled on every registry or inbox change. Its sender is
    /// dropped by [`Tasks::close`], so a closed registry's receiver reports a
    /// closed channel.
    pub fn updates(&self) -> watch::Receiver<u64> {
        self.updates_receiver.clone()
    }

    /// Whether the registry has been closed.
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    fn entry<'a>(state: &'a MutexGuard<'a, State>, reference: &str) -> Option<&'a Entry> {
        if let Some(entry) = state.entries.get(reference) {
            return Some(entry);
        }
        let id = state.names.get(reference)?;
        state.entries.get(id)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn signal(&self) {
        self.updates.signal();
    }
}

impl TaskRegistry for Tasks {
    /// Cancels every non-final task and closes the update channel. Idempotent.
    fn close(&self) {
        let cancels = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            let ids = state.order.clone();
            ids.iter()
                .filter_map(|id| state.entries.get(id))
                .filter(|entry| !entry.task.is_final())
                .filter_map(|entry| entry.cancel.clone())
                .collect::<Vec<_>>()
        };
        self.updates
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        for token in cancels {
            token.cancel();
        }
    }
}

/// Whether `name` is `^[A-Za-z0-9][A-Za-z0-9_-]*$`.
fn is_task_name(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    characters
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
}

/// Whether `name` has the shape of an auto-assigned task id, `^t[0-9]+$`.
fn is_reserved_task_name(name: &str) -> bool {
    match name.strip_prefix('t') {
        Some(digits) => !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use otto_core::model::{Block, BlockType, Role};

    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("a valid timestamp")
    }

    fn user_message(text: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![Block {
                block_type: BlockType::Text,
                text: text.to_string(),
                ..Block::default()
            }],
            ..Message::default()
        }
    }

    #[tokio::test]
    async fn a_task_moves_from_queued_through_running_to_canceled() {
        let tasks = Arc::new(Tasks::new());
        let cancel = CancellationToken::new();
        let history = vec![user_message("child prompt")];
        let hook = {
            let history = history.clone();
            Arc::new(move || history.clone()) as Arc<dyn Fn() -> Vec<Message> + Send + Sync>
        };
        let updates = tasks.updates();
        let task = tasks
            .add(
                Task {
                    agent: "explorer".into(),
                    prompt: "find sessions".into(),
                    ..Task::default()
                },
                Some(cancel.clone()),
                Some(hook),
            )
            .expect("the first task is valid");
        assert_eq!(task.id, "t1");
        assert_eq!(task.status, TaskStatus::Queued);
        assert!(
            updates.has_changed().expect("the channel is open"),
            "add must signal updates"
        );

        assert_eq!(tasks.get("t1").expect("t1 exists").prompt, "find sessions");
        let stored = tasks.history("t1").expect("t1 exists");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text(), "child prompt");

        tasks.mark_running("t1", at(1));
        tasks.record_provider_step("t1", Usage::default(), "", false);
        tasks.record_provider_step("t1", Usage::default(), "", true);

        let waiter = {
            let tasks = Arc::clone(&tasks);
            tokio::spawn(async move { tasks.wait("t1", &CancellationToken::new()).await })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(20), async {})
                .await
                .is_ok()
        );
        assert!(!waiter.is_finished(), "wait returned before the task ended");

        tasks.cancel("t1").expect("a running task can be canceled");
        assert!(cancel.is_cancelled(), "cancel must reach the token");
        tasks.finish("t1", TaskStatus::Canceled, at(2), "", "");
        let waited = waiter
            .await
            .expect("the waiter task ran")
            .expect("the wait succeeded");
        assert_eq!(waited.status, TaskStatus::Canceled);

        let list = tasks.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, TaskStatus::Canceled);
        assert_eq!(list[0].steps, 2);
        assert!(list[0].usage_present, "a present-usage step sets the flag");
        assert_eq!(list[0].usage, Usage::default());

        assert_eq!(
            tasks.cancel("t1"),
            Err(TaskError::Finished("t1".into())),
            "a finished task cannot be canceled"
        );
        assert_eq!(
            tasks.cancel("missing"),
            Err(TaskError::NotFound("missing".into()))
        );

        tasks
            .notifications()
            .push(otto_core::agent::inbox::Notification {
                task_id: "t1".into(),
                kind: Some(otto_core::agent::inbox::NotificationKind::TaskFinished),
                text: "x".into(),
                usage: None,
            });
        assert_eq!(tasks.pending(), 1);

        let second_cancel = CancellationToken::new();
        let second = tasks
            .add(
                Task {
                    prompt: "second".into(),
                    ..Task::default()
                },
                Some(second_cancel.clone()),
                None,
            )
            .expect("a second task is valid");
        assert_eq!(second.id, "t2");

        tasks.close();
        assert!(
            second_cancel.is_cancelled(),
            "close must cancel the non-final task"
        );
        assert!(
            tasks.updates().has_changed().is_err(),
            "close must close the update channel"
        );
        assert_eq!(
            tasks.add(Task::default(), None, None),
            Err(TaskError::Closed)
        );
        tasks.close();
    }

    #[test]
    fn add_clears_runtime_fields_and_only_valid_transitions_apply() {
        let tasks = Tasks::new();
        let created = tasks
            .add(
                Task {
                    prompt: "inspect".into(),
                    status: TaskStatus::Succeeded,
                    started_at: Some(at(1)),
                    finished_at: Some(at(2)),
                    steps: 9,
                    tool_calls: 8,
                    last_tool: "stale".into(),
                    last_text: "stale".into(),
                    usage: Usage {
                        input_tokens: 10,
                        ..Usage::default()
                    },
                    result: "stale".into(),
                    error: "stale".into(),
                    ..Task::default()
                },
                None,
                None,
            )
            .expect("the task is valid");
        assert_eq!(
            created,
            Task {
                id: "t1".into(),
                prompt: "inspect".into(),
                status: TaskStatus::Queued,
                ..Task::default()
            },
            "add must drop every stale runtime field"
        );

        tasks.mark_running("t1", at(10));
        tasks.mark_running("t1", at(11));
        tasks.record_tool_call("t1", "grep \"session\"");
        tasks.record_provider_step(
            "t1",
            Usage {
                input_tokens: 3,
                output_tokens: 2,
                ..Usage::default()
            },
            "found it",
            true,
        );

        let started = tasks.get("t1").expect("t1 exists");
        assert_eq!(started.status, TaskStatus::Running);
        assert_eq!(
            started.started_at,
            Some(at(10)),
            "a repeat start is ignored"
        );
        assert_eq!(started.tool_calls, 1);
        assert_eq!(started.steps, 1);
        assert_eq!(started.last_tool, "grep \"session\"");
        assert_eq!(started.last_text, "found it");

        tasks.finish("t1", TaskStatus::Succeeded, at(20), "done", "");
        tasks.finish("t1", TaskStatus::Failed, at(30), "overwritten", "bad");
        tasks.record_tool_call("t1", "late");
        tasks.record_provider_step(
            "t1",
            Usage {
                input_tokens: 1,
                ..Usage::default()
            },
            "late",
            true,
        );
        let finished = tasks.get("t1").expect("t1 exists");
        assert_eq!(finished.status, TaskStatus::Succeeded);
        assert_eq!(finished.result, "done");
        assert_eq!(finished.error, "");
        assert_eq!(finished.finished_at, Some(at(20)));
        assert_eq!(finished.tool_calls, 1, "updates after finish are no-ops");
    }

    #[tokio::test]
    async fn a_name_resolves_to_the_same_task_as_its_id() {
        let tasks = Arc::new(Tasks::new());
        let cancel = CancellationToken::new();
        let hook = Arc::new(|| vec![user_message("child prompt")])
            as Arc<dyn Fn() -> Vec<Message> + Send + Sync>;
        let task = tasks
            .add(
                Task {
                    name: "lint-check".into(),
                    prompt: "lint".into(),
                    ..Task::default()
                },
                Some(cancel.clone()),
                Some(hook),
            )
            .expect("the name is valid");
        assert_eq!(task.id, "t1");
        assert_eq!(task.name, "lint-check");
        assert_eq!(tasks.get("lint-check"), tasks.get("t1"));
        let history = tasks.history("lint-check").expect("the name resolves");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text(), "child prompt");

        let waiter = {
            let tasks = Arc::clone(&tasks);
            tokio::spawn(async move { tasks.wait("lint-check", &CancellationToken::new()).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "wait returned before the task ended");

        tasks.cancel("lint-check").expect("the name resolves");
        assert!(cancel.is_cancelled());
        tasks.finish("t1", TaskStatus::Canceled, at(2), "", "");
        waiter
            .await
            .expect("the waiter task ran")
            .expect("the wait succeeded");
    }

    #[test]
    fn names_are_validated_and_reserved() {
        let tasks = Tasks::new();
        tasks
            .add(
                Task {
                    name: "lint-check".into(),
                    ..Task::default()
                },
                None,
                None,
            )
            .expect("the first name is free");
        let duplicate = tasks
            .add(
                Task {
                    name: "lint-check".into(),
                    ..Task::default()
                },
                None,
                None,
            )
            .expect_err("a duplicate name is rejected");
        assert_eq!(
            duplicate.to_string(),
            "task \"lint-check\" already used by t1"
        );

        let reserved = Tasks::new();
        let error = reserved
            .add(
                Task {
                    name: "t7".into(),
                    ..Task::default()
                },
                None,
                None,
            )
            .expect_err("a task-id shape is reserved");
        assert_eq!(error.to_string(), "task \"t7\" is reserved for task ids");
        assert!(
            reserved.get("t7").is_none(),
            "a rejected add must create nothing"
        );

        for name in ["bad name", &"a".repeat(65)] {
            let invalid = Tasks::new();
            let error = invalid
                .add(
                    Task {
                        name: name.to_string(),
                        ..Task::default()
                    },
                    None,
                    None,
                )
                .expect_err("the name is invalid");
            assert!(
                error.to_string().contains("is invalid"),
                "{name:?} error = {error}"
            );
            let after = invalid
                .add(Task::default(), None, None)
                .expect("a rejected add consumes no id");
            assert_eq!(after.id, "t1");
        }

        let unnamed = Tasks::new();
        let first = unnamed.add(Task::default(), None, None).expect("valid");
        let second = unnamed.add(Task::default(), None, None).expect("valid");
        assert_ne!(first.id, second.id);
    }

    #[tokio::test]
    async fn wait_stops_on_cancellation_and_rejects_an_unknown_reference() {
        let tasks = Tasks::new();
        tasks.add(Task::default(), None, None).expect("valid");

        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            tasks.wait("t1", &cancel).await,
            Err(TaskError::Canceled),
            "a cancelled token ends the wait"
        );
        assert_eq!(
            tasks.wait("missing", &CancellationToken::new()).await,
            Err(TaskError::NotFound("missing".into()))
        );
    }
}
