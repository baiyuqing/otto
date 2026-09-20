//! The queue of notifications delivered into the next provider request.
//!
//! A subagent task pushes its terminal result or a progress report here; a
//! parent pushes a message for a child. The agent drains the queue at the top
//! of each turn and turns every item into one context message.
//!
//! Ownership: the queue is shared. Producers hold an `Arc<Inbox>` and the agent
//! holds another. Nothing takes ownership of the items until `drain`.
//!
//! Concurrency: every method locks an internal mutex and is safe to call from
//! any task. The change callback runs after the lock is released, so it may
//! call back into the inbox without deadlocking.
//!
//! Errors: none. A poisoned lock is recovered rather than reported, because
//! losing queued notifications is worse than continuing with the queue a
//! panicking producer left behind.

use std::sync::Mutex;

use crate::model::Usage;

/// Why a notification was pushed. This selects the context type and, for the
/// parent-facing runner, the rendered text format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    /// A task reached a terminal state.
    TaskFinished,
    /// A running task reported progress.
    TaskReport,
    /// A message addressed to this agent.
    Message,
}

impl NotificationKind {
    /// The `NotificationKind` string, as it appears in persisted sessions.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TaskFinished => "task_finished",
            Self::TaskReport => "task_report",
            Self::Message => "message",
        }
    }
}

/// One item queued for delivery into the next provider request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Notification {
    pub task_id: String,
    pub kind: Option<NotificationKind>,
    pub text: String,
    pub usage: Option<Usage>,
}

impl Notification {
    /// The `Message.context_type` a delivered notification is persisted and
    /// emitted under.
    pub fn context_type(&self) -> &'static str {
        if self.kind == Some(NotificationKind::Message) {
            "parent_message"
        } else {
            "task_notification"
        }
    }
}

/// A first-in first-out queue of notifications.
#[derive(Default)]
pub struct Inbox {
    items: Mutex<Vec<Notification>>,
    on_change: Option<Box<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Inbox")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl Inbox {
    /// Creates an empty inbox.
    ///
    /// `on_change`, when given, runs after every `push`, after a `drain` that
    /// removed items, and after a `remove` that removed an item. It runs
    /// without the lock held. Frontends use it to wake a waiting turn.
    pub fn new(on_change: Option<Box<dyn Fn() + Send + Sync>>) -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            on_change,
        }
    }

    /// Appends a notification to the back of the queue.
    pub fn push(&self, notification: Notification) {
        self.lock().push(notification);
        self.notify();
    }

    /// Returns every queued notification in push order and empties the queue.
    pub fn drain(&self) -> Vec<Notification> {
        let items = std::mem::take(&mut *self.lock());
        if !items.is_empty() {
            self.notify();
        }
        items
    }

    /// Removes and returns the first notification with this task id and kind.
    pub fn remove(&self, task_id: &str, kind: NotificationKind) -> Option<Notification> {
        let removed = {
            let mut items = self.lock();
            items
                .iter()
                .position(|item| item.task_id == task_id && item.kind == Some(kind))
                .map(|index| items.remove(index))
        };
        if removed.is_some() {
            self.notify();
        }
        removed
    }

    /// The number of queued notifications.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the queue holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies queued notifications without draining them.
    pub fn snapshot(&self) -> Vec<Notification> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Notification>> {
        self.items
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn notify(&self) {
        if let Some(on_change) = &self.on_change {
            on_change();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn notification(task_id: &str, kind: NotificationKind) -> Notification {
        Notification {
            task_id: task_id.into(),
            kind: Some(kind),
            text: format!("{task_id} {}", kind.as_str()),
            usage: None,
        }
    }

    #[test]
    fn drain_returns_items_in_push_order_and_empties_the_queue() {
        let inbox = Inbox::new(None);
        inbox.push(notification("t1", NotificationKind::TaskReport));
        inbox.push(notification("t2", NotificationKind::TaskFinished));
        assert_eq!(inbox.len(), 2);

        let drained = inbox.drain();
        assert_eq!(
            drained
                .iter()
                .map(|item| item.task_id.as_str())
                .collect::<Vec<_>>(),
            ["t1", "t2"]
        );
        assert!(inbox.is_empty());
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn snapshot_copies_without_draining() {
        let inbox = Inbox::new(None);
        inbox.push(notification("t1", NotificationKind::Message));
        let copied = inbox.snapshot();
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].task_id, "t1");
        assert_eq!(inbox.len(), 1);
    }

    #[test]
    fn remove_takes_the_first_match_and_leaves_the_rest() {
        let inbox = Inbox::new(None);
        inbox.push(notification("t1", NotificationKind::TaskReport));
        inbox.push(notification("t1", NotificationKind::TaskFinished));
        inbox.push(notification("t1", NotificationKind::TaskReport));

        let removed = inbox
            .remove("t1", NotificationKind::TaskReport)
            .expect("a report was queued");
        assert_eq!(removed.text, "t1 task_report");
        assert_eq!(inbox.len(), 2);
        assert!(inbox.remove("t2", NotificationKind::TaskReport).is_none());
        assert_eq!(inbox.len(), 2);
    }

    #[test]
    fn the_change_callback_runs_for_every_mutation_that_moved_an_item() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let inbox = Inbox::new(Some(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })));

        inbox.push(notification("t1", NotificationKind::Message));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert!(inbox.remove("t2", NotificationKind::Message).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a miss must not notify");

        assert!(inbox.remove("t1", NotificationKind::Message).is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        assert!(inbox.drain().is_empty());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "an empty drain must not notify"
        );

        inbox.push(notification("t3", NotificationKind::TaskFinished));
        assert_eq!(inbox.drain().len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn the_context_type_separates_parent_messages_from_task_notifications() {
        assert_eq!(
            notification("t1", NotificationKind::Message).context_type(),
            "parent_message"
        );
        assert_eq!(
            notification("t1", NotificationKind::TaskReport).context_type(),
            "task_notification"
        );
        assert_eq!(
            notification("t1", NotificationKind::TaskFinished).context_type(),
            "task_notification"
        );
        assert_eq!(
            Notification::default().context_type(),
            "task_notification",
            "an unset kind is not a parent message"
        );
    }

    #[test]
    fn kind_strings_match_the_persisted_names() {
        assert_eq!(NotificationKind::TaskFinished.as_str(), "task_finished");
        assert_eq!(NotificationKind::TaskReport.as_str(), "task_report");
        assert_eq!(NotificationKind::Message.as_str(), "message");
    }
}
