//! Runs the Otto agent inside another Rust program.
//!
//! [`Otto::builder`] composes the same runtime `otto acp` builds for one
//! workspace, config file and environment: the same tools, sandbox,
//! approvals, session files, logs and startup warnings. The host disables
//! individual [`Component`]s in code; it cannot enable one that the config or
//! the managed policy disables, and it cannot run bash without the sandbox
//! (only `[sandbox].driver = "off"` in config does that).
//!
//! ```no_run
//! use otto::embed::{ApprovalDecision, Component, ConfigSource, Input, Otto};
//!
//! # async fn demo() -> Result<(), otto::embed::Error> {
//! # let overrides = otto_core::config::Overrides::default();
//! # let log_file = std::fs::File::create("otto.log").unwrap();
//! # let cancel = tokio_util::sync::CancellationToken::new();
//! let otto = Otto::builder()
//!     .workspace("/path/to/repo")            // required
//!     .config(ConfigSource::Default)         // default; or ConfigSource::Path(..)
//!     .environment(std::env::vars())         // default: the process environment
//!     .overrides(overrides)                  // optional otto_core::config::Overrides
//!     .diagnostics(Box::new(log_file))       // default: the process stderr, as `otto acp`
//!     .disable(Component::Mcp)               // any number of times
//!     .build()
//!     .await?;
//!
//! let session = otto.new_session(None).await?; // None: the startup workspace
//! session.set_auto_mode(false)?;                // MANUAL: approvals reach `decide`
//! let stop = session
//!     .prompt(
//!         Input::Text("fix the failing test"),
//!         &mut |event| { /* otto_core::agent::Event */ },
//!         &cancel,
//!         |request| async move { ApprovalDecision::Deny },
//!     )
//!     .await?;
//! session.close().await;
//! otto.close().await?;
//! # let _ = stop;
//! # Ok(())
//! # }
//! ```
//!
//! # Compatibility surface
//!
//! The covered surface is this module and the types its signatures name:
//! the `otto_core` types `agent::Event`, `agent::EventSink`,
//! `agent::AgentError`, `agent::CompactionResult`, `agent::steering::Message`,
//! `config::Overrides`, `model::Message` and `model::Block`, and the `otto`
//! types this module re-exports: [`ApprovalDecision`], [`ApprovalRequest`],
//! [`Stop`], [`Info`], [`ForkedFrom`], [`SandboxInfo`], [`SandboxMode`],
//! [`SandboxNetwork`] and [`SandboxReason`]. Every other `pub` item of the
//! `otto` crate stays `pub` for the frontends and the tests and is not
//! covered.
//!
//! This module changes only by addition:
//!
//! - no item is removed or renamed and no signature changes incompatibly;
//! - [`Component`], [`ConfigSource`], [`Input`] and [`Error`] are
//!   `#[non_exhaustive]`, so a variant can be added;
//! - [`Otto`], [`OttoBuilder`] and [`Session`] have no public fields, and an
//!   option is a builder method, so a new option is a new method;
//! - when a behavior must change, a new item is added next to the old one
//!   and the old one keeps working.
//!
//! # One instance per process
//!
//! The config file lock is held per process, so [`OttoBuilder::build`]
//! returns [`Error::AlreadyRunning`] while another [`Otto`] of this process is
//! alive. [`Otto::close`], or dropping the last clone, releases it. Use
//! [`Otto::open_workspace`] for more than one repository.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::future::Future;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use otto_core::agent::steering::Message as SteeringMessage;
use otto_core::agent::{AgentError, CompactionResult, EventSink};
use otto_core::config::Overrides;
use otto_core::config::resolve::Runtime;
use otto_core::model::{Block, Message};
use tokio_util::sync::CancellationToken;

use crate::app::approval::Step;
use crate::app::{CLOSED, Controller, PROMPT_ACTIVE, SandboxControl};
use crate::runtime::builder::Builder;
use crate::runtime::sandbox_switch::SandboxReloader;
use crate::runtime::startup::{self, Admission, Disabled, StartOptions, WorkspaceSandbox};
use crate::session::{self as sessionfs, MAX_LIST_SESSIONS};

pub use crate::app::approval::{ApprovalDecision, ApprovalRequest, Stop};
pub use crate::app::{ForkedFrom, Info};
pub use crate::runtime::info::{SandboxInfo, SandboxMode, SandboxNetwork, SandboxReason};

/// Why an operation failed. The text never contains a configured secret.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Another [`Otto`] of this process is alive.
    #[error("another Otto instance is already running in this process")]
    AlreadyRunning,
    /// The environment, config, credentials or storage could not be loaded.
    #[error("{0}")]
    Config(String),
    /// A workspace path is missing, not a directory, not admitted or cannot
    /// be loaded.
    #[error("{0}")]
    Workspace(String),
    /// No session file with this id exists in the workspace.
    #[error("session not found: {0}")]
    SessionNotFound(String),
    /// [`Component::SessionPersistence`] is disabled, so there are no session
    /// files to resume or list.
    #[error("session persistence is disabled")]
    PersistenceDisabled,
    /// A turn is still running on this session.
    #[error("a prompt is already active")]
    PromptActive,
    /// The [`Otto`] or the [`Session`] is closed.
    #[error("closed")]
    Closed,
    /// A turn, steering message or compaction failed.
    #[error("{0}")]
    Agent(AgentError),
    /// A session, sandbox or store failed to close. Every other part still
    /// closed.
    #[error("close: {0}")]
    Close(String),
}

/// Where the user config comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigSource {
    /// `~/.config/otto/config.toml`, as `otto` without `--config`. A missing
    /// file is an empty config.
    Default,
    /// This file, as `otto --config <path>`. A missing file is an error.
    Path(PathBuf),
}

/// A part of the runtime the host can switch off. A disabled component adds
/// no tool, opens no database file and starts no child process. Each state is
/// one the CLI reaches through a config key, a flag or an unavailable store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Component {
    /// No `bash` tool and no sandbox executor; the sandbox reports bash
    /// unavailable.
    Bash,
    /// No MCP server process and no MCP tool (`[mcp].enabled = false`).
    Mcp,
    /// No memory service and no memory tool (`[memory].enabled = false`).
    Memory,
    /// No skill catalog and no `skill` tool (`[skills].enabled = false`).
    Skills,
    /// No plugin tool and no plugin hook (`[plugins].enabled = false`).
    Plugins,
    /// No subagent tool (`[agents].enabled = false`).
    Subagents,
    /// No reflect tool and no automatic reflection (`[reflection].enabled =
    /// false`).
    Reflection,
    /// `schedule.db` is not opened. The schedule tools stay registered and
    /// report that scheduled delivery is unavailable, as they do when the
    /// store fails to open.
    Schedule,
    /// No session file is written; [`Otto::resume_session`] and
    /// [`Otto::list_sessions`] return [`Error::PersistenceDisabled`]
    /// (`--no-session`).
    SessionPersistence,
    /// `usage.db` is not opened.
    Usage,
    /// `tasks.db` is not opened.
    TaskHistory,
}

impl Component {
    fn mark(self, disabled: &mut Disabled) {
        match self {
            Self::Bash => disabled.bash = true,
            Self::Mcp => disabled.mcp = true,
            Self::Memory => disabled.memory = true,
            Self::Skills => disabled.skills = true,
            Self::Plugins => disabled.plugins = true,
            Self::Subagents => disabled.subagents = true,
            Self::Reflection => disabled.reflection = true,
            Self::Schedule => disabled.schedule = true,
            Self::SessionPersistence => disabled.session_persistence = true,
            Self::Usage => disabled.usage = true,
            Self::TaskHistory => disabled.task_history = true,
        }
    }
}

/// The first step of a turn.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Input<'a> {
    /// A user message.
    Text(&'a str),
    /// A user message with one image block.
    Image(&'a str, Block),
}

/// One row of [`Otto::list_sessions`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionSummary {
    /// The id [`Otto::resume_session`] takes.
    pub id: String,
    /// The workspace the session belongs to.
    pub cwd: String,
    /// The name set for the session; empty when none.
    pub name: String,
    /// The text of the last user message; empty when none.
    pub last_user_text: String,
    /// Messages in the session file.
    pub message_count: i64,
    /// When the session file was created.
    pub created: SystemTime,
    /// When the session file was last written.
    pub updated: SystemTime,
    /// Profile, provider and model the session last ran with.
    pub profile: String,
    /// See `profile`.
    pub provider: String,
    /// See `profile`.
    pub model: String,
}

// ---- process guard ----

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Holds the process-wide flag; dropping it clears the flag.
struct Guard;

impl Guard {
    fn acquire() -> Result<Self, Error> {
        RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| Self)
            .map_err(|_| Error::AlreadyRunning)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

// ---- diagnostics ----

/// One writer shared by the startup warnings, the repair warnings and the
/// logger.
#[derive(Clone)]
struct Diagnostics(Arc<Mutex<Box<dyn Write + Send>>>);

impl Write for Diagnostics {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).flush()
    }
}

// ---- builder ----

/// Configures and builds an [`Otto`].
pub struct OttoBuilder {
    workspace: Option<PathBuf>,
    config: ConfigSource,
    environment: Option<Vec<Vec<u8>>>,
    overrides: Overrides,
    diagnostics: Option<Box<dyn Write + Send>>,
    disabled: Disabled,
}

impl OttoBuilder {
    /// The startup workspace, as `--cwd`. Required.
    pub fn workspace(mut self, path: impl AsRef<Path>) -> Self {
        self.workspace = Some(path.as_ref().to_path_buf());
        self
    }

    /// The config source. Default: [`ConfigSource::Default`].
    pub fn config(mut self, source: ConfigSource) -> Self {
        self.config = source;
        self
    }

    /// Replaces the environment snapshot. The snapshot holds the API keys and
    /// decides the home directory (`HOME`). Default: the process environment.
    pub fn environment<I, K, V>(mut self, entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.environment = Some(entries.into_iter().map(|(k, v)| entry(k, v)).collect());
        self
    }

    /// The values of the `--profile`, `--model`, `--provider`, `--base-url`,
    /// `--thinking` and related flags.
    pub fn overrides(mut self, overrides: Overrides) -> Self {
        self.overrides = overrides;
        self
    }

    /// Where startup warnings, session repair warnings and log lines go.
    /// Default: the process stderr, where `otto acp` writes them.
    pub fn diagnostics(mut self, writer: Box<dyn Write + Send>) -> Self {
        self.diagnostics = Some(writer);
        self
    }

    /// Disables one component. May be called any number of times.
    pub fn disable(mut self, component: Component) -> Self {
        component.mark(&mut self.disabled);
        self
    }

    /// Composes the runtime. Fails with [`Error::AlreadyRunning`] while
    /// another [`Otto`] of this process is alive.
    pub async fn build(self) -> Result<Otto, Error> {
        let guard = Guard::acquire()?;
        let workspace = self
            .workspace
            .ok_or_else(|| Error::Workspace("a workspace is required".to_string()))?;
        let custom_diagnostics = self.diagnostics.is_some();
        let diagnostics = Diagnostics(Arc::new(Mutex::new(
            self.diagnostics
                .unwrap_or_else(|| Box::new(std::io::stderr())),
        )));
        let logger = if custom_diagnostics {
            crate::log::Logger::new(Box::new(diagnostics.clone()))
        } else {
            crate::log::Logger::stderr()
        };
        let (config_path, explicit_config) = match self.config {
            ConfigSource::Default => (String::new(), false),
            ConfigSource::Path(path) => (path.to_string_lossy().into_owned(), true),
        };
        let host_entries = self.environment.unwrap_or_else(|| {
            std::env::vars_os()
                .map(|(name, value)| entry(name, value))
                .collect()
        });
        let cancel = CancellationToken::new();
        let started = startup::start(
            StartOptions {
                host_entries,
                config_path,
                explicit_config,
                cwd: workspace.to_string_lossy().into_owned(),
                overrides: self.overrides,
                logger: Arc::new(logger),
                disabled: self.disabled,
            },
            &cancel,
            &mut diagnostics.clone(),
        )
        .await
        .map_err(startup_error)?;
        let startup::Started {
            builder,
            runtime,
            workspace_path,
            sandbox,
        } = started;
        let roots =
            otto_core::config::resolve_server(&builder.config, &builder.environment, "", "")
                .map_err(|error| error.to_string())
                .and_then(|server| startup::canonicalize_workspace_roots(&server.workspace_roots))
                .map_err(|message| Error::Config(builder.redact_error(&message, Some(&runtime))));
        let roots = match roots {
            Ok(roots) => roots,
            Err(error) => {
                let _ = sandbox.control.close().await;
                return Err(error);
            }
        };
        let config_path = builder.config_path.clone();
        Ok(Otto {
            inner: Arc::new(Inner {
                builder: Arc::new(builder),
                runtime,
                workspace: PathBuf::from(workspace_path),
                sandbox,
                disabled: self.disabled,
                roots,
                config_path,
                loaded: tokio::sync::Mutex::new(BTreeMap::new()),
                sessions: Mutex::new(Vec::new()),
                cancel,
                diagnostics,
                closed: AtomicBool::new(false),
                close_lock: tokio::sync::Mutex::new(()),
                guard: Mutex::new(Some(guard)),
            }),
        })
    }
}

fn entry(name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Vec<u8> {
    let mut bytes = name.as_ref().as_bytes().to_vec();
    bytes.push(b'=');
    bytes.extend_from_slice(value.as_ref().as_bytes());
    bytes
}

/// `startup::start` returns redacted text only. The two workspace steps
/// prefix their messages; every other step (environment, home, config,
/// storage, provider, sandbox, memory) is a configuration failure.
fn startup_error(message: String) -> Error {
    const WORKSPACE_STEPS: [&str; 3] = ["resolve cwd:", "create workspace:", "startup cancelled"];
    if WORKSPACE_STEPS.iter().any(|step| message.starts_with(step)) {
        Error::Workspace(message)
    } else {
        Error::Config(message)
    }
}

// ---- the runtime ----

/// A workspace opened after startup: its builder and, when its sandbox is
/// usable, the reloader that owns its sandbox switch.
type Loaded = (Arc<Builder>, Option<Arc<SandboxReloader>>);

struct Inner {
    builder: Arc<Builder>,
    runtime: Runtime,
    workspace: PathBuf,
    sandbox: WorkspaceSandbox,
    disabled: Disabled,
    roots: Vec<PathBuf>,
    config_path: PathBuf,
    loaded: tokio::sync::Mutex<BTreeMap<PathBuf, Loaded>>,
    sessions: Mutex<Vec<Weak<Controller>>>,
    cancel: CancellationToken,
    diagnostics: Diagnostics,
    closed: AtomicBool,
    close_lock: tokio::sync::Mutex<()>,
    guard: Mutex<Option<Guard>>,
}

/// The agent runtime of this process. Cloning shares it.
#[derive(Clone)]
pub struct Otto {
    inner: Arc<Inner>,
}

/// What a session is created on: one workspace's builder, runtime and
/// sandbox reloader.
struct Target {
    builder: Arc<Builder>,
    runtime: Runtime,
    reloader: Option<Arc<SandboxReloader>>,
}

impl Otto {
    /// Starts configuring an [`Otto`].
    pub fn builder() -> OttoBuilder {
        OttoBuilder {
            workspace: None,
            config: ConfigSource::Default,
            environment: None,
            overrides: Overrides::default(),
            diagnostics: None,
            disabled: Disabled::default(),
        }
    }

    fn ensure_open(&self) -> Result<(), Error> {
        if self.inner.closed.load(Ordering::SeqCst) {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }

    fn redact(&self, message: &str) -> String {
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

    async fn workspace_target(&self, path: &Path) -> Result<(PathBuf, Option<Loaded>), Error> {
        let inner = &self.inner;
        let requested = path.to_string_lossy();
        let canonical = startup::admit_workspace(
            &requested,
            &inner.workspace,
            &inner.roots,
            &startup::trusted_projects(&inner.config_path),
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
        let (builder, reloader) = startup::load_workspace(
            Arc::clone(&inner.builder.shared),
            &canonical,
            &inner.disabled,
            &inner.cancel,
            &mut writer,
        )
        .await
        .map_err(|message| Error::Workspace(self.redact(&message)))?;
        let entry = (Arc::new(builder), reloader);
        loaded.insert(canonical.clone(), entry.clone());
        Ok((canonical, Some(entry)))
    }

    async fn target(&self, workspace: Option<&Path>) -> Result<Target, Error> {
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

    fn register(&self, controller: Controller, reloader: Option<Arc<SandboxReloader>>) -> Session {
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
        let _ = inner.builder.memory.service.close();
        drop(inner.guard.lock().unwrap_or_else(|p| p.into_inner()).take());
        match failure {
            None => Ok(()),
            Some(message) => Err(Error::Close(self.redact(&message))),
        }
    }
}

/// `close_mcp`, then the blocking `close` on the blocking pool.
async fn close_controller(controller: Arc<Controller>) -> Result<(), String> {
    controller.close_mcp().await;
    match tokio::task::spawn_blocking(move || controller.close()).await {
        Ok(result) => result,
        Err(join) => Err(join.to_string()),
    }
}

// ---- sessions ----

/// One conversation. Cloning shares it; sessions run turns concurrently with
/// each other.
#[derive(Clone)]
pub struct Session {
    id: String,
    controller: Arc<Controller>,
}

fn agent_error(error: AgentError) -> Error {
    match error {
        AgentError::Other(message) => text_error(message),
        other => Error::Agent(other),
    }
}

fn text_error(message: String) -> Error {
    match message.as_str() {
        PROMPT_ACTIVE => Error::PromptActive,
        CLOSED => Error::Closed,
        _ => Error::Agent(AgentError::Other(message)),
    }
}

impl Session {
    /// The session id [`Otto::resume_session`] takes.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Runs one turn and every approval retry it needs. `emit` receives each
    /// event in order. In MANUAL approval mode `decide` is called once per
    /// request for a command outside the sandbox grants or for an approval
    /// the model asked for; a headless host returns
    /// [`ApprovalDecision::Deny`]. In AUTO mode, the default for a new
    /// session, those requests are approved without calling `decide`; see
    /// [`Session::set_auto_mode`]. A second call on this session while a turn
    /// runs returns [`Error::PromptActive`].
    pub async fn prompt<F, Fut>(
        &self,
        input: Input<'_>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        decide: F,
    ) -> Result<Stop, Error>
    where
        F: FnMut(ApprovalRequest) -> Fut,
        Fut: Future<Output = ApprovalDecision>,
    {
        let step = match input {
            Input::Text(text) => Step::Text(text),
            Input::Image(text, block) => Step::Image(text, block),
        };
        self.controller
            .run_with_approvals(step, emit, cancel, decide)
            .await
            .map_err(agent_error)
    }

    /// Queues guidance for the running turn, delivered at its next provider
    /// boundary. Fails when no turn is running.
    pub fn steer(&self, text: &str) -> Result<SteeringMessage, Error> {
        self.controller.steer(text).map_err(text_error)
    }

    /// Compacts the transcript, keeping what `focus` names (may be empty).
    pub async fn compact(
        &self,
        focus: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
    ) -> Result<CompactionResult, Error> {
        self.controller
            .compact(focus, emit, cancel)
            .await
            .map_err(agent_error)
    }

    /// The messages of the session so far.
    pub fn history(&self) -> Vec<Message> {
        self.controller.history()
    }

    /// Sets the approval mode of this session: `true` is AUTO, which approves
    /// requests without calling `decide`; `false` is MANUAL. The mode is
    /// stored in `~/.otto/approvals.db` per workspace and session, as
    /// `/auto on|off` and ACP `_otto/approval_mode/set` store it.
    /// [`Info::auto_mode`] reports the mode in effect.
    pub fn set_auto_mode(&self, automatic: bool) -> Result<(), Error> {
        let controller = &self.controller;
        controller
            .approval_service()
            .set_auto_mode(&controller.approval_scope(), automatic)
            .map_err(|e| Error::Agent(AgentError::Other(e.to_string())))
    }

    /// Identity, model, usage, sandbox and plan of the session.
    pub fn info(&self) -> Info {
        self.controller.info()
    }

    /// Closes MCP, then the session file. Idempotent.
    pub async fn close(&self) -> Result<(), Error> {
        close_controller(Arc::clone(&self.controller))
            .await
            .map_err(Error::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::builder::TEST_PROVIDERS;
    use otto_core::{
        agent::Event,
        model::{BlockType, FinishReason, Role},
        operation::OperationControl,
        provider::{Provider, ProviderSettlement, Request, Response, StreamSink},
    };
    use serde_json::value::RawValue;
    use std::collections::VecDeque;

    /// `build()` is exclusive per process, so every test that builds an
    /// `Otto` holds this for its whole body.
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        workspace: PathBuf,
    }

    const PROFILE: &str = r#"
default_profile = "alpha"

[profiles.alpha]
provider = "openai-compatible"
base_url = "https://example.com/v1"
model = "gpt-test"
api_key_env = "ALPHA_KEY"
"#;

    /// A temp home with a user config of `PROFILE`, `extra` and
    /// `[sandbox] driver = "off"`, and an empty workspace.
    fn fixture(extra: &str) -> Fixture {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path().canonicalize().expect("canonical root");
        let home = root.join("home");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        let extra = extra.replace("{ROOT}", &root.to_string_lossy());
        std::fs::write(
            home.join(".config/otto/config.toml"),
            format!("{PROFILE}\n{extra}\n[sandbox]\ndriver = \"off\"\n"),
        )
        .expect("write config");
        Fixture {
            _directory: directory,
            root,
            home,
            workspace,
        }
    }

    fn environment(fixture: &Fixture) -> Vec<(String, String)> {
        vec![
            (
                "HOME".to_string(),
                fixture.home.to_string_lossy().into_owned(),
            ),
            ("ALPHA_KEY".to_string(), "sk-alpha".to_string()),
        ]
    }

    fn builder(fixture: &Fixture) -> OttoBuilder {
        Otto::builder()
            .workspace(&fixture.workspace)
            .environment(environment(fixture))
    }

    /// A diagnostics writer the test can read back.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// Replays `replies` in order and records every request.
    #[derive(Default)]
    struct Script {
        replies: Mutex<VecDeque<Message>>,
        requests: Mutex<Vec<Request>>,
    }

    #[async_trait::async_trait]
    impl Provider for Script {
        async fn complete(
            &self,
            request: &Request,
            _: StreamSink<'_>,
            _: &dyn OperationControl,
        ) -> ProviderSettlement {
            self.requests.lock().unwrap().push(request.clone());
            let message = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| reply("done"));
            ProviderSettlement::succeeded(Response { message }, 1)
        }
    }

    fn reply(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![Block::text(text)],
            finish_reason: Some(FinishReason::Stop),
            ..Message::default()
        }
    }

    fn tool_call(name: &str, arguments: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "call-1".into(),
                tool_name: name.into(),
                arguments: Some(RawValue::from_string(arguments.into()).unwrap()),
                ..Block::default()
            }],
            finish_reason: Some(FinishReason::ToolCalls),
            ..Message::default()
        }
    }

    /// A scripted provider that runners built under a served fixture's home
    /// use until dropped.
    struct Installed(Arc<Script>);

    impl Installed {
        fn new(replies: Vec<Message>) -> (Self, Arc<Script>) {
            let script = Arc::new(Script {
                replies: Mutex::new(replies.into()),
                ..Script::default()
            });
            (Self(script.clone()), script)
        }

        /// Makes runners built under `fixture`'s home use this script.
        fn serve(&self, fixture: &Fixture) {
            TEST_PROVIDERS
                .lock()
                .unwrap()
                .push((fixture.home.to_string_lossy().into_owned(), self.0.clone()));
        }
    }

    impl Drop for Installed {
        fn drop(&mut self) {
            TEST_PROVIDERS
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|(_, provider)| {
                    !std::ptr::addr_eq(Arc::as_ptr(provider), Arc::as_ptr(&self.0))
                });
        }
    }

    async fn deny(_: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }

    async fn say(session: &Session, text: &str) -> Stop {
        session
            .prompt(
                Input::Text(text),
                &mut |_| {},
                &CancellationToken::new(),
                deny,
            )
            .await
            .expect("prompt")
    }

    /// The tool names of a new session, with `disabled` switched off, and the
    /// entry names under the test's `~/.otto` after the runtime closed.
    async fn observe(fixture: &Fixture, disabled: &[Component]) -> (Vec<String>, Vec<String>) {
        let mut options = builder(fixture);
        for component in disabled {
            options = options.disable(*component);
        }
        let otto = options.build().await.expect("build");
        let session = otto.new_session(None).await.expect("session");
        let tools = session.controller.tool_names();
        drop(session);
        otto.close().await.expect("close");
        let mut entries: Vec<String> = std::fs::read_dir(fixture.home.join(".otto"))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        (tools, entries)
    }

    fn has(names: &[String], wanted: &[&str]) -> bool {
        wanted.iter().all(|name| names.iter().any(|n| n == name))
    }

    fn lacks(names: &[String], unwanted: &[&str]) -> bool {
        unwanted.iter().all(|name| !names.iter().any(|n| n == name))
    }

    /// Runs the switch for `component` against a control without it:
    /// `tools` appear in the control and not with the switch; `files` are
    /// created by the control and not with the switch.
    async fn switch(component: Component, extra: &str, tools: &[&str], files: &[&str]) {
        let _serial = LOCK.lock().await;
        let (control_tools, control_files) = observe(&fixture(extra), &[]).await;
        assert!(
            has(&control_tools, tools),
            "control tools {control_tools:?}"
        );
        for file in files {
            assert!(
                control_files.iter().any(|f| f == file),
                "control files {control_files:?} lack {file}"
            );
        }
        let (tools_off, files_off) = observe(&fixture(extra), &[component]).await;
        assert!(
            lacks(&tools_off, tools),
            "{component:?} tools {tools_off:?}"
        );
        for file in files {
            assert!(
                !files_off.iter().any(|f| f == file),
                "{component:?} files {files_off:?} hold {file}"
            );
        }
    }

    #[tokio::test]
    async fn bash_off_removes_the_tool_and_reports_bash_unavailable() {
        switch(Component::Bash, "", &["bash"], &[]).await;
        let _serial = LOCK.lock().await;
        let fixture = fixture("");
        let otto = builder(&fixture)
            .disable(Component::Bash)
            .build()
            .await
            .unwrap();
        let session = otto.new_session(None).await.unwrap();
        assert!(!session.info().sandbox.bash_available);
        otto.close().await.unwrap();
        let control = builder(&fixture).build().await;
        assert!(control.is_ok(), "bash stays available without the switch");
        let control = control.unwrap();
        assert!(
            control
                .new_session(None)
                .await
                .unwrap()
                .info()
                .sandbox
                .bash_available
        );
        control.close().await.unwrap();
    }

    #[tokio::test]
    async fn memory_off_removes_the_memory_tools() {
        switch(
            Component::Memory,
            "",
            &["memory_search", "remember", "forget"],
            &[],
        )
        .await;
    }

    #[tokio::test]
    async fn subagents_off_removes_the_subagent_tools() {
        switch(
            Component::Subagents,
            "",
            &["agent", "agent_wait", "agent_status", "agent_send"],
            &[],
        )
        .await;
    }

    #[tokio::test]
    async fn reflection_off_removes_the_tool_and_the_store() {
        switch(Component::Reflection, "", &["reflect"], &["reflection.db"]).await;
    }

    #[tokio::test]
    async fn schedule_off_does_not_create_the_store_and_keeps_the_tools() {
        switch(Component::Schedule, "", &[], &["schedule.db"]).await;
        let _serial = LOCK.lock().await;
        let (tools, _) = observe(&fixture(""), &[Component::Schedule]).await;
        assert!(has(
            &tools,
            &["schedule", "schedule_list", "schedule_cancel"]
        ));
    }

    #[tokio::test]
    async fn usage_off_does_not_create_the_store() {
        switch(Component::Usage, "", &[], &["usage.db"]).await;
    }

    #[tokio::test]
    async fn task_history_off_does_not_create_the_store() {
        switch(Component::TaskHistory, "", &[], &["tasks.db"]).await;
    }

    /// An MCP server that records that it was spawned, then exits.
    const MARKER_SERVER: &str = r#"
[mcp]
enabled = true

[mcp.servers.marker]
transport = "stdio"
command = "/bin/sh"
args = ["-c", "/usr/bin/touch '{ROOT}/spawned'"]
"#;

    async fn spawned(fixture: &Fixture) -> bool {
        let marker = fixture.root.join("spawned");
        for _ in 0..40 {
            if marker.exists() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        false
    }

    #[tokio::test]
    async fn mcp_off_does_not_spawn_a_configured_server() {
        let _serial = LOCK.lock().await;
        let control = fixture(MARKER_SERVER);
        let otto = builder(&control).build().await.unwrap();
        let session = otto.new_session(None).await.unwrap();
        assert!(spawned(&control).await, "the control spawns its server");
        drop(session);
        otto.close().await.unwrap();

        let off = fixture(MARKER_SERVER);
        let otto = builder(&off).disable(Component::Mcp).build().await.unwrap();
        let session = otto.new_session(None).await.unwrap();
        assert!(
            !spawned(&off).await,
            "the switch leaves the server unspawned"
        );
        drop(session);
        otto.close().await.unwrap();
    }

    /// `[mcp].enabled = true` in config does not beat the switch, in the
    /// startup workspace or in one opened later.
    #[tokio::test]
    async fn disable_wins_over_config_in_every_workspace() {
        let _serial = LOCK.lock().await;
        let extra =
            format!("{MARKER_SERVER}\n[server]\nworkspace_roots = [\"{{ROOT}}/projects\"]\n");
        let second = |fixture: &Fixture| {
            let path = fixture.root.join("projects/two");
            std::fs::create_dir_all(&path).unwrap();
            path
        };

        let control = fixture(&extra);
        let two = second(&control);
        let otto = builder(&control).build().await.unwrap();
        let session = otto.new_session(Some(&two)).await.unwrap();
        assert!(
            spawned(&control).await,
            "the second workspace spawns without the switch"
        );
        drop(session);
        otto.close().await.unwrap();

        let off = fixture(&extra);
        let two = second(&off);
        let otto = builder(&off).disable(Component::Mcp).build().await.unwrap();
        otto.new_session(None).await.unwrap();
        let session = otto.new_session(Some(&two)).await.unwrap();
        assert!(!spawned(&off).await);
        let canonical = two.canonicalize().unwrap();
        let loaded = otto.inner.loaded.lock().await;
        let (builder, _) = loaded.get(&canonical).expect("workspace was loaded");
        assert!(
            !builder.mcp.enabled,
            "the second workspace config has MCP off"
        );
        drop(loaded);
        drop(session);
        otto.close().await.unwrap();
    }

    #[tokio::test]
    async fn skills_off_removes_the_skill_catalog() {
        let _serial = LOCK.lock().await;
        let skill = |fixture: &Fixture| {
            let directory = fixture.home.join(".otto/skills/demo");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("SKILL.md"),
                "---\nname: demo\ndescription: demo skill\n---\nbody\n",
            )
            .unwrap();
        };
        let (provider, script) = Installed::new(vec![reply("one"), reply("two")]);
        for (off, expected) in [(false, true), (true, false)] {
            let fixture = fixture("");
            provider.serve(&fixture);
            skill(&fixture);
            let mut options = builder(&fixture);
            if off {
                options = options.disable(Component::Skills);
            }
            let otto = options.build().await.unwrap();
            let session = otto.new_session(None).await.unwrap();
            say(&session, "hello").await;
            let request = script.requests.lock().unwrap().pop().unwrap();
            let listed = request.system_prompt.contains("demo skill")
                || request.tools.iter().any(|tool| tool.name == "skill");
            assert_eq!(listed, expected, "skills off = {off}");
            drop(session);
            otto.close().await.unwrap();
        }
    }

    /// The plugin catalog needs an installed package, which no offline
    /// fixture creates; this pins that the switch reaches the resolved
    /// config, which is where the plugin wiring reads it.
    #[tokio::test]
    async fn plugins_off_sets_the_resolved_config_key() {
        let _serial = LOCK.lock().await;
        for (off, expected) in [(false, true), (true, false)] {
            let fixture = fixture("");
            let mut options = builder(&fixture);
            if off {
                options = options.disable(Component::Plugins);
            }
            let otto = options.build().await.unwrap();
            assert_eq!(
                otto.inner.builder.config.plugins.enabled.unwrap_or(true),
                expected
            );
            otto.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn session_persistence_off_writes_no_session_file_and_refuses_resume_and_list() {
        let _serial = LOCK.lock().await;
        let (provider, _script) = Installed::new(Vec::new());
        let count = |otto: &Otto| -> usize {
            let root = &otto.inner.builder.session_root;
            walk(root)
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
                .count()
        };
        let fixture_on = fixture("");
        provider.serve(&fixture_on);
        let otto = builder(&fixture_on).build().await.unwrap();
        let session = otto.new_session(None).await.unwrap();
        say(&session, "hello").await;
        assert_eq!(count(&otto), 1, "the control writes one session file");
        drop(session);
        otto.close().await.unwrap();

        let fixture_off = fixture("");
        provider.serve(&fixture_off);
        let otto = builder(&fixture_off)
            .disable(Component::SessionPersistence)
            .build()
            .await
            .unwrap();
        let session = otto.new_session(None).await.unwrap();
        say(&session, "hello").await;
        assert_eq!(
            session.history().len(),
            2,
            "the conversation is kept in memory"
        );
        assert_eq!(count(&otto), 0);
        assert!(matches!(
            otto.list_sessions(None, 10).await,
            Err(Error::PersistenceDisabled)
        ));
        assert!(matches!(
            otto.resume_session(&"0".repeat(32), None).await,
            Err(Error::PersistenceDisabled)
        ));
        drop(session);
        otto.close().await.unwrap();
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    found.push(path);
                }
            }
        }
        found
    }

    #[tokio::test]
    async fn only_one_otto_runs_per_process() {
        let _serial = LOCK.lock().await;
        let fixture = fixture("");
        let first = builder(&fixture).build().await.unwrap();
        assert!(matches!(
            builder(&fixture).build().await,
            Err(Error::AlreadyRunning)
        ));
        let clone = first.clone();
        first.close().await.unwrap();
        assert!(
            matches!(first.new_session(None).await, Err(Error::Closed)),
            "a closed runtime refuses new sessions"
        );
        first.close().await.expect("a second close is a no-op");
        drop(first);
        drop(clone);
        builder(&fixture)
            .build()
            .await
            .expect("a build after close succeeds")
            .close()
            .await
            .unwrap();

        let dropped = builder(&fixture).build().await.unwrap();
        drop(dropped);
        builder(&fixture)
            .build()
            .await
            .expect("a build after the last clone dropped succeeds")
            .close()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_prompt_that_needs_approval_calls_decide_and_a_denial_ends_the_turn() {
        let _serial = LOCK.lock().await;
        let (provider, script) = Installed::new(vec![
            tool_call(
                "request_approval",
                r#"{"title":"Publish","reason":"push branch","proposal":"git push"}"#,
            ),
            reply("understood"),
        ]);
        let fixture = fixture("");
        provider.serve(&fixture);
        let otto = builder(&fixture).build().await.unwrap();
        let session = otto.new_session(None).await.unwrap();
        assert!(session.info().auto_mode, "a new session starts in AUTO");
        session.set_auto_mode(false).unwrap();
        assert!(!session.info().auto_mode);
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let asked = Arc::clone(&seen);
        let stop = session
            .prompt(
                Input::Text("push it"),
                &mut move |event| sink.lock().unwrap().push(event),
                &CancellationToken::new(),
                move |request| {
                    asked.lock().unwrap().push(request);
                    async { ApprovalDecision::Deny }
                },
            )
            .await
            .expect("prompt");
        assert!(matches!(stop, Stop::EndTurn));
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "decide runs once");
            assert!(
                seen[0].command.contains("git push"),
                "{:?}",
                seen[0].command
            );

            let requests = script.requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "the denial result reaches the model");
            let result = requests[1]
                .messages
                .iter()
                .flat_map(|message| &message.blocks)
                .find(|block| block.block_type == BlockType::ToolResult)
                .expect("a tool result block");
            assert!(
                result.text.contains("\"approved\":false")
                    || result.text.contains("\"approved\": false"),
                "{}",
                result.text
            );

            let events = events.lock().unwrap();
            let position = |wanted: fn(&Event) -> bool| events.iter().position(wanted);
            let started = position(|e| matches!(e, Event::AgentStarted)).expect("AgentStarted");
            let call =
                position(|e| matches!(e, Event::ToolCallStarted { .. })).expect("call started");
            let finished =
                position(|e| matches!(e, Event::ToolCallFinished { .. })).expect("call finished");
            let done = position(|e| matches!(e, Event::AgentFinished)).expect("AgentFinished");
            assert!(started < call && call < finished && finished < done);
        }
        drop(session);
        otto.close().await.unwrap();
    }

    /// The startup warnings the CLI test pins in
    /// `startup_warnings_keep_their_order_and_text`, written to the
    /// diagnostics writer.
    #[tokio::test]
    async fn diagnostics_receive_the_startup_warnings_the_cli_writes_to_stderr() {
        let _serial = LOCK.lock().await;
        let fixture = fixture("[memory]\nenabled = false\n[mcp]\nenabled = false\n");
        for store in ["usage.db", "tasks.db", "schedule.db"] {
            std::fs::create_dir_all(fixture.home.join(".otto").join(store)).unwrap();
        }
        std::fs::create_dir_all(fixture.workspace.join(".otto")).unwrap();
        std::fs::write(
            fixture.workspace.join(".otto/config.toml"),
            "[sandbox]\ndriver = \"seatbelt\"\n",
        )
        .unwrap();
        let capture = Capture::default();
        let otto = builder(&fixture)
            .diagnostics(Box::new(capture.clone()))
            .build()
            .await
            .unwrap();
        otto.close().await.unwrap();
        let text = capture
            .text()
            .replace(&*fixture.workspace.to_string_lossy(), "<workspace>");
        assert_eq!(
            text,
            "warning: sandbox in <workspace>/.otto/config.toml: field is restricted to user or managed configuration\n\
             warning: usage store unavailable, continuing without usage history\n\
             warning: task history store unavailable, continuing without task recording\n\
             warning: schedule store unavailable, continuing without scheduled deliveries\n\
             warning: sandbox is off; bash runs unsandboxed as your user\n"
        );
    }

    /// Blocking stdin for `otto acp` that the test feeds one line at a time,
    /// because a request needs the session id the previous reply returned.
    struct Lines {
        rx: std::sync::mpsc::Receiver<Vec<u8>>,
        buffer: Vec<u8>,
        position: usize,
    }

    impl std::io::Read for Lines {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let available = std::io::BufRead::fill_buf(self)?;
            let count = available.len().min(out.len());
            out[..count].copy_from_slice(&available[..count]);
            std::io::BufRead::consume(self, count);
            Ok(count)
        }
    }

    impl std::io::BufRead for Lines {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            if self.position >= self.buffer.len() {
                match self.rx.recv() {
                    Ok(next) => {
                        self.buffer = next;
                        self.position = 0;
                    }
                    Err(_) => return Ok(&[]),
                }
            }
            Ok(&self.buffer[self.position..])
        }
        fn consume(&mut self, count: usize) {
            self.position += count;
        }
    }

    /// `otto acp --cwd <workspace>` run in-process through the CLI entry
    /// point, with the fixture's home as the whole environment.
    struct Acp {
        input: Option<std::sync::mpsc::Sender<Vec<u8>>>,
        stdout: Capture,
        stderr: Capture,
        task: tokio::task::JoinHandle<i32>,
        next: u64,
    }

    impl Acp {
        fn start(fixture: &Fixture) -> Self {
            let (input, rx) = std::sync::mpsc::channel();
            let (stdout, stderr) = (Capture::default(), Capture::default());
            let (mut out, mut err) = (stdout.clone(), stderr.clone());
            let args = vec![
                "acp".to_string(),
                "--cwd".to_string(),
                fixture.workspace.to_string_lossy().into_owned(),
            ];
            let entries: Vec<Vec<u8>> = environment(fixture)
                .iter()
                .map(|(name, value)| entry(name, value))
                .collect();
            let task = tokio::spawn(async move {
                crate::cli::run::run(
                    &args,
                    Box::new(Lines {
                        rx,
                        buffer: Vec::new(),
                        position: 0,
                    }),
                    &mut out,
                    &mut err,
                    entries,
                    false,
                    &CancellationToken::new(),
                    &crate::cli::terminate::Terminate::new(),
                )
                .await
            });
            Self {
                input: Some(input),
                stdout,
                stderr,
                task,
                next: 0,
            }
        }

        /// Sends one request and returns the `result` of its reply, or panics
        /// with the error the reply carries.
        async fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
            self.next += 1;
            let id = self.next;
            let frame =
                serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            let line = format!("{frame}\n");
            self.input
                .as_ref()
                .unwrap()
                .send(line.into_bytes())
                .unwrap();
            for _ in 0..600 {
                for line in self.stdout.text().lines() {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    if value["id"] == id && value.get("method").is_none() {
                        assert!(value.get("error").is_none(), "{method}: {value}");
                        return value["result"].clone();
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            panic!("no reply to {method}; stdout: {}", self.stdout.text());
        }

        async fn session(&mut self, fixture: &Fixture) -> String {
            self.call("initialize", serde_json::json!({"protocolVersion": 1}))
                .await;
            let reply = self
                .call(
                    "session/new",
                    serde_json::json!({"cwd": fixture.workspace, "mcpServers": []}),
                )
                .await;
            reply["sessionId"].as_str().expect("session id").to_string()
        }

        async fn prompt(&mut self, session: &str, text: &str) {
            self.call(
                "session/prompt",
                serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
            )
            .await;
        }

        /// Ends stdin and returns the exit code and stderr.
        async fn finish(mut self) -> (i32, String) {
            self.input = None;
            let code = self.task.await.expect("acp task");
            (code, self.stderr.text())
        }
    }

    #[tokio::test]
    async fn sessions_made_by_otto_and_by_the_cli_open_in_each_other() {
        let _serial = LOCK.lock().await;
        let fixture = fixture("");

        let (provider, _script) = Installed::new(Vec::new());
        provider.serve(&fixture);
        let otto = builder(&fixture).build().await.unwrap();
        let session = otto.new_session(None).await.unwrap();
        say(&session, "hello from otto").await;
        let from_otto = session.id().to_string();
        drop(session);
        otto.close().await.unwrap();

        let mut acp = Acp::start(&fixture);
        let from_cli = acp.session(&fixture).await;
        acp.prompt(&from_cli, "hello from the cli").await;
        let listed = acp
            .call(
                "session/list",
                serde_json::json!({"cwd": fixture.workspace}),
            )
            .await
            .to_string();
        assert!(
            listed.contains(&from_otto),
            "the CLI lists the Otto session: {listed}"
        );
        acp.call(
            "session/load",
            serde_json::json!({"sessionId": from_otto, "cwd": fixture.workspace, "mcpServers": []}),
        )
        .await;
        let (code, stderr) = acp.finish().await;
        assert_eq!(code, 0, "{stderr}");

        let otto = builder(&fixture).build().await.unwrap();
        let summaries = otto.list_sessions(None, 10).await.unwrap();
        for id in [&from_otto, &from_cli] {
            assert!(summaries.iter().any(|s| &s.id == id), "Otto lists {id}");
        }
        let resumed = otto.resume_session(&from_cli, None).await.unwrap();
        assert!(
            format!("{:?}", resumed.history()).contains("hello from the cli"),
            "Otto resumes the CLI session with its messages"
        );
        drop(resumed);
        otto.close().await.unwrap();
    }

    /// The files under `~/.otto` and the session root of a fixture, as
    /// relative paths with the session id replaced.
    fn tree(fixture: &Fixture, session: &str) -> Vec<String> {
        let mut names: Vec<String> = walk(&fixture.home)
            .iter()
            .filter(|path| {
                let name = path.to_string_lossy();
                !name.ends_with("-wal") && !name.ends_with("-shm") && !name.contains("/memory/")
            })
            .map(|path| {
                let relative = path.strip_prefix(&fixture.home).unwrap();
                let mut parts: Vec<String> = relative
                    .components()
                    .map(|part| {
                        part.as_os_str()
                            .to_string_lossy()
                            .replace(session, "<session>")
                    })
                    .collect();
                // `.otto/sessions/<hash of the workspace path>/<id>.jsonl`
                if parts.len() == 4 && parts[1] == "sessions" {
                    parts[2] = "<workspace>".to_string();
                }
                parts.join("/")
            })
            .collect();
        names.sort();
        names
    }

    fn normalize(text: &str, fixture: &Fixture, session: &str) -> String {
        text.replace(&*fixture.root.to_string_lossy(), "<root>")
            .replace(session, "<session>")
    }

    /// For one config, environment and prompt, a default `Otto` and
    /// `otto acp` send the provider the same request (model, system prompt
    /// and tool definitions), write the same diagnostics, and leave the same
    /// files under the home. A behavior of one composition that the provider
    /// request, the diagnostics and the file tree do not reflect is not
    /// compared.
    #[tokio::test]
    async fn a_default_otto_matches_otto_acp() {
        let _serial = LOCK.lock().await;
        let (provider, script) = Installed::new(vec![reply("one"), reply("two")]);

        let cli = fixture("");
        provider.serve(&cli);
        let mut acp = Acp::start(&cli);
        let cli_session = acp.session(&cli).await;
        acp.prompt(&cli_session, "parity").await;
        let (code, cli_stderr) = acp.finish().await;
        assert_eq!(code, 0, "{cli_stderr}");

        let embedded = fixture("");
        provider.serve(&embedded);
        let capture = Capture::default();
        let otto = builder(&embedded)
            .diagnostics(Box::new(capture.clone()))
            .build()
            .await
            .unwrap();
        let session = otto.new_session(None).await.unwrap();
        say(&session, "parity").await;
        let otto_session = session.id().to_string();
        drop(session);
        otto.close().await.unwrap();
        drop(provider);

        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let (from_cli, from_otto) = (&requests[0], &requests[1]);
        assert!(
            from_cli.tools.len() > 20,
            "the request carries the tool set"
        );
        assert_eq!(from_cli.tools, from_otto.tools, "tool definitions");
        assert_eq!(from_cli.model, from_otto.model);
        assert_eq!(from_cli.thinking, from_otto.thinking);
        assert_eq!(
            normalize(&from_cli.system_prompt, &cli, &cli_session),
            normalize(&from_otto.system_prompt, &embedded, &otto_session),
            "system prompt"
        );
        assert_eq!(
            normalize(&cli_stderr, &cli, &cli_session),
            normalize(&capture.text(), &embedded, &otto_session),
            "diagnostics"
        );
        assert_eq!(
            tree(&cli, &cli_session),
            tree(&embedded, &otto_session),
            "files under the home"
        );
    }
}
