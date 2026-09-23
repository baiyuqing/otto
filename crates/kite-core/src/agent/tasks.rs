//! The subagent task registry contract.
//!
//! A concurrent registry of subagent tasks lives in `crate::subagent`, but the
//! turn loop itself touches only one method: it closes the registry when the
//! agent shuts down.
//!
//! This module therefore ships only the boundary the turn loop needs. The full
//! registry, its task records, and its name validation live in the native
//! crate, which implements this trait.
//!
//! Ownership: the agent borrows a registry through `Options`. Closing it
//! belongs to the agent, because the agent's shutdown is what must stop running
//! tasks.
//!
//! Concurrency: an implementation is shared with every running task, so `close`
//! takes `&self` and must be safe to call from any task and more than once.
//!
//! Errors: none. Closing is best effort; a registry that is already closed does
//! nothing.

/// The part of the subagent task registry the turn loop depends on.
pub trait TaskRegistry {
    /// Stops accepting new tasks and cancels the running ones. Calling it
    /// twice is allowed and does nothing the second time.
    fn close(&self);
}

/// A registry that holds no tasks. It is what an agent gets when subagents
/// are not configured, so the shutdown path needs no null check.
#[derive(Debug, Default)]
pub struct NoTasks;

impl TaskRegistry for NoTasks {
    fn close(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_the_empty_registry_twice_is_allowed() {
        let tasks = NoTasks;
        tasks.close();
        tasks.close();
    }
}
