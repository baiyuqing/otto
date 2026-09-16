//! Shared session lifecycle and the capability contracts every frontend uses.
//!
//! Port of `internal/app`. The REPL, `otto serve`, and a later TUI all drive
//! one [`Controller`]: it owns the current session, the runner built for it,
//! and the admission rule that lets exactly one operation touch them at a
//! time.
//!
//! Concurrency: one `Mutex<State>` guards everything. Admission is an RAII
//! guard ([`Admission`]), so a dropped future releases the claim. No lock is
//! held across an `await`.
//!
//! Close: [`Controller::request_close`] never blocks and never closes, which
//! is what a callback running inside an operation needs.
//! [`Controller::close`] completes the close, waiting on a condition variable
//! when an operation is still in flight. It blocks a thread, so an async
//! caller cancels the in-flight work and awaits it before calling `close`.
//!
//! Errors: every failure is already-redacted text, matching
//! [`Builder::redact_error`]. Callers that must distinguish a cause compare
//! against the constants below, the way `cli::run` already compares
//! [`SESSION_OPERATION_UNAVAILABLE`].

pub mod sandbox;
pub mod tasks;
pub mod wake;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use otto_core::agent::inbox::Notification;
use otto_core::agent::{AgentError, CompactionResult, EventSink};
use otto_core::config::resolve::Runtime;
use otto_core::model::{Message, Usage};
use otto_core::session::{ListResult, RuntimeMetadata, Session};
use tokio_util::sync::CancellationToken;

use crate::cli::info::SandboxInfo;
use crate::cli::runtime_builder::{Builder, Runner, RuntimeInfo, SharedSession};
use crate::session::{self as sessionfs, ArchiveResult, MAX_LIST_SESSIONS};

pub use sandbox::SandboxControl;
pub use tasks::{Task, TaskStatus, TaskView};
pub use wake::WakeOperation;

/// Go's `app.ErrPersistenceDisabled`.
pub const PERSISTENCE_DISABLED: &str = "session persistence is disabled";
/// Go's `app.ErrProfileSwitchUnavailable`.
pub const PROFILE_SWITCH_UNAVAILABLE: &str = "profile switching is not available";
/// Go's `app.ErrSandboxReloadUnavailable`.
pub const SANDBOX_RELOAD_UNAVAILABLE: &str = "sandbox reload is not available";
/// Go's `errSessionOperationUnavailable` in `cmd/otto/runtime_builder.go`.
pub const SESSION_OPERATION_UNAVAILABLE: &str = "session operation is unavailable";
/// Go's `app.ErrPromptActive`. The server answers it with 409 `turn_active`.
pub const PROMPT_ACTIVE: &str = "a prompt is already active";
/// Go's `app.ErrClosed`.
pub const CLOSED: &str = "controller is closed";
/// Go's `app.ErrSessionRenameUnavailable`.
pub const SESSION_RENAME_UNAVAILABLE: &str = "session rename is unavailable";
/// Go's `session.ErrInvalidSession` text for a blank name.
pub const INVALID_SESSION_NAME: &str = "session is invalid: session name is required";

/// What a frontend may display. Port of `app.Info`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    pub session_id: String,
    pub session_name: String,
    pub session_path: String,
    pub workspace: String,
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub usage: Usage,
    pub usage_present: bool,
    pub context_window: i64,
    pub context_input_tokens: i64,
    pub context_input_tokens_present: bool,
    pub context_input_tokens_pending: bool,
    pub sandbox: SandboxInfo,
}

/// What a completed session replacement reports. Port of `app.ResumeResult`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResumeResult {
    pub session_path: String,
    pub warnings: Vec<String>,
}

/// The session, runner and resolved runtime currently in force.
struct Current {
    session: SharedSession,
    runner: Arc<Runner>,
    info: RuntimeInfo,
    /// The canonical workspace recorded in the session header.
    workspace: String,
}

impl Current {
    fn build(session: SharedSession, runner: Arc<Runner>, info: RuntimeInfo) -> Self {
        let workspace = canonical_session_path(&session.header().workspace);
        Self {
            session,
            runner,
            info,
            workspace,
        }
    }

    /// The session file path as the store reports it. A lazily created store
    /// has none until its first append, so this is read live rather than
    /// captured; `Info.session_path` and every path comparison use it.
    fn path(&self) -> String {
        self.session.path()
    }

    /// The same path, symlink-resolved, for comparing two references to one
    /// file. Port of `canonicalSessionPath` applied to `currentPath`.
    fn canonical_path(&self) -> String {
        canonical_session_path(&self.session.path())
    }

    fn discard(self) -> Result<(), String> {
        self.runner.close();
        self.session.close()
    }
}

#[derive(Default)]
struct State {
    current: Option<Current>,
    /// An operation holds admission. Covers Go's `active` and `replace` both,
    /// because every caller treats them identically.
    busy: bool,
    /// Bumped on every admission so a stale [`Admission`] guard is a no-op
    /// after [`Controller::request_close`] releases an unstarted wake.
    generation: u64,
    /// A wake claim that has not run yet. `request_close` releases it
    /// instead of waiting, matching Go's `RequestClose`.
    wake_unstarted: bool,
    closed: bool,
    close_pending: bool,
    close_in_progress: bool,
    close_done: bool,
    close_err: Option<String>,
}

pub struct Controller {
    builder: Arc<Builder>,
    /// False when the redaction boundary is closed. Every operation that
    /// would persist or display provider identity is then refused, matching
    /// Go's `dynamicContent` gate.
    dynamic_content: bool,
    sandbox: Option<Arc<dyn SandboxControl>>,
    state: Mutex<State>,
    close_signal: Condvar,
}

/// One admitted operation. Dropping it releases admission and performs a
/// close that arrived while the operation was running.
pub struct Admission<'a> {
    controller: &'a Controller,
    generation: u64,
}

impl std::fmt::Debug for Admission<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Admission")
            .field("generation", &self.generation)
            .finish()
    }
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.controller.release(self.generation);
    }
}

impl Controller {
    /// The composition-root constructor `cli::run` uses.
    pub fn new(
        builder: Builder,
        dynamic_content: bool,
        session: SharedSession,
        runner: Runner,
        info: RuntimeInfo,
    ) -> Self {
        Self::with_builder(Arc::new(builder), dynamic_content, session, runner, info)
    }

    /// The per-session constructor `server` uses: one [`Builder`] backs every
    /// open session, so it is shared rather than moved.
    pub fn with_builder(
        builder: Arc<Builder>,
        dynamic_content: bool,
        session: SharedSession,
        runner: Runner,
        info: RuntimeInfo,
    ) -> Self {
        Self {
            builder,
            dynamic_content,
            sandbox: None,
            state: Mutex::new(State {
                current: Some(Current::build(session, Arc::new(runner), info)),
                ..State::default()
            }),
            close_signal: Condvar::new(),
        }
    }

    /// Wires the process sandbox. Port of `app.WithSandboxControl`: the
    /// control reports the state now in effect, so every controller built
    /// from one composition root agrees after a reload.
    pub fn with_sandbox_control(mut self, control: Arc<dyn SandboxControl>) -> Self {
        self.sandbox = Some(control);
        self
    }

    pub fn builder(&self) -> &Arc<Builder> {
        &self.builder
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    // ---- admission ----

    /// Port of `beginOperation`.
    pub fn begin_operation(&self) -> Result<Admission<'_>, String> {
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

    /// Port of `endOperation`.
    fn release(&self, generation: u64) {
        let victim = {
            let mut state = self.lock();
            if !state.busy || state.generation != generation {
                return;
            }
            state.busy = false;
            if !state.close_pending {
                return;
            }
            state.close_pending = false;
            state.close_in_progress = true;
            state.current.take()
        };
        self.finish_close(victim);
    }

    fn finish_close(&self, victim: Option<Current>) {
        let error = victim.and_then(|current| current.discard().err());
        let mut state = self.lock();
        if state.close_err.is_none() {
            state.close_err = error;
        }
        state.close_in_progress = false;
        state.close_done = true;
        drop(state);
        self.close_signal.notify_all();
    }

    /// Rejects new work and hands the close to whichever operation is in
    /// flight. Never waits and never closes, so a callback running inside an
    /// operation may call it. Port of `Controller.RequestClose`.
    pub fn request_close(&self) {
        let mut state = self.lock();
        state.closed = true;
        if state.busy {
            if state.wake_claim_is_unstarted() {
                // Release the claim the way Go does, so the close need not
                // wait for a wake turn that will now never run.
                state.busy = false;
                state.generation += 1;
                state.wake_unstarted = false;
            } else {
                state.close_pending = true;
            }
        }
    }

    /// Completes the close, waiting for an in-flight operation to finish.
    /// Port of `Controller.Close`. Blocking: an async caller must cancel and
    /// await its own work first.
    pub fn close(&self) -> Result<(), String> {
        self.request_close();
        let victim = {
            let mut state = self.lock();
            if state.close_done {
                return state.close_err.clone().map_or(Ok(()), Err);
            }
            if state.busy || state.close_in_progress {
                let state = self
                    .close_signal
                    .wait_while(state, |state| !state.close_done)
                    .unwrap_or_else(|poison| poison.into_inner());
                return state.close_err.clone().map_or(Ok(()), Err);
            }
            state.close_in_progress = true;
            state.current.take()
        };
        self.finish_close(victim);
        self.lock().close_err.clone().map_or(Ok(()), Err)
    }

    // ---- reads ----

    /// The captured ChatGPT credential path, empty when none was captured.
    pub fn auth_path(&self) -> &str {
        &self.builder.auth_path
    }

    /// The process-wide memory service and its scopes. `/memory` and
    /// `/remember` read them through `Controller::memory_manager`.
    pub(crate) fn memory_wiring(&self) -> &crate::cli::wiring::MemoryWiring {
        &self.builder.memory
    }

    /// The runner currently in force. `/tasks` and `/task` read its sub-agent
    /// task registry, Go's `taskOwner`.
    pub(crate) fn current_runner(&self) -> Option<Arc<Runner>> {
        self.runner().ok()
    }

    pub fn config_path(&self) -> &PathBuf {
        &self.builder.config_path
    }

    /// The sandbox state now in effect: the live process sandbox when one is
    /// wired, otherwise what the builder resolved at startup. Port of
    /// `currentSandboxInfoLocked`.
    pub fn sandbox_info(&self) -> SandboxInfo {
        match &self.sandbox {
            Some(control) => control.info(),
            None => self.builder.effective_sandbox_info(),
        }
    }

    pub fn dynamic_content(&self) -> bool {
        self.dynamic_content
    }

    /// Port of `Controller.DynamicContentAvailable`.
    pub fn dynamic_content_available(&self) -> bool {
        !self.lock().closed && self.dynamic_content
    }

    pub fn workspace(&self) -> &str {
        &self.builder.workspace_path
    }

    /// Port of `Controller.Info`. A closed boundary reports the sandbox only.
    pub fn info(&self) -> Info {
        let sandbox = self.sandbox_info();
        if !self.dynamic_content {
            return Info {
                sandbox,
                ..Info::default()
            };
        }
        let state = self.lock();
        let Some(current) = state.current.as_ref() else {
            return Info {
                sandbox,
                ..Info::default()
            };
        };
        let header = current.session.header();
        let snapshot = current.session.snapshot();
        Info {
            session_id: header.id,
            session_name: current.session.name(),
            session_path: current.path(),
            workspace: header.workspace,
            provider: current.info.provider.clone(),
            profile: current.info.profile.clone(),
            model: current.info.model.clone(),
            usage: snapshot.aggregate_usage,
            usage_present: snapshot.aggregate_usage_present,
            context_window: current.info.context_window,
            context_input_tokens: snapshot.context_input_tokens,
            context_input_tokens_present: snapshot.context_input_tokens_present,
            context_input_tokens_pending: snapshot.context_input_tokens_pending,
            sandbox,
        }
    }

    /// Port of `Controller.History`.
    pub fn history(&self) -> Vec<Message> {
        if !self.dynamic_content {
            return Vec::new();
        }
        let state = self.lock();
        match state.current.as_ref() {
            Some(current) => current.session.messages(),
            None => Vec::new(),
        }
    }

    /// The session currently in force, or `None` after a close.
    pub fn current_session_opt(&self) -> Option<SharedSession> {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.session.clone())
    }

    /// The session currently in force. Panics only if called after a close,
    /// which no frontend does; `current_session_opt` is the checked form.
    pub fn current_session(&self) -> SharedSession {
        self.current_session_opt()
            .expect("controller has no current session")
    }

    /// The system prompt the current runner was built with.
    pub fn system_prompt(&self) -> String {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.runner.system_prompt().to_string())
            .unwrap_or_default()
    }

    /// The skill catalog fixed for the current runner.
    pub fn skills(&self) -> crate::skill::Catalog {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.runner.skills().clone())
            .unwrap_or_default()
    }

    fn runner(&self) -> Result<Arc<Runner>, String> {
        self.lock()
            .current
            .as_ref()
            .map(|current| Arc::clone(&current.runner))
            .ok_or_else(|| CLOSED.to_string())
    }

    // ---- turns ----

    /// Port of `Controller.Prompt`.
    pub async fn prompt(
        &self,
        text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _admission = self.begin_operation().map_err(AgentError::Other)?;
        let runner = self.runner().map_err(AgentError::Other)?;
        runner.run(text, emit, cancel).await
    }

    /// Port of `Controller.Compact`.
    pub async fn compact(
        &self,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, AgentError> {
        let _admission = self.begin_operation().map_err(AgentError::Other)?;
        let runner = self.runner().map_err(AgentError::Other)?;
        runner.compact(focus, emit, cancel).await
    }

    // ---- profiles ----

    /// The configured profile names, sorted. Port of `Controller.Profiles`.
    pub fn profiles(&self) -> Vec<String> {
        if !self.dynamic_content_available() {
            return Vec::new();
        }
        let mut names: Vec<String> = self.builder.config.profiles.keys().cloned().collect();
        names.sort();
        names
    }

    /// Port of `Controller.SetDefaultProfile`.
    pub fn set_default_profile(&self, profile: &str) -> Result<(), String> {
        if self.lock().closed {
            return Err(CLOSED.to_string());
        }
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        crate::config::set_default_profile_file(&self.builder.config_path, profile)
            .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    // ---- session replacement ----

    /// Port of `Controller.RenameSession`.
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

    /// Admission for an operation that replaces the session. Ordering matches
    /// Go: closed first, then the capability gate, then the active check.
    fn begin_replacement(&self) -> Result<Admission<'_>, String> {
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

    /// Port of `Controller.NewSession`.
    pub async fn new_session(&self) -> Result<(), String> {
        let admission = self.begin_replacement()?;
        let metadata = self.current_metadata()?;
        let runtime = self.replacement_runtime(&metadata)?;
        let replacement = self.fresh_replacement(&runtime).await?;
        self.commit(replacement, admission)?;
        Ok(())
    }

    /// Port of `Controller.SwitchProfile`, discarding the resume result the
    /// line frontend does not use.
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
        let replacement = self.fresh_replacement(&runtime).await?;
        let path = self.commit(replacement, admission)?;
        Ok(ResumeResult {
            session_path: path,
            warnings: Vec::new(),
        })
    }

    /// Port of `Controller.ListSessions`. Rows carry `current` for the file
    /// already open.
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
        sessionfs::list(
            &self.builder.session_root,
            &self.builder.workspace_path,
            &current_path,
            limit,
        )
        .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    /// Port of `Controller.ResumeSession`. Resuming the session already in
    /// force is a no-op that reports its path.
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

    /// Port of `Controller.ArchiveSession`: archiving the current session
    /// delegates so a picker row behaves like `/archive`.
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
        sessionfs::archive(
            &self.builder.session_root,
            &self.builder.workspace_path,
            Path::new(path),
        )
        .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    /// Port of `Controller.ArchiveCurrentSession`.
    ///
    /// The replacement is built before the archive move, so every failure
    /// path leaves the current session intact. The move is the last and only
    /// committed state change.
    pub async fn archive_current_session(&self) -> Result<ArchiveResult, String> {
        let admission = self.begin_replacement()?;
        let (path, metadata) = {
            let state = self.lock();
            let current = state.current.as_ref().ok_or_else(|| CLOSED.to_string())?;
            (current.session.path(), metadata_of(&current.info))
        };
        if path.is_empty() {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let runtime = self.replacement_runtime(&metadata)?;
        let replacement = self.fresh_replacement(&runtime).await?;
        match sessionfs::archive(
            &self.builder.session_root,
            &self.builder.workspace_path,
            Path::new(&path),
        ) {
            Ok(result) => {
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

    /// The current runner's task registry, or `None` when it tracks none or
    /// the controller is closed. Port of `Controller.Tasks`.
    pub fn tasks(&self) -> Option<Arc<dyn TaskView>> {
        let state = self.lock();
        if state.closed {
            return None;
        }
        tasks::task_view(state.current.as_ref()?.runner.as_ref())
    }

    /// Claims a turn only when the runner has pending task notifications.
    /// Port of `Controller.PrepareWake`.
    pub fn prepare_wake(&self) -> Result<Option<WakeOperation<'_>>, String> {
        let pending = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            if state.busy {
                return Err(PROMPT_ACTIVE.to_string());
            }
            let Some(current) = state.current.as_ref() else {
                return Err(CLOSED.to_string());
            };
            tasks::task_view(current.runner.as_ref())
                .map(|view| view.pending())
                .unwrap_or(0)
        };
        if pending == 0 {
            return Ok(None);
        }
        let admission = self.begin_operation()?;
        self.lock().wake_unstarted = true;
        Ok(Some(WakeOperation::new(self, admission)))
    }

    /// Pushes `notification` into the current runner's inbox. Returns true
    /// when a registry received it. A closed controller or a runner without
    /// tasks drops it.
    pub fn notify(&self, notification: Notification) -> bool {
        let tasks = {
            let state = self.lock();
            if state.closed {
                return false;
            }
            let Some(tasks) = state
                .current
                .as_ref()
                .and_then(|current| current.runner.tasks.clone())
            else {
                return false;
            };
            tasks
        };
        tasks.notifications().push(notification);
        true
    }

    /// Port of `Controller.ReloadSandbox`.
    pub async fn reload_sandbox(&self) -> Result<SandboxInfo, String> {
        let control = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            let Some(control) = self.sandbox.clone() else {
                return Err(SANDBOX_RELOAD_UNAVAILABLE.to_string());
            };
            if state.busy {
                return Err(PROMPT_ACTIVE.to_string());
            }
            control
        };
        control.reload().await
    }

    /// Grants one pending elevated Bash command for the current session.
    pub fn approve_bash(&self, id: &str) -> Result<String, String> {
        let session_id = {
            let state = self.lock();
            if state.closed {
                return Err(CLOSED.to_string());
            }
            if state.busy {
                return Err(PROMPT_ACTIVE.to_string());
            }
            state
                .current
                .as_ref()
                .ok_or_else(|| CLOSED.to_string())?
                .session
                .header()
                .id
        };
        let approvals = self
            .builder
            .bash_approvals
            .as_ref()
            .ok_or_else(|| "temporary elevation is unavailable".to_string())?;
        approvals.approve(&session_id, id).map_err(str::to_string)?;
        Ok(format!(
            "The user approved {id} for one exact command. Retry the same elevated Bash command now."
        ))
    }

    // ---- replacement helpers ----

    fn current_metadata(&self) -> Result<RuntimeMetadata, String> {
        self.lock()
            .current
            .as_ref()
            .map(|current| metadata_of(&current.info))
            .ok_or_else(|| CLOSED.to_string())
    }

    /// The two gates Go applies before every replacement: persistence must be
    /// enabled, and the boundary must be open both before and after resolving.
    fn replacement_runtime(&self, metadata: &RuntimeMetadata) -> Result<Runtime, String> {
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        resolve_replacement(&self.builder, metadata)
    }

    /// Port of `freshReplacement`: a new session and runner for an
    /// already-resolved runtime. Every failure closes what it built.
    async fn fresh_replacement(&self, runtime: &Runtime) -> Result<Current, String> {
        let session = self.builder.create_session(runtime)?;
        attach_runner(&self.builder, session, runtime).await
    }

    /// The resume path: pin the named file, activate it, and build a runner
    /// for the runtime its header records.
    async fn resumed_replacement(&self, path: &Path) -> Result<(Current, Vec<String>), String> {
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        open_current(&self.builder, path).await
    }

    /// Installs the replacement and closes what it displaced. A close that
    /// arrived while the replacement was building closes the replacement too,
    /// which is Go's deferred-close branch in `runReplacement`.
    fn commit(&self, replacement: Current, admission: Admission<'_>) -> Result<String, String> {
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
        match close_error {
            Some(error) => Err(self.builder.redact_error(&error, None)),
            None => Ok(path),
        }
    }

    // ---- the `otto serve` factories ----

    /// A controller over a brand new session. Port of `serveFactories.create`
    /// together with `controllerFromReplacement`: there is nothing to
    /// replace, so the built session and runner become the controller's
    /// first [`Current`] directly.
    ///
    /// The caller has already confirmed the redaction boundary is open (Go
    /// does it in `runWithDependencies` before dispatching to `runServe`), so
    /// `dynamic_content` is true for every controller built this way.
    pub async fn create(builder: Arc<Builder>, runtime: &Runtime) -> Result<Self, String> {
        let session = builder.create_session(runtime)?;
        let current = attach_runner(&builder, session, runtime).await?;
        Ok(Self::from_current(builder, current))
    }

    /// A controller over the session file at `path`, with the repair warnings
    /// activating it produced. Port of `serveFactories.open`.
    pub async fn open(builder: Arc<Builder>, path: &Path) -> Result<(Self, Vec<String>), String> {
        let (current, warnings) = open_current(&builder, path).await?;
        Ok((Self::from_current(builder, current), warnings))
    }

    fn from_current(builder: Arc<Builder>, current: Current) -> Self {
        Self {
            builder,
            dynamic_content: true,
            sandbox: None,
            state: Mutex::new(State {
                current: Some(current),
                ..State::default()
            }),
            close_signal: Condvar::new(),
        }
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
    let runner = match builder.build_runner(&session, runtime).await {
        Ok(runner) => Arc::new(runner),
        Err(error) => {
            let _ = session.close();
            return Err(error);
        }
    };
    if let Err(error) = builder.update_session_runtime(&session, runtime) {
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

/// Pins the session file at `path`, activates it, and builds a runner for the
/// runtime its header records. Port of `openReplacement`.
async fn open_current(
    builder: &Arc<Builder>,
    path: &Path,
) -> Result<(Current, Vec<String>), String> {
    let prepared =
        sessionfs::Prepared::prepare_listed(&builder.session_root, &builder.workspace_path, path)
            .map_err(|error| builder.redact_error(&error.to_string(), None))?;
    let info = prepared.info();
    let metadata = RuntimeMetadata {
        profile: info.profile.clone(),
        provider: info.provider.clone(),
        model: info.model.clone(),
    };
    let runtime = match resolve_replacement(builder, &metadata) {
        Ok(runtime) => runtime,
        Err(message) => {
            let _ = prepared.close();
            return Err(message);
        }
    };
    let (store, warnings) = prepared
        .activate()
        .map_err(|error| builder.redact_error(&error.to_string(), Some(&runtime)))?;
    let warnings = warnings
        .into_iter()
        .map(|warning| builder.redact_error(&warning.message, Some(&runtime)))
        .collect();
    let session = SharedSession::new(Arc::new(store));
    let current = attach_runner(builder, session, &runtime).await?;
    Ok((current, warnings))
}

impl State {
    fn wake_claim_is_unstarted(&self) -> bool {
        self.wake_unstarted
    }
}

fn metadata_of(info: &RuntimeInfo) -> RuntimeMetadata {
    RuntimeMetadata {
        profile: info.profile.clone(),
        provider: info.provider.clone(),
        model: info.model.clone(),
    }
}

/// Port of `canonicalSessionPath`: absolute, then symlink-resolved, then
/// lexically cleaned, falling back a step at a time.
pub fn canonical_session_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let path = Path::new(path);
    let absolute = match std::path::absolute(path) {
        Ok(absolute) => absolute,
        Err(_) => return clean(path),
    };
    match std::fs::canonicalize(&absolute) {
        Ok(canonical) => clean(&canonical),
        Err(_) => clean(&absolute),
    }
}

fn clean(path: &Path) -> String {
    sessionfs::clean_go_path(&path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::testutil::{builder, controller, initial_runtime, user};
    use otto_core::agent::inbox::NotificationKind;

    #[tokio::test]
    async fn info_reports_the_current_session_and_the_resolved_profile() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let info = controller.info();
        assert_eq!(info.profile, "alpha");
        assert_eq!(info.model, "gpt-alpha");
        assert_eq!(info.provider, "openai-compatible");
        assert_eq!(info.session_id.len(), 32);
        // The file is lazy, so nothing exists until the first message.
        assert_eq!(info.session_path, "");
        assert_eq!(info.sandbox.summary(), controller.sandbox_info().summary());
        assert_eq!(info.workspace, controller.workspace());
    }

    #[tokio::test]
    async fn bash_approval_fails_closed_when_unavailable_or_busy() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        assert_eq!(
            controller.approve_bash("approval-1"),
            Err("temporary elevation is unavailable".to_string())
        );
        let _admission = controller.begin_operation().expect("admit turn");
        assert_eq!(
            controller.approve_bash("approval-1"),
            Err(PROMPT_ACTIVE.to_string())
        );
    }

    #[tokio::test]
    async fn info_carries_the_context_window_and_the_usage_snapshot() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let info = controller.info();
        assert!(!info.usage_present);
        assert_eq!(info.usage, Usage::default());
        assert_eq!(info.context_input_tokens, 0);
        assert!(!info.context_input_tokens_present);
    }

    #[tokio::test]
    async fn new_session_replaces_the_session_and_keeps_the_runtime() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        let before = controller.info();

        controller.new_session().await.expect("new session");

        let after = controller.info();
        assert_ne!(after.session_id, before.session_id);
        assert_eq!(after.model, "gpt-alpha");
        assert_eq!(after.profile, "alpha");
    }

    #[tokio::test]
    async fn switching_profile_switches_the_model() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        assert_eq!(controller.profiles(), vec!["alpha", "beta"]);

        controller.switch_profile("beta").await.expect("switch");

        let info = controller.info();
        assert_eq!(info.profile, "beta");
        assert_eq!(info.model, "gpt-beta");
        assert_ne!(info.session_id, "");
    }

    #[tokio::test]
    async fn an_unknown_profile_leaves_the_session_in_place() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        let before = controller.info();

        let error = controller
            .switch_profile("missing")
            .await
            .expect_err("unknown profile");
        assert!(error.contains("not found"), "{error}");
        assert_eq!(controller.info().session_id, before.session_id);
    }

    #[tokio::test]
    async fn renaming_records_the_name_on_the_session() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        controller
            .rename_session("  release notes  ")
            .expect("rename");

        assert_eq!(controller.info().session_name, "release notes");
        assert_eq!(
            controller.rename_session("   ").expect_err("blank"),
            INVALID_SESSION_NAME
        );
    }

    #[tokio::test]
    async fn archiving_moves_the_file_and_starts_a_fresh_session() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        // The store is lazy: one appended message gives the session a file.
        controller
            .current_session()
            .append(user("hello"))
            .await
            .expect("append");
        let before = controller.info();
        assert!(!before.session_path.is_empty());

        let result = controller.archive_current_session().await.expect("archive");

        assert!(result.path.contains("archive"), "{}", result.path);
        assert!(Path::new(&result.path).exists());
        assert!(!Path::new(&before.session_path).exists());
        assert_ne!(controller.info().session_id, before.session_id);
    }

    #[tokio::test]
    async fn a_session_with_no_file_cannot_be_archived() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let error = controller
            .archive_current_session()
            .await
            .expect_err("no file");
        assert_eq!(error, PERSISTENCE_DISABLED);
    }

    #[tokio::test]
    async fn the_default_profile_is_written_to_the_configuration_file() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        std::fs::write(
            controller.config_path(),
            "default_profile = \"alpha\"\n\n[profiles.alpha]\n\n[profiles.beta]\n",
        )
        .expect("write config");

        controller.set_default_profile("beta").expect("set default");

        let content = std::fs::read_to_string(controller.config_path()).expect("read config");
        assert!(content.contains("default_profile = \"beta\""), "{content}");
    }

    #[tokio::test]
    async fn a_closed_boundary_refuses_every_replacement() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let builder = builder(workspace.path(), sessions.path());
        let runtime = initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let info = builder.runtime_info(&runtime);
        let controller = Controller::new(builder, false, session, runner, info);

        assert_eq!(controller.profiles(), Vec::<String>::new());
        assert_eq!(controller.info().model, "");
        assert_eq!(
            controller.new_session().await.expect_err("new"),
            PERSISTENCE_DISABLED
        );
        assert_eq!(
            controller.switch_profile("beta").await.expect_err("switch"),
            PROFILE_SWITCH_UNAVAILABLE
        );
        assert_eq!(
            controller.set_default_profile("beta").expect_err("default"),
            PROFILE_SWITCH_UNAVAILABLE
        );
        assert_eq!(
            controller.rename_session("name").expect_err("rename"),
            PERSISTENCE_DISABLED
        );
        assert_eq!(
            controller.list_sessions(5).expect_err("list"),
            PERSISTENCE_DISABLED
        );
        assert!(controller.history().is_empty());
    }

    // ---- admission ----

    #[tokio::test]
    async fn one_operation_at_a_time_is_admitted() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let admission = controller.begin_operation().expect("first");
        assert_eq!(
            controller.begin_operation().expect_err("second"),
            PROMPT_ACTIVE
        );
        assert_eq!(
            controller.rename_session("name").expect_err("rename"),
            PROMPT_ACTIVE
        );
        assert_eq!(
            controller.new_session().await.expect_err("new"),
            PROMPT_ACTIVE
        );
        assert_eq!(
            controller.reload_sandbox().await.expect_err("reload"),
            SANDBOX_RELOAD_UNAVAILABLE
        );
        drop(admission);
        controller.begin_operation().expect("after release");
    }

    #[tokio::test]
    async fn a_closed_controller_refuses_every_operation() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        controller.close().expect("close");

        assert_eq!(controller.begin_operation().expect_err("begin"), CLOSED);
        assert_eq!(controller.list_sessions(5).expect_err("list"), CLOSED);
        assert_eq!(
            controller.rename_session("name").expect_err("rename"),
            CLOSED
        );
        assert_eq!(controller.new_session().await.expect_err("new"), CLOSED);
        assert_eq!(
            controller.reload_sandbox().await.expect_err("reload"),
            CLOSED
        );
        assert!(controller.tasks().is_none());
        assert!(!controller.dynamic_content_available());
        assert!(controller.current_session_opt().is_none());
    }

    #[tokio::test]
    async fn close_is_idempotent_and_reports_the_same_result() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        assert!(controller.close().is_ok());
        assert!(controller.close().is_ok());
    }

    #[tokio::test]
    async fn a_close_requested_during_an_operation_runs_when_it_ends() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let admission = controller.begin_operation().expect("admit");
        controller.request_close();
        // Still held, so the session is untouched.
        assert!(controller.current_session_opt().is_some());

        drop(admission);

        assert!(controller.current_session_opt().is_none());
        assert!(controller.close().is_ok());
    }

    #[tokio::test]
    async fn request_close_never_closes_an_idle_controller() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        controller.request_close();

        assert!(controller.current_session_opt().is_some());
        assert_eq!(controller.begin_operation().expect_err("begin"), CLOSED);
        controller.close().expect("close");
        assert!(controller.current_session_opt().is_none());
    }

    // ---- browsing ----

    #[tokio::test]
    async fn listing_marks_the_open_session_and_resuming_it_is_a_no_op() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        controller
            .current_session()
            .append(user("hello"))
            .await
            .expect("append");
        let open = controller.info();

        let listed = controller.list_sessions(5).expect("list");
        let row = listed
            .sessions
            .iter()
            .find(|row| row.id == open.session_id)
            .expect("row for the open session");
        assert!(row.current, "the open session must be marked current");

        let resumed = controller
            .resume_session(&open.session_path)
            .await
            .expect("resume");
        assert_eq!(
            canonical_session_path(&resumed.session_path),
            canonical_session_path(&open.session_path)
        );
        assert_eq!(controller.info().session_id, open.session_id);
    }

    #[tokio::test]
    async fn resuming_another_session_replaces_the_current_one() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        controller
            .current_session()
            .append(user("first"))
            .await
            .expect("append");
        let first = controller.info();

        controller.new_session().await.expect("new session");
        controller
            .current_session()
            .append(user("second"))
            .await
            .expect("append");
        let second = controller.info();
        assert_ne!(first.session_id, second.session_id);

        let resumed = controller
            .resume_session(&first.session_path)
            .await
            .expect("resume");
        assert!(resumed.warnings.is_empty(), "{:?}", resumed.warnings);
        assert_eq!(controller.info().session_id, first.session_id);
        assert_eq!(controller.history().len(), 1);
    }

    #[tokio::test]
    async fn archiving_a_listed_session_leaves_the_current_one_open() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        controller
            .current_session()
            .append(user("first"))
            .await
            .expect("append");
        let first = controller.info();
        controller.new_session().await.expect("new session");
        controller
            .current_session()
            .append(user("second"))
            .await
            .expect("append");
        let second = controller.info();

        let result = controller
            .archive_session(&first.session_path)
            .await
            .expect("archive");

        assert_eq!(result.id, first.session_id);
        assert!(!Path::new(&first.session_path).exists());
        assert_eq!(controller.info().session_id, second.session_id);
    }

    #[tokio::test]
    async fn archiving_the_current_session_by_path_starts_a_fresh_one() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        controller
            .current_session()
            .append(user("hello"))
            .await
            .expect("append");
        let before = controller.info();

        controller
            .archive_session(&before.session_path)
            .await
            .expect("archive");

        assert_ne!(controller.info().session_id, before.session_id);
    }

    // ---- tasks and wake ----

    /// Port of `TestControllerTasksNilWhenRunnerLacksTaskLister`.
    #[tokio::test]
    async fn a_runner_without_a_registry_reports_no_tasks_and_no_wake() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let mut builder = builder(workspace.path(), sessions.path());
        builder.config.agents.enabled = Some(false);
        let runtime = initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let info = builder.runtime_info(&runtime);
        let controller = Controller::new(builder, true, session, runner, info);

        assert!(controller.tasks().is_none());
        assert!(controller.prepare_wake().expect("prepare").is_none());
        // No claim was taken, so an operation is still admissible.
        controller.begin_operation().expect("admit");
    }

    /// The registry is reachable through the view, and an empty inbox still
    /// admits no wake turn. Port of `TestPrepareWakeNoPendingNotifications`.
    #[tokio::test]
    async fn a_runner_with_an_empty_registry_exposes_it_but_admits_no_wake() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        let tasks = controller.tasks().expect("registry");
        assert!(tasks.list().is_empty());
        assert_eq!(tasks.pending(), 0);
        assert!(controller.prepare_wake().expect("prepare").is_none());
        controller.begin_operation().expect("admit");
    }

    /// A pending notification claims a turn, and dropping the claim without
    /// running it releases it. Port of `TestPrepareWakeClaimsTurn`.
    #[tokio::test]
    async fn a_pending_notification_claims_a_wake_turn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        controller
            .current_runner()
            .expect("runner")
            .tasks
            .as_ref()
            .expect("registry")
            .notifications()
            .push(otto_core::agent::inbox::Notification {
                text: "[task-notification] task t1 succeeded".to_string(),
                ..otto_core::agent::inbox::Notification::default()
            });

        let wake = controller
            .prepare_wake()
            .expect("prepare")
            .expect("claimed");
        assert_eq!(
            controller.begin_operation().expect_err("busy"),
            PROMPT_ACTIVE
        );
        wake.cancel();
        controller.begin_operation().expect("admit");
    }

    #[tokio::test]
    async fn notify_queues_a_message_that_claims_a_wake_turn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        assert!(controller.notify(Notification {
            kind: Some(NotificationKind::Message),
            text: "[feishu] hello".to_string(),
            ..Notification::default()
        }));
        let wake = controller
            .prepare_wake()
            .expect("prepare")
            .expect("claimed");
        wake.cancel();
        controller.close().expect("close");
        assert!(!controller.notify(Notification {
            text: "late".to_string(),
            ..Notification::default()
        }));
    }
}
