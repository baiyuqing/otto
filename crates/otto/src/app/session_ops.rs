//! Session rename, replacement, listing, resume, fork and archive.

use super::*;

impl Controller {
    pub fn rename_session(&self, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(INVALID_SESSION_NAME.to_string());
        }
        let _admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let session = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.session.clone())
            .ok_or_else(|| CLOSED.to_string())?;
        session
            .rename(name)
            .map_err(|error| self.builder.redact_error(&error, None))
    }

    /// Admission for an operation that replaces the session. Ordering: closed
    /// first, then the capability gate, then the active check.
    pub(super) fn begin_replacement(&self) -> Result<Admission<'_>, String> {
        let mut state = self.lock();
        if state.closed {
            return Err(CLOSED.to_string());
        }
        if state.busy {
            return Err(PROMPT_ACTIVE.to_string());
        }
        state.busy = true;
        state.generation += 1;
        Ok(Admission {
            controller: self,
            generation: state.generation,
        })
    }

    pub async fn new_session(&self) -> Result<(), String> {
        let admission = self.begin_replacement()?;
        let runtime = self.current_runtime()?;
        let replacement = self.fresh_replacement(&runtime).await?;
        self.commit(replacement, admission)?;
        Ok(())
    }

    /// Discards the resume result the line frontend does not use.
    pub async fn switch_profile(&self, profile: &str) -> Result<(), String> {
        self.switch_profile_result(profile).await.map(|_| ())
    }

    pub async fn switch_profile_result(&self, profile: &str) -> Result<ResumeResult, String> {
        let admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        let runtime = self.builder.resolve_profile(profile)?;
        if !self.builder.boundary_allows_dynamic(Some(&runtime)) {
            return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
        }
        let session = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.session.clone())
            .ok_or_else(|| CLOSED.to_string())?;
        let runner = Arc::new(self.builder.build_runner_quiet(&session, &runtime).await?);
        if let Err(error) = self.builder.update_session_runtime(&session, &runtime) {
            runner.close();
            return Err(error);
        }
        let path = session.path();
        {
            let mut state = self.lock();
            let current = state.current.as_mut().ok_or_else(|| CLOSED.to_string())?;
            current.info = self.builder.runtime_info(&runtime);
            let old = std::mem::replace(&mut current.runner, runner);
            old.close();
        }
        drop(admission);
        Ok(ResumeResult {
            session_path: path,
            warnings: Vec::new(),
        })
    }

    /// Rows carry `current` for the file already open.
    pub fn list_sessions(&self, limit: usize) -> Result<ListResult, String> {
        if self.lock().closed {
            return Err(CLOSED.to_string());
        }
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let limit = limit.clamp(1, MAX_LIST_SESSIONS);
        let current_path = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.session.path())
            .unwrap_or_default();
        let session_root = self
            .builder
            .storage
            .session_root(&self.builder.session_root)
            .map_err(|error| self.builder.redact_error(&error, None))?;
        self.builder
            .storage
            .sessions()
            .and_then(|repository| {
                repository.list(
                    &session_root,
                    &self.builder.workspace_path,
                    &current_path,
                    limit,
                )
            })
            .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    /// Resuming the session already in force is a no-op that reports its path.
    pub async fn resume_session(&self, path: &str) -> Result<ResumeResult, String> {
        let requested = canonical_session_path(path);
        let admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        {
            let state = self.lock();
            if let Some(current) = state.current.as_ref()
                && !requested.is_empty()
                && requested == current.canonical_path()
            {
                return Ok(ResumeResult {
                    session_path: current.path(),
                    warnings: Vec::new(),
                });
            }
        }
        let (replacement, warnings) = self.resumed_replacement(Path::new(path)).await?;
        let workspace_before = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.workspace.clone())
            .unwrap_or_default();
        if replacement.path().is_empty() {
            let _ = replacement.discard();
            return Err("replacement session path is required".to_string());
        }
        if replacement.workspace != workspace_before {
            let _ = replacement.discard();
            return Err(
                "replacement session workspace does not match current workspace".to_string(),
            );
        }
        let path = self.commit(replacement, admission)?;
        Ok(ResumeResult {
            session_path: path,
            warnings,
        })
    }

    /// The places the current session can be forked, oldest first. A session
    /// with no file yet has none.
    pub fn fork_points(&self) -> Result<Vec<otto_core::session::ForkPoint>, String> {
        let path = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            if !self.dynamic_content {
                return Err(PERSISTENCE_DISABLED.to_string());
            }
            state
                .current
                .as_ref()
                .map(|current| current.session.path())
                .unwrap_or_default()
        };
        if path.is_empty() {
            return Ok(Vec::new());
        }
        self.builder
            .session_fork_points(&path)
            .map_err(|error| self.builder.redact_error(&error, None))
    }

    /// Writes a new session forked from the current one, stopping before the
    /// turn the user message `before` opens, or at the last turn boundary
    /// without `before`. The current session is unchanged and stays open.
    pub fn fork_file(&self, before: Option<&str>) -> Result<crate::session::Forked, ForkFailure> {
        self.fork_file_with_id(before, None, None)
    }

    pub fn fork_file_with_id(
        &self,
        before: Option<&str>,
        id: Option<&str>,
        request_id: Option<&str>,
    ) -> Result<crate::session::Forked, ForkFailure> {
        let path = {
            let state = self.lock();
            if state.closed {
                return Err(ForkFailure::Failed(CLOSED.to_string()));
            }
            if !self.dynamic_content {
                return Err(ForkFailure::Failed(PERSISTENCE_DISABLED.to_string()));
            }
            state
                .current
                .as_ref()
                .map(|current| current.session.path())
                .unwrap_or_default()
        };
        if path.is_empty() {
            return Err(ForkFailure::Failed(NOTHING_TO_FORK.to_string()));
        }
        (if let Some(id) = id {
            self.builder
                .fork_session_file_with_id(&path, before, id, request_id)
        } else {
            self.builder.fork_session_file(&path, before)
        })
        .map_err(|error| match error {
            otto_core::session::ForkError::InvalidPoint(_) => {
                ForkFailure::InvalidPoint(error.to_string())
            }
            otto_core::session::ForkError::Session(error) => {
                ForkFailure::Failed(self.builder.redact_error(&error.to_string(), None))
            }
        })
    }

    /// Forks the current session (see [`Controller::fork_file`]) and switches
    /// to the fork. Returns the switch result and the prompt the fork stops
    /// before, for the editor.
    pub async fn fork_session(
        &self,
        before: Option<&str>,
    ) -> Result<(ResumeResult, Option<String>), String> {
        if self.lock().busy {
            return Err(PROMPT_ACTIVE.to_string());
        }
        let forked = self.fork_file(before).map_err(|error| error.to_string())?;
        let result = self.resume_session(&forked.path.to_string_lossy()).await?;
        Ok((result, forked.point.map(|point| point.prompt)))
    }

    /// Archiving the current session delegates so a picker row behaves like
    /// `/archive`.
    pub async fn archive_session(&self, path: &str) -> Result<ArchiveResult, String> {
        let requested = canonical_session_path(path);
        let is_current = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            if !self.dynamic_content {
                return Err(PERSISTENCE_DISABLED.to_string());
            }
            if state.busy {
                return Err(PROMPT_ACTIVE.to_string());
            }
            state.current.as_ref().is_some_and(|current| {
                !requested.is_empty() && requested == current.canonical_path()
            })
        };
        if is_current {
            return self.archive_current_session().await;
        }
        let session_root = self
            .builder
            .storage
            .session_root(&self.builder.session_root)
            .map_err(|error| self.builder.redact_error(&error, None))?;
        self.builder
            .storage
            .sessions()
            .and_then(|repository| {
                repository.archive(&session_root, &self.builder.workspace_path, path)
            })
            .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    /// The replacement is built before the archive move, so every failure path
    /// leaves the current session intact. The move is the last and only
    /// committed state change.
    ///
    /// Archiving ends the session, so its outstanding timers end with it:
    /// `sessionfs::archive` removes the sidecar and the registry is cleared
    /// here. The clear runs only after the move succeeds, because a refused
    /// archive leaves the session running and its timers must survive. It also
    /// runs before the swap, so a timer that fires between the two cannot write
    /// the sidecar back.
    pub async fn archive_current_session(&self) -> Result<ArchiveResult, String> {
        let admission = self.begin_replacement()?;
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
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let runtime = self.current_runtime_from_info(info)?;
        let replacement = self.fresh_replacement(&runtime).await?;
        match session.archive(&self.builder.session_root, &self.builder.workspace_path) {
            Ok(result) => {
                if let Some(reminders) = self.reminders() {
                    reminders.clear();
                }
                self.commit(replacement, admission)?;
                Ok(result)
            }
            Err(error) => {
                let _ = replacement.discard();
                Err(self
                    .builder
                    .redact_error(&error.to_string(), Some(&runtime)))
            }
        }
    }

    // ---- tasks and wake ----
}
