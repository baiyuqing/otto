//! Decides what a `SIGTERM` does, without touching the signal itself.
//!
//! `main.rs` owns the process signal registration; this module owns the
//! decision. The first `SIGTERM` this process ever receives is turned into
//! [`Action::Migrate`] when the process holds at least one session lease
//! (see [`crate::failover::lease::holds_lease`]), so `run.rs`/`serve.rs` can
//! run the migration in [the spec's
//! order](../../../docs/specs/2026-09-28-session-failover.md) after their
//! frontend returns, instead of exiting immediately. A process with no
//! lease keeps today's behavior: `otto serve` cancels its listen loop and
//! exits cleanly; every other frontend is killed by the signal, since it
//! never installs a SIGTERM handler at all outside `otto serve`.
//!
//! `Terminate` is `Send + Sync` and holds only atomics, so one instance is
//! created in `main.rs` and shared by reference with the signal task and
//! with `run`/`serve`.

use std::sync::atomic::{AtomicU8, Ordering};

/// What a delivered `SIGTERM` should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Cancel the process token; the frontend's normal exit path then runs
    /// a migration instead of a plain close.
    Migrate,
    /// Cancel the process token; the frontend exits normally, no migration.
    Cancel,
    /// Restore the default `SIGTERM` disposition and re-raise it.
    Die,
    /// Do nothing.
    Ignore,
}

/// Not yet signaled.
const UNSET: u8 = 0;
/// The first signal decided to migrate.
const MIGRATING: u8 = 1;
/// The first signal decided to cancel (a lease-less `otto serve`).
const CANCELED: u8 = 2;

/// Tracks the outcome of the first `SIGTERM` this process receives, so every
/// later one is decided from that outcome alone, never from `holds_lease`
/// again (a migration may have released the lease by the time a second
/// signal arrives).
#[derive(Debug, Default)]
pub struct Terminate {
    state: AtomicU8,
    serve: std::sync::atomic::AtomicBool,
}

impl Terminate {
    /// A fresh, unsignaled instance.
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(UNSET),
            serve: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Marks this process as running `otto serve`, so a lease-less first
    /// `SIGTERM` decides [`Action::Cancel`] instead of [`Action::Die`]. Call
    /// before entering `serve::run`.
    pub fn set_serve(&self) {
        self.serve.store(true, Ordering::Release);
    }

    /// Decides what this `SIGTERM` delivery should do. `holds_lease` is the
    /// caller's current read of [`crate::failover::lease::holds_lease`],
    /// taken fresh for every signal.
    pub fn on_signal(&self, holds_lease: bool) -> Action {
        let first = if holds_lease { MIGRATING } else { CANCELED };
        let previous = self
            .state
            .compare_exchange(UNSET, first, Ordering::AcqRel, Ordering::Acquire)
            .unwrap_or_else(|actual| actual);
        if previous == UNSET {
            // This delivery decided the outcome.
            return match first {
                MIGRATING => Action::Migrate,
                _ if self.serve.load(Ordering::Acquire) => Action::Cancel,
                _ => Action::Die,
            };
        }
        match previous {
            MIGRATING => Action::Die,
            _ => Action::Ignore,
        }
    }

    /// True once a signal has decided [`Action::Migrate`].
    pub fn migrating(&self) -> bool {
        self.state.load(Ordering::Acquire) == MIGRATING
    }
}

/// Restores the default `SIGTERM` disposition and re-raises it against this
/// process, so the process dies exactly as it would have without a handler
/// installed (core dump policy, exit status, everything a supervisor
/// expects from a bare `SIGTERM`). `_exit(143)` (128 + `SIGTERM`) is a
/// fallback for the case where the re-raised signal is somehow not
/// delivered before this function would otherwise return.
pub fn die_by_sigterm() -> ! {
    #![allow(unsafe_code)]
    // SAFETY: `signal` and `raise` are both async-signal-safe and take no
    // pointers here; SIG_DFL is a valid disposition constant.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }
    // SAFETY: raising a signal this process just restored to its default
    // disposition is always valid.
    unsafe {
        libc::raise(libc::SIGTERM);
    }
    // SAFETY: _exit is always valid to call and does not run destructors.
    unsafe { libc::_exit(143) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_signal_with_a_lease_migrates() {
        let terminate = Terminate::new();
        assert_eq!(terminate.on_signal(true), Action::Migrate);
        assert!(terminate.migrating());
    }

    #[test]
    fn first_signal_without_a_lease_on_serve_cancels() {
        let terminate = Terminate::new();
        terminate.set_serve();
        assert_eq!(terminate.on_signal(false), Action::Cancel);
        assert!(!terminate.migrating());
    }

    #[test]
    fn first_signal_without_a_lease_off_serve_dies() {
        let terminate = Terminate::new();
        assert_eq!(terminate.on_signal(false), Action::Die);
        assert!(!terminate.migrating());
    }

    #[test]
    fn a_later_signal_after_migrate_dies() {
        let terminate = Terminate::new();
        assert_eq!(terminate.on_signal(true), Action::Migrate);
        assert_eq!(terminate.on_signal(true), Action::Die);
        assert_eq!(terminate.on_signal(false), Action::Die);
    }

    #[test]
    fn a_later_signal_after_cancel_is_ignored() {
        let terminate = Terminate::new();
        terminate.set_serve();
        assert_eq!(terminate.on_signal(false), Action::Cancel);
        assert_eq!(terminate.on_signal(false), Action::Ignore);
        assert_eq!(terminate.on_signal(true), Action::Ignore);
    }

    #[test]
    fn terminate_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Terminate>();
    }
}
