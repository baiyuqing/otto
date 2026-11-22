//! The monotonic clock behind the active-time limit of a turn budget.

use std::sync::Arc;
use std::time::{Duration, Instant};

use otto_core::agent::budget::{MonotonicClock, SharedClock};

/// Reads the process monotonic clock, counted from the moment it was created.
/// It never goes back when the wall clock is set.
#[derive(Debug, Clone, Copy)]
pub struct ProcessClock {
    origin: Instant,
}

impl ProcessClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for ProcessClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for ProcessClock {
    fn elapsed(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A new [`ProcessClock`] as a [`SharedClock`].
pub fn process_clock() -> SharedClock {
    Arc::new(ProcessClock::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_clock_does_not_go_back() {
        let clock = process_clock();
        let first = clock.elapsed();
        std::thread::sleep(Duration::from_millis(2));
        let second = clock.elapsed();
        assert!(second >= first + Duration::from_millis(2));
    }
}
