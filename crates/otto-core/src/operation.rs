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

    /// Why no further work may be dispatched, if the operation has stopped.
    fn stop_reason(&self) -> Option<OperationStopReason>;
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
