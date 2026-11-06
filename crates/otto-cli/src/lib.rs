//! The `otto` binary: flag parsing, process lifecycle, and the composition of
//! the frontends over [`otto::runtime`], [`otto_server`] and [`otto_acp`].
//!
//! The composition root lives here rather than in `main.rs` so the whole
//! startup path is reachable from integration tests without spawning a process.
//! The terminal frontend (`otto::tui`, `otto::cli::repl`, `otto::cli::login`)
//! still lives in the `otto` crate.

pub mod acp;
pub mod config_command;
pub mod eval_command;
pub mod flags;
pub mod mcp;
pub mod memory_command;
pub mod memory_eval_command;
pub mod panic_cleanup;
pub mod plugin;
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
