//! Operation-wide cancellation and deadline state.
//!
//! The owner computes time and updates the state. Core consumers only observe
//! a cancellation token, an optional remaining budget, and the typed reason an
//! operation must stop, keeping this contract wasm-safe and free of clocks.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::model::OperationStopReason;

/// Shared control for every provider, compaction, and tool step in one
/// operation. Implementations must not reset the remaining budget between
/// calls.
pub trait OperationControl: Send + Sync {
    /// Token used to interrupt in-flight asynchronous work.
    fn cancellation_token(&self) -> &CancellationToken;

    /// Remaining operation budget, if the operation has a deadline.
    fn remaining(&self) -> Option<Duration>;

    /// Why the operation has already stopped. This is stable terminal state;
    /// reading it must not turn a completed operation into a timeout.
    fn stop_reason(&self) -> Option<OperationStopReason>;

    /// Checks whether new work may be admitted. Native implementations use
    /// this checkpoint to advance an expired absolute deadline into typed stop
    /// state before an effectful future is first polled.
    fn admission_stop_reason(&self) -> Option<OperationStopReason> {
        self.stop_reason()
    }
}

/// Backward-compatible unlimited operation control. Cancellation of the token
/// is interpreted as an explicit user cancellation.
impl OperationControl for CancellationToken {
    fn cancellation_token(&self) -> &CancellationToken {
        self
    }

    fn remaining(&self) -> Option<Duration> {
        None
    }

    fn stop_reason(&self) -> Option<OperationStopReason> {
        self.is_cancelled()
            .then_some(OperationStopReason::UserCancellation)
    }

    fn admission_stop_reason(&self) -> Option<OperationStopReason> {
        self.stop_reason()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_token_is_an_unlimited_compatibility_control() {
        let token = CancellationToken::new();
        assert_eq!(token.remaining(), None);
        assert_eq!(OperationControl::stop_reason(&token), None);
        token.cancel();
        assert_eq!(
            OperationControl::stop_reason(&token),
            Some(OperationStopReason::UserCancellation)
        );
    }
}
