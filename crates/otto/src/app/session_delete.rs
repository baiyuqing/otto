//! Irreversible session removal, separate from append-only archive.
use super::*;

impl Controller {
    /// Permanently removes a session listed in this workspace. A current
    /// session is replaced first; an active prompt or writer is never cancelled.
    pub async fn delete_session(&self, path: &str) -> Result<(), String> {
        let requested = canonical_session_path(path);
        let is_current = {
            let state = self.lock();
            if state.is_closed() {
                return Err(CLOSED.into());
            }
            if !self.dynamic_content {
                return Err(PERSISTENCE_DISABLED.into());
            }
            state
                .current
                .as_ref()
                .is_some_and(|current| current.canonical_path() == requested)
        };
        if is_current {
            let current_path = self.info().session_path;
            if crate::fsops::clean_go_path(path) != crate::fsops::clean_go_path(&current_path) {
                return Err(
                    "permanent delete requires the listed session path, not a path alias".into(),
                );
            }
            return self.delete_current_session().await;
        }
        let _admission = self.begin_replacement()?;
        let session_root = self.builder.session_root.clone();
        self.builder
            .storage
            .sessions()
            .permanent_delete(&session_root, &self.builder.workspace_path, path)
            .map_err(|error| self.builder.redact_error(&error, None))
    }

    /// Prepares a replacement before irreversible removal. Only idle sessions
    /// are admitted. Cleanup failures are errors, never successful deletion.
    pub(crate) async fn delete_current_session(&self) -> Result<(), String> {
        let admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.into());
        }
        let (session, path, info) = {
            let state = self.lock();
            let current = state.current.as_ref().ok_or_else(|| CLOSED.to_string())?;
            (
                current.session.clone(),
                current.session.path(),
                current.info.clone(),
            )
        };
        if path.is_empty() {
            return Err(PERSISTENCE_DISABLED.into());
        }
        if self
            .subagent_tasks()
            .is_some_and(|tasks| tasks.list().iter().any(|task| !task.status.is_final()))
        {
            return Err(PROMPT_ACTIVE.into());
        }
        let runtime = self.current_runtime_from_info(info)?;
        let replacement = self.fresh_replacement(&runtime).await?;
        let delete =
            || session.permanent_delete(&self.builder.session_root, &self.builder.workspace_path);
        let result = match self.reminders() {
            Some(reminders) => reminders.deleting_session(delete),
            None => delete(),
        };
        match result {
            Ok(()) => {
                self.commit(replacement, admission)?;
                Ok(())
            }
            Err(error) => {
                let _ = replacement.discard();
                Err(self.builder.redact_error(&error, Some(&runtime)))
            }
        }
    }
}
