//! Assembles one runnable agent from resolved configuration: the provider
//! client, the tool registry and its sandbox, memory, skills, sub-agents and
//! the system prompt.
//!
//! This is the runtime layer that `app`, `tool`, `reflection` and the
//! frontends build on. It must not depend on a frontend (`cli`, `tui`, `acp`,
//! `server`, `client`); `tests/runtime_boundary.rs` enforces that. Flag
//! parsing, the REPL and process lifecycle stay in [`crate::cli`], which
//! composes this layer with the frontends.

pub mod advice;
pub mod boundary;
pub mod builder;
pub mod info;
pub mod prompt;
pub mod sandbox_config;
pub mod sandbox_runtime;
pub mod storage;
pub mod wiring;
pub mod workspace_context;
