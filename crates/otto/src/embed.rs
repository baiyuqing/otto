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
    /// `schedule.db` is not opened and there is no schedule tool.
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
        self.0.lock().unwrap_or_else(|p| p.into_inner()).write(bytes)
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
        let roots = otto_core::config::resolve_server(&builder.config, &builder.environment, "", "")
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
        self.inner.builder.redact_error(message, Some(&self.inner.runtime))
    }

    /// Loads another workspace and returns its canonical path, as `POST
    /// /v1/workspaces` of `otto serve` does. The path must be absolute and
    /// be the startup workspace, lie under a `[server].workspace_roots`
    /// entry or lie under a directory trusted with `otto trust`, after
    /// symlinks are resolved. A workspace loads once; its config is
    /// resolved with the component switches applied.
    pub async fn open_workspace(&self, path: impl AsRef<Path>) -> Result<PathBuf, Error> {
        self.ensure_open()?;
        self.workspace_target(path.as_ref()).await.map(|(path, _)| path)
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
        let mut sessions = self.inner.sessions.lock().unwrap_or_else(|p| p.into_inner());
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
        let controllers: Vec<Arc<Controller>> = std::mem::take(
            &mut *inner.sessions.lock().unwrap_or_else(|p| p.into_inner()),
        )
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
    /// event in order. `decide` is called once per request for a command
    /// outside the sandbox grants or for an approval the model asked for;
    /// a headless host returns [`ApprovalDecision::Deny`]. A second call on
    /// this session while a turn runs returns [`Error::PromptActive`].
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
