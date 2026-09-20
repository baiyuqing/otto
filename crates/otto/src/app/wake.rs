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
        self.controller.lock().wake_unstarted = false;
        let runner = match self.controller.runner() {
            Ok(runner) => runner,
            Err(message) => {
                drop(admission);
                return Err(AgentError::Other(message));
            }
        };
        let result = runner.run("", emit, cancel).await;
        drop(admission);
        result
    }

    /// Releases an unstarted claim. Dropping does the same.
    pub fn cancel(self) {}
}
