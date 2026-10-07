//! Turn-bound text accepted while a run is active.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};

use crate::agent::inbox::{MailboxError, Notification, NotificationKind};
use crate::mailbox::Mailbox;

const MAX_MESSAGES: usize = 16;
const MAX_BYTES: usize = 32_000;
const MAX_HISTORY: usize = 32;
/// Losses held for the frontend to report. A run can discard at most
/// [`MAX_MESSAGES`] at once; the bound only protects a frontend that never
/// asks.
const MAX_DISCARDED: usize = MAX_MESSAGES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    Inactive,
    Empty,
    TooLong,
    Full,
    /// The durable copy could not be saved; the guidance was not accepted.
    NotPersisted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Delivered,
    Withdrawn,
    Discarded,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Message {
    pub id: u64,
    pub text: String,
    pub status: Status,
    /// The stable input id the delivered user message carries: the client's
    /// `request_id`, or `steer:<turn id>:<n>`. Empty for a run without a turn
    /// id (tests and sessions without a journal).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub input_id: String,
}

#[derive(Default)]
struct State {
    generation: u64,
    next_id: u64,
    active: bool,
    accepting: bool,
    messages: VecDeque<Message>,
    history: VecDeque<Message>,
    /// Guidance a run end discarded and no frontend has reported yet, so a
    /// canceled or failed turn never drops the user's words in silence.
    discarded: VecDeque<Message>,
    /// The `otto.turn` id of the active run; empty when it has none.
    turn: String,
    /// The interrupted turn whose journaled guidance this run restored.
    restored: Option<String>,
}

/// Transient FIFO for messages bound to one active run. Messages remain
/// observable for the run even after delivery, but are never persisted unless
/// delivery reaches the agent loop.
///
/// Guidance a run end finds undelivered is discarded — it never reaches a
/// later run — and the frontend reports that loss through
/// [`Steering::take_discarded`], so an interrupted turn cannot make the user's
/// words vanish without a word.
///
/// With a journal ([`Steering::set_journal`]), accepted guidance is also kept
/// in a durable mailbox, scoped to the run's `otto.turn` id. Every way a run
/// ends removes it again; only process loss leaves it, for the automatic
/// resume of that same turn ([`Steering::begin_turn`]) to deliver.
#[derive(Default)]
pub struct Steering {
    state: Mutex<State>,
    journal: OnceLock<Arc<dyn Mailbox>>,
}

impl Steering {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Installs the durable journal. A later call is ignored.
    pub fn set_journal(&self, journal: Arc<dyn Mailbox>) {
        let _ = self.journal.set(journal);
    }

    /// Removes the journal entry for `input_id`, if any.
    fn forget(&self, input_id: &str) {
        let Some(journal) = self.journal.get() else {
            return;
        };
        if input_id.is_empty() {
            return;
        }
        for entry in journal.queued() {
            if entry.notification.id == input_id {
                journal.ack(entry.seq);
            }
        }
    }

    /// Removes every journal entry scoped to `turn`.
    fn forget_turn(&self, turn: &str) {
        let Some(journal) = self.journal.get() else {
            return;
        };
        for entry in journal.queued() {
            if entry.notification.scope == turn {
                journal.ack(entry.seq);
            }
        }
    }

    /// Acknowledges a delivered message: its journal entry is no longer
    /// needed once the session holds it.
    pub fn delivered(&self, message: &Message) {
        self.forget(&message.input_id);
    }

    /// The current run's ticket, if it still accepts guidance.
    pub fn ticket(&self) -> Option<u64> {
        let state = self.lock();
        (state.active && state.accepting).then_some(state.generation)
    }

    /// Queues guidance for the current run.
    pub fn submit(&self, text: &str) -> Result<Message, SubmitError> {
        let Some(ticket) = self.ticket() else {
            return Err(SubmitError::Inactive);
        };
        self.submit_for(ticket, text)
    }

    /// Queues guidance only for the exact run identified by `ticket`.
    pub fn submit_for(&self, ticket: u64, text: &str) -> Result<Message, SubmitError> {
        self.submit_input(ticket, text, None)
    }

    /// [`Self::submit_for`] with the client's `request_id` as the input id.
    /// Guidance whose input id is already queued or delivered in this run is
    /// not queued again; the earlier message is returned.
    pub fn submit_input(
        &self,
        ticket: u64,
        text: &str,
        request_id: Option<&str>,
    ) -> Result<Message, SubmitError> {
        if text.len() > MAX_BYTES {
            return Err(SubmitError::TooLong);
        }
        let text = text.trim();
        if text.is_empty() {
            return Err(SubmitError::Empty);
        }
        let mut state = self.lock();
        if !state.active || !state.accepting || state.generation != ticket {
            return Err(SubmitError::Inactive);
        }
        if let Some(request_id) = request_id
            && let Some(existing) = state
                .history
                .iter()
                .chain(state.messages.iter())
                .find(|message| message.input_id == request_id)
        {
            return Ok(existing.clone());
        }
        if state.messages.len() >= MAX_MESSAGES {
            return Err(SubmitError::Full);
        }
        state.next_id = state.next_id.wrapping_add(1);
        let input_id = match request_id {
            Some(request_id) => request_id.to_owned(),
            None if state.turn.is_empty() => String::new(),
            None => format!("steer:{}:{}", state.turn, state.next_id),
        };
        let message = Message {
            id: state.next_id,
            text: text.to_owned(),
            status: Status::Queued,
            input_id,
        };
        if let Some(journal) = self.journal.get()
            && !state.turn.is_empty()
            && let Err(MailboxError::NotPersisted { seq, .. }) = journal.enqueue(Notification {
                kind: Some(NotificationKind::UserMessage),
                text: message.text.clone(),
                id: message.input_id.clone(),
                scope: state.turn.clone(),
                ..Notification::default()
            })
        {
            journal.ack(seq);
            return Err(SubmitError::NotPersisted);
        }
        state.messages.push_back(message.clone());
        Ok(message)
    }

    /// Lists current-run messages. Queued messages may be withdrawn; all other
    /// statuses are historical facts for this transient run.
    pub fn messages(&self) -> Vec<Message> {
        let state = self.lock();
        state
            .history
            .iter()
            .chain(state.messages.iter())
            .cloned()
            .collect()
    }

    /// Removes and returns the guidance the last run end discarded, oldest
    /// first. The frontend that owns the run reports each loss exactly once,
    /// which is why this drains instead of reading: a discard that no one
    /// reports is a loss the user never learns about.
    pub fn take_discarded(&self) -> Vec<Message> {
        self.lock().discarded.drain(..).collect()
    }

    /// Withdraws exactly one undelivered message from the active run.
    pub fn withdraw(&self, id: u64) -> Option<Message> {
        let mut state = self.lock();
        if !state.active {
            return None;
        }
        let index = state.messages.iter().position(|message| message.id == id)?;
        let mut message = state.messages.remove(index)?;
        message.status = Status::Withdrawn;
        push_history(&mut state.history, message.clone());
        drop(state);
        self.forget(&message.input_id);
        Some(message)
    }

    #[cfg(test)]
    pub(super) fn begin(self: &Arc<Self>) -> Run {
        self.begin_turn("", None)
    }

    /// Starts accepting guidance for the run of turn `turn`. When the run
    /// resumes an interrupted turn (`resumes`), journaled guidance that turn
    /// never received is queued first, in order. Journal entries of any other
    /// turn are stale (their turn ended or will not resume) and are removed.
    pub(super) fn begin_turn(self: &Arc<Self>, turn: &str, resumes: Option<&str>) -> Run {
        let mut state = self.lock();
        state.generation = state.generation.wrapping_add(1);
        state.active = true;
        state.accepting = true;
        state.messages.clear();
        state.history.clear();
        state.turn = turn.to_owned();
        state.restored = resumes.map(str::to_owned);
        if let Some(journal) = self.journal.get() {
            for entry in journal.queued() {
                let scope = entry.notification.scope.as_str();
                if !turn.is_empty() && resumes == Some(scope) {
                    state.next_id = state.next_id.wrapping_add(1);
                    let message = Message {
                        id: state.next_id,
                        text: entry.notification.text,
                        status: Status::Queued,
                        input_id: entry.notification.id,
                    };
                    state.messages.push_back(message);
                } else if scope != turn {
                    journal.ack(entry.seq);
                }
            }
        }
        Run {
            steering: Arc::clone(self),
            ticket: state.generation,
        }
    }
}

fn push_history(history: &mut VecDeque<Message>, message: Message) {
    history.push_back(message);
    if history.len() > MAX_HISTORY {
        history.pop_front();
    }
}

/// The active run's admission guard. Dropping it marks undelivered text as
/// discarded rather than allowing it to wake a later run, and hands that loss
/// to [`Steering::take_discarded`] for the frontend to report.
pub(super) struct Run {
    steering: Arc<Steering>,
    ticket: u64,
}

impl Run {
    pub(super) fn drain(&self) -> Vec<Message> {
        let mut state = self.steering.lock();
        if !state.active || state.generation != self.ticket {
            return Vec::new();
        }
        let drained: Vec<_> = state.messages.drain(..).collect();
        for message in &drained {
            let mut delivered = message.clone();
            delivered.status = Status::Delivered;
            push_history(&mut state.history, delivered);
        }
        drained
    }

    /// Atomically consumes pending text or closes acceptance before success.
    pub(super) fn take_or_close(&self) -> Option<Vec<Message>> {
        let mut state = self.steering.lock();
        if !state.active || state.generation != self.ticket {
            return None;
        }
        if state.messages.is_empty() {
            state.accepting = false;
            None
        } else {
            let drained: Vec<_> = state.messages.drain(..).collect();
            for message in &drained {
                let mut delivered = message.clone();
                delivered.status = Status::Delivered;
                push_history(&mut state.history, delivered);
            }
            Some(drained)
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let mut state = self.steering.lock();
        let ended =
            (state.generation == self.ticket).then(|| (state.turn.clone(), state.restored.take()));
        if state.active && state.generation == self.ticket {
            let discarded: Vec<_> = state.messages.drain(..).collect();
            for message in discarded {
                let message = Message {
                    status: Status::Discarded,
                    ..message
                };
                state.discarded.push_back(message.clone());
                if state.discarded.len() > MAX_DISCARDED {
                    state.discarded.pop_front();
                }
                push_history(&mut state.history, message);
            }
            state.active = false;
            state.accepting = false;
        }
        drop(state);
        // Every way a run ends discards its undelivered guidance, journal
        // included; only process loss skips this drop.
        if let Some((turn, restored)) = ended {
            if !turn.is_empty() {
                self.steering.forget_turn(&turn);
            }
            if let Some(restored) = restored {
                self.steering.forget_turn(&restored);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_bounds_and_assigns_visible_ids() {
        let steering = Arc::new(Steering::default());
        let run = steering.begin();
        let ticket = steering.ticket().expect("active ticket");
        assert_eq!(steering.submit_for(ticket, "  \n"), Err(SubmitError::Empty));
        assert_eq!(
            steering.submit_for(ticket, &"x".repeat(MAX_BYTES + 1)),
            Err(SubmitError::TooLong)
        );
        let first = steering.submit_for(ticket, "ok").expect("within capacity");
        assert_eq!(first.id, 1);
        assert_eq!(first.status, Status::Queued);
        for _ in 1..MAX_MESSAGES {
            steering.submit_for(ticket, "ok").expect("within capacity");
        }
        assert_eq!(steering.submit_for(ticket, "full"), Err(SubmitError::Full));
        assert_eq!(run.drain().len(), MAX_MESSAGES);
        assert!(
            steering
                .messages()
                .iter()
                .all(|message| message.status == Status::Delivered)
        );
    }

    #[test]
    fn withdraw_only_removes_undelivered_message() {
        let steering = Arc::new(Steering::default());
        let run = steering.begin();
        let ticket = steering.ticket().unwrap();
        let message = steering.submit_for(ticket, "withdraw me").unwrap();
        assert_eq!(
            steering.withdraw(message.id).unwrap().status,
            Status::Withdrawn
        );
        assert!(run.drain().is_empty());
        assert_eq!(steering.messages()[0].text, "withdraw me");
        assert_eq!(steering.messages()[0].status, Status::Withdrawn);
        assert!(steering.withdraw(message.id).is_none());
    }

    #[test]
    fn completion_discards_undelivered_messages() {
        let steering = Arc::new(Steering::default());
        let run = steering.begin();
        let ticket = steering.ticket().unwrap();
        let message = steering.submit_for(ticket, "drop me").unwrap();
        drop(run);
        assert_eq!(steering.ticket(), None);
        assert_eq!(
            steering.messages(),
            vec![Message {
                id: message.id,
                text: "drop me".into(),
                status: Status::Discarded,
                input_id: String::new(),
            }]
        );
    }

    #[test]
    fn a_run_end_hands_its_undelivered_guidance_to_the_frontend_once() {
        let steering = Arc::new(Steering::default());
        let run = steering.begin_turn("op_t1", None);
        let ticket = steering.ticket().unwrap();
        steering.submit_for(ticket, "delivered").unwrap();
        assert_eq!(run.drain().len(), 1);
        let dropped = steering.submit_for(ticket, "dropped").unwrap();
        assert!(
            steering.take_discarded().is_empty(),
            "an active run has lost nothing"
        );
        drop(run);
        let reported = steering.take_discarded();
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].id, dropped.id);
        assert_eq!(reported[0].text, "dropped");
        assert_eq!(reported[0].status, Status::Discarded);
        assert!(
            steering.take_discarded().is_empty(),
            "a loss is reported exactly once"
        );
    }

    #[test]
    fn a_withdrawn_message_is_never_reported_as_a_loss() {
        let steering = Arc::new(Steering::default());
        let run = steering.begin_turn("op_t1", None);
        let ticket = steering.ticket().unwrap();
        let withdrawn = steering.submit_for(ticket, "withdraw me").unwrap();
        steering.withdraw(withdrawn.id).unwrap();
        drop(run);
        assert!(steering.take_discarded().is_empty());
    }

    #[test]
    fn ticket_cannot_target_a_later_run_and_close_is_atomic() {
        let steering = Arc::new(Steering::default());
        let first = steering.begin();
        let old_ticket = steering.ticket().expect("first ticket");
        assert_eq!(first.take_or_close(), None);
        assert_eq!(
            steering.submit_for(old_ticket, "late"),
            Err(SubmitError::Inactive)
        );
        drop(first);

        let second = steering.begin();
        let new_ticket = steering.ticket().expect("second ticket");
        assert_ne!(old_ticket, new_ticket);
        assert_eq!(
            steering.submit_for(old_ticket, "stale"),
            Err(SubmitError::Inactive)
        );
        steering.submit_for(new_ticket, "current").unwrap();
        assert_eq!(second.take_or_close().unwrap()[0].text, "current");
        assert_eq!(second.take_or_close(), None);
        assert_eq!(steering.ticket(), None);
    }

    fn journaled() -> (Arc<Steering>, Arc<crate::agent::inbox::Inbox>) {
        let journal = Arc::new(crate::agent::inbox::Inbox::default());
        let steering = Arc::new(Steering::default());
        steering.set_journal(journal.clone());
        (steering, journal)
    }

    #[test]
    fn journaled_guidance_is_scoped_to_its_turn_and_removed_when_delivered() {
        let (steering, journal) = journaled();
        let run = steering.begin_turn("op_t1", None);
        let ticket = steering.ticket().unwrap();
        let message = steering.submit_for(ticket, "look at b.rs").unwrap();
        assert_eq!(message.input_id, "steer:op_t1:1");
        let queued = journal.queued();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].notification.scope, "op_t1");
        assert_eq!(queued[0].notification.id, "steer:op_t1:1");
        for delivered in run.drain() {
            steering.delivered(&delivered);
        }
        assert!(journal.is_empty());
    }

    #[test]
    fn every_run_end_discards_journaled_guidance() {
        let (steering, journal) = journaled();
        let run = steering.begin_turn("op_t1", None);
        let ticket = steering.ticket().unwrap();
        steering.submit_for(ticket, "late").unwrap();
        let withdrawn = steering.submit_for(ticket, "withdraw me").unwrap();
        steering.withdraw(withdrawn.id).unwrap();
        assert_eq!(journal.len(), 1);
        drop(run);
        assert!(journal.is_empty());
    }

    #[test]
    fn process_loss_hands_guidance_to_the_resuming_turn_only() {
        let (steering, journal) = journaled();
        let run = steering.begin_turn("op_lost", None);
        let ticket = steering.ticket().unwrap();
        steering.submit_for(ticket, "use the new API").unwrap();
        // Process loss: the run is never dropped.
        std::mem::forget(run);
        journal.push(Notification {
            text: "stale".into(),
            id: "steer:op_older:1".into(),
            scope: "op_older".into(),
            ..Notification::default()
        });

        // The next process: a fresh registry over the same journal.
        let restarted = Arc::new(Steering::default());
        restarted.set_journal(journal.clone());
        let run = restarted.begin_turn("op_resume", Some("op_lost"));
        let restored = run.drain();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].text, "use the new API");
        assert_eq!(restored[0].input_id, "steer:op_lost:1");
        assert_eq!(
            journal.len(),
            1,
            "the stale entry is gone and the restored one stays until delivery"
        );
        restarted.delivered(&restored[0]);
        assert!(journal.is_empty());
    }

    #[test]
    fn a_turn_that_resumes_nothing_discards_every_other_turns_guidance() {
        let (steering, journal) = journaled();
        journal.push(Notification {
            text: "old".into(),
            id: "steer:op_old:1".into(),
            scope: "op_old".into(),
            ..Notification::default()
        });
        let run = steering.begin_turn("op_new", None);
        assert!(run.drain().is_empty());
        assert!(journal.is_empty());
    }

    #[test]
    fn guidance_that_cannot_be_saved_is_not_accepted() {
        let journal = Arc::new(crate::agent::inbox::Inbox::default());
        journal.set_persist(Box::new(|_| Err("disk full".into())));
        let steering = Arc::new(Steering::default());
        steering.set_journal(journal.clone());
        let run = steering.begin_turn("op_t1", None);
        let ticket = steering.ticket().unwrap();
        assert_eq!(
            steering.submit_for(ticket, "hello"),
            Err(SubmitError::NotPersisted)
        );
        assert!(run.drain().is_empty());
        assert!(journal.is_empty());
    }

    #[test]
    fn a_repeated_request_id_returns_the_first_message() {
        let steering = Arc::new(Steering::default());
        let _run = steering.begin_turn("op_t1", None);
        let ticket = steering.ticket().unwrap();
        let first = steering
            .submit_input(ticket, "hello", Some("req-1"))
            .unwrap();
        let again = steering
            .submit_input(ticket, "hello", Some("req-1"))
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(first.input_id, "req-1");
        assert_eq!(steering.messages().len(), 1);
    }
}
