//! Turn-bound text accepted while a run is active.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

const MAX_MESSAGES: usize = 16;
const MAX_BYTES: usize = 32_000;
const MAX_HISTORY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    Inactive,
    Empty,
    TooLong,
    Full,
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
}

#[derive(Default)]
struct State {
    generation: u64,
    next_id: u64,
    active: bool,
    accepting: bool,
    messages: VecDeque<Message>,
    history: VecDeque<Message>,
}

/// Transient FIFO for messages bound to one active run. Messages remain
/// observable for the run even after delivery, but are never persisted unless
/// delivery reaches the agent loop.
#[derive(Default)]
pub struct Steering(Mutex<State>);

impl Steering {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
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
        if state.messages.len() >= MAX_MESSAGES {
            return Err(SubmitError::Full);
        }
        state.next_id = state.next_id.wrapping_add(1);
        let message = Message {
            id: state.next_id,
            text: text.to_owned(),
            status: Status::Queued,
        };
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
        Some(message)
    }

    pub(super) fn begin(self: &Arc<Self>) -> Run {
        let mut state = self.lock();
        state.generation = state.generation.wrapping_add(1);
        state.active = true;
        state.accepting = true;
        state.messages.clear();
        state.history.clear();
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
/// discarded rather than allowing it to wake a later run.
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
        if state.active && state.generation == self.ticket {
            let discarded: Vec<_> = state.messages.drain(..).collect();
            for message in discarded {
                push_history(
                    &mut state.history,
                    Message {
                        status: Status::Discarded,
                        ..message
                    },
                );
            }
            state.active = false;
            state.accepting = false;
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
                status: Status::Discarded
            }]
        );
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
}
