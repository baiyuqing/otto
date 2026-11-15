//! Detecting a stopped process and revalidating the lease after it.
//!
//! `Instant` advances while the process is stopped (SIGSTOP, a terminal
//! suspend, a debugger). After a stop longer than 5`L`/6 a lease's last
//! successful renewal is older than 5`L`/6 although no other host acted, and
//! the renewal thread has not yet had a tick to renew.
//!
//! The watchdog thread performs no file I/O in its loop, so it only runs
//! later than its schedule when the process (or machine) did not run. The
//! [`Keeper`] records the `Instant` of the watchdog's latest tick. A stall is
//! observed when more than [`STALL_TICKS`] `tick_interval`s passed since that
//! tick. The watchdog then opens a grace period of [`GRACE_TICKS`]
//! `tick_interval`s.
//!
//! [`Lease::should_fence`] defers the 5`L`/6 rule while a grace period is
//! open, or while the latest watchdog tick is older than the stall threshold
//! (a tool thread calling [`Lease::check`] first after the resume, before the
//! watchdog noticed). What replaces the rule depends on the caller:
//!
//! - [`Lease::check`], on a tool thread, reads whether `epoch-<n+1>` exists:
//!   if it does, the lease is superseded; if the read fails, the lease
//!   directory cannot be trusted; otherwise the call proceeds.
//! - The watchdog reads nothing. It stays free of file I/O, so an unreachable
//!   file system cannot delay it. The renewal thread performs the same
//!   `epoch-<n+1>` check on each of its ticks and sets `superseded`, which
//!   the watchdog reads on its next tick.
//!
//! The renewal thread renews in the same period. When the grace period ends
//! without a successful renewal, the 5`L`/6 rule applies and the process is
//! fenced; the bound after a stall is therefore [`GRACE_TICKS`] ticks, also
//! when the lease directory cannot be read.
//!
//! A renewal thread blocked in file I/O while the process runs does not
//! delay the watchdog's ticks, so no stall is observed and no grace period
//! opens: the process is fenced when renewal is older than 5`L`/6.
//!
//! Lock ordering: `Lease::last_renewal`, then `Keeper::stall`, each taken
//! alone and released before the next; neither is held across file I/O.

use super::*;

/// Ticks of the watchdog's schedule that must pass without a tick for the
/// watchdog to observe a stall.
const STALL_TICKS: u32 = 5;

/// Ticks a grace period lasts after an observed stall.
const GRACE_TICKS: u32 = 3;

/// The watchdog's schedule, kept by the [`Keeper`].
#[derive(Default)]
pub(super) struct StallState {
    /// The latest watchdog tick; `None` before the watchdog thread started.
    last_tick: Option<Instant>,
    /// End of the open grace period.
    grace_until: Option<Instant>,
}

impl Keeper {
    fn stall_threshold(&self) -> Duration {
        self.tick_interval * STALL_TICKS
    }

    /// Called by the watchdog thread at the start of each tick. When more
    /// than the stall threshold passed since the previous tick, opens a grace
    /// period.
    pub(super) fn record_watchdog_tick(&self) {
        let now = Instant::now();
        let mut state = self.stall.lock().unwrap();
        if let Some(previous) = state.last_tick
            && now.saturating_duration_since(previous) > self.stall_threshold()
        {
            state.grace_until = Some(now + self.tick_interval * GRACE_TICKS);
        }
        state.last_tick = Some(now);
    }

    /// True while a grace period is open, or while the latest watchdog tick
    /// is older than the stall threshold.
    pub(super) fn stall_defers_time_rule(&self) -> bool {
        let now = Instant::now();
        let state = self.stall.lock().unwrap();
        let in_grace = state.grace_until.is_some_and(|until| now < until);
        let unnoticed = state
            .last_tick
            .is_some_and(|tick| now.saturating_duration_since(tick) > self.stall_threshold());
        in_grace || unnoticed
    }

    /// Test-only: sets the recorded watchdog tick `age` into the past, as
    /// after a stop of that length (a test cannot stop its own process).
    #[cfg(test)]
    pub(super) fn age_watchdog_tick_for_test(&self, age: Duration) {
        self.stall.lock().unwrap().last_tick = Instant::now().checked_sub(age);
    }

    /// Test-only: whether a stall has opened a grace period since creation.
    #[cfg(test)]
    pub(super) fn grace_was_opened_for_test(&self) -> bool {
        self.stall.lock().unwrap().grace_until.is_some()
    }
}

impl Lease {
    /// `Some(reason)` when the `Keeper`'s watchdog should fence this
    /// process: a successor epoch has appeared, or renewal has not succeeded
    /// in over 5`L`/6 and no stall of the process explains it (see the module
    /// documentation). With `probe`, a deferred time rule is replaced by a
    /// read of the lease directory; without it, by nothing. The watchdog
    /// passes `false`.
    pub(super) fn should_fence(&self, probe: bool) -> Option<String> {
        if self.released.load(Ordering::Acquire) {
            return None;
        }
        if self.superseded.load(Ordering::Acquire) {
            return Some(self.superseded_reason());
        }
        let instant = (*self.last_renewal.lock().unwrap())?;
        let elapsed = instant.elapsed();
        if elapsed <= self.duration() * 5 / 6 {
            return None;
        }
        if self.keeper.stall_defers_time_rule() {
            return if probe {
                self.revalidate_after_stall()
            } else {
                None
            };
        }
        Some(format!(
            "no successful heartbeat renewal for {:?}, over 5/6 of the {:?} lease",
            elapsed,
            self.duration()
        ))
    }

    fn superseded_reason(&self) -> String {
        format!(
            "a successor epoch was created while this process held epoch {}",
            self.epoch
        )
    }

    /// Reads whether `epoch-<n+1>` exists, in place of the time rule.
    fn revalidate_after_stall(&self) -> Option<String> {
        match fsops::exists_at_no_follow(&self.dir, &format!("epoch-{}", self.epoch + 1)) {
            Ok(true) => {
                self.superseded.store(true, Ordering::Release);
                Some(self.superseded_reason())
            }
            Ok(false) => None,
            Err(error) => Some(format!(
                "the lease directory could not be read after a process stall: {error}"
            )),
        }
    }
}
