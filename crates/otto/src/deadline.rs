//! Monotonic deadlines for native asynchronous operations.

use std::future::pending;
use std::sync::Mutex;
use std::time::Duration;

use otto_core::model::OperationStopReason;
use otto_core::operation::OperationControl;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// An absolute monotonic deadline, or no deadline at all.
///
/// Constructing a deadline computes its absolute instant once. Cloning it or
/// waiting on it never restarts the duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline(Option<Instant>);

impl Deadline {
    /// Returns a deadline that never expires.
    pub const fn unlimited() -> Self {
        Self(None)
    }

    /// Returns a deadline `duration` from now.
    ///
    /// A duration too large to represent as an [`Instant`] is treated as an
    /// unlimited deadline rather than panicking or wrapping.
    pub fn after(duration: Duration) -> Self {
        Self(Instant::now().checked_add(duration))
    }

    /// Creates a child deadline no later than either the parent budget or the
    /// local duration. An absent local duration inherits the parent exactly.
    pub fn child(parent_remaining: Option<Duration>, local: Option<Duration>) -> Self {
        match (parent_remaining, local) {
            (Some(parent), Some(local)) => Self::after(parent.min(local)),
            (Some(parent), None) => Self::after(parent),
            (None, Some(local)) => Self::after(local),
            (None, None) => Self::unlimited(),
        }
    }

    /// Returns the earlier of `parent` and a local deadline `duration` from
    /// now.
    ///
    /// An unlimited parent imposes no bound. If the local duration cannot be
    /// represented, the parent is retained.
    pub fn earlier(parent: Self, duration: Duration) -> Self {
        let local = Instant::now().checked_add(duration);
        Self(match (parent.0, local) {
            (Some(parent), Some(local)) => Some(parent.min(local)),
            (Some(parent), None) => Some(parent),
            (None, local) => local,
        })
    }

    /// Returns the time remaining, or `None` when the deadline is unlimited.
    ///
    /// Expired deadlines have zero remaining duration.
    pub fn remaining(&self) -> Option<Duration> {
        self.0
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// Returns whether this deadline has elapsed.
    pub fn is_expired(&self) -> bool {
        self.0.is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Waits until this deadline expires.
    ///
    /// Waiting on an unlimited deadline never completes.
    pub async fn sleep_until(&self) {
        match self.0 {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => pending::<()>().await,
        }
    }

    /// Waits until this deadline expires.
    pub async fn expired(&self) {
        self.sleep_until().await;
    }
}

/// Invocation-scoped native operation control.
///
/// The deadline is absolute and immutable. The first stop request wins, then
/// cancels the private token used to interrupt in-flight asynchronous work.
pub struct Control {
    deadline: Deadline,
    token: CancellationToken,
    stop_reason: Mutex<Option<OperationStopReason>>,
}

impl Control {
    pub fn new(deadline: Deadline) -> Self {
        Self {
            deadline,
            token: CancellationToken::new(),
            stop_reason: Mutex::new(None),
        }
    }

    /// Records the first stop reason and interrupts in-flight work.
    pub fn stop(&self, reason: OperationStopReason) {
        let mut current = self.stop_reason.lock().expect("operation stop reason");
        if current.is_none() {
            *current = Some(reason);
            self.token.cancel();
        }
    }

    pub fn deadline(&self) -> Deadline {
        self.deadline
    }
}

impl OperationControl for Control {
    fn cancellation_token(&self) -> &CancellationToken {
        &self.token
    }

    fn remaining(&self) -> Option<Duration> {
        self.deadline.remaining()
    }

    fn stop_reason(&self) -> Option<OperationStopReason> {
        *self.stop_reason.lock().expect("operation stop reason")
    }

    fn admission_stop_reason(&self) -> Option<OperationStopReason> {
        if self.deadline.is_expired() {
            self.stop(OperationStopReason::Deadline);
        }
        self.stop_reason()
    }
}

#[cfg(test)]
mod tests {
    use super::{Control, Deadline};
    use otto_core::model::OperationStopReason;
    use otto_core::operation::OperationControl;
    use std::time::Duration;

    #[test]
    fn control_preserves_the_first_typed_stop_reason() {
        let control = Control::new(Deadline::unlimited());
        assert_eq!(control.stop_reason(), None);
        assert!(!control.cancellation_token().is_cancelled());

        control.stop(OperationStopReason::Deadline);
        control.stop(OperationStopReason::UserCancellation);

        assert_eq!(control.stop_reason(), Some(OperationStopReason::Deadline));
        assert!(control.cancellation_token().is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn control_reports_the_same_absolute_remaining_budget() {
        let control = Control::new(Deadline::after(Duration::from_secs(5)));
        assert_eq!(control.remaining(), Some(Duration::from_secs(5)));
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(control.remaining(), Some(Duration::from_secs(3)));
    }

    #[tokio::test(start_paused = true)]
    async fn child_never_extends_the_parent_budget() {
        assert_eq!(
            Deadline::child(Some(Duration::from_secs(3)), Some(Duration::from_secs(9))).remaining(),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            Deadline::child(Some(Duration::from_secs(9)), Some(Duration::from_secs(3))).remaining(),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            Deadline::child(Some(Duration::from_secs(4)), None).remaining(),
            Some(Duration::from_secs(4))
        );
        assert_eq!(Deadline::child(None, None), Deadline::unlimited());
    }

    #[tokio::test(start_paused = true)]
    async fn earlier_uses_the_earliest_absolute_deadline() {
        let parent = Deadline::after(Duration::from_secs(10));
        tokio::time::advance(Duration::from_secs(2)).await;

        let inherited = Deadline::earlier(parent, Duration::from_secs(20));
        let local = Deadline::earlier(Deadline::unlimited(), Duration::from_secs(3));

        assert_eq!(inherited.remaining(), Some(Duration::from_secs(8)));
        assert_eq!(local.remaining(), Some(Duration::from_secs(3)));
    }

    #[tokio::test(start_paused = true)]
    async fn remaining_counts_down_without_resetting() {
        let deadline = Deadline::after(Duration::from_secs(5));

        assert_eq!(deadline.remaining(), Some(Duration::from_secs(5)));
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(deadline.remaining(), Some(Duration::from_secs(3)));
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_never_expires_or_completes() {
        let deadline = Deadline::unlimited();

        assert_eq!(deadline.remaining(), None);
        assert!(!deadline.is_expired());
        assert!(
            tokio::time::timeout(Duration::from_secs(1), deadline.expired())
                .await
                .is_err()
        );
        assert!(!deadline.is_expired());
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_uses_the_original_absolute_instant() {
        let deadline = Deadline::after(Duration::from_secs(5));
        tokio::time::advance(Duration::from_secs(2)).await;
        let wait = deadline.expired();
        tokio::pin!(wait);

        assert!(
            tokio::time::timeout(Duration::from_secs(2), &mut wait)
                .await
                .is_err()
        );
        assert!(!deadline.is_expired());

        deadline.sleep_until().await;
        assert!(deadline.is_expired());
        assert_eq!(deadline.remaining(), Some(Duration::ZERO));
    }

    #[tokio::test(start_paused = true)]
    async fn overflowing_duration_is_safe() {
        let parent = Deadline::after(Duration::from_secs(4));

        assert_eq!(Deadline::after(Duration::MAX), Deadline::unlimited());
        assert_eq!(Deadline::earlier(parent, Duration::MAX), parent);
    }
}
