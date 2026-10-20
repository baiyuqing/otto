//! The `Otto` runtime handle: workspaces, sessions and shutdown.

use super::*;

impl Otto {
    /// Starts configuring an [`Otto`].
    pub fn builder() -> OttoBuilder {
        OttoBuilder {
            memory_extension: None,
            workspace: None,
            config: ConfigSource::Default,
            environment: None,
            overrides: Overrides::default(),
            diagnostics: None,
            disabled: Disabled::default(),
        }
    }

    pub(super) fn ensure_open(&self) -> Result<(), Error> {
        if self.inner.closed.load(Ordering::SeqCst) {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }

    pub(super) fn redact(&self, message: &str) -> String {
        self.inner
            .builder
            .redact_error(message, Some(&self.inner.runtime))
    }

    /// Loads another workspace and returns its canonical path, as `POST
    /// /v1/workspaces` of `otto serve` does. The path must be absolute and
    /// be the startup workspace, lie under a `[server].workspace_roots`
    /// entry or lie under a directory trusted with `otto trust`, after
    /// symlinks are resolved. A workspace loads once; its config is
    /// resolved with the component switches applied.
    pub async fn open_workspace(&self, path: impl AsRef<Path>) -> Result<PathBuf, Error> {
        self.ensure_open()?;
        self.workspace_target(path.as_ref())
            .await
            .map(|(path, _)| path)
    }

    pub(super) async fn workspace_target(
        &self,
        path: &Path,
    ) -> Result<(PathBuf, Option<Loaded>), Error> {
        let inner = &self.inner;
        let requested = path.to_string_lossy();
        let canonical = admission::admit_workspace(
            &requested,
            &inner.workspace,
            &inner.roots,
            &admission::trusted_projects(&inner.config_path),
        )
        .map_err(|admission| match admission {
            Admission::Invalid => Error::Workspace(format!(
                "{requested}: not an absolute path to an existing directory"
            )),
            Admission::NotAdmitted => Error::Workspace(format!(
                "{requested}: outside the startup workspace and configured roots"
            )),
        })?;
        if canonical == inner.workspace {
            return Ok((canonical, None));
        }
        let mut loaded = inner.loaded.lock().await;
        if let Some(found) = loaded.get(&canonical) {
            return Ok((canonical, Some(found.clone())));
        }
        let mut writer = inner.diagnostics.clone();
        let (builder, sandbox) = startup::load_workspace(
            Arc::clone(&inner.builder.shared),
            &canonical,
            &inner.disabled,
            &inner.cancel,
            &mut writer,
        )
        .await
        .map_err(|message| Error::Workspace(self.redact(&message)))?;
        let entry = (Arc::new(builder), sandbox.reloader);
        loaded.insert(canonical.clone(), entry.clone());
        Ok((canonical, Some(entry)))
    }

    pub(super) async fn target(&self, workspace: Option<&Path>) -> Result<Target, Error> {
        self.ensure_open()?;
        let inner = &self.inner;
        let loaded = match workspace {
            None => None,
            Some(path) => self.workspace_target(path).await?.1,
        };
        match loaded {
            None => Ok(Target {
                builder: Arc::clone(&inner.builder),
                runtime: inner.runtime.clone(),
                reloader: inner.sandbox.reloader.clone(),
            }),
            Some((builder, reloader)) => {
                let runtime = crate::runtime::builder::resolve_initial_runtime(
                    &builder.config,
                    &builder.environment,
                    None,
                    &builder.overrides,
                )
                .map_err(|error| Error::Config(self.redact(&error.to_string())))?;
                Ok(Target {
                    builder,
                    runtime,
                    reloader,
                })
            }
        }
    }

    pub(super) fn register(
        &self,
        controller: Controller,
        reloader: Option<Arc<SandboxReloader>>,
    ) -> Session {
        let controller = match reloader {
            Some(control) => controller.with_sandbox_control(control as Arc<dyn SandboxControl>),
            None => controller,
        };
        let controller = Arc::new(controller);
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        sessions.retain(|session| session.strong_count() > 0);
        sessions.push(Arc::downgrade(&controller));
        Session {
            id: controller.info().session_id,
            controller,
        }
    }

    /// Creates a session in `workspace`, or in the startup workspace for
    /// `None`, as ACP `session/new` does. A named workspace is opened first
    /// as [`Otto::open_workspace`] does.
    pub async fn new_session(&self, workspace: Option<&Path>) -> Result<Session, Error> {
        let target = self.target(workspace).await?;
        let controller = Controller::create(Arc::clone(&target.builder), &target.runtime)
            .await
            .map_err(|message| Error::Agent(AgentError::Other(self.redact(&message))))?;
        Ok(self.register(controller, target.reloader))
    }

    /// Reopens the session file `id` of `workspace` (the startup workspace
    /// for `None`), as ACP `session/load` does. Repair warnings go to the
    /// diagnostics writer as `warning: <text>`.
    pub async fn resume_session(
        &self,
        id: &str,
        workspace: Option<&Path>,
    ) -> Result<Session, Error> {
        let target = self.target(workspace).await?;
        if target.builder.no_session {
            return Err(Error::PersistenceDisabled);
        }
        if !sessionfs::is_session_id(id) {
            return Err(Error::SessionNotFound(id.to_string()));
        }
        let path = sessionfs::session_directory(
            &target.builder.session_root,
            &target.builder.workspace_path,
        )
        .map_err(|error| Error::Workspace(self.redact(&error.to_string())))?
        .join(format!("{id}.jsonl"));
        if !path.is_file() {
            return Err(Error::SessionNotFound(id.to_string()));
        }
        let (controller, warnings) = Controller::open(Arc::clone(&target.builder), &path)
            .await
            .map_err(|message| Error::Agent(AgentError::Other(self.redact(&message))))?;
        let mut writer = self.inner.diagnostics.clone();
        for warning in warnings {
            let _ = writeln!(writer, "warning: {}", self.redact(&warning));
        }
        Ok(self.register(controller, target.reloader))
    }

    /// The sessions of `workspace` (the startup workspace for `None`), newest
    /// first, at most `limit` of them (and never more than the CLI lists).
    pub async fn list_sessions(
        &self,
        workspace: Option<&Path>,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, Error> {
        let target = self.target(workspace).await?;
        if target.builder.no_session {
            return Err(Error::PersistenceDisabled);
        }
        let builder = Arc::clone(&target.builder);
        let listed = tokio::task::spawn_blocking(move || {
            if !builder.session_root.exists() {
                return Ok(Vec::new());
            }
            sessionfs::list(
                &builder.session_root,
                &builder.workspace_path,
                "",
                MAX_LIST_SESSIONS,
            )
            .map(|result| result.sessions)
            .map_err(|error| builder.redact_error(&error.to_string(), None))
        })
        .await
        .map_err(|join| Error::Workspace(join.to_string()))?
        .map_err(Error::Workspace)?;
        Ok(listed
            .into_iter()
            .take(limit)
            .map(|info| SessionSummary {
                id: info.id,
                cwd: info.cwd,
                name: info.name,
                last_user_text: info.last_user_text,
                message_count: info.message_count,
                created: info.created.into(),
                updated: info.modified.into(),
                profile: info.profile,
                provider: info.provider,
                model: info.model,
            })
            .collect())
    }

    /// Closes the runtime in the CLI's order: every open session (MCP, then
    /// the session file), the sandbox of each additional workspace, the
    /// startup sandbox, the approval executor and the memory service; then it
    /// releases the process guard. The first failure is returned after every
    /// step ran. A second call returns `Ok(())`. After the first call every
    /// other method returns [`Error::Closed`].
    pub async fn close(&self) -> Result<(), Error> {
        let inner = &self.inner;
        let _serial = inner.close_lock.lock().await;
        if inner.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        inner.cancel.cancel();
        let mut failure: Option<String> = None;
        let controllers: Vec<Arc<Controller>> =
            std::mem::take(&mut *inner.sessions.lock().unwrap_or_else(|p| p.into_inner()))
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
        for controller in controllers {
            if let Err(message) = close_controller(controller).await {
                failure.get_or_insert(message);
            }
        }
        let additional: Vec<Loaded> = inner.loaded.lock().await.values().cloned().collect();
        for (_, reloader) in additional {
            if let Some(reloader) = reloader
                && reloader.control.close().await.is_err()
            {
                failure.get_or_insert("sandbox runtime close failed".to_string());
            }
        }
        if inner.sandbox.control.close().await.is_err() {
            failure.get_or_insert("sandbox runtime close failed".to_string());
        }
        if let Some(executor) = &inner.sandbox.approval_executor
            && executor.close().is_err()
        {
            failure.get_or_insert("sandbox runtime close failed".to_string());
        }
        if let Some(turns) = &inner.builder.memory.turns {
            turns.close().await;
        }
        if let Some(client) = &inner.builder.memory.client
            && let Err(error) = client.close(&CancellationToken::new()).await
        {
            failure.get_or_insert_with(|| format!("close memory: {error}"));
        }
        let _ = inner.builder.memory.service.close();
        drop(inner.guard.lock().unwrap_or_else(|p| p.into_inner()).take());
        match failure {
            None => Ok(()),
            Some(message) => Err(Error::Close(self.redact(&message))),
        }
    }
}

/// `close_mcp`, then the blocking `close` on the blocking pool.
pub(super) async fn close_controller(controller: Arc<Controller>) -> Result<(), String> {
    controller.close_mcp().await;
    match tokio::task::spawn_blocking(move || controller.close()).await {
        Ok(result) => result,
        Err(join) => Err(join.to_string()),
    }
}
