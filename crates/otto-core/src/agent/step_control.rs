//! Per-step operation control: lets a step interrupt stop one provider call
//! or tool batch without stopping the turn.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::model::OperationStopReason;
use crate::operation::OperationControl;

/// The control of one agent-loop step. Its token is a child of the turn token,
/// so a turn cancellation reaches the step; cancelling the step token alone
/// leaves the turn control unstopped.
pub(super) struct StepControl<'a> {
    token: CancellationToken,
    parent: &'a dyn OperationControl,
}

impl<'a> StepControl<'a> {
    pub(super) fn new(parent: &'a dyn OperationControl) -> Self {
        Self {
            token: parent.cancellation_token().child_token(),
            parent,
        }
    }

    /// Whether this step ended because of a step interrupt: its own token is
    /// cancelled while the turn control is not stopped.
    pub(super) fn interrupted(&self) -> bool {
        self.token.is_cancelled() && self.parent.admission_stop_reason().is_none()
    }
}

impl OperationControl for StepControl<'_> {
    fn cancellation_token(&self) -> &CancellationToken {
        &self.token
    }

    fn remaining(&self) -> Option<Duration> {
        self.parent.remaining()
    }

    fn stop_reason(&self) -> Option<OperationStopReason> {
        self.parent.stop_reason()
    }

    fn admission_stop_reason(&self) -> Option<OperationStopReason> {
        self.parent.admission_stop_reason().or_else(|| {
            self.token
                .is_cancelled()
                .then_some(OperationStopReason::UserCancellation)
        })
    }
}
