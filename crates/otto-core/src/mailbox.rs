//! Otto's shared mailbox contract.
//!
//! A mailbox is a FIFO queue of agent notifications. Producers enqueue a
//! [`Notification`]; a consumer obtains a stable [`Entry`] snapshot and
//! acknowledges each entry by sequence number *after* its durable effect has
//! completed. This gives the consumer at-least-once delivery: a failed effect
//! leaves that entry, and every later entry, visible for a later attempt.
//!
//! The contract deliberately does not claim distributed leasing, delayed retry
//! or exactly-once delivery. Those require a durable backend with a consumer
//! ownership protocol and are extension concerns, not properties an in-memory
//! mailbox can truthfully offer.
//!
//! `otto-core` owns this WASM-safe port and payload semantics. Native runtime
//! code owns adapter construction and persistence. The bundled [`Inbox`]
//! implementation is the default in-memory adapter; it exists for backwards
//! compatibility while consumers migrate to this port.

use std::fmt::Debug;

pub use crate::agent::inbox::{Entry, Inbox, Notification, NotificationKind, PersistHook};

/// A synchronous FIFO notification port.
///
/// All methods are safe for concurrent callers. `queued` returns a point-in-
/// time ordered snapshot. Consumers must call `ack` only after their durable
/// effect succeeds; acknowledging an entry that is already absent is harmless.
/// `remove` supports domain cancellation of a pending notification.
pub trait Mailbox: Debug + Send + Sync {
    /// Appends a notification after every notification already visible.
    fn enqueue(&self, notification: Notification);
    /// Compatibility spelling for enqueue. New callers should use `enqueue`.
    fn push(&self, notification: Notification) {
        self.enqueue(notification);
    }
    /// Returns the current ordered delivery candidates without claiming them.
    fn queued(&self) -> Vec<Entry>;
    /// Acknowledges one delivery after its effect is durable.
    fn ack(&self, sequence: u64) -> Option<Notification>;
    /// Removes the first notification matching a domain identity.
    fn remove(&self, task_id: &str, kind: NotificationKind) -> Option<Notification>;
    /// Copies notifications without their transport sequence numbers.
    fn snapshot(&self) -> Vec<Notification>;
    /// Returns the number of notifications currently visible.
    fn len(&self) -> usize;
    /// Whether no notification is currently visible.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Restores a complete ordered snapshot before producers receive the
    /// mailbox. Durable adapters use this only during construction.
    fn load(&self, entries: Vec<Entry>);
}

impl Mailbox for Inbox {
    fn enqueue(&self, notification: Notification) {
        self.push(notification);
    }

    fn queued(&self) -> Vec<Entry> {
        self.queued()
    }

    fn ack(&self, sequence: u64) -> Option<Notification> {
        self.remove_seq(sequence)
    }

    fn remove(&self, task_id: &str, kind: NotificationKind) -> Option<Notification> {
        self.remove(task_id, kind)
    }

    fn snapshot(&self) -> Vec<Notification> {
        self.snapshot()
    }

    fn len(&self) -> usize {
        self.len()
    }

    fn load(&self, entries: Vec<Entry>) {
        self.load(entries);
    }
}

/// The bundled in-memory mailbox adapter.
///
/// This alias names the default adapter without making callers depend on it;
/// application code should accept [`Mailbox`] instead.
pub type InMemoryMailbox = Inbox;

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(text: &str) -> Notification {
        Notification {
            text: text.into(),
            ..Notification::default()
        }
    }

    #[test]
    fn default_adapter_obeys_fifo_ack_contract() {
        let mailbox = InMemoryMailbox::default();
        mailbox.enqueue(notification("first"));
        mailbox.enqueue(notification("second"));
        let queued = mailbox.queued();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].notification.text, "first");
        mailbox.ack(queued[0].seq);
        assert_eq!(mailbox.snapshot()[0].text, "second");
    }
}
