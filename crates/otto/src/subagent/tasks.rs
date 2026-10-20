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
//!
//! An optional [`crate::subagent::record::Recorder`], set at construction via
//! [`Tasks::with_recorder`], mirrors every task this registry changes into
//! `~/.otto/tasks.db`. `add`, `mark_running`, `record_provider_step`,
//! `record_tool_call` and `finish` call it with the task's state after the
//! change, outside the registry mutex, using the fixed
//! [`crate::subagent::record::TaskContext`] passed at construction. The
//! recorder trait cannot fail its caller, so this never affects a task's
//! outcome.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use otto_core::agent::inbox::{Inbox, NotificationKind};
use otto_core::agent::tasks::TaskRegistry;
use otto_core::mailbox::Mailbox;
use otto_core::model::{Message, Usage};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

use super::record::{Recorder, TaskContext};

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
        }
    }

    /// Whether this is a terminal status.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Canceled | Self::Interrupted
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

/// How many `agent_report` calls a child may make on its own initiative over
/// the life of one task, before [`Tasks::send_message`] grants another. See
/// [`crate::subagent::runner::ChildTools::report`].
pub const REPORT_BUDGET: usize = 10;

/// What the registry stores beside the record.
struct Entry {
    task: Task,
    in_tool: bool,
    cancel: Option<CancellationToken>,
    #[allow(clippy::type_complexity)]
    history: Option<Arc<dyn Fn() -> Vec<Message> + Send + Sync>>,
    inbox: Arc<dyn Mailbox>,
    /// Remaining `agent_report` calls, shared with the child's `ChildTools`.
    /// Starts at [`REPORT_BUDGET`] and gains one for each parent message
    /// delivered through [`Tasks::send_message`].
    reports_left: Arc<AtomicUsize>,
    done: Arc<Notify>,
}

#[derive(Default)]
struct State {
    closed: bool,
    /// Set once by [`Tasks::begin_migration`]. Distinct from `closed`
    /// (which migration also sets, via [`TaskRegistry::close`]) so
    /// [`Tasks::is_migrating`] answers "is this a migration close" rather
    /// than just "is this registry closed".
    migrating: bool,
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
    /// `inbox` as the port every consumer uses.
    mailbox: Arc<dyn Mailbox>,
    updates: Arc<Updates>,
    updates_receiver: watch::Receiver<u64>,
    recorder: Option<(Arc<dyn Recorder>, TaskContext)>,
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
        Self::new_inner(None, 0)
    }

    /// Creates an empty registry, without a recorder, whose id counter starts
    /// at `starting_counter`. See [`Tasks::with_recorder_from`].
    pub fn new_from(starting_counter: u64) -> Self {
        Self::new_inner(None, starting_counter)
    }

    /// Creates an empty registry that mirrors every task it changes into
    /// `recorder`, tagged with the fixed `context` (parent session,
    /// workspace, and owning process).
    pub fn with_recorder(recorder: Arc<dyn Recorder>, context: TaskContext) -> Self {
        Self::new_inner(Some((recorder, context)), 0)
    }

    /// Creates an empty registry whose id counter starts at `starting_counter`,
    /// so the next task added gets id `t{starting_counter + 1}`. Used to
    /// resume a session without reusing a task id a prior run already gave
    /// to a child transcript; see [`highest_task_counter`].
    pub fn with_recorder_from(
        recorder: Arc<dyn Recorder>,
        context: TaskContext,
        starting_counter: u64,
    ) -> Self {
        Self::new_inner(Some((recorder, context)), starting_counter)
    }

    fn new_inner(
        recorder: Option<(Arc<dyn Recorder>, TaskContext)>,
        starting_counter: u64,
    ) -> Self {
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
            state: Mutex::new(State {
                counter: starting_counter,
                ..State::default()
            }),
            mailbox: Arc::clone(&inbox) as Arc<dyn Mailbox>,
            inbox,
            updates,
            updates_receiver: receiver,
            recorder,
        }
    }

    /// Calls the recorder, if any, with `id`'s current state. A no-op for an
    /// unknown id (already removed, or the state changed underneath a
    /// caller that saw the update).
    fn record(&self, id: &str) {
        let Some((recorder, context)) = &self.recorder else {
            return;
        };
        if let Some(task) = self.get(id) {
            recorder.upsert(context, &task);
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
        task: Task,
        cancel: Option<CancellationToken>,
        history: Option<Arc<dyn Fn() -> Vec<Message> + Send + Sync>>,
    ) -> Result<Task, TaskError> {
        self.insert(None, task, cancel, history)
    }

    /// Registers a task under a caller-chosen id instead of the next counted
    /// one, and does not advance the id counter. Used to resume an
    /// interrupted sub-agent task under the id its transcript already
    /// recorded. `id` must not already be in use by another entry in this
    /// registry; name validation is the same as [`Tasks::add`].
    pub fn add_with_id(
        &self,
        id: String,
        task: Task,
        cancel: Option<CancellationToken>,
        history: Option<Arc<dyn Fn() -> Vec<Message> + Send + Sync>>,
    ) -> Result<Task, TaskError> {
        self.insert(Some(id), task, cancel, history)
    }

    fn insert(
        &self,
        id_override: Option<String>,
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
            let id = match id_override {
                Some(id) => {
                    if state.entries.contains_key(&id) {
                        return Err(TaskError::Invalid(format!("task \"{id}\" already exists")));
                    }
                    id
                }
                None => {
                    state.counter += 1;
                    format!("t{}", state.counter)
                }
            };
            task.id = id.clone();
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
            let created = task.clone();
            state.entries.insert(
                id.clone(),
                Entry {
                    task,
                    in_tool: false,
                    cancel,
                    history,
                    inbox: Arc::new(Inbox::new(None)) as Arc<dyn Mailbox>,
                    reports_left: Arc::new(AtomicUsize::new(REPORT_BUDGET)),
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
        self.record(&created.id);
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
            self.record(id);
        }
    }

    /// Records the file the task's transcript is written to. Unknown ids are
    /// ignored.
    pub fn set_session_path(&self, id: &str, path: &str) {
        if let Some(entry) = self.lock().entries.get_mut(id) {
            entry.task.session_path = path.to_string();
        }
    }

    /// Records one completed provider round trip. It only applies while the
    /// task is running; invalid or unknown updates are no-ops.
    ///
    /// `cached_input_tokens_present` reports whether this round trip carried
    /// the cached-input-token breakdown. The task keeps the breakdown only
    /// when every report that carried usage carried it too.
    pub fn record_provider_step(
        &self,
        id: &str,
        usage: Usage,
        text: &str,
        usage_present: bool,
        cached_input_tokens_present: bool,
    ) {
        if usage.validate().is_err() {
            return;
        }
        let changed = {
            let mut state = self.lock();
            match state.entries.get_mut(id) {
                Some(entry) if entry.task.status == TaskStatus::Running => {
                    entry.task.steps += 1;
                    if usage_present {
                        let first_report = !entry.task.usage_present;
                        entry.task.usage_present = true;
                        entry.task.cached_input_tokens_present = if first_report {
                            cached_input_tokens_present
                        } else {
                            entry.task.cached_input_tokens_present && cached_input_tokens_present
                        };
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
            self.record(id);
        }
    }

    /// Records the latest tool call while the task is running. Invalid or
    /// unknown updates are no-ops.
    pub fn record_tool_call(&self, id: &str, last_tool: &str) {
        let changed = {
            let mut state = self.lock();
            match state.entries.get_mut(id) {
                Some(entry) if entry.task.status == TaskStatus::Running => {
                    entry.in_tool = true;
                    entry.task.tool_calls += 1;
                    entry.task.last_tool = last_tool.to_string();
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.signal();
            self.record(id);
        }
    }

    /// Finishes the current tool phase without changing historical tool data.
    pub(crate) fn record_tool_finished(&self, id: &str) {
        let changed = {
            let mut state = self.lock();
            if let Some(entry) = state.entries.get_mut(id)
                && entry.task.status == TaskStatus::Running
                && entry.in_tool
            {
                entry.in_tool = false;
                true
            } else {
                false
            }
        };
        if changed {
            self.signal();
        }
    }

    /// Live process-local phase. Provider/runtime includes model requests,
    /// retries and orchestration; it is not a claim that a provider is stalled.
    pub(crate) fn activity(&self, reference: &str) -> Option<&'static str> {
        let state = self.lock();
        let entry = Self::entry(&state, reference)?;
        Some(match entry.task.status {
            TaskStatus::Running if entry.in_tool => "tool",
            TaskStatus::Running => "provider_or_runtime",
            _ => entry.task.status.as_str(),
        })
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
            self.record(id);
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

    /// Removes one final task and its undelivered completion notification.
    /// Durable workflows use this after copying the result to their own store;
    /// interactive tasks remain queryable because they never call it.
    pub fn remove_final(&self, id: &str) -> Option<Task> {
        let task = {
            let mut state = self.lock();
            let entry = state.entries.get(id)?;
            if !entry.task.is_final() {
                return None;
            }
            let name = entry.task.name.clone();
            let task = state.entries.remove(id)?.task;
            state.order.retain(|candidate| candidate != id);
            if !name.is_empty() {
                state.names.remove(&name);
            }
            task
        };
        self.inbox.remove(id, NotificationKind::TaskFinished);
        self.signal();
        Some(task)
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

    /// The task's private inbox, used to deliver parent messages into the
    /// child at its next agent-loop notification checkpoint.
    pub fn child_inbox(&self, reference: &str) -> Option<Arc<dyn Mailbox>> {
        let state = self.lock();
        let entry = Self::entry(&state, reference)?;
        Some(Arc::clone(&entry.inbox))
    }

    /// Replaces the task's mailbox with `mailbox` (a durable one, built once
    /// the child's transcript path is known). Messages already queued in the
    /// old mailbox move after the ones `mailbox` restored, in order.
    pub fn replace_child_inbox(&self, reference: &str, mailbox: Arc<dyn Mailbox>) {
        let old = {
            let mut state = self.lock();
            let Some(id) = Self::entry(&state, reference).map(|entry| entry.task.id.clone()) else {
                return;
            };
            let Some(entry) = state.entries.get_mut(&id) else {
                return;
            };
            std::mem::replace(&mut entry.inbox, Arc::clone(&mailbox))
        };
        for entry in old.queued() {
            mailbox.push(entry.notification);
            old.ack(entry.seq);
        }
    }

    /// The task's `agent_report` budget counter, shared with the running
    /// child's `ChildTools`. `None` when `reference` names no task.
    pub fn report_allowance(&self, reference: &str) -> Option<Arc<AtomicUsize>> {
        let state = self.lock();
        let entry = Self::entry(&state, reference)?;
        Some(Arc::clone(&entry.reports_left))
    }

    /// Queues a parent message for a queued or running child. The returned id
    /// is the canonical task id, even when `reference` was a task name. Also
    /// grants the child one more `agent_report` call, on top of
    /// [`REPORT_BUDGET`], so it can answer the message.
    pub fn send_message(&self, reference: &str, message: &str) -> Result<String, TaskError> {
        let (id, inbox, reports_left) = {
            let state = self.lock();
            let Some(entry) = Self::entry(&state, reference) else {
                return Err(TaskError::NotFound(reference.to_string()));
            };
            if entry.task.is_final() {
                return Err(TaskError::Finished(reference.to_string()));
            }
            (
                entry.task.id.clone(),
                Arc::clone(&entry.inbox),
                Arc::clone(&entry.reports_left),
            )
        };
        if let Err(otto_core::agent::inbox::MailboxError::NotPersisted { seq, reason }) = inbox
            .enqueue(otto_core::agent::inbox::Notification {
                task_id: id.clone(),
                kind: Some(NotificationKind::Message),
                text: format!("[parent-message] {message}"),
                usage: None,
                ..Default::default()
            })
        {
            // Not admitted: a message the caller is told was not sent must
            // not reach the child anyway.
            inbox.ack(seq);
            return Err(TaskError::NotPersisted(reason));
        }
        reports_left.fetch_add(1, Ordering::SeqCst);
        self.signal();
        Ok(id)
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

    /// Begins a migration: marks the registry migrating, closes it the same
    /// way [`TaskRegistry::close`] does (cancels every non-final task's
    /// token, admits no more tasks, drops the update sender), and returns
    /// the ids that were not yet final at that moment. Idempotent: a second
    /// call returns an empty list.
    pub fn begin_migration(&self) -> Vec<String> {
        let ids = {
            let mut state = self.lock();
            if state.migrating {
                return Vec::new();
            }
            state.migrating = true;
            let order = state.order.clone();
            order
                .iter()
                .filter(|id| {
                    state
                        .entries
                        .get(id.as_str())
                        .is_some_and(|entry| !entry.task.is_final())
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        self.close();
        ids
    }

    /// Whether [`Tasks::begin_migration`] has run.
    pub fn is_migrating(&self) -> bool {
        self.lock().migrating
    }

    /// Waits for every id in `ids` to reach a final status. Built on
    /// [`Tasks::wait`] with a token that never fires: a task whose cancel
    /// token [`Tasks::begin_migration`] fired always reaches a final status
    /// on its own. An id no longer in the registry is skipped.
    pub async fn wait_final(&self, ids: &[String]) {
        let never = CancellationToken::new();
        for id in ids {
            let _ = self.wait(id, &never).await;
        }
    }

    /// The number of notifications waiting for the parent.
    pub fn pending(&self) -> usize {
        self.inbox.len()
    }

    /// The parent's notification mailbox.
    pub fn notifications(&self) -> &Arc<dyn Mailbox> {
        &self.mailbox
    }

    /// The in-memory adapter behind [`Self::notifications`], for the runtime
    /// composition root that makes it durable before sharing it.
    pub(crate) fn notifications_inbox(&self) -> &Arc<Inbox> {
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

/// The highest task counter used by a child transcript file name
/// `t<counter>-<random id>.jsonl` in `dir`, or 0 if `dir` does not exist or
/// has no matching name. Names that do not match are ignored. Pass the
/// result as `starting_counter` to [`Tasks::with_recorder_from`] so a
/// resumed session's task ids do not collide with a prior run's.
pub fn highest_task_counter(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut highest = 0;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix('t') else {
            continue;
        };
        let Some(digits) = rest.split('-').next() else {
            continue;
        };
        if digits.is_empty() || !rest[digits.len()..].starts_with('-') {
            continue;
        }
        if let Ok(counter) = digits.parse::<u64>() {
            highest = highest.max(counter);
        }
    }
    highest
}

#[cfg(test)]
mod tests;
