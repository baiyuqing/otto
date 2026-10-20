//! Herdr reporting and memory-review availability for the controller.

use super::*;

impl Controller {
    /// Starts reporting this session's lifecycle to the Herdr pane that
    /// started the process. `cli::run` calls this for the frontends that are
    /// the pane's agent; without it nothing is reported.
    ///
    /// The pane is announced here, not at the first turn: a Herdr pane is
    /// nobody until the agent inside it reports, so Otto would be missing from
    /// the agent list — and unreachable through `herdr agent ...` — until the
    /// user sent a prompt.
    pub fn install_herdr(&self, reporter: Arc<crate::herdr::Reporter>) {
        reporter.ready(&self.herdr_session());
        *self
            .herdr
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reporter);
    }

    pub(super) fn herdr_reporter(&self) -> Option<Arc<crate::herdr::Reporter>> {
        self.herdr
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// What Herdr shows and restores for this session. Read without the
    /// [`Controller::info`] snapshot, which clones the transcript.
    pub(super) fn herdr_session(&self) -> crate::herdr::SessionRef {
        let state = self.lock();
        match state.current.as_ref() {
            Some(current) => crate::herdr::SessionRef {
                id: current.session.header().id,
                path: current.path(),
            },
            None => crate::herdr::SessionRef {
                id: String::new(),
                path: String::new(),
            },
        }
    }

    /// Queues the notices about memory that needs the user's attention:
    /// pending candidates in the current user and workspace scopes, and
    /// standing preferences that recall does not inject because the standing
    /// set is over its budget. Called once per session open: on construction
    /// and when `commit` installs a replacement session.
    pub(super) fn notify_memory_state(&self) {
        let memory = &self.builder.memory;
        if !memory.usable {
            return;
        }
        let scopes = [
            memory.user_scope.clone(),
            self.builder.workspace_scope.clone(),
        ];
        if let Ok(pending) = self.approval_snapshot()
            && let Ok(summary) = crate::approval::memory::human_pending_summary(
                &memory.service,
                &scopes,
                &pending.requests,
            )
            && let Some(oldest) = summary.oldest
        {
            let line = match summary.count {
                1 => format!(
                    "1 memory proposal is pending review (since {}). Run /memory review to accept or reject it.",
                    oldest.format("%Y-%m-%d")
                ),
                count => format!(
                    "{count} memory proposals are pending review (oldest since {}). Run /memory review to accept or reject them.",
                    oldest.format("%Y-%m-%d")
                ),
            };
            self.notices.push(line);
        }
        if let Ok(usage) = memory.service.standing_usage(&memory.user_scope)
            && usage.omitted > 0
        {
            let budget = usage.budget;
            self.notices.push(match usage.omitted {
                1 => format!(
                    "1 standing preference is not recalled because the standing set is over its {budget}-token budget. Forget one with /memory forget <id>; list them with /memory list --scope user."
                ),
                omitted => format!(
                    "{omitted} standing preferences are not recalled because the standing \
                        set is over its {budget}-token budget. Forget some with /memory forget \
                        <id>; list them with /memory list --scope user."
                ),
            });
        }
    }
}
