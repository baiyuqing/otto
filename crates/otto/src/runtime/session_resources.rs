//! Resources that live as long as one session, not as long as one runner.
//!
//! A [`super::builder::Runner`] is rebuilt for a profile, model or thinking
//! switch and for the background MCP swap; the session stays. The sub-agent
//! task registry, the notification inbox, the timer registry, the bash output
//! logs, the workflow delivery binding and the connected MCP servers belong
//! to the session, are created once by the first runner build
//! ([`Builder::session_resources`]) and are borrowed by every later runner.
//!
//! Ownership rule: a runner never closes a session resource. Resources end
//! through [`SharedSession::close`] (the session is discarded) or
//! [`SharedSession::close_resources`] (process exit and `/new`/`/load`
//! teardown that awaits the shutdown).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use otto_core::agent::inbox::Inbox;

use super::builder::SharedSession;
use super::workflow_notifications::Binding;
use crate::subagent::tasks::Tasks;
use crate::tool::bash_output::Logs;
use crate::tool::remind::Reminders;

mod construction;
mod session_mcp;

pub(crate) use session_mcp::SessionMcp;

/// How long [`SharedSession::close_resources`] waits for MCP clients to exit.
const MCP_CLOSE_WAIT: Duration = Duration::from_secs(5);

/// The resources of one session. See the module documentation.
pub(crate) struct SessionResources {
    /// The parent's notification inbox: sub-agent reports, timers, recovery
    /// notices and workflow lifecycle events. Persisted as
    /// `<session>.inbox.json` when the session has a file.
    notifications: Arc<Inbox>,
    /// The sub-agent task registry; absent when sub-agents are off.
    tasks: Option<Arc<Tasks>>,
    /// The timer registry; absent when sub-agents are off.
    reminders: Option<Arc<Reminders>>,
    /// Retention for bash output files; shared by the parent and its children.
    output_logs: Arc<Logs>,
    /// Connected once, by the first runner build that connects MCP.
    mcp: tokio::sync::OnceCell<SessionMcp>,
    /// The workflow lifecycle delivery worker of the latest runner build.
    workflow_binding: Mutex<Option<Arc<Binding>>>,
    closed: AtomicBool,
}

impl SessionResources {
    pub(crate) fn notifications(&self) -> &Arc<Inbox> {
        &self.notifications
    }

    pub(crate) fn tasks(&self) -> Option<&Arc<Tasks>> {
        self.tasks.as_ref()
    }

    pub(crate) fn reminders(&self) -> Option<&Arc<Reminders>> {
        self.reminders.as_ref()
    }

    pub(crate) fn output_logs(&self) -> &Arc<Logs> {
        &self.output_logs
    }

    pub(crate) fn mcp(&self) -> Option<&SessionMcp> {
        self.mcp.get()
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Installs `binding` as the delivery worker and stops the previous one,
    /// so two workers never write the same inbox. A closed session stops the
    /// new binding at once.
    pub(crate) fn replace_workflow_binding(&self, binding: Arc<Binding>) {
        let mut slot = self
            .workflow_binding
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if self.is_closed() {
            binding.stop();
        }
        if let Some(old) = slot.replace(binding) {
            old.stop();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move { old.join().await });
            }
        }
    }

    /// Stops background work without waiting: the workflow binding, the
    /// sub-agent tasks (their cancel tokens fire), the timers, and, when a
    /// Tokio runtime is present, the MCP servers. Idempotent.
    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(binding) = self
            .workflow_binding
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            binding.stop();
        }
        if let Some(tasks) = &self.tasks {
            tasks.close();
        }
        if let Some(reminders) = &self.reminders {
            reminders.stop();
        }
        if let (Some(mcp), Ok(runtime)) = (self.mcp.get(), tokio::runtime::Handle::try_current()) {
            let servers = Arc::clone(&mcp.servers);
            runtime.spawn(async move { servers.close().await });
        }
    }

    /// Waits for the workflow worker to exit and for MCP clients to close, up
    /// to [`MCP_CLOSE_WAIT`] for the clients.
    async fn wait_closed(&self) {
        let binding = self
            .workflow_binding
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(binding) = binding {
            binding.join().await;
        }
        if let Some(mcp) = self.mcp.get() {
            let _ = tokio::time::timeout(MCP_CLOSE_WAIT, mcp.servers.close()).await;
        }
    }

    /// Sets the MCP servers of a scripted test session; ignored when set.
    #[cfg(test)]
    pub(crate) fn set_test_mcp(&self, servers: Arc<crate::mcp::Servers>) {
        let _ = self.mcp.set(SessionMcp {
            servers,
            connected: Vec::new(),
        });
    }

    /// Resources for a scripted test runner built around `tasks`.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn for_tests(session_id: &str, tasks: Option<Arc<Tasks>>) -> Arc<Self> {
        let notifications = match &tasks {
            Some(tasks) => Arc::clone(tasks.notifications_inbox()),
            None => Arc::new(Inbox::default()),
        };
        let reminders = tasks
            .as_ref()
            .map(|_| Arc::new(Reminders::new(notifications.clone())));
        Arc::new(Self {
            notifications,
            tasks,
            reminders,
            output_logs: Arc::new(Logs::for_session(session_id)),
            mcp: tokio::sync::OnceCell::new(),
            workflow_binding: Mutex::new(None),
            closed: AtomicBool::new(false),
        })
    }
}

impl SharedSession {
    /// Stops the session's background work and waits for it to finish: the
    /// workflow delivery worker exits and the MCP clients close (bounded).
    /// Idempotent. Does not touch the transcript or the hook state; the owner
    /// that discards the session also calls [`SharedSession::close`].
    pub async fn close_resources(&self) {
        if let Some(resources) = self.resources() {
            resources.close();
            resources.wait_closed().await;
        }
    }

    /// Stops the resources without waiting (see [`SessionResources::close`]).
    pub(crate) fn stop_resources(&self) {
        if let Some(resources) = self.resources() {
            resources.close();
        }
    }
}
