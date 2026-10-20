//! Replacement runners, controller construction and the free functions that build them.

use super::*;

impl Controller {
    pub(crate) fn current_runtime(&self) -> Result<Runtime, String> {
        let info = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.info.clone())
            .ok_or_else(|| CLOSED.to_string())?;
        self.current_runtime_from_info(info)
    }

    pub(super) fn current_runtime_from_info(&self, info: RuntimeInfo) -> Result<Runtime, String> {
        let mut runtime = self.replacement_runtime(&metadata_of(&info))?;
        runtime.thinking = info.thinking;
        Ok(runtime)
    }

    /// The two gates before every replacement: persistence must be enabled, and
    /// the boundary must be open both before and after resolving.
    pub(super) fn replacement_runtime(
        &self,
        metadata: &RuntimeMetadata,
    ) -> Result<Runtime, String> {
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        resolve_replacement(&self.builder, metadata)
    }

    /// A new session and runner for an already-resolved runtime. Every failure
    /// closes what it built.
    pub(super) async fn fresh_replacement(&self, runtime: &Runtime) -> Result<Current, String> {
        let session = self.builder.create_session(runtime)?;
        attach_runner(&self.builder, session, runtime).await
    }

    /// Resume conversation state without replacing the process's active selection.
    pub(super) async fn resumed_replacement(
        &self,
        path: &Path,
    ) -> Result<(Current, Vec<String>), String> {
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let runtime = self.current_runtime()?;
        open_current(&self.builder, path, &runtime).await
    }

    /// Installs the replacement and closes what it displaced. A close that
    /// arrived while the replacement was building closes the replacement too.
    pub(super) fn commit(
        &self,
        replacement: Current,
        admission: Admission<'_>,
    ) -> Result<String, String> {
        let path = replacement.path();
        let (displaced, deferred) = {
            let mut state = self.lock();
            if state.closed {
                (Some(replacement), true)
            } else {
                (state.current.replace(replacement), false)
            }
        };
        let close_error = displaced.and_then(|current| current.discard().err());
        drop(admission);
        if deferred {
            return Err(CLOSED.to_string());
        }
        self.bind_reflection();
        self.notify_memory_state();
        match close_error {
            Some(error) => Err(self.builder.redact_error(&error, None)),
            None => Ok(path),
        }
    }

    // ---- the `otto serve` factories ----

    /// A controller over a brand new session. There is nothing to replace, so
    /// the built session and runner become the controller's first [`Current`]
    /// directly.
    ///
    /// The caller has already confirmed the redaction boundary is open, so
    /// `dynamic_content` is true for every controller built this way.
    pub async fn create(builder: Arc<Builder>, runtime: &Runtime) -> Result<Self, String> {
        let session = builder.create_session(runtime)?;
        let current = attach_runner(&builder, session, runtime).await?;
        Ok(Self::from_current(builder, current))
    }

    pub async fn create_with_receipt(
        builder: Arc<Builder>,
        runtime: &Runtime,
        id: String,
        request_id: &str,
    ) -> Result<Self, String> {
        let session = builder.create_session_with_id(runtime, id)?;
        otto_core::session::Session::append_custom(
            &session,
            "otto.session_command",
            &serde_json::json!({"schemaVersion":1,"command":"create","requestId":request_id})
                .to_string(),
        )
        .await
        .map_err(|error| error.to_string())?;
        let current = attach_runner(&builder, session, runtime).await?;
        Ok(Self::from_current(builder, current))
    }

    pub fn session_custom_data(&self, kind: &str) -> Option<String> {
        self.lock()
            .current
            .as_ref()?
            .session
            .handle()
            .custom_data(kind)
    }

    pub async fn write_session_command_receipt(&self, request_id: &str) -> Result<(), String> {
        let session = self.lock().current.as_ref().ok_or(CLOSED)?.session.clone();
        otto_core::session::Session::append_custom(
            &session,
            "otto.session_command",
            &serde_json::json!({"schemaVersion":1,"command":"create","requestId":request_id})
                .to_string(),
        )
        .await
        .map_err(|error| error.to_string())
    }

    /// A controller over the session file at `path`, with the repair warnings
    /// activating it produced.
    pub async fn open(builder: Arc<Builder>, path: &Path) -> Result<(Self, Vec<String>), String> {
        let runtime = crate::runtime::startup::initial_runtime(&builder, None)?;
        let (current, warnings) = open_current(&builder, path, &runtime).await?;
        Ok((Self::from_current(builder, current), warnings))
    }

    pub(super) fn from_current(builder: Arc<Builder>, current: Current) -> Self {
        let controller = Self {
            builder,
            dynamic_content: true,
            sandbox: None,
            state: Mutex::new(State {
                current: Some(current),
                ..State::default()
            }),
            close_signal: Condvar::new(),
            notices: Arc::new(auto_reflection::Notices::new()),
            wake_replies: auto_reflection::Notices::new(),
            auto_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            auto_cancel: CancellationToken::new(),
            approval_control: tokio::sync::Mutex::new(()),
            bell: std::sync::atomic::AtomicBool::new(false),
            herdr: std::sync::RwLock::new(None),
            trace_cache: trace::TraceCache::default(),
        };
        controller.notify_memory_state();
        controller.bind_reflection();
        controller
    }
}

/// The boundary half of the replacement gates, without a controller to read
/// `dynamic_content` from: the persistence gate stays with the caller.
fn resolve_replacement(builder: &Builder, metadata: &RuntimeMetadata) -> Result<Runtime, String> {
    if !builder.boundary_allows_dynamic(None) {
        return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
    }
    let runtime = builder.resolve_session(metadata)?;
    if !builder.boundary_allows_dynamic(Some(&runtime)) {
        return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
    }
    Ok(runtime)
}

/// Builds the runner for `session` and pairs them into a [`Current`]. Every
/// failure closes what it built.
async fn attach_runner(
    builder: &Arc<Builder>,
    session: SharedSession,
    runtime: &Runtime,
) -> Result<Current, String> {
    let runner = match builder.build_runner_quiet(&session, runtime).await {
        Ok(runner) => Arc::new(runner),
        Err(error) => {
            let _ = session.close();
            return Err(error);
        }
    };
    if let Err(error) = builder.update_session_runtime(&session, runtime) {
        runner.close_mcp().await;
        runner.close();
        let _ = session.close();
        return Err(error);
    }
    Ok(Current::build(
        session,
        runner,
        builder.runtime_info(runtime),
    ))
}

/// Pins and activates conversation history using the caller's active runtime.
async fn open_current(
    builder: &Arc<Builder>,
    path: &Path,
    runtime: &Runtime,
) -> Result<(Current, Vec<String>), String> {
    let create_lease = builder
        .create_lease()
        .map_err(|error| builder.redact_error(&error, None))?;
    // A lease-managed open can block up to 7*lease_seconds/6 (35s at the
    // default 30s lease) waiting out another host's holder; run it on a
    // blocking thread so it does not stall this async task's runtime worker.
    let session_root = builder
        .storage
        .session_root(&builder.session_root)
        .map_err(|error| builder.redact_error(&error, None))?;
    let storage = Arc::clone(&builder.storage);
    let workspace = builder.workspace_path.clone();
    let blocking_path = path.to_path_buf();
    let prepared = tokio::task::spawn_blocking(move || {
        storage.sessions()?.prepare_listed(
            &session_root,
            &workspace,
            &blocking_path.to_string_lossy(),
            create_lease,
        )
    })
    .await
    .map_err(|error| format!("session open task panicked: {error}"))?
    .map_err(|error| builder.redact_error(&error.to_string(), None))?;
    if !builder.boundary_allows_dynamic(Some(runtime)) {
        let _ = prepared.close();
        return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
    }
    let prepared = prepared.with_replayable(crate::tool::safety::is_replayable);
    let (session, warnings) = tokio::task::spawn_blocking(move || prepared.activate())
        .await
        .map_err(|error| format!("session activation task panicked: {error}"))?
        .map_err(|error| builder.redact_error(&error, Some(runtime)))?;
    let warnings = warnings
        .into_iter()
        .map(|warning| builder.redact_error(&warning.message, Some(runtime)))
        .collect();
    let session = SharedSession::new(session);
    let current = attach_runner(builder, session, runtime).await?;
    Ok((current, warnings))
}
