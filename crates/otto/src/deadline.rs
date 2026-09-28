//! Monotonic deadlines for native asynchronous operations.

use std::future::pending;
use std::time::Duration;
use tokio::time::Instant;

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

#[cfg(test)]
mod tests {
    use super::Deadline;
    use std::time::Duration;

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
