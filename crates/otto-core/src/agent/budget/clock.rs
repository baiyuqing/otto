//! The active-time clock of one root.
//!
//! The clock adds up the time during which work of the root makes progress.
//! It has one explicit state ([`Clock`]), changed only by [`Change`]s:
//!
//! - [`Clock::Idle`]: no holder of the root is running. Idle time adds
//!   nothing, whether the root turn ended, a timer is pending or the user is
//!   away.
//! - [`Clock::Running`]: at least one running holder is not waiting for a user
//!   decision. The elapsed time is added to the total.
//! - [`Clock::AllWaiting`]: every running holder is waiting for a user
//!   decision. Nothing is added.
//!
//! Time comes from an injected [`MonotonicClock`]: `otto-core` performs no
//! clock access of its own.

use std::fmt;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// A source of monotonic time. `otto-platform` provides the process clock; a
/// test uses [`ManualClock`].
pub trait MonotonicClock: Send + Sync + fmt::Debug {
    /// The time elapsed since an origin the implementation fixes once. Never
    /// decreases between calls.
    fn elapsed(&self) -> Duration;
}

/// A shared handle to a [`MonotonicClock`].
pub type SharedClock = Arc<dyn MonotonicClock>;

/// A clock that never advances: a root built on it measures no active time, so
/// `max_duration` never stops it. For a run that has no controller turn (a
/// manual compaction, a test of something else).
#[derive(Debug, Default, Clone, Copy)]
pub struct Untimed;

impl MonotonicClock for Untimed {
    fn elapsed(&self) -> Duration {
        Duration::ZERO
    }
}

/// [`Untimed`] as a [`SharedClock`].
pub fn untimed() -> SharedClock {
    Arc::new(Untimed)
}

/// A clock that moves only when [`Self::advance`] is called. Clones share the
/// same time, so a test keeps one handle and gives another to the code under
/// test.
#[derive(Debug, Default, Clone)]
pub struct ManualClock(Arc<Mutex<Duration>>);

impl ManualClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *now += by;
    }

    /// This clock as a [`SharedClock`] that shares its time.
    pub fn shared(&self) -> SharedClock {
        Arc::new(self.clone())
    }
}

impl MonotonicClock for ManualClock {
    fn elapsed(&self) -> Duration {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The state of the clock. `active` counts the holders that are running;
/// `waiting` counts those of them that wait for a user decision. `waiting` is
/// always below `active` in [`Self::Running`]; at equality the state is
/// [`Self::AllWaiting`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clock {
    Idle,
    /// `since` is the clock reading at which the elapsed time was last added.
    Running {
        since: Duration,
        active: NonZeroU32,
        waiting: u32,
    },
    AllWaiting {
        active: NonZeroU32,
    },
}

impl Clock {
    /// `(active, waiting)`.
    fn counts(self) -> (u32, u32) {
        match self {
            Self::Idle => (0, 0),
            Self::Running {
                active, waiting, ..
            } => (active.get(), waiting),
            Self::AllWaiting { active } => (active.get(), active.get()),
        }
    }
}

/// A change of the holder counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Change {
    /// A holder starts running.
    Enter,
    /// A running holder ends; `waiting` says whether it was waiting for a user
    /// decision at that moment.
    Leave { waiting: bool },
    /// A running holder starts waiting for a user decision.
    Wait,
    /// A waiting holder resumes.
    Resume,
}

/// The active time of one root: the total so far and the clock.
#[derive(Debug)]
pub(super) struct ActiveTime {
    used: Duration,
    clock: Clock,
}

impl ActiveTime {
    pub(super) fn new() -> Self {
        Self {
            used: Duration::ZERO,
            clock: Clock::Idle,
        }
    }

    /// The active time at the clock reading `now`, including the part of a
    /// running interval that has not been added yet.
    pub(super) fn used(&self, now: Duration) -> Duration {
        match self.clock {
            Clock::Running { since, .. } => self.used + now.saturating_sub(since),
            Clock::Idle | Clock::AllWaiting { .. } => self.used,
        }
    }

    /// Adds the elapsed time of a running interval, then applies `change`.
    pub(super) fn apply(&mut self, now: Duration, change: Change) {
        self.used = self.used(now);
        let (active, waiting) = self.clock.counts();
        let (active, waiting) = match change {
            Change::Enter => (active.saturating_add(1), waiting),
            Change::Leave { waiting: was } => (
                active.saturating_sub(1),
                waiting.saturating_sub(u32::from(was)),
            ),
            Change::Wait => (active, waiting.saturating_add(1)),
            Change::Resume => (active, waiting.saturating_sub(1)),
        };
        self.clock = match NonZeroU32::new(active) {
            None => Clock::Idle,
            Some(active) if waiting >= active.get() => Clock::AllWaiting { active },
            Some(active) => Clock::Running {
                since: now,
                active,
                waiting,
            },
        };
    }

    #[cfg(test)]
    pub(super) fn counts(&self) -> (u32, u32) {
        self.clock.counts()
    }
}
