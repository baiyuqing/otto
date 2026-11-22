//! Sub-agent tasks: child agent loops started by the parent's `agent` tool,
//! tracked in the session's task registry.
//!
//! A child runs with its own transcript, the parent's tool set minus the
//! agent-control and memory tools, and no ability to start children of its own:
//! delegation depth is fixed at one.
//!
//! Security: an `AGENT.md` definition is untrusted text. Its body is appended
//! to the child's system prompt under a fixed heading and can never widen the
//! child's tool set beyond what the parent already holds, nor reach the memory
//! tools.

mod acp_approval;
mod acp_process;
pub(crate) mod acp_session;
pub(crate) mod acp_transport;
pub mod definition;
pub mod format;
pub mod inherit;
pub mod interrupted;
pub mod prompt;
pub(crate) mod provenance;
pub mod record;
pub(crate) mod recovery;
mod reports;
pub mod runner;
mod scratch;
pub mod session_recovery;
pub mod sqlite;
pub mod tasks;
pub mod tools;

pub(crate) use definition::WritePolicy;
pub use definition::{Catalog, Definition};
pub(crate) use inherit::inherit_snapshot;
pub(crate) use prompt::prompt_section;
pub use tasks::{Task, TaskStatus, Tasks};

/// A new identifier for a root budget, empty when randomness is unavailable.
/// It only correlates the log records of one root.
pub(crate) fn new_budget_id() -> String {
    otto_platform::random::random_id()
        .map(|random| format!("bud_{random}"))
        .unwrap_or_default()
}
