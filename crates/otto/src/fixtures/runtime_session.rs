//! Starts the startup composition of `runtime::startup` for one workspace and
//! drives its sessions from a test: the memory integration tests use it to run
//! prompts through the same builder, sandbox switch and close order that
//! `otto acp` uses.
//!
//! One [`Runtime`] holds the config file lock of its home directory until
//! [`Runtime::close`]; tests that start a runtime serialize themselves.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use otto_core::agent::EventSink;
use otto_core::config::resolve::Runtime as ResolvedRuntime;
use otto_core::memory::{MemoryContext, Origin};
use tokio_util::sync::CancellationToken;

use crate::app::Controller;
use crate::app::approval::{ApprovalDecision, ApprovalRequest, Step, Stop};
use crate::memory::client::Client;
use crate::runtime::builder::{Builder, resolve_initial_runtime};
use crate::runtime::sandbox_switch::SandboxReloader;
use crate::runtime::startup::{self, StartOptions, WorkspaceSandbox};
use crate::runtime::wiring::MemoryExtension;
use crate::sandbox::control::SandboxControl;

/// What [`Runtime::start`] builds the runtime from.
pub struct Options {
    /// The startup workspace.
    pub workspace: PathBuf,
    /// The environment snapshot; `HOME` selects the config file.
    pub environment: Vec<(String, String)>,
    /// A memory service the test supplies instead of the configured one.
    pub memory_extension: Option<MemoryExtension>,
}

/// A workspace opened after startup, with the reloader of its sandbox.
type Loaded = (Arc<Builder>, Option<Arc<SandboxReloader>>);

/// The runtime `startup::start` composed, plus the workspaces and sessions
/// opened on it.
pub struct Runtime {
    builder: Arc<Builder>,
    resolved: ResolvedRuntime,
    workspace: PathBuf,
    sandbox: WorkspaceSandbox,
    loaded: tokio::sync::Mutex<BTreeMap<PathBuf, Loaded>>,
    sessions: Mutex<Vec<Weak<Controller>>>,
    cancel: CancellationToken,
    closed: tokio::sync::Mutex<bool>,
}

/// One conversation of a [`Runtime`].
pub struct Session {
    controller: Arc<Controller>,
}

fn entry(name: &str, value: &str) -> Vec<u8> {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(b'=');
    bytes.extend_from_slice(value.as_bytes());
    bytes
}

impl Runtime {
    /// Runs `startup::start` for `options.workspace` with the default config
    /// path. The error text is the redacted message `startup::start` returns.
    pub async fn start(options: Options) -> Result<Self, String> {
        let cancel = CancellationToken::new();
        let host_entries = options
            .environment
            .iter()
            .map(|(name, value)| entry(name, value))
            .collect();
        let started = startup::start(
            StartOptions {
                memory_extension: options.memory_extension,
                host_entries,
                config_path: String::new(),
                explicit_config: false,
                cwd: options.workspace.to_string_lossy().into_owned(),
                overrides: Default::default(),
                logger: Arc::new(crate::log::Logger::stderr()),
            },
            &cancel,
            &mut std::io::stderr(),
        )
        .await?;
        let startup::Started {
            builder,
            runtime,
            workspace_path,
            sandbox,
        } = started;
        Ok(Self {
            builder: Arc::new(builder),
            resolved: runtime,
            workspace: PathBuf::from(workspace_path),
            sandbox,
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
            sessions: Mutex::new(Vec::new()),
            cancel,
            closed: tokio::sync::Mutex::new(false),
        })
    }

    fn redact(&self, message: &str) -> String {
        self.builder.redact_error(message, Some(&self.resolved))
    }

    /// Creates a session in `workspace`, or in the startup workspace for
    /// `None`. A directory other than the startup workspace is loaded once
    /// with `startup::load_workspace`; it is not checked against
    /// `[server].workspace_roots`.
    pub async fn new_session(&self, workspace: Option<&Path>) -> Result<Session, String> {
        let (builder, resolved, reloader) = match workspace {
            None => (
                Arc::clone(&self.builder),
                self.resolved.clone(),
                self.sandbox.reloader.clone(),
            ),
            Some(path) => {
                let canonical = path.canonicalize().map_err(|error| error.to_string())?;
                if canonical == self.workspace {
                    (
                        Arc::clone(&self.builder),
                        self.resolved.clone(),
                        self.sandbox.reloader.clone(),
                    )
                } else {
                    let (builder, reloader) = self.load(canonical).await?;
                    let resolved = resolve_initial_runtime(
                        &builder.config,
                        &builder.environment,
                        None,
                        &builder.overrides,
                    )
                    .map_err(|error| self.redact(&error.to_string()))?;
                    (builder, resolved, reloader)
                }
            }
        };
        let controller = Controller::create(builder, &resolved)
            .await
            .map_err(|message| self.redact(&message))?;
        let controller = match reloader {
            Some(control) => controller.with_sandbox_control(control as Arc<dyn SandboxControl>),
            None => controller,
        };
        let controller = Arc::new(controller);
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        sessions.retain(|session| session.strong_count() > 0);
        sessions.push(Arc::downgrade(&controller));
        Ok(Session { controller })
    }

    async fn load(&self, canonical: PathBuf) -> Result<Loaded, String> {
        let mut loaded = self.loaded.lock().await;
        if let Some(found) = loaded.get(&canonical) {
            return Ok(found.clone());
        }
        let (builder, sandbox) = startup::load_workspace(
            Arc::clone(&self.builder.shared),
            &canonical,
            &self.cancel,
            &mut std::io::stderr(),
        )
        .await
        .map_err(|message| self.redact(&message))?;
        let entry = (Arc::new(builder), sandbox.reloader);
        loaded.insert(canonical, entry.clone());
        Ok(entry)
    }

    /// Closes every open session, the sandbox of each additional workspace,
    /// the startup sandbox, the approval executor and the memory service, in
    /// that order. Every step runs; the first failure is returned. A second
    /// call returns `Ok(())`.
    pub async fn close(&self) -> Result<(), String> {
        let mut closed = self.closed.lock().await;
        if std::mem::replace(&mut *closed, true) {
            return Ok(());
        }
        self.cancel.cancel();
        let mut failure: Option<String> = None;
        let controllers: Vec<Arc<Controller>> =
            std::mem::take(&mut *self.sessions.lock().unwrap_or_else(|p| p.into_inner()))
                .iter()
                .filter_map(Weak::upgrade)
                .collect();
        for controller in controllers {
            if let Err(message) = close_controller(controller).await {
                failure.get_or_insert(message);
            }
        }
        let additional: Vec<Loaded> = self.loaded.lock().await.values().cloned().collect();
        for (_, reloader) in additional {
            if let Some(reloader) = reloader
                && reloader.control.close().await.is_err()
            {
                failure.get_or_insert("sandbox runtime close failed".to_string());
            }
        }
        if self.sandbox.control.close().await.is_err() {
            failure.get_or_insert("sandbox runtime close failed".to_string());
        }
        if let Some(executor) = &self.sandbox.approval_executor
            && executor.close().is_err()
        {
            failure.get_or_insert("sandbox runtime close failed".to_string());
        }
        if let Some(client) = &self.builder.memory.client
            && let Err(error) = client.close(&CancellationToken::new()).await
        {
            failure.get_or_insert_with(|| format!("close memory: {error}"));
        }
        let _ = self.builder.memory.service.close();
        match failure {
            None => Ok(()),
            Some(message) => Err(format!("close: {}", self.redact(&message))),
        }
    }
}

/// `close_resources`, then the blocking `close` on the blocking pool.
async fn close_controller(controller: Arc<Controller>) -> Result<(), String> {
    controller.close_resources().await;
    match tokio::task::spawn_blocking(move || controller.close()).await {
        Ok(result) => result,
        Err(join) => Err(join.to_string()),
    }
}

impl Session {
    /// Runs one turn of `text` and every approval retry it needs; `decide`
    /// answers each approval request.
    pub async fn prompt<F, Fut>(
        &self,
        text: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        decide: F,
    ) -> Result<Stop, String>
    where
        F: FnMut(ApprovalRequest) -> Fut,
        Fut: Future<Output = ApprovalDecision>,
    {
        self.controller
            .run_with_approvals(Step::Text(text), emit, cancel, decide)
            .await
            .map_err(|error| error.to_string())
    }

    /// The memory client the session recalls through; `None` when memory is
    /// disabled.
    pub fn memory_service(&self) -> Option<Arc<Client>> {
        self.controller.memory_service()
    }

    /// The memory context the session sends with a request of `origin`.
    pub fn memory_context(&self, origin: Origin) -> MemoryContext {
        self.controller.memory_context(origin)
    }

    /// Closes MCP, then the session file. Idempotent.
    pub async fn close(&self) -> Result<(), String> {
        close_controller(Arc::clone(&self.controller)).await
    }
}
