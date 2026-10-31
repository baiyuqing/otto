//! Runner replacement, shutdown of runtime services, permission commands and approval snapshots.

use super::*;

impl Controller {
    pub(super) fn runner(&self) -> Result<Arc<Runner>, String> {
        self.lock()
            .current
            .as_ref()
            .map(|current| Arc::clone(&current.runner))
            .ok_or_else(|| CLOSED.to_string())
    }

    /// Installs a fully rebuilt runner only when no operation is active and
    /// the session is still the one the caller built against. This is the
    /// background MCP hot-swap path: the slow runner build happens outside
    /// the controller lock, and this small commit step refuses to run mid-turn.
    /// A rejected candidate is closed before returning.
    pub fn replace_runner_if_current(
        &self,
        session_id: &str,
        runner: Runner,
        info: RuntimeInfo,
    ) -> Result<bool, String> {
        enum Decision {
            Installed(Arc<Runner>),
            Busy,
            Closed,
            StaleSession,
        }

        let mut runner = Some(runner);
        let decision = {
            let mut state = self.lock();
            if state.closed {
                Decision::Closed
            } else if state.busy {
                Decision::Busy
            } else {
                let current = state.current.as_mut().ok_or_else(|| CLOSED.to_string())?;
                if current.session.header().id != session_id {
                    Decision::StaleSession
                } else {
                    current.info = info;
                    Decision::Installed(std::mem::replace(
                        &mut current.runner,
                        Arc::new(runner.take().expect("runner is still available")),
                    ))
                }
            }
        };
        match decision {
            Decision::Installed(old) => {
                old.close();
                Ok(true)
            }
            Decision::Busy => {
                if let Some(runner) = runner.take() {
                    runner.close();
                }
                Err(PROMPT_ACTIVE.to_string())
            }
            Decision::Closed => {
                if let Some(runner) = runner.take() {
                    runner.close();
                }
                Err(CLOSED.to_string())
            }
            Decision::StaleSession => {
                if let Some(runner) = runner.take() {
                    runner.close();
                }
                Ok(false)
            }
        }
    }

    /// Join this workspace's shared workflow owner before process shutdown.
    /// Session replacement does not close it.
    pub async fn close_workflows(&self) {
        crate::runtime::workflow::close(&self.builder).await;
    }

    /// Ends the current session's resources and waits for them: session-end
    /// hooks run, sub-agent tasks are cancelled, the workflow delivery worker
    /// exits and MCP servers close (up to 5 seconds). A no-op on a controller
    /// with no current runner (already closed, or never opened).
    /// [`Self::close`] also stops the resources, but without waiting
    /// (`SharedSession::close`, via `Current::discard`): a process exiting
    /// right after `close` returns would drop the runtime before the spawned
    /// MCP close ran. At a process-exit site, call this before `close`.
    pub async fn close_resources(&self) {
        let runner = self
            .lock()
            .current
            .as_ref()
            .map(|current| Arc::clone(&current.runner));
        if let Some(runner) = runner {
            runner.end_session_hooks().await;
            runner.session().close_resources().await;
        }
    }

    /// Runs a SIGTERM migration's session-side work, in the spec's order:
    /// cancel (already done by the caller before this is called), wait for
    /// the cancelled sub-agent tasks and record them interrupted, notify the
    /// parent session (all three inside [`Runner::migrate`]), close MCP,
    /// release the lease. Called from a frontend's exit tail after its own
    /// turn was already cancelled, so it takes admission the same way a turn
    /// does rather than assuming the controller is idle: retries on
    /// [`PROMPT_ACTIVE`] every 20ms (the in-flight operation is the
    /// just-cancelled turn winding down) and gives up with no warnings on
    /// [`CLOSED`] (nothing left to migrate). Requests a close while the
    /// admission is still held, so dropping it runs [`Self::finish_close`]
    /// (closes the runner and the session, which writes the lease's
    /// `released: true`); the caller's own `close_resources`/`close` pair still
    /// runs afterward and finds the controller already closed.
    pub async fn migrate(&self) -> Vec<String> {
        let admission = loop {
            match self.begin_operation() {
                Ok(admission) => break admission,
                Err(ref message) if message == PROMPT_ACTIVE => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(_) => return Vec::new(),
            }
        };
        let Ok(runner) = self.runner() else {
            return Vec::new();
        };
        let warnings = runner.migrate().await;
        runner.end_session_hooks().await;
        runner.session().close_resources().await;
        self.request_close();
        drop(admission);
        warnings
    }

    /// `/permissions [args]` and its `/auto` alias: applies a preset or a
    /// policy/reviewer change, then returns the approval report.
    pub fn permissions_command(&self, args: &str) -> Result<String, String> {
        let scope = self.approval_scope();
        let current = self
            .approval_service()
            .settings(&scope)
            .map_err(|e| e.to_string())?;
        let settings = match parse_permissions_command(args, current)? {
            Some(PermissionsChange::Preset(mode)) => {
                self.approval_service()
                    .set_mode(&scope, mode)
                    .map_err(|e| e.to_string())?;
                self.approval_service()
                    .settings(&scope)
                    .map_err(|e| e.to_string())?
            }
            Some(PermissionsChange::Settings(policy, reviewer)) => self
                .approval_service()
                .set_settings(&scope, current.revision, policy, reviewer)
                .map_err(|e| e.to_string())?,
            None => current,
        };
        self.approval_snapshot().map_err(|e| e.to_string())?;
        let activity = self
            .approval_service()
            .activity_snapshot(&scope.workspace, Some(&scope.session_id))
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "{}\n{} human pending · {} reviewing · {} application failed",
            format_permissions(settings),
            activity.counts.human_pending,
            activity.counts.reviewing,
            activity.counts.application_failed
        ))
    }

    pub fn approval_service(&self) -> &Arc<crate::approval::Service> {
        &self.builder.approvals
    }

    pub fn approval_scope(&self) -> otto_core::approval::Scope {
        let info = self.info();
        otto_core::approval::Scope {
            workspace: info.workspace,
            session_id: info.session_id,
            task_id: String::new(),
            run_id: String::new(),
        }
    }

    pub fn review_memory(
        &self,
        request: &crate::memory::ReviewRequest,
        transport: &str,
    ) -> crate::memory::Result<crate::memory::ReviewResult> {
        use crate::memory::{CandidateState, Error, ErrorKind, ReviewDecision};
        let (memory, user, workspace) = self
            .memory_manager()
            .ok_or_else(|| Error::new(ErrorKind::Unavailable))?;
        if request.reference.scope != user && request.reference.scope != workspace {
            return Err(Error::new(ErrorKind::NotFound));
        }
        let candidate = memory.get_candidate(&request.reference)?;
        if candidate.state != CandidateState::Pending {
            return Err(Error::new(ErrorKind::Conflict));
        }
        let snapshot = self
            .approval_snapshot()
            .map_err(|_| Error::new(ErrorKind::Unavailable))?;
        let id = format!(
            "{}/{}/{}/{}",
            user.id,
            request.reference.scope.namespace,
            request.reference.scope.id,
            request.reference.id
        );
        let approval = snapshot
            .requests
            .into_iter()
            .find(|r| r.subject.source == "memory" && r.subject.id == id)
            .ok_or_else(|| Error::new(ErrorKind::Unavailable))?;
        self.approval_service()
            .decide_and_claim(
                &approval.id,
                &approval.scope,
                approval.revision,
                request.decision == ReviewDecision::Accept,
                &crate::approval::Actor::local(transport),
            )
            .map_err(|_| Error::new(ErrorKind::Conflict))?;
        let result = memory.review(request);
        let state = if result.is_ok() {
            otto_core::approval::ApplicationState::Applied
        } else {
            otto_core::approval::ApplicationState::Failed
        };
        self.approval_service()
            .application(
                &approval.id,
                &approval.scope,
                otto_core::approval::ApplicationState::Applying,
                state,
                if result.is_err() {
                    "Memory review could not be applied."
                } else {
                    ""
                },
            )
            .map_err(|_| Error::new(ErrorKind::Unavailable))?;
        result
    }

    pub fn approval_snapshot(
        &self,
    ) -> Result<otto_core::approval::Snapshot, crate::approval::Error> {
        let scope = self.approval_scope();
        if let Some((memory, user, workspace)) = self.memory_manager() {
            crate::approval::memory::attach(
                self.approval_service(),
                &memory,
                vec![user.clone(), workspace.clone()],
                &scope.workspace,
            )?;
            crate::approval::memory::reconcile(
                self.approval_service(),
                &memory,
                &[user, workspace],
                &scope.workspace,
            )?;
        }
        self.approval_service().apply_available(&scope.workspace)?;
        self.approval_service()
            .snapshot(&scope.workspace, Some(&scope.session_id))
    }

    // ---- turns ----
}
