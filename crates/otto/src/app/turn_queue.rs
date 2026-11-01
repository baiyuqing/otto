//! The durable copy of the prompts waiting for a session's active turn.
//!
//! `otto serve` and local ACP keep their turn queues in memory and mirror
//! every waiting prompt into the session's `<id>.turns.json` mailbox journal.
//! A prompt is admitted only once its entry is saved, and its entry is removed
//! once the prompt's user message is in the session file (it then carries the
//! prompt's input id) or the prompt is cancelled for good. After process loss
//! or a shutdown, the frontend that next opens the session restores the
//! entries whose input id the transcript does not hold yet, and runs them in
//! order after the interrupted turn's recovery.
//!
//! An entry is a [`Notification`] whose `id` is the prompt's input id, whose
//! `task_id` is the serve turn id (empty for ACP), and whose `text` is the
//! JSON-encoded [`Saved`] prompt. Images are never saved: a 10 MiB image
//! would be rewritten with every queue change.

use std::sync::Arc;

use otto_core::agent::inbox::{MailboxError, Notification};
use otto_core::mailbox::Mailbox;
use serde::{Deserialize, Serialize};

use super::Channel;
use crate::schedule::Origin;

/// The error text of a prompt whose durable copy could not be saved.
pub const PROMPT_NOT_PERSISTED: &str = "the prompt could not be saved; nothing was queued";

/// One prompt waiting for the active turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    /// The input id its user message will carry.
    pub input_id: String,
    /// The serve turn id; empty for a local ACP prompt.
    pub turn_id: String,
    pub text: String,
    pub channel: Option<Channel>,
    pub origin: Option<Origin>,
    /// The prompt carried an image that was not saved.
    pub image_dropped: bool,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channel: Option<Channel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(
        default,
        rename = "imageDropped",
        skip_serializing_if = "std::ops::Not::not"
    )]
    image_dropped: bool,
}

/// The `turns.json` journal of one session.
pub struct TurnQueue {
    journal: Arc<dyn Mailbox>,
}

impl TurnQueue {
    pub(crate) fn new(journal: Arc<dyn Mailbox>) -> Self {
        Self { journal }
    }

    /// Saves `prompt`. On failure nothing stays queued and the caller must
    /// not admit the prompt.
    pub fn save(&self, prompt: &QueuedPrompt) -> Result<(), String> {
        let saved = Saved {
            text: prompt.text.clone(),
            channel: prompt.channel,
            origin: prompt
                .origin
                .as_ref()
                .map(|origin| origin.as_str().to_owned()),
            image_dropped: prompt.image_dropped,
        };
        let text = serde_json::to_string(&saved).map_err(|_| PROMPT_NOT_PERSISTED.to_string())?;
        match self.journal.enqueue(Notification {
            id: prompt.input_id.clone(),
            task_id: prompt.turn_id.clone(),
            text,
            ..Notification::default()
        }) {
            Ok(_) => Ok(()),
            Err(MailboxError::NotPersisted { seq, .. }) => {
                self.journal.ack(seq);
                Err(PROMPT_NOT_PERSISTED.into())
            }
        }
    }

    /// Removes the entry with `input_id`, if any.
    pub fn forget(&self, input_id: &str) {
        for entry in self.journal.queued() {
            if entry.notification.id == input_id {
                self.journal.ack(entry.seq);
            }
        }
    }

    /// Removes every entry.
    pub fn clear(&self) {
        for entry in self.journal.queued() {
            self.journal.ack(entry.seq);
        }
    }

    /// The saved prompts, oldest first. An entry that no longer decodes is
    /// removed rather than run with wrong content.
    pub fn pending(&self) -> Vec<QueuedPrompt> {
        let mut prompts = Vec::new();
        for entry in self.journal.queued() {
            let saved = serde_json::from_str::<Saved>(&entry.notification.text);
            match saved {
                Ok(saved) if !entry.notification.id.is_empty() => prompts.push(QueuedPrompt {
                    input_id: entry.notification.id,
                    turn_id: entry.notification.task_id,
                    text: saved.text,
                    channel: saved.channel,
                    origin: saved
                        .origin
                        .as_deref()
                        .and_then(|origin| Origin::parse(origin).ok()),
                    image_dropped: saved.image_dropped,
                }),
                _ => {
                    self.journal.ack(entry.seq);
                }
            }
        }
        prompts
    }
}

/// The context notice the first restored prompt delivers before its user
/// message, so the model and every client know these prompts were queued
/// before Otto stopped rather than sent just now.
pub(crate) fn restored_notice(prompts: &[QueuedPrompt]) -> String {
    let dropped = prompts.iter().filter(|prompt| prompt.image_dropped).count();
    let mut text = format!(
        "Otto stopped before {} queued prompt{} could run. {} now run in the order they were queued, starting with the next message.",
        prompts.len(),
        if prompts.len() == 1 { "" } else { "s" },
        if prompts.len() == 1 { "It" } else { "They" },
    );
    if dropped > 0 {
        text.push_str(&format!(
            " {dropped} of them had an image, which was not kept; ask the user to attach it again if it is needed."
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::mailbox::InMemoryMailbox;

    fn prompt(input_id: &str, text: &str) -> QueuedPrompt {
        QueuedPrompt {
            input_id: input_id.into(),
            turn_id: format!("turn-{input_id}"),
            text: text.into(),
            channel: Some(Channel::Telegram),
            origin: Some(Origin::chat("telegram", "c1").unwrap()),
            image_dropped: false,
        }
    }

    #[test]
    fn saved_prompts_round_trip_in_order_and_are_forgotten_by_input_id() {
        let queue = TurnQueue::new(Arc::new(InMemoryMailbox::new(None)));
        queue.save(&prompt("a", "first")).unwrap();
        queue.save(&prompt("b", "second")).unwrap();
        assert_eq!(
            queue.pending(),
            vec![prompt("a", "first"), prompt("b", "second")]
        );
        queue.forget("a");
        assert_eq!(queue.pending(), vec![prompt("b", "second")]);
        queue.clear();
        assert!(queue.pending().is_empty());
    }

    #[test]
    fn a_prompt_that_cannot_be_saved_is_not_queued() {
        let journal = Arc::new(InMemoryMailbox::new(None));
        journal.set_persist(Box::new(|_| Err("disk full".into())));
        let queue = TurnQueue::new(journal);
        assert_eq!(
            queue.save(&prompt("a", "first")),
            Err(PROMPT_NOT_PERSISTED.to_string())
        );
        assert!(queue.pending().is_empty());
    }

    #[test]
    fn an_undecodable_entry_is_dropped() {
        let journal = Arc::new(InMemoryMailbox::new(None));
        journal
            .enqueue(Notification {
                id: "x".into(),
                text: "not json".into(),
                ..Notification::default()
            })
            .unwrap();
        let queue = TurnQueue::new(journal);
        assert!(queue.pending().is_empty());
        assert!(queue.journal.queued().is_empty());
    }

    #[test]
    fn the_notice_counts_prompts_and_dropped_images() {
        let mut second = prompt("b", "second");
        second.image_dropped = true;
        let notice = restored_notice(&[prompt("a", "first"), second]);
        assert!(
            notice.starts_with("Otto stopped before 2 queued prompts"),
            "{notice}"
        );
        assert!(notice.contains("1 of them had an image"), "{notice}");
        assert!(restored_notice(&[prompt("a", "x")]).contains("1 queued prompt could run. It"));
    }
}
