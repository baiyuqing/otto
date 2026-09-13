//! Sub-agent tasks: child agent loops started by the parent's `agent` tool,
//! tracked in the session's task registry.
//!
//! Ported from Go `internal/subagent` plus the registry in
//! `internal/agent/tasks.go`. A child runs with its own transcript, the
//! parent's tool set minus the agent-control and memory tools, and no ability
//! to start children of its own: delegation depth is fixed at one.
//!
//! Security: an `AGENT.md` definition is untrusted text. Its body is appended
//! to the child's system prompt under a fixed heading and can never widen the
//! child's tool set beyond what the parent already holds, nor reach the memory
//! tools.

pub mod definition;
pub mod format;
pub mod inherit;
pub mod prompt;
pub mod runner;
pub mod tasks;
#[cfg(test)]
pub(crate) mod testsupport;
pub mod tools;

pub use definition::{Catalog, Definition};
pub use inherit::inherit_snapshot;
pub use prompt::prompt_section;
pub use tasks::{Task, TaskError, TaskStatus, Tasks};
