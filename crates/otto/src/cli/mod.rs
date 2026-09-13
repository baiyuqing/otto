//! The `otto` binary: flag parsing, runtime composition, and the frontends.
//!
//! Port of `cmd/otto`. The composition root lives here rather than in
//! `main.rs` so the whole startup path is reachable from integration tests
//! without spawning a process.

pub mod boundary;
pub mod controller;
pub mod flags;
pub mod info;
pub mod memory_command;
pub mod prompt;
pub mod repl;
pub mod repl_commands;
pub mod run;
pub mod runtime_builder;
pub mod sandbox_runtime;
#[cfg(test)]
pub mod testutil;
pub mod wiring;
pub mod workspace_context;
