//! Shared session lifecycle and the capability contracts every frontend uses.
//!
//! The REPL, `otto serve`, and a later TUI all drive one [`Controller`]: it
//! owns the current session, the runner built for it, and the admission rule
//! that lets exactly one operation touch them at a time.
//!
//! Concurrency: one `Mutex<State>` guards everything. Admission is an RAII
//! guard ([`Admission`]), so a dropped future releases the claim. No lock is
//! held across an `await`.
//!
//! Close: [`Controller::request_close`] never blocks and never closes, which is
//! what a callback running inside an operation needs. [`Controller::close`]
//! completes the close, waiting on a condition variable when an operation is
//! still in flight. It blocks a thread, so an async caller cancels the
//! in-flight work and awaits it before calling `close`.
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
use otto_core::model::{Block, Message, Usage};
use otto_core::session::{ListResult, RuntimeMetadata, Session};
use tokio_util::sync::CancellationToken;

use crate::cli::info::SandboxInfo;
use crate::cli::runtime_builder::{Builder, Runner, RuntimeInfo, SharedSession};
use crate::cli::sandbox_setup::{self, SandboxChange};
use crate::session::{self as sessionfs, ArchiveResult, MAX_LIST_SESSIONS};
use crate::tool::remind::Reminders;

pub use sandbox::SandboxControl;
pub use tasks::{Task, TaskStatus, TaskView};
pub use wake::WakeOperation;

pub const PERSISTENCE_DISABLED: &str = "session persistence is disabled";
pub const PROFILE_SWITCH_UNAVAILABLE: &str = "profile switching is not available";
pub const SANDBOX_RELOAD_UNAVAILABLE: &str = "sandbox reload is not available";
pub const SESSION_OPERATION_UNAVAILABLE: &str = "session operation is unavailable";
/// The server answers it with 409 `turn_active`.
pub const PROMPT_ACTIVE: &str = "a prompt is already active";
pub const CLOSED: &str = "controller is closed";
pub const SESSION_RENAME_UNAVAILABLE: &str = "session rename is unavailable";
pub const INVALID_SESSION_NAME: &str = "session is invalid: session name is required";

/// One configured profile row a frontend can display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileSummary {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
}

/// What a frontend may display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    pub session_id: String,
    pub session_name: String,
    pub session_path: String,
    pub workspace: String,
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub thinking: String,
    pub usage: Usage,
    pub usage_present: bool,
    pub context_window: i64,
    pub context_input_tokens: i64,
    pub context_input_tokens_present: bool,
    pub context_input_tokens_pending: bool,
    pub sandbox: SandboxInfo,
}

/// What a completed session replacement reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResumeResult {
    pub session_path: String,
    pub warnings: Vec<String>,
}

fn validate_thinking_arg(thinking: &str) -> Result<(), String> {
    if matches!(
        thinking,
        "" | "unset" | "default" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        return Ok(());
    }
    Err("thinking must be one of unset, low, medium, high, xhigh, max".to_string())
}

fn normalize_thinking_arg(thinking: &str) -> String {
    match thinking {
        "unset" | "default" => String::new(),
        other => other.to_string(),
    }
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
    /// file.
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
    /// An operation holds admission. Covers both the active and the replacing
    /// state, because every caller treats them identically.
    busy: bool,
    /// Bumped on every admission so a stale [`Admission`] guard is a no-op
    /// after [`Controller::request_close`] releases an unstarted wake.
    generation: u64,
    /// A wake claim that has not run yet. `request_close` releases it instead
    /// of waiting.
    wake_unstarted: bool,
    closed: bool,
    close_pending: bool,
    close_in_progress: bool,
    close_done: bool,
    close_err: Option<String>,
}

pub struct Controller {
    builder: Arc<Builder>,
    /// False when the redaction boundary is closed. Every operation that would
    /// persist or display provider identity is then refused.
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

    /// Wires the process sandbox. The control reports the state now in effect,
    /// so every controller built from one composition root agrees after a
    /// reload.
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
    /// operation may call it.
    pub fn request_close(&self) {
        let mut state = self.lock();
        state.closed = true;
        if state.busy {
            if state.wake_claim_is_unstarted() {
                // Release the claim, so the close need not wait for a wake turn
                // that will now never run.
                state.busy = false;
                state.generation += 1;
                state.wake_unstarted = false;
            } else {
                state.close_pending = true;
            }
        }
    }

    /// Completes the close, waiting for an in-flight operation to finish.
    /// Blocking: an async caller must cancel and await its own work first.
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
    /// task registry.
    pub(crate) fn current_runner(&self) -> Option<Arc<Runner>> {
        self.runner().ok()
    }

    pub fn config_path(&self) -> &PathBuf {
        &self.builder.config_path
    }

    /// The sandbox state now in effect: the live process sandbox when one is
    /// wired, otherwise what the builder resolved at startup.
    pub fn sandbox_info(&self) -> SandboxInfo {
        match &self.sandbox {
            Some(control) => control.info(),
            None => self.builder.effective_sandbox_info(),
        }
    }

    pub fn dynamic_content(&self) -> bool {
        self.dynamic_content
    }

    pub fn dynamic_content_available(&self) -> bool {
        !self.lock().closed && self.dynamic_content
    }

    pub fn workspace(&self) -> &str {
        &self.builder.workspace_path
    }

    /// A closed boundary reports the sandbox only.
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
            thinking: current.info.thinking.clone(),
            usage: snapshot.aggregate_usage,
            usage_present: snapshot.aggregate_usage_present,
            context_window: current.info.context_window,
            context_input_tokens: snapshot.context_input_tokens,
            context_input_tokens_present: snapshot.context_input_tokens_present,
            context_input_tokens_pending: snapshot.context_input_tokens_pending,
            sandbox,
        }
    }

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

    /// What the next provider request contains, or `None` after a close or
    /// when the redaction boundary withholds dynamic content.
    pub fn context_report(&self) -> Option<otto_core::agent::context_report::ContextReport> {
        if !self.dynamic_content {
            return None;
        }
        let runner = self.current_runner()?;
        Some(runner.context_report())
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

    /// The MCP server status rows for `/mcp`, in configuration order. Empty
    /// for a closed controller.
    pub fn mcp(&self) -> Vec<crate::mcp::ServerStatus> {
        self.lock()
            .current
            .as_ref()
            .map(|current| current.runner.mcp.status())
            .unwrap_or_default()
    }

    fn runner(&self) -> Result<Arc<Runner>, String> {
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

    /// Shuts every connected MCP server down, waiting up to 5 seconds. A
    /// no-op on a controller with no current runner (already closed, or
    /// never opened). [`Self::close`] also starts an MCP close, but only in
    /// the background (`Runner::close`, via `Current::discard`): a process
    /// exiting right after `close` returns would drop the runtime before
    /// that spawned close ran. At a process-exit site, call this *before*
    /// `close`, not after: [`crate::mcp::Servers::close`] uses `mem::take`
    /// internally, so whichever of this awaited call or `close`'s
    /// background one runs first is the one that actually closes anything.
    pub async fn close_mcp(&self) {
        let runner = self
            .lock()
            .current
            .as_ref()
            .map(|current| Arc::clone(&current.runner));
        if let Some(runner) = runner {
            runner.close_mcp().await;
        }
    }

    // ---- turns ----

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

    pub async fn prompt_with_image(
        &self,
        text: &str,
        image: Block,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let _admission = self.begin_operation().map_err(AgentError::Other)?;
        let runner = self.runner().map_err(AgentError::Other)?;
        runner.run_with_image(text, image, emit, cancel).await
    }

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

    /// The configured profile names, sorted.
    pub fn profiles(&self) -> Vec<String> {
        if !self.dynamic_content_available() {
            return Vec::new();
        }
        let mut names: Vec<String> = self.builder.config.profiles.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn profile_summaries(&self) -> Vec<ProfileSummary> {
        if !self.dynamic_content_available() {
            return Vec::new();
        }
        let current = self
            .lock()
            .current
            .as_ref()
            .map(|current| (current.info.profile.clone(), current.info.thinking.clone()));
        let mut rows: Vec<ProfileSummary> = self
            .builder
            .config
            .profiles
            .iter()
            .map(|(name, profile)| ProfileSummary {
                name: name.clone(),
                provider: profile.provider.clone(),
                model: profile.model.clone(),
                thinking: current
                    .as_ref()
                    .filter(|(current_name, _)| current_name == name)
                    .map(|(_, thinking)| thinking.clone())
                    .unwrap_or_else(|| profile.thinking.clone()),
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

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

    pub fn profile_effective_thinking(&self, profile: &str) -> String {
        if !self.dynamic_content_available() {
            return String::new();
        }
        self.builder
            .resolve_profile(profile)
            .map(|runtime| runtime.thinking)
            .unwrap_or_default()
    }

    pub async fn set_thinking(&self, thinking: &str) -> Result<(), String> {
        validate_thinking_arg(thinking)?;
        let admission = self.begin_replacement()?;
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        let (session, runtime) = {
            let state = self.lock();
            let current = state.current.as_ref().ok_or_else(|| CLOSED.to_string())?;
            let metadata = RuntimeMetadata {
                profile: current.info.profile.clone(),
                provider: current.info.provider.clone(),
                model: current.info.model.clone(),
            };
            let mut runtime = self.replacement_runtime(&metadata)?;
            runtime.profile = current.info.profile.clone();
            runtime.provider = current.info.provider.clone();
            runtime.model = current.info.model.clone();
            runtime.thinking = normalize_thinking_arg(thinking);
            (current.session.clone(), runtime)
        };
        let runner = Arc::new(self.builder.build_runner(&session, &runtime).await?);
        session
            .update_thinking_level(&runtime.thinking)
            .map_err(|error| self.builder.redact_error(&error, Some(&runtime)))?;
        {
            let mut state = self.lock();
            let current = state.current.as_mut().ok_or_else(|| CLOSED.to_string())?;
            current.info = self.builder.runtime_info(&runtime);
            let old = std::mem::replace(&mut current.runner, runner);
            old.close();
        }
        drop(admission);
        Ok(())
    }

    pub fn save_profile_thinking(&self, thinking: &str) -> Result<(), String> {
        validate_thinking_arg(thinking)?;
        if self.lock().closed {
            return Err(CLOSED.to_string());
        }
        if !self.dynamic_content {
            return Err(PROFILE_SWITCH_UNAVAILABLE.to_string());
        }
        let profile = self.info().profile;
        if profile.is_empty() {
            return Err("current profile is empty".to_string());
        }
        crate::config::set_profile_thinking_file(
            &self.builder.config_path,
            &profile,
            &normalize_thinking_arg(thinking),
        )
        .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    // ---- session replacement ----

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
        let runner = Arc::new(self.builder.build_runner(&session, &runtime).await?);
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
        sessionfs::list(
            &self.builder.session_root,
            &self.builder.workspace_path,
            &current_path,
            limit,
        )
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
        sessionfs::archive(
            &self.builder.session_root,
            &self.builder.workspace_path,
            Path::new(path),
        )
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
        let (path, info) = {
            let state = self.lock();
            let current = state.current.as_ref().ok_or_else(|| CLOSED.to_string())?;
            (current.session.path(), current.info.clone())
        };
        if path.is_empty() {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        let runtime = self.current_runtime_from_info(info)?;
        let replacement = self.fresh_replacement(&runtime).await?;
        match sessionfs::archive(
            &self.builder.session_root,
            &self.builder.workspace_path,
            Path::new(&path),
        ) {
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

    /// The current runner's task registry, or `None` when it tracks none or the
    /// controller is closed.
    pub fn tasks(&self) -> Option<Arc<dyn TaskView>> {
        let state = self.lock();
        if state.closed {
            return None;
        }
        tasks::task_view(state.current.as_ref()?.runner.as_ref())
    }

    /// The current runner's timer registry, or `None` when the timer tools
    /// are not registered or the controller is closed. `/timers` lists it and
    /// [`Self::archive_current_session`] clears it.
    pub fn reminders(&self) -> Option<Arc<Reminders>> {
        let state = self.lock();
        if state.closed {
            return None;
        }
        state.current.as_ref()?.runner.reminders.clone()
    }

    /// Claims a turn only when the runner has pending task notifications.
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

    /// Resolves a user-supplied `/sandbox allow` path against this process's
    /// home directory and the filesystem, without changing anything.
    pub fn resolve_sandbox_read_path(&self, input: &str) -> Result<String, String> {
        sandbox_setup::resolve_read_path(input, &self.builder.home)
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
        let path = self.builder.config_path.clone();
        let amendment = sandbox_setup::amend_sandbox_config(&path, &change)?;
        match control.reload().await {
            Ok(info) => Ok(info),
            Err(message) => Err(
                match sandbox_setup::revert_sandbox_config(&path, &amendment) {
                    Ok(()) => format!("{message}; the configuration change was rolled back"),
                    Err(failure) => format!(
                        "{message}; the configuration change could not be rolled back: {failure}"
                    ),
                },
            ),
        }
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

    fn current_runtime(&self) -> Result<Runtime, String> {
        let info = self
            .lock()
            .current
            .as_ref()
            .map(|current| current.info.clone())
            .ok_or_else(|| CLOSED.to_string())?;
        self.current_runtime_from_info(info)
    }

    fn current_runtime_from_info(&self, info: RuntimeInfo) -> Result<Runtime, String> {
        let mut runtime = self.replacement_runtime(&metadata_of(&info))?;
        runtime.thinking = info.thinking;
        Ok(runtime)
    }

    /// The two gates before every replacement: persistence must be enabled, and
    /// the boundary must be open both before and after resolving.
    fn replacement_runtime(&self, metadata: &RuntimeMetadata) -> Result<Runtime, String> {
        if !self.dynamic_content {
            return Err(PERSISTENCE_DISABLED.to_string());
        }
        resolve_replacement(&self.builder, metadata)
    }

    /// A new session and runner for an already-resolved runtime. Every failure
    /// closes what it built.
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
    /// arrived while the replacement was building closes the replacement too.
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

    /// A controller over the session file at `path`, with the repair warnings
    /// activating it produced.
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

/// Pins the session file at `path`, activates it, and builds a runner for the
/// runtime its header records.
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
    let session_thinking = prepared.info().thinking.clone();
    let mut runtime = match resolve_replacement(builder, &metadata) {
        Ok(runtime) => runtime,
        Err(message) => {
            let _ = prepared.close();
            return Err(message);
        }
    };
    if !session_thinking.is_empty() {
        runtime.thinking = session_thinking;
    }
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

/// Absolute, then symlink-resolved, then lexically cleaned, falling back a step
/// at a time.
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
    use crate::cli::info::SandboxNetwork;
    use crate::cli::sandbox_setup::SandboxChange;
    use crate::cli::testutil::{
        FakeSandbox, builder, controller, initial_runtime, seatbelt_info, user,
    };
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
    async fn the_context_report_is_withheld_without_dynamic_content() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let open = controller(workspace.path(), sessions.path()).await;
        let report = open.context_report().expect("report");
        assert_eq!(report.model, "gpt-alpha");
        assert!(!report.sections.is_empty());

        let builder = builder(workspace.path(), sessions.path());
        let runtime = initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let info = builder.runtime_info(&runtime);
        let withheld = Controller::new(builder, false, session, runner, info);
        assert!(withheld.context_report().is_none());
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
    async fn replacing_runner_only_happens_between_turns() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        let session = controller.current_session();
        let session_id = session.header().id;
        let runtime = initial_runtime(controller.builder());
        let info = controller.builder().runtime_info(&runtime);

        let candidate = controller
            .builder()
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let admission = controller.begin_operation().expect("admit turn");
        assert_eq!(
            controller.replace_runner_if_current(&session_id, candidate, info.clone()),
            Err(PROMPT_ACTIVE.to_string())
        );
        drop(admission);

        let candidate = controller
            .builder()
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        assert_eq!(
            controller.replace_runner_if_current(&session_id, candidate, info),
            Ok(true)
        );
    }

    #[tokio::test]
    async fn amending_the_sandbox_writes_the_configuration_and_reloads_it() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let (control, calls) = FakeSandbox::new(
            seatbelt_info(SandboxNetwork::Allowed),
            seatbelt_info(SandboxNetwork::Denied),
            None,
        );
        let controller = controller(workspace.path(), sessions.path())
            .await
            .with_sandbox_control(control);
        std::fs::create_dir(workspace.path().join("cache")).expect("cache");
        let resolved = controller
            .resolve_sandbox_read_path("~/cache")
            .expect("resolve");

        let info = controller
            .amend_sandbox(SandboxChange::AllowReadPath(resolved.clone()))
            .await
            .expect("amend");

        assert_eq!(info.network, SandboxNetwork::Denied);
        assert_eq!(*calls.lock().expect("calls"), 1);
        let written = std::fs::read_to_string(controller.config_path()).expect("read config");
        assert!(written.contains(&resolved), "{written}");
    }

    #[tokio::test]
    async fn a_failed_reload_rolls_the_configuration_back() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let (control, calls) = FakeSandbox::new(
            seatbelt_info(SandboxNetwork::Allowed),
            seatbelt_info(SandboxNetwork::Allowed),
            Some("sandbox reload failed: self-test-failed"),
        );
        let controller = controller(workspace.path(), sessions.path())
            .await
            .with_sandbox_control(control);
        std::fs::write(controller.config_path(), "# preserved\n").expect("write config");

        let error = controller
            .amend_sandbox(SandboxChange::Network("deny".to_string()))
            .await
            .expect_err("reload fails");

        assert!(error.contains("self-test-failed"), "{error}");
        assert!(error.contains("rolled back"), "{error}");
        assert_eq!(*calls.lock().expect("calls"), 1);
        assert_eq!(
            std::fs::read_to_string(controller.config_path()).expect("read config"),
            "# preserved\n"
        );
    }

    #[tokio::test]
    async fn amending_without_a_sandbox_control_or_while_busy_writes_nothing() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        assert_eq!(
            controller
                .amend_sandbox(SandboxChange::Network("deny".to_string()))
                .await,
            Err(SANDBOX_RELOAD_UNAVAILABLE.to_string())
        );

        let (control, _) = FakeSandbox::new(
            seatbelt_info(SandboxNetwork::Allowed),
            seatbelt_info(SandboxNetwork::Denied),
            None,
        );
        let controller = controller.with_sandbox_control(control);
        let _admission = controller.begin_operation().expect("admit turn");
        assert_eq!(
            controller
                .amend_sandbox(SandboxChange::Network("deny".to_string()))
                .await,
            Err(PROMPT_ACTIVE.to_string())
        );
        assert!(!controller.config_path().exists());
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
    async fn new_session_keeps_runtime_changed_in_the_current_session() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;

        controller.switch_profile("beta").await.expect("switch");
        controller.set_thinking("high").await.expect("thinking");
        let before = controller.info();

        controller.new_session().await.expect("new session");

        let after = controller.info();
        assert_ne!(after.session_id, before.session_id);
        assert_eq!(after.profile, "beta");
        assert_eq!(after.model, "gpt-beta");
        assert_eq!(after.thinking, "high");
    }

    #[tokio::test]
    async fn resuming_a_session_keeps_the_session_thinking_level() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        controller
            .current_session()
            .append(user("hello"))
            .await
            .expect("append");

        controller.set_thinking("high").await.expect("thinking");
        let path = controller.info().session_path;
        assert!(!path.is_empty());
        controller.close().expect("close");

        let builder = builder(workspace.path(), sessions.path());
        let (reopened, warnings) = Controller::open(Arc::new(builder), Path::new(&path))
            .await
            .expect("open");

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(reopened.info().thinking, "high");
    }

    #[tokio::test]
    async fn switching_profile_switches_the_model() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        assert_eq!(controller.profiles(), vec!["alpha", "beta"]);

        let before = controller.info();

        controller.switch_profile("beta").await.expect("switch");

        let info = controller.info();
        assert_eq!(info.session_id, before.session_id);
        assert_eq!(info.profile, "beta");
        assert_eq!(info.model, "gpt-beta");
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
    async fn archiving_cancels_the_session_timers() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let controller = controller(workspace.path(), sessions.path()).await;
        controller
            .current_session()
            .append(user("hello"))
            .await
            .expect("append");
        let reminders = controller.reminders().expect("timer registry");
        reminders
            .schedule(std::time::Duration::from_secs(60), "outstanding".into())
            .expect("schedule");

        controller.archive_current_session().await.expect("archive");

        assert!(
            reminders.list().is_empty(),
            "archiving must cancel outstanding timers"
        );
        assert!(controller.reminders().expect("registry").list().is_empty());
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
    /// admits no wake turn.
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
    /// running it releases it.
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
