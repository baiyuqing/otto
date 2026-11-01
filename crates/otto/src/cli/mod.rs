//! The `otto` binary: flag parsing, the REPL, process lifecycle, and the
//! composition of the frontends over [`crate::runtime`].
//!
//! The composition root lives here rather than in `main.rs` so the whole
//! startup path is reachable from integration tests without spawning a process.

pub mod acp;
/// The phase-4c seam is gone: `otto::app` is the real controller and every
/// `cli::controller::…` path keeps resolving to it.
pub use crate::app as controller;
/// The sandbox switch moved into the runtime layer; the old path keeps resolving.
pub use crate::runtime::sandbox_switch;
pub mod config_command;
pub mod eval_command;
pub mod flags;
pub mod login;
pub mod mcp;
pub mod memory_command;
pub mod panic_cleanup;
pub mod plugin;
pub mod repl;
pub mod repl_commands;
pub mod run;
pub mod sandbox_setup;
pub mod serve;
pub mod setup;
pub mod storage;
mod storage_transfer;
pub mod terminate;
pub mod trust;
pub mod workflow;
pub mod workspace_command;
