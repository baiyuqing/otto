//! The `otto` binary: flag parsing, runtime composition, and the frontends.
//!
//! Port of `cmd/otto`. The composition root lives here rather than in
//! `main.rs` so the whole startup path is reachable from integration tests
//! without spawning a process.

pub mod boundary;
/// The phase-4c seam is gone: `otto::app` is the real controller and every
/// `cli::controller::…` path keeps resolving to it.
pub use crate::app as controller;
pub mod flags;
pub mod info;
pub mod login;
pub mod mcp;
pub mod memory_command;
pub mod prompt;
pub mod repl;
pub mod repl_commands;
pub mod run;
pub mod runtime_builder;
pub mod sandbox_runtime;
pub mod sandbox_setup;
pub mod sandbox_switch;
pub mod serve;
#[cfg(test)]
pub mod testutil;
pub mod wiring;
pub mod workspace_context;
