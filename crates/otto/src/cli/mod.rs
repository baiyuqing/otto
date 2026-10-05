//! The `otto` binary: flag parsing, the REPL, process lifecycle, and the
//! composition of the frontends over [`crate::runtime`].
//!
//! The composition root lives here rather than in `main.rs` so the whole
//! startup path is reachable from integration tests without spawning a process.

pub mod acp;
/// The phase-4c seam is gone: `otto::app` is the real controller and every
/// `cli::controller::…` path keeps resolving to it.
pub use crate::app as controller;
pub mod flags;
pub mod login;
pub mod mcp;
pub mod memory_command;
pub mod repl;
pub mod repl_commands;
pub mod run;
pub mod sandbox_setup;
pub mod sandbox_switch;
pub mod serve;
pub mod setup;
pub mod storage;
pub mod terminate;
#[cfg(test)]
pub mod testutil;
pub mod trust;
pub mod workflow;
pub mod workspace_command;
