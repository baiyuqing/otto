//! Sandbox reload and amendment, and tool and bash approvals.

use super::*;

/// Refusal for a read grant on a path the read denylist does not cover.
const PATH_ALREADY_READABLE: &str = "path is already readable; no read_paths entry is needed";

impl Controller {
    pub async fn reload_sandbox(&self) -> Result<SandboxInfo, String> {
        let control = {
            let state = self.lock();
            if state.is_closed() {
                return Err(CLOSED.to_string());
            }
            let Some(control) = self.sandbox.clone() else {
                return Err(SANDBOX_RELOAD_UNAVAILABLE.to_string());
            };
            if state.is_busy() {
                return Err(PROMPT_ACTIVE.to_string());
            }
            control
        };
        control.reload().await
    }

    /// Checks an explicit local attachment against private-file policy without
    /// granting any access. The frontend must open the direct path without following links.
    pub fn can_read_image_attachment(&self, path: &std::path::Path) -> bool {
        if !path.is_absolute() || self.lock().is_closed() {
            return false;
        }
        let denylist = self
            .sandbox
            .as_ref()
            .and_then(|control| control.read_denylist())
            .unwrap_or_else(|| {
                crate::sandbox::read_denylist::ReadDenylist::new(
                    self.builder.home.clone(),
                    self.builder.config.sandbox.deny_read_paths.clone(),
                )
            });
        !denylist.covers(path)
    }

    /// Resolves a user-supplied `/sandbox allow` path against this process's
    /// home directory and the filesystem, without changing anything.
    pub fn resolve_sandbox_read_path(&self, input: &str) -> Result<String, String> {
        sandbox_config::resolve_read_path(input, &self.builder.home)
    }

    /// Persists one `[sandbox]` change to the configuration file and applies
    /// it to the live sandbox.
    ///
    /// Nothing is written unless a sandbox control exists and no turn is
    /// running, and a reload that fails puts the previous configuration back,
    /// so the file on disk always describes a sandbox this process accepted.
    /// Only `read_paths` and `network` are offered: an `allow_env` change
    /// cannot be reloaded into a running process at all.
    pub async fn amend_sandbox(&self, change: SandboxChange) -> Result<SandboxInfo, String> {
        let _admission = self.begin_operation()?;
        self.amend_sandbox_with_admission(change).await
    }

    pub(super) async fn amend_sandbox_with_admission(
        &self,
        change: SandboxChange,
    ) -> Result<SandboxInfo, String> {
        let control = {
            let state = self.lock();
            if state.is_closed() {
                return Err(CLOSED.to_string());
            }
            let Some(control) = self.sandbox.clone() else {
                return Err(SANDBOX_RELOAD_UNAVAILABLE.to_string());
            };
            control
        };
        if let SandboxChange::AllowReadPath(read_path) = &change
            && let Some(denylist) = control.read_denylist()
            && std::fs::canonicalize(read_path).is_ok_and(|canonical| !denylist.covers(&canonical))
        {
            return Err(PATH_ALREADY_READABLE.to_string());
        }
        let path = self.builder.config_path.clone();
        let amendment = sandbox_config::amend_sandbox_config(&path, &change)?;
        match control.reload().await {
            Ok(info) => Ok(info),
            Err(message) => Err(
                match sandbox_config::revert_sandbox_config(&path, &amendment) {
                    Ok(()) => format!("{message}; the configuration change was rolled back"),
                    Err(failure) => format!(
                        "{message}; the configuration change could not be rolled back: {failure}"
                    ),
                },
            ),
        }
    }

    pub(super) fn admitted_session_id(&self) -> Result<String, String> {
        let state = self.lock();
        if state.is_closed() {
            return Err(CLOSED.to_string());
        }
        Ok(state
            .current
            .as_ref()
            .ok_or_else(|| CLOSED.to_string())?
            .session
            .header()
            .id)
    }

    pub(super) fn bash_approvals(&self) -> Result<&Arc<crate::tool::bash::BashApprovals>, String> {
        self.builder
            .bash_approvals
            .as_ref()
            .ok_or_else(|| "temporary elevation is unavailable".to_string())
    }

    /// Called only by human/client decision handlers, never by model tools.
    pub fn reserve_approval(&self, id: &str) -> Result<(), String> {
        self.bash_approvals()?
            .reserve(&self.info().session_id, id)
            .map_err(str::to_string)
    }

    /// Applies an approved sandbox read grant or approves one elevated command.
    pub async fn approve_tool(&self, id: &str) -> Result<String, String> {
        let _admission = self.begin_operation()?;
        let session_id = self.admitted_session_id()?;
        self.approve_tool_for_session(&session_id, id).await
    }

    pub(crate) async fn approve_tool_for_session(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<String, String> {
        let approvals = self.bash_approvals()?;
        let _decision = approvals.decision.lock().await;
        let path = approvals
            .pending_read_path(session_id, id)
            .ok_or_else(|| "approval request not found".to_string())?;
        if path.is_empty() {
            return self.approve_bash_for_session(session_id, id);
        }
        // A symlink must still resolve to the path the user reviewed.
        if self.resolve_sandbox_read_path(&path)? != path {
            return Err("sandbox read path changed; request approval again".to_string());
        }
        self.amend_sandbox_with_admission(SandboxChange::AllowReadPath(path))
            .await?;
        approvals.approve(session_id, id).map_err(str::to_string)?;
        Ok(read_grant_followup(id))
    }

    pub fn deny_tool(&self, id: &str) -> Result<(), String> {
        let _admission = self.begin_operation()?;
        let session_id = self.admitted_session_id()?;
        self.deny_tool_for_session(&session_id, id)
    }

    pub(crate) fn deny_tool_for_session(&self, session_id: &str, id: &str) -> Result<(), String> {
        self.bash_approvals()?.discard(session_id, id);
        Ok(())
    }

    pub(super) fn approve_bash_for_session(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<String, String> {
        let approvals = self.bash_approvals()?;
        if approvals
            .pending_read_path(session_id, id)
            .is_some_and(|path| !path.is_empty())
        {
            return Err("this request requires sandbox read approval".to_string());
        }
        approvals.approve(session_id, id).map_err(str::to_string)?;
        Ok(format!(
            "The user approved {id} for one exact command. Retry the same elevated Bash command now."
        ))
    }

    /// Excludes the exact pending command from the sandbox, then grants the
    /// pending command once. Nothing is written when
    /// `excluded_command_entry` refuses the command.
    pub async fn approve_bash_always(&self, id: &str) -> Result<String, String> {
        let _admission = self.begin_operation()?;
        let session_id = self.admitted_session_id()?;
        self.approve_bash_always_for_session(&session_id, id).await
    }

    pub(super) async fn approve_bash_always_for_session(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<String, String> {
        let approvals = self.bash_approvals()?;
        if approvals
            .pending_read_path(session_id, id)
            .is_some_and(|path| !path.is_empty())
        {
            return self.approve_tool_for_session(session_id, id).await;
        }
        self.exclude_and_grant_pending_command(session_id, id).await
    }

    /// The `always` half of an approval decision made while the turn that
    /// requested the approval is still waiting for it. That turn holds the
    /// controller admission and applies the ordinary grant itself when the
    /// decision arrives, so this method does not take an admission. A command
    /// request is excluded from the sandbox and granted here, ahead of the
    /// decision; the turn's own grant afterwards finds the request already
    /// granted and succeeds again. A read-path request is left to the turn,
    /// exactly as `/approve <id> always` treats it. Returns the follow-up
    /// prompt text.
    pub async fn approve_bash_always_while_waiting(&self, id: &str) -> Result<String, String> {
        let session_id = self.info().session_id;
        let approvals = self.bash_approvals()?;
        if approvals
            .pending_read_path(&session_id, id)
            .is_some_and(|path| !path.is_empty())
        {
            return Ok(read_grant_followup(id));
        }
        self.exclude_and_grant_pending_command(&session_id, id)
            .await
    }

    async fn exclude_and_grant_pending_command(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<String, String> {
        let approvals = self.bash_approvals()?;
        let _decision = approvals.decision.lock().await;
        let command = approvals
            .pending_command(session_id, id)
            .ok_or_else(|| "approval request not found".to_string())?;
        let entry = otto_core::config::sandbox::excluded_command_entry(&command)
            .map_err(|reason| {
                format!(
                    "cannot exclude this command from the sandbox: {reason}; use /approve <id> instead for one run, or /sandbox exclude '<program> *' to exclude a program deliberately"
                )
            })?;
        self.amend_sandbox_with_admission(SandboxChange::ExcludeCommand(entry.clone()))
            .await?;
        approvals.approve(session_id, id).map_err(str::to_string)?;
        Ok(format!(
            "The user approved {id} and excluded '{entry}' from the sandbox. Retry the same Bash command now."
        ))
    }

    // ---- replacement helpers ----
}

fn read_grant_followup(id: &str) -> String {
    format!(
        "The user approved {id} for persistent sandbox read access. Retry the same Bash call inside the sandbox now."
    )
}
