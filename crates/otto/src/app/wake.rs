//! One admitted, empty-text notification turn.
//!
//! A wake exists so a frontend can deliver sub-agent notifications without
//! racing a user turn: the claim is taken under the same admission rule as a
//! prompt, and the turn runs at most once.
//!
//! Lifetime: the claim is held by an [`Admission`], so dropping a
//! [`WakeOperation`] without running it releases the claim.

use otto_core::agent::{AgentError, EventSink};
use tokio_util::sync::CancellationToken;

use super::{Admission, CLOSED, Controller};

/// A claimed wake turn. Run it, or drop it to release the claim.
pub struct WakeOperation<'a> {
    controller: &'a Controller,
    admission: Option<Admission<'a>>,
}

impl<'a> WakeOperation<'a> {
    pub(super) fn new(controller: &'a Controller, admission: Admission<'a>) -> Self {
        Self {
            controller,
            admission: Some(admission),
        }
    }

    /// Executes the claimed turn once with empty user text, which is how the
    /// agent signals "deliver pending notifications".
    pub async fn run(
        mut self,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let Some(admission) = self.admission.take() else {
            return Err(AgentError::Other(CLOSED.to_string()));
        };
        // The claim is no longer releasable by `request_close`: it started.
        if !self.controller.lock().start_wake(admission.generation) {
            return Err(AgentError::Other(CLOSED.to_string()));
        }
        let runner = match self.controller.runner() {
            Ok(runner) => runner,
            Err(message) => {
                drop(admission);
                return Err(AgentError::Other(message));
            }
        };
        // Decided before the turn runs: running it records a new turn. A wake
        // inside `run_with_channel` already has a task that adopted the
        // follow-ups and runs them itself.
        let resumes =
            self.controller.resumable_interrupted_turn() && !runner.task_control().is_active();
        let mut result = runner.run("", &mut *emit, cancel).await;
        // A wake turn that resumed an interrupted task runs the follow-ups
        // that task had scheduled and not yet started.
        if result.is_ok() && resumes && !cancel.is_cancelled() {
            result = self
                .controller
                .run_resumed_followups(&runner, &mut *emit, cancel)
                .await;
        }
        drop(admission);
        result
    }

    /// Releases an unstarted claim. Dropping does the same.
    pub fn cancel(self) {}
}
