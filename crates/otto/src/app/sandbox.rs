//! The optional sandbox-reload capability.
//!
//! The composition root owns exactly one sandbox, so every
//! [`Controller`](super::Controller) built from it reports the same live state
//! rather than the value captured at startup.
//!
//! Ownership: the implementation is `cli::sandbox_switch::SandboxReloader`,
//! built by the composition root in `cli::run` and shared by the REPL, the TUI
//! and `otto serve`. A process whose bash never came up gets no control at all,
//! so `/sandbox reload` reports
//! [`SANDBOX_RELOAD_UNAVAILABLE`](super::SANDBOX_RELOAD_UNAVAILABLE) rather
//! than a failure with nothing to re-point.

use crate::cli::info::SandboxInfo;

/// Reads the live sandbox state and replaces it.
///
/// Concurrency: `info` is called while a controller holds its own lock, so an
/// implementation must not block on a reload in progress.
#[async_trait::async_trait]
pub trait SandboxControl: Send + Sync {
    /// The sandbox state now in effect.
    fn info(&self) -> SandboxInfo;

    /// Re-reads the sandbox configuration and applies it, returning the state
    /// that is now in effect. On failure the previous sandbox stays in force
    /// and the message says why.
    async fn reload(&self) -> Result<SandboxInfo, String>;
}
