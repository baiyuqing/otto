//! Creation of a session's resources by the first runner build.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use otto_core::agent::inbox::Inbox;

use super::SessionResources;
use crate::failover;
use crate::runtime::builder::{Builder, SharedSession};
use crate::subagent;
use crate::subagent::tasks::Tasks;
use crate::tool::bash_output::Logs;
use crate::tool::remind::Reminders;

impl Builder {
    /// Returns the session's resources, creating them on the first call for
    /// that session. Later calls, from any runner build or any profile, return
    /// the same value and ignore `subagents_enabled` and `max_output_bytes`.
    ///
    /// Creation installs the `<session>.inbox.json` sidecar, restores the
    /// unread notifications from it, appends the failover recovery notice and
    /// reconstructs non-running child snapshots on every saved activation.
    /// Saved sessions retain diagnostic task inventory even when no executor
    /// is enabled; no child is spawned by resource creation.
    pub(crate) fn session_resources(
        &self,
        session: &SharedSession,
        subagents_enabled: bool,
        max_output_bytes: usize,
        warnings: &mut dyn Write,
    ) -> Arc<SessionResources> {
        session.resources_or_create(|| {
            let children_dir = PathBuf::from(session.path()).with_extension("");
            let tasks = (subagents_enabled || !self.no_session).then(|| {
                Arc::new(
                    self.new_tasks(session)
                        .with_admission_transcript(Arc::clone(session.handle())),
                )
            });
            let notifications = match &tasks {
                Some(tasks) => Arc::clone(tasks.notifications_inbox()),
                None => Arc::new(Inbox::default()),
            };
            self.attach_inbox_sidecar(session, &notifications, warnings);
            if let Some(tasks) = &tasks {
                if !self.no_session && !session.path().is_empty() {
                    crate::subagent::recovery::activate(
                        Path::new(&session.path()),
                        tasks,
                        notifications.as_ref(),
                        warnings,
                    );
                }
                self.notify_recovery(
                    session,
                    &notifications,
                    &children_dir,
                    max_output_bytes,
                    warnings,
                );
            }
            let reminders = tasks.as_ref().map(|_| {
                let mailbox = notifications.clone();
                Arc::new(match self.sidecar_path(session, "reminders.json") {
                    Some(path) => Reminders::with_persist(mailbox, path),
                    None => Reminders::new(mailbox),
                })
            });
            Arc::new(SessionResources {
                notifications,
                tasks,
                reminders,
                output_logs: Arc::new(Logs::for_session(&session.header().id)),
                mcp: tokio::sync::OnceCell::new(),
                workflow_binding: Mutex::new(None),
                closed: AtomicBool::new(false),
            })
        })
    }

    /// The task registry. A task id stays unique within the session across
    /// resumes, so the counter starts after the highest `t<k>` already used by
    /// a child transcript file.
    fn new_tasks(&self, session: &SharedSession) -> Tasks {
        // Activation's bounded inventory advances the counter and fences new
        // admission if its upper bound is unknown; never perform a silent,
        // unbounded filename scan here.
        let starting_counter = 0;
        match &self.task_recorder {
            Some(store) => {
                let (pid, process_started_at) = subagent::record::current_process();
                let parent_session = if self.no_session {
                    format!("memory:{pid}:{process_started_at}")
                } else {
                    session.header().id
                };
                let session_handle = session.downgrade();
                Tasks::with_recorder_from(
                    Arc::clone(store) as Arc<dyn subagent::record::Recorder>,
                    subagent::record::TaskContext {
                        parent_session,
                        parent_session_path: session.path(),
                        workspace: self.workspace_path.clone(),
                        pid,
                        process_started_at,
                    },
                    starting_counter,
                )
                .with_parent_session_path_resolver(Arc::new(move || {
                    session_handle
                        .upgrade()
                        .map(|session| session.path())
                        .unwrap_or_default()
                }))
            }
            None => Tasks::new_from(starting_counter),
        }
    }

    /// Persists `inbox` to `<session>.inbox.json` and restores its unread
    /// notifications. The persist hook resolves the path at write time: a lazy
    /// session store has no path until its first write, so deciding now would
    /// skip persistence for the whole session.
    fn attach_inbox_sidecar(
        &self,
        session: &SharedSession,
        inbox: &Arc<Inbox>,
        warnings: &mut dyn Write,
    ) {
        // The session owns the inbox, so the hook holds the session weakly.
        let source = session.downgrade();
        let no_session = self.no_session;
        crate::runtime::sidecar_mailbox::install(inbox, ".inbox.json", move || {
            let path = source.upgrade()?.path();
            (!no_session && !path.is_empty()).then(|| Path::new(&path).with_extension("inbox.json"))
        });
        if let Some(path) = self.sidecar_path(session, "inbox.json")
            && let Some(warning) = crate::runtime::sidecar_mailbox::load(inbox, &path)
        {
            let _ = writeln!(warnings, "warning: {warning}");
        }
    }

    /// Reports a lease takeover to the model. A session without a persisted
    /// inbox path has no takeover to report.
    fn notify_recovery(
        &self,
        session: &SharedSession,
        inbox: &Arc<Inbox>,
        children_dir: &Path,
        max_output_bytes: usize,
        warnings: &mut dyn Write,
    ) {
        if self.sidecar_path(session, "inbox.json").is_none() {
            return;
        }
        let recovery_warnings = failover::recovery::notify(
            inbox.as_ref(),
            session.take_recovery(),
            children_dir,
            max_output_bytes,
            &otto_core::session::Session::messages(session),
        );
        for warning in &recovery_warnings {
            let _ = writeln!(warnings, "warning: {warning}");
        }
    }
}
