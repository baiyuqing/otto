//! Turn-bound text accepted while a run is active.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

const MAX_MESSAGES: usize = 16;
const MAX_BYTES: usize = 32_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    Inactive,
    Empty,
    TooLong,
    Full,
}

#[derive(Default)]
struct State {
    generation: u64,
    active: bool,
    accepting: bool,
    messages: VecDeque<String>,
}

/// Transient FIFO for messages bound to one active run.
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

    /// Queues guidance for the current run. The caller should keep a ticket
    /// captured when it began handling this request and use `submit_for`.
    pub fn submit(&self, text: &str) -> Result<(), SubmitError> {
        let Some(ticket) = self.ticket() else {
            return Err(SubmitError::Inactive);
        };
        self.submit_for(ticket, text)
    }

    /// Queues guidance only for the exact run identified by `ticket`.
    pub fn submit_for(&self, ticket: u64, text: &str) -> Result<(), SubmitError> {
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
        state.messages.push_back(text.to_owned());
        Ok(())
    }

    pub(super) fn begin(self: &Arc<Self>) -> Run {
        let mut state = self.lock();
        state.generation = state.generation.wrapping_add(1);
        state.active = true;
        state.accepting = true;
        state.messages.clear();
        Run {
            steering: Arc::clone(self),
            ticket: state.generation,
        }
    }
}

/// The active run's admission guard. Dropping it discards undelivered text.
pub(super) struct Run {
    steering: Arc<Steering>,
    ticket: u64,
}

impl Run {
    pub(super) fn drain(&self) -> Vec<String> {
        let mut state = self.steering.lock();
        if state.active && state.generation == self.ticket {
            state.messages.drain(..).collect()
        } else {
            Vec::new()
        }
    }

    /// Atomically consumes pending text or closes acceptance before success.
    pub(super) fn take_or_close(&self) -> Option<Vec<String>> {
        let mut state = self.steering.lock();
        if !state.active || state.generation != self.ticket {
            return None;
        }
        if state.messages.is_empty() {
            state.accepting = false;
            None
        } else {
            Some(state.messages.drain(..).collect())
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let mut state = self.steering.lock();
        if state.active && state.generation == self.ticket {
            state.active = false;
            state.accepting = false;
            state.messages.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_bounds_messages() {
        let steering = Arc::new(Steering::default());
        let run = steering.begin();
        let ticket = steering.ticket().expect("active ticket");
        assert_eq!(steering.submit_for(ticket, "  \n"), Err(SubmitError::Empty));
        assert_eq!(
            steering.submit_for(ticket, &"x".repeat(MAX_BYTES + 1)),
            Err(SubmitError::TooLong)
        );
        for _ in 0..MAX_MESSAGES {
            steering.submit_for(ticket, "ok").expect("within capacity");
        }
        assert_eq!(steering.submit_for(ticket, "full"), Err(SubmitError::Full));
        assert_eq!(run.drain(), vec!["ok"; MAX_MESSAGES]);
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
        assert_eq!(second.take_or_close(), Some(vec!["current".into()]));
        assert_eq!(second.take_or_close(), None);
        assert_eq!(steering.ticket(), None);
    }

    #[test]
    fn dropping_a_run_discards_pending_guidance() {
        let steering = Arc::new(Steering::default());
        let run = steering.begin();
        let ticket = steering.ticket().unwrap();
        steering.submit_for(ticket, "drop me").unwrap();
        drop(run);
        assert_eq!(steering.ticket(), None);
        let next = steering.begin();
        assert!(next.drain().is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn completion_race_never_loses_an_accepted_message() {
        use std::sync::Barrier;

        let steering = Arc::new(Steering::default());
        let run = steering.begin();
        let ticket = steering.ticket().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let producer = Arc::clone(&steering);
        let producer_barrier = Arc::clone(&barrier);
        let join = std::thread::spawn(move || {
            producer_barrier.wait();
            producer.submit_for(ticket, "raced")
        });
        barrier.wait();
        let closed = run.take_or_close();
        let submitted = join.join().unwrap();
        match submitted {
            Ok(()) => assert_eq!(closed, Some(vec!["raced".into()])),
            Err(SubmitError::Inactive) => assert_eq!(closed, None),
            other => panic!("unexpected race result: {other:?}"),
        }
    }
}
