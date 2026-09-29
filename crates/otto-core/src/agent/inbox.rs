//! The queue of notifications delivered into the next provider request.
//!
//! A subagent task pushes its terminal result or a progress report here; a
//! parent pushes a message for a child, and a frontend can queue user input
//! for a running turn. The agent drains the queue at safe checkpoints and
//! persists user input as user messages and every other item as context.
//!
//! Ownership: the queue is shared. Producers hold an `Arc<Inbox>` and the agent
//! holds another. Nothing takes ownership of the items until they are removed.
//!
//! Concurrency: every method locks an internal mutex and is safe to call from
//! any task. The change callback runs after the lock is released, so it may
//! call back into the inbox without deadlocking. The persistence hook (see
//! [`Inbox::set_persist`]) runs while the lock is held, so the written file
//! never disagrees with the in-memory order.
//!
//! Errors: none. A poisoned lock is recovered rather than reported, because
//! losing queued notifications is worse than continuing with the queue a
//! panicking producer left behind.

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::model::Usage;

/// Why a notification was pushed. This selects the context type and, for the
/// parent-facing runner, the rendered text format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// A task reached a terminal state.
    TaskFinished,
    /// A running task reported progress.
    TaskReport,
    /// A message addressed to this agent.
    Message,
    /// User input submitted while this agent is already running.
    UserMessage,
}

impl NotificationKind {
    /// The `NotificationKind` string, as it appears in persisted sessions.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TaskFinished => "task_finished",
            Self::TaskReport => "task_report",
            Self::Message => "message",
            Self::UserMessage => "user_message",
        }
    }
}

/// One item queued for delivery into the next provider request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

/// A notification plus the sequence number it was pushed with. Sequence
/// numbers are unique within one inbox and increase with every push; a
/// reloaded inbox continues counting after the highest number it loaded
/// (see [`Inbox::load`]). Delivery removes an item by sequence number
/// instead of by position, so a push that lands during delivery is neither
/// lost nor reordered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    #[serde(flatten)]
    pub notification: Notification,
}

#[derive(Default)]
struct State {
    items: Vec<Entry>,
    next_seq: u64,
}

/// The signature [`Inbox::set_persist`] takes.
pub type PersistHook = Box<dyn Fn(&[Entry]) + Send + Sync>;

/// A first-in first-out queue of notifications.
#[derive(Default)]
pub struct Inbox {
    state: Mutex<State>,
    on_change: Option<Box<dyn Fn() + Send + Sync>>,
    /// Set at most once, before the inbox is shared with producers. Runs
    /// under the state lock on every mutation; see the module documentation.
    persist: OnceLock<PersistHook>,
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
    /// removed items, after a `remove`/`remove_seq` that removed an item, and
    /// after a `load` that seeded at least one entry. It runs without the
    /// lock held. Frontends use it to wake a waiting turn.
    pub fn new(on_change: Option<Box<dyn Fn() + Send + Sync>>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            on_change,
            persist: OnceLock::new(),
        }
    }

    /// Installs the persistence hook. It is called with the full, current
    /// item list, in queue order, under the state lock, after every mutation.
    /// A later call is ignored: only the first hook installed takes effect.
    /// Call this, and [`Self::load`] if there is a file to restore, before
    /// sharing the inbox with any producer.
    pub fn set_persist(&self, hook: PersistHook) {
        let _ = self.persist.set(hook);
    }

    /// Seeds the inbox from previously persisted entries, in file order, and
    /// continues sequence numbering after the highest one loaded. For use
    /// once, right after construction and before any push. It never runs the
    /// persistence hook, because it restores exactly what is already on
    /// disk, but it does run the change callback when `entries` is
    /// non-empty, so a frontend that only starts a wake turn on a change
    /// signal still picks up notifications that were already queued when the
    /// session was reopened.
    pub fn load(&self, entries: Vec<Entry>) {
        let loaded_any = !entries.is_empty();
        {
            let mut state = self.lock();
            state.next_seq = entries
                .iter()
                .map(|entry| entry.seq)
                .max()
                .map_or(0, |highest| highest + 1);
            state.items = entries;
        }
        if loaded_any {
            self.notify();
        }
    }

    /// Appends a notification to the back of the queue.
    pub fn push(&self, notification: Notification) {
        {
            let mut state = self.lock();
            let seq = state.next_seq;
            state.next_seq += 1;
            state.items.push(Entry { seq, notification });
            self.persist(&state.items);
        }
        self.notify();
    }

    /// Returns every queued notification in push order and empties the queue.
    pub fn drain(&self) -> Vec<Notification> {
        let items = {
            let mut state = self.lock();
            let items = std::mem::take(&mut state.items);
            if !items.is_empty() {
                self.persist(&state.items);
            }
            items
        };
        if !items.is_empty() {
            self.notify();
        }
        items.into_iter().map(|entry| entry.notification).collect()
    }

    /// Removes and returns the first notification with this task id and kind.
    pub fn remove(&self, task_id: &str, kind: NotificationKind) -> Option<Notification> {
        let removed = {
            let mut state = self.lock();
            let removed = state
                .items
                .iter()
                .position(|entry| {
                    entry.notification.task_id == task_id && entry.notification.kind == Some(kind)
                })
                .map(|index| state.items.remove(index));
            if removed.is_some() {
                self.persist(&state.items);
            }
            removed
        };
        if removed.is_some() {
            self.notify();
        }
        removed.map(|entry| entry.notification)
    }

    /// Removes the entry with this sequence number, if it is still queued.
    /// Delivery uses this, rather than draining or removing by position, so a
    /// notification pushed while delivery is in progress stays queued in
    /// order instead of being skipped or delivered twice.
    pub fn remove_seq(&self, seq: u64) -> Option<Notification> {
        let removed = {
            let mut state = self.lock();
            let removed = state
                .items
                .iter()
                .position(|entry| entry.seq == seq)
                .map(|index| state.items.remove(index));
            if removed.is_some() {
                self.persist(&state.items);
            }
            removed
        };
        if removed.is_some() {
            self.notify();
        }
        removed.map(|entry| entry.notification)
    }

    /// The number of queued notifications.
    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    /// Whether the queue holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies queued notifications without draining them.
    pub fn snapshot(&self) -> Vec<Notification> {
        self.lock()
            .items
            .iter()
            .map(|entry| entry.notification.clone())
            .collect()
    }

    /// Copies queued entries, sequence numbers included, without draining
    /// them. Delivery reads the queue this way, then removes each entry by
    /// sequence number once its append has returned.
    pub fn queued(&self) -> Vec<Entry> {
        self.lock().items.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Calls the persistence hook, if any. The caller holds the state lock.
    fn persist(&self, items: &[Entry]) {
        if let Some(hook) = self.persist.get() {
            hook(items);
        }
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
        assert_eq!(NotificationKind::UserMessage.as_str(), "user_message");
    }

    #[test]
    fn sequence_numbers_are_unique_and_increase_with_every_push() {
        let inbox = Inbox::new(None);
        inbox.push(notification("t1", NotificationKind::Message));
        inbox.push(notification("t2", NotificationKind::Message));
        let queued = inbox.queued();
        assert_eq!(queued.len(), 2);
        assert!(queued[0].seq < queued[1].seq);
    }

    #[test]
    fn load_continues_numbering_after_the_highest_loaded_sequence() {
        let inbox = Inbox::new(None);
        inbox.load(vec![
            Entry {
                seq: 5,
                notification: notification("t1", NotificationKind::Message),
            },
            Entry {
                seq: 9,
                notification: notification("t2", NotificationKind::Message),
            },
        ]);
        assert_eq!(inbox.len(), 2);
        inbox.push(notification("t3", NotificationKind::Message));
        let queued = inbox.queued();
        assert_eq!(queued[2].seq, 10);
    }

    #[test]
    fn load_with_entries_signals_a_change_but_never_persists() {
        let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&changes);
        let inbox = Inbox::new(Some(Box::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })));
        let persists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recorder = Arc::clone(&persists);
        inbox.set_persist(Box::new(move |_entries| {
            recorder.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        inbox.load(vec![
            Entry {
                seq: 0,
                notification: notification("t1", NotificationKind::Message),
            },
            Entry {
                seq: 1,
                notification: notification("t2", NotificationKind::Message),
            },
        ]);

        assert_eq!(
            changes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a load that seeds entries runs the change callback exactly once"
        );
        assert_eq!(
            persists.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "load restores what is already on disk, so it must not persist"
        );
    }

    #[test]
    fn load_with_no_entries_signals_nothing() {
        let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&changes);
        let inbox = Inbox::new(Some(Box::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })));
        let persists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recorder = Arc::clone(&persists);
        inbox.set_persist(Box::new(move |_entries| {
            recorder.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        inbox.load(Vec::new());

        assert_eq!(
            changes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an empty load has nothing to wake a turn for"
        );
        assert_eq!(persists.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn remove_seq_takes_only_the_matching_entry_and_a_concurrent_push_stays_queued() {
        let inbox = Inbox::new(None);
        inbox.push(notification("t1", NotificationKind::Message));
        inbox.push(notification("t2", NotificationKind::Message));
        let queued = inbox.queued();
        assert_eq!(queued.len(), 2);

        // A push that lands between reading the queue and removing its first
        // entry (simulating a push racing a delivery loop) must not be lost
        // or reordered.
        inbox.push(notification("t3", NotificationKind::Message));

        let removed = inbox.remove_seq(queued[0].seq).expect("first entry");
        assert_eq!(removed.task_id, "t1");

        let remaining = inbox.queued();
        assert_eq!(
            remaining
                .iter()
                .map(|entry| entry.notification.task_id.as_str())
                .collect::<Vec<_>>(),
            ["t2", "t3"]
        );

        assert!(inbox.remove_seq(queued[0].seq).is_none(), "already removed");
    }

    #[test]
    fn the_persist_hook_sees_the_full_list_after_every_mutation() {
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let inbox = Inbox::new(None);
        inbox.set_persist(Box::new(move |entries| {
            recorder.lock().unwrap().push(
                entries
                    .iter()
                    .map(|entry| entry.notification.task_id.clone())
                    .collect(),
            );
        }));

        inbox.push(notification("t1", NotificationKind::Message));
        inbox.push(notification("t2", NotificationKind::Message));
        let seq = inbox.queued()[0].seq;
        inbox.remove_seq(seq);

        let calls = seen.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                vec!["t1".to_string()],
                vec!["t1".to_string(), "t2".to_string()],
                vec!["t2".to_string()],
            ]
        );
    }

    #[test]
    fn a_second_persist_hook_is_ignored() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let inbox = Inbox::new(None);
        let first = Arc::clone(&first_calls);
        inbox.set_persist(Box::new(move |_| {
            first.fetch_add(1, Ordering::SeqCst);
        }));
        let second = Arc::clone(&second_calls);
        inbox.set_persist(Box::new(move |_| {
            second.fetch_add(1, Ordering::SeqCst);
        }));

        inbox.push(notification("t1", NotificationKind::Message));

        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    }
}
