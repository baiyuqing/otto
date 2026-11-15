//! Assembles one runnable agent from resolved configuration: the provider
//! client, the tool registry and its sandbox, memory, skills, sub-agents and
//! the system prompt.
//!
//! This is the runtime layer that `app`, `tool` and the
//! frontends build on. It must not depend on a frontend (`cli`, `tui`, `acp`,
//! `server`, `client`); `tests/layer_boundary.rs` enforces that. Flag
//! parsing, the REPL and process lifecycle stay in [`crate::cli`], which
//! composes this layer with the frontends.

pub mod advice;
pub mod approval_review;
pub mod boundary;
pub mod builder;
#[cfg(test)]
pub(crate) mod mcp_server_fixture_tests;
pub mod prompt;
pub mod retry_safety;
pub mod sandbox_runtime;
pub mod sandbox_switch;
pub(crate) mod session_resources;
pub mod sidecar_mailbox;
pub mod startup;
pub mod storage;
pub mod task_control;
pub mod verification;
pub mod wiring;
pub mod workflow;
pub mod workflow_notifications;
pub mod workspace_context;

/// Returned when a session operation cannot run in the current boundary or
/// process; callers compare against it to tell this cause apart.
pub const SESSION_OPERATION_UNAVAILABLE: &str = "session operation is unavailable";
