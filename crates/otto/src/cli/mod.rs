//! The parts of the `otto` command line that the terminal frontend shares:
//! the REPL, its slash commands and the login flow. The binary, flag parsing,
//! the process lifecycle and the composition root live in the `otto-cli`
//! crate, which builds on this one.

/// `otto::app` is the real controller; every `cli::controller::…` path keeps
/// resolving to it.
pub use crate::app as controller;
/// The sandbox switch lives in the runtime layer; the old path keeps resolving.
pub use crate::runtime::sandbox_switch;
pub mod login;
pub mod repl;
pub mod repl_commands;
