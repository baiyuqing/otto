//! `otto serve`: the HTTP composition root.
//!
//! Everything the REPL path builds is already in place when [`run`] is called;
//! this module only picks the listener and wires one [`Factory`] over the
//! shared [`Builder`]. The process sandbox arrives already behind the
//! [`SandboxSwitch`](super::sandbox_switch::SandboxSwitch) that `POST
//! /v1/sandbox/reload` replaces.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::{Arc, Weak};

use async_trait::async_trait;

use chrono::Utc;
use otto_core::config::ServerRuntime;
use otto_core::config::resolve::Runtime;
use otto_core::model::{Block, Message, Role};
use otto_core::session::{ListResult, Session};
use tokio_util::sync::CancellationToken;

use crate::app::{Controller, SandboxControl};
use crate::server::listen::{Listener, listen_tcp, listen_unix};
use crate::server::{self, Factory, Info, Options, SESSION_NOT_FOUND, Server};
use crate::session::{self as sessionfs, MAX_LIST_SESSIONS};

use super::sandbox_switch::{SandboxReloader, SandboxSwitch};
use crate::runtime::builder::Builder;
use crate::runtime::info::SandboxInfo;
use crate::runtime::sandbox_runtime::canonical_directory;
use crate::runtime::startup::{Disabled, load_workspace};
use crate::runtime::workspace_admission::{
    Admission, admit_workspace, canonicalize_workspace_roots, trusted_projects,
};

/// Writes `otto: {message}\n` and returns exit code 1.
fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}

// ---- the workspace registry ----

/// One loaded workspace: its composition root, its own `/sandbox reload`
/// state, and, when the workflow lock was acquired, its workflow controller.
struct DeliverySetupCapability {
    executor: Arc<crate::sandbox::Executor>,
    environment: Vec<String>,
}

impl DeliverySetupCapability {
    fn worktree_executor(&self) -> crate::delivery::WorktreeExecutor {
        crate::delivery::WorktreeExecutor::new(self.executor.clone(), self.environment.clone())
    }
}

struct WorkspaceHost {
    builder: Arc<Builder>,
    workflows: Option<Arc<crate::workflow::Controller>>,
    /// The elevated executor plus its matching environment, retained solely
    /// for constrained delivery worktree setup. It is intentionally private:
    /// ordinary server routes and source runners cannot obtain either a target
    /// executor or the environment needed to run it.
    delivery_setup: Option<DeliverySetupCapability>,
    /// This workspace's own sandbox reloader (`None` when it never got a
    /// usable sandbox). `POST /v1/sandbox/reload` reloads the startup
    /// workspace through `ServeFactory::reload_sandbox` and every other
    /// loaded workspace through `reload_other_sandboxes`.
    reloader: Option<Arc<SandboxReloader>>,
}

/// The workspaces this server process has loaded, keyed by canonical path.
///
/// The startup workspace is loaded before this struct exists and never
/// leaves `loaded`, so it is cached separately in `startup_host`: every
/// existing `Factory` method that reads it (`builder`, `usage_summary`,
/// ...) stays synchronous rather than taking the async registry lock.
/// `loaded` is a `tokio::sync::Mutex` (safe to hold across `.await`, unlike
/// `std::sync::Mutex`) so [`Workspaces::load`] can build a new workspace's
/// sandbox and workflow controller while holding it, making two concurrent
/// loads of one path build exactly one host.
struct Workspaces {
    startup: String,
    startup_host: Arc<WorkspaceHost>,
    /// Canonical `[server].workspace_roots`.
    roots: Vec<PathBuf>,
    /// Where `otto trust` writes `[projects]`, re-read on every [`admit`]
    /// call so a directory trusted after startup is admitted without a
    /// restart.
    ///
    /// [`admit`]: Self::admit
    config_path: PathBuf,
    loaded: tokio::sync::Mutex<BTreeMap<String, Arc<WorkspaceHost>>>,
}

impl Workspaces {
    fn startup_host(&self) -> &Arc<WorkspaceHost> {
        &self.startup_host
    }

    /// Whether `requested` may be opened: the startup workspace, a
    /// descendant of one of `self.roots`, or a trusted directory (or its
    /// descendant) from the config file's current `[projects]` tables.
    fn admit(&self, requested: &str) -> Result<PathBuf, Admission> {
        let trusted = trusted_projects(&self.config_path);
        admit_workspace(requested, Path::new(&self.startup), &self.roots, &trusted)
    }

    /// The host loaded for `path`, if any.
    async fn host(&self, path: &str) -> Option<Arc<WorkspaceHost>> {
        if path == self.startup {
            return Some(Arc::clone(&self.startup_host));
        }
        self.loaded.lock().await.get(path).cloned()
    }

    /// Every loaded workspace, startup first then by path.
    async fn list(&self) -> Vec<(String, Arc<WorkspaceHost>)> {
        let loaded = self.loaded.lock().await;
        let mut rest: Vec<(String, Arc<WorkspaceHost>)> = loaded
            .iter()
            .filter(|(path, _)| **path != self.startup)
            .map(|(path, host)| (path.clone(), Arc::clone(host)))
            .collect();
        rest.sort_by(|a, b| a.0.cmp(&b.0));
        let mut all = vec![(self.startup.clone(), Arc::clone(&self.startup_host))];
        all.append(&mut rest);
        all
    }

    /// Loads `canonical`, or returns the host already loaded for it.
    /// `(_, true)` when this call built a new host; the registry mutex is
    /// held across the build, so a second concurrent call for the same path
    /// waits for the first and then sees `(_, false)`.
    ///
    /// A failed load (`load_workspace`'s sandbox/MCP errors) returns without
    /// touching `loaded`, matching the spec's "leaves nothing in the
    /// registry". A failed workflow-controller lock is not this kind of
    /// failure: it mirrors startup's own handling, where the workspace loads
    /// with `workflows: None` and a stderr warning.
    ///
    /// `persist` records a newly-built host's path into the persisted
    /// workspace list file, still under `loaded`'s lock so two concurrent
    /// calls for different paths cannot race each other's read-modify-write.
    /// Startup's own reload of that same file passes `false`: those paths
    /// are already in it.
    async fn load(
        &self,
        canonical: &Path,
        shared: &Arc<crate::runtime::builder::Shared>,
        runtime: &Runtime,
        cancel: &CancellationToken,
        stderr: &mut (dyn Write + Send),
        persist: bool,
    ) -> Result<(Arc<WorkspaceHost>, bool), String> {
        let path = canonical.to_string_lossy().into_owned();
        let mut loaded = self.loaded.lock().await;
        if let Some(host) = loaded.get(&path) {
            return Ok((Arc::clone(host), false));
        }
        let (builder, sandbox) = load_workspace(
            Arc::clone(shared),
            canonical,
            &Disabled::default(),
            cancel,
            stderr,
        )
        .await?;
        let builder = Arc::new(builder);
        let workspace_runtime = crate::runtime::builder::resolve_initial_runtime(
            &builder.config,
            &builder.environment,
            None,
            &builder.overrides,
        )
        .map_err(|error| builder.redact_error(&error.to_string(), Some(runtime)))?;
        let workflows = match super::workflow::build_controller(
            Arc::clone(&builder),
            &workspace_runtime,
            stderr,
        )
        .await
        {
            Ok(controller) => Some(controller),
            Err(error) => {
                let redacted = builder.redact_error(&error, Some(runtime));
                if !redacted.is_empty() {
                    let _ = writeln!(stderr, "warning: workflows disabled for {path}: {redacted}");
                }
                None
            }
        };
        let host = Arc::new(WorkspaceHost {
            builder,
            workflows,
            delivery_setup: match (sandbox.approval_executor, sandbox.approval_environment) {
                (Some(executor), Some(environment)) => Some(DeliverySetupCapability {
                    executor,
                    environment,
                }),
                _ => None,
            },
            reloader: sandbox.reloader,
        });
        loaded.insert(path.clone(), Arc::clone(&host));
        if persist && let Err(error) = add_to_workspace_list(&shared.home, &path) {
            let _ = writeln!(stderr, "warning: cannot save workspace list: {error}");
        }
        Ok((host, true))
    }

    /// Unloads `path` per the spec's four-step procedure, holding `loaded`'s
    /// lock across all of it (a concurrent load or remove of the same path is
    /// serialized). The startup workspace is never removable. `home` locates
    /// the persisted list; a failure to update it is a stderr warning, not an
    /// error, matching [`load`](Self::load)'s own handling.
    async fn remove(
        &self,
        path: &str,
        home: &str,
        stderr: &mut (dyn Write + Send),
    ) -> Result<(), server::WorkspaceRemoveError> {
        if path == self.startup {
            return Err(server::WorkspaceRemoveError::IsStartup);
        }
        let mut loaded = self.loaded.lock().await;
        let host = loaded.get(path).cloned();
        if let Some(host) = &host
            && let Some(controller) = &host.workflows
            && controller.active_runs() > 0
        {
            return Err(server::WorkspaceRemoveError::InUse(
                "an active workflow run".to_string(),
            ));
        }
        if host.is_none() {
            let in_list = read_workspace_list(home)
                .unwrap_or_default()
                .iter()
                .any(|entry| entry == path);
            if !in_list {
                return Err(server::WorkspaceRemoveError::NotFound);
            }
        }
        loaded.remove(path);
        if let Some(host) = host
            && let Some(controller) = &host.workflows
        {
            controller.close().await;
        }
        if let Err(error) = remove_from_workspace_list(home, path) {
            let _ = writeln!(stderr, "warning: cannot update workspace list: {error}");
        }
        Ok(())
    }
}

// ---- admission ----

/// The canonical form of `path`, or `path` itself when it no longer resolves
/// (its directory was deleted since it was added). Unlike [`admit_workspace`],
/// `DELETE /v1/workspaces` must still match a persisted entry whose directory
/// is gone, so a literal-string fallback replaces admission here.
fn canonical_or_literal(path: &str) -> String {
    canonical_directory(Path::new(path))
        .map(|canonical| canonical.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

// ---- persisted workspace list ----

/// `~/.otto/serve-workspaces.json`: every workspace ever added via `POST
/// /v1/workspaces`, so a restarted `otto serve` reloads them. One file per
/// user, shared by every `otto serve` process.
fn workspace_list_path(home: &str) -> PathBuf {
    Path::new(home).join(".otto/serve-workspaces.json")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct WorkspaceListFile {
    workspaces: Vec<String>,
}

/// Reads the persisted workspace list. A missing file reads as an empty
/// list; an unparsable file is `Err` so the caller can warn without touching
/// it, leaving it for the next successful [`add_to_workspace_list`] to
/// overwrite.
fn read_workspace_list(home: &str) -> Result<Vec<String>, String> {
    let bytes = match std::fs::read(workspace_list_path(home)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    serde_json::from_slice::<WorkspaceListFile>(&bytes)
        .map(|file| file.workspaces)
        .map_err(|error| error.to_string())
}

/// Writes `workspaces` through a temporary file in `~/.otto` followed by
/// `rename`, so a reader never observes a partial file. Shared by
/// [`add_to_workspace_list`] and [`remove_from_workspace_list`].
fn write_workspace_list(home: &str, workspaces: Vec<String>) -> std::io::Result<()> {
    let directory = Path::new(home).join(".otto");
    std::fs::create_dir_all(&directory)?;
    let json = serde_json::to_vec(&WorkspaceListFile { workspaces })
        .expect("a list of strings always serializes");
    let suffix = crate::runtime::builder::random_id()?;
    let temp = directory.join(format!(".serve-workspaces-{suffix}.json"));
    std::fs::write(&temp, &json)?;
    std::fs::rename(&temp, workspace_list_path(home))
}

/// Adds `path` to the persisted workspace list (sorted, deduplicated). A
/// currently unparsable file is treated as an empty list rather than
/// preserved, matching the spec's "the next successful add ... rewrites it
/// from the empty list plus the new path".
fn add_to_workspace_list(home: &str, path: &str) -> std::io::Result<()> {
    let mut workspaces = read_workspace_list(home).unwrap_or_default();
    if !workspaces.iter().any(|entry| entry == path) {
        workspaces.push(path.to_string());
        workspaces.sort();
    }
    write_workspace_list(home, workspaces)
}

/// Removes `path` from the persisted workspace list, preserving the order of
/// the remaining entries. A path not in the list is a no-op.
fn remove_from_workspace_list(home: &str, path: &str) -> std::io::Result<()> {
    let mut workspaces = read_workspace_list(home).unwrap_or_default();
    workspaces.retain(|entry| entry != path);
    write_workspace_list(home, workspaces)
}

/// Loads every path in the persisted workspace list into `workspaces`, after
/// the startup workspace is already loaded. A path that fails admission or
/// fails to load is skipped with a stderr warning naming the path and the
/// reason; it stays in the file for the next start to retry. Called once at
/// startup, so it never persists what it loads back to the file (those paths
/// are already in it).
async fn load_persisted_workspaces(
    workspaces: &Workspaces,
    shared: &Arc<crate::runtime::builder::Shared>,
    runtime: &Runtime,
    cancel: &CancellationToken,
    stderr: &mut (dyn Write + Send),
) {
    let paths = match read_workspace_list(&shared.home) {
        Ok(paths) => paths,
        Err(error) => {
            let _ = writeln!(stderr, "warning: cannot read workspace list: {error}");
            Vec::new()
        }
    };
    for path in paths {
        let canonical = match workspaces.admit(&path) {
            Ok(canonical) => canonical,
            Err(Admission::Invalid) => {
                let _ = writeln!(
                    stderr,
                    "warning: cannot load workspace {path}: not an existing directory"
                );
                continue;
            }
            Err(Admission::NotAdmitted) => {
                let _ = writeln!(
                    stderr,
                    "warning: cannot load workspace {path}: outside the startup workspace and configured roots"
                );
                continue;
            }
        };
        if let Err(error) = workspaces
            .load(&canonical, shared, runtime, cancel, stderr, false)
            .await
        {
            let _ = writeln!(stderr, "warning: cannot load workspace {path}: {error}");
        }
    }
}

/// Serve-owned delivery preflight broker. It retains only weak process
/// references: installing it into a parent-tool slot cannot retain a cycle
/// through loaded hosts or builders. It returns immutable previews only, never
/// a target workspace handle or generic command executor.
///
/// The workspace registry is attached after construction. The parent-tool slot
/// must be installed before the workflow controller is built, while
/// `Workspaces` owns that controller through its host, so the weak registry
/// reference cannot be a constructor argument.
#[derive(Clone)]
struct DeliveryBroker {
    workspaces: Arc<std::sync::RwLock<Option<Weak<Workspaces>>>>,
    shared: Weak<crate::runtime::builder::Shared>,
    runtime: Runtime,
    cancel: CancellationToken,
}

impl DeliveryBroker {
    fn new(
        shared: &Arc<crate::runtime::builder::Shared>,
        runtime: Runtime,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            workspaces: Arc::new(std::sync::RwLock::new(None)),
            shared: Arc::downgrade(shared),
            runtime,
            cancel,
        }
    }

    /// Publishes the workspace registry once it exists. This happens before
    /// the server accepts a request, so no delivery can observe the gap.
    fn attach_workspaces(&self, workspaces: &Arc<Workspaces>) {
        *self
            .workspaces
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(Arc::downgrade(workspaces));
    }

    fn workspaces(&self) -> Result<Arc<Workspaces>, String> {
        self.workspaces
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| "delivery broker is no longer available".to_string())
    }

    async fn target_host(
        &self,
        requested: &str,
        cancel: &CancellationToken,
    ) -> Result<
        (
            PathBuf,
            crate::delivery::AdmissionPolicy,
            Arc<WorkspaceHost>,
        ),
        String,
    > {
        if cancel.is_cancelled() || self.cancel.is_cancelled() {
            return Err("delivery target preflight cancelled".into());
        }
        let workspaces = self.workspaces()?;
        let shared = self
            .shared
            .upgrade()
            .ok_or("delivery broker is no longer available")?;
        let admitted = workspaces
            .admit(requested)
            .map_err(|_| "delivery target is not an admitted workspace")?;
        let policy = crate::delivery::AdmissionPolicy {
            startup: PathBuf::from(&workspaces.startup),
            roots: workspaces.roots.clone(),
            trusted: trusted_projects(&workspaces.config_path),
        };
        let host = match workspaces.host(&admitted.to_string_lossy()).await {
            Some(host) => host,
            None => {
                let mut stderr = std::io::sink();
                workspaces
                    .load(
                        &admitted,
                        &shared,
                        &self.runtime,
                        cancel,
                        &mut stderr,
                        false,
                    )
                    .await?
                    .0
            }
        };
        Ok((admitted, policy, host))
    }
}

#[async_trait]
impl crate::delivery::Preflight for DeliveryBroker {
    async fn preview(
        &self,
        source_workspace: &str,
        request: &crate::delivery::Request,
        cancel: &CancellationToken,
    ) -> Result<crate::delivery::Preview, String> {
        let (admitted, policy, host) = self.target_host(&request.target, cancel).await?;
        let executor = host
            .builder
            .command_executor
            .as_ref()
            .cloned()
            .ok_or("delivery target sandbox is unavailable")?;
        let environment = host.builder.sandbox_environment.clone().unwrap_or_default();
        crate::delivery::Preflight::preview(
            &crate::delivery::SandboxPreflight::new(
                policy,
                crate::delivery::SandboxTargetInspector::new(executor, environment),
            ),
            source_workspace,
            &crate::delivery::Request {
                target: admitted.to_string_lossy().into_owned(),
                objective: request.objective.clone(),
                branch: request.branch.clone(),
                source_refs: request.source_refs.clone(),
            },
            cancel,
        )
        .await
    }
}

/// The terminal target-turn boundary, reached only after the constrained
/// worktree effect and worktree-root builder composition have completed.
/// It cannot run setup Git or change roots.
#[async_trait]
trait DeliveryRun: Send {
    fn target_session(&self) -> String;
    fn worktree(&self) -> String;
    async fn run_frozen(&self, prompt: &str, cancel: &CancellationToken) -> Result<String, String>;
    async fn close(self: Box<Self>);
}

/// Server-owned resources for one started delivery. None of these values cross
/// the delivery service boundary into a source session/tool.
struct DeliveryRuntime {
    _primary_host: Arc<WorkspaceHost>,
    worktree_sandbox: crate::runtime::startup::WorkspaceSandbox,
    _worktree_builder: Arc<Builder>,
    worktree_session: crate::runtime::builder::SharedSession,
    worktree_runner: Arc<crate::runtime::builder::Runner>,
}

#[async_trait]
impl DeliveryRun for DeliveryRuntime {
    fn target_session(&self) -> String {
        self.worktree_session.header().id
    }

    fn worktree(&self) -> String {
        self.worktree_session.header().workspace
    }

    async fn run_frozen(&self, prompt: &str, cancel: &CancellationToken) -> Result<String, String> {
        let mut emit = |_| {};
        self.worktree_runner
            .run(prompt, &mut emit, cancel)
            .await
            .map_err(|error| error.to_string())?;
        Ok(self
            .worktree_session
            .messages()
            .into_iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(|message| message.text())
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| "target delivery runner completed".into()))
    }

    async fn close(self: Box<Self>) {
        self.worktree_runner.close_mcp().await;
        self.worktree_runner.close();
        let _ = self.worktree_session.close();
        let _ = self.worktree_sandbox.control.close().await;
    }
}

struct PreparedDelivery {
    primary_host: Arc<WorkspaceHost>,
    executor: crate::delivery::WorktreeExecutor,
}

/// Receipt-owned delivery effect service, private to `otto serve`. Its only
/// mutable Git operation is `WorktreeExecutor::add`; every target runner is
/// built after that operation around the newly-created worktree root.
struct DeliveryService {
    broker: DeliveryBroker,
    receipts: Arc<crate::delivery::ReceiptStore>,
    /// Append-only source transcripts keyed by approval id. These are never
    /// passed to the target runner and contain no source filesystem handle.
    source_sessions: Mutex<HashMap<String, crate::runtime::builder::SharedSession>>,
    prepared: Mutex<HashMap<String, PreparedDelivery>>,
    runtimes: Mutex<HashMap<String, Box<dyn DeliveryRun>>>,
    /// Test-only injection at the post-setup worktree-runtime boundary. It
    /// is absent from production, so it cannot bypass constrained setup.
    #[cfg(test)]
    ready_runs: Mutex<HashMap<String, Box<dyn DeliveryRun>>>,
}

impl DeliveryService {
    fn new(
        broker: DeliveryBroker,
        receipts: Arc<crate::delivery::ReceiptStore>,
    ) -> Result<Self, String> {
        // A process crash after `begin` has unknown external Git outcome.
        // Never infer success or replay it on construction.
        receipts.mark_interrupted_unknown()?;
        // A target run still `running` here was interrupted with the process.
        receipts.mark_running_tasks_unknown()?;
        Ok(Self {
            broker,
            receipts,
            source_sessions: Mutex::new(HashMap::new()),
            prepared: Mutex::new(HashMap::new()),
            runtimes: Mutex::new(HashMap::new()),
            #[cfg(test)]
            ready_runs: Mutex::new(HashMap::new()),
        })
    }

    fn bind_source_session(
        &self,
        approval_id: &str,
        source: crate::runtime::builder::SharedSession,
    ) -> Result<(), String> {
        self.source_sessions
            .lock()
            .map_err(|_| "delivery source session store unavailable")?
            .insert(approval_id.into(), source);
        Ok(())
    }

    async fn prepare_target(
        &self,
        receipt: &crate::delivery::Receipt,
        cancel: &CancellationToken,
    ) -> Result<PreparedDelivery, String> {
        let (target, _policy, host) = self
            .broker
            .target_host(
                &receipt.preview.plan.target.repository.to_string_lossy(),
                cancel,
            )
            .await?;
        if target != receipt.preview.plan.target.repository {
            return Err("delivery target changed before worktree setup".into());
        }
        let executor = host
            .delivery_setup
            .as_ref()
            .ok_or("delivery target does not have constrained worktree setup capability")?
            .worktree_executor();
        Ok(PreparedDelivery {
            primary_host: host,
            executor,
        })
    }

    fn source_session(
        &self,
        approval_id: &str,
    ) -> Result<crate::runtime::builder::SharedSession, String> {
        self.source_sessions
            .lock()
            .map_err(|_| "delivery source session store unavailable")?
            .get(approval_id)
            .cloned()
            .ok_or("delivery receipt has no source session binding".to_string())
    }

    async fn notify_source(&self, task: &crate::delivery::StoredTask) -> Result<(), String> {
        let Some(task) = self.receipts.claim_source_notification(&task.approval_id)? else {
            return Ok(());
        };
        let source = self.source_session(&task.approval_id)?;
        let summary = match task.state {
            crate::delivery::TaskState::Succeeded => format!(
                "Delivery task {} completed. {}",
                task.approval_id, task.result
            ),
            crate::delivery::TaskState::Canceled => {
                format!("Delivery task {} was canceled.", task.approval_id)
            }
            crate::delivery::TaskState::Failed | crate::delivery::TaskState::Unknown => format!(
                "Delivery task {} did not complete: {}",
                task.approval_id, task.error
            ),
            crate::delivery::TaskState::Running => return Ok(()),
        };
        source
            .append(Message {
                role: Role::Assistant,
                blocks: vec![Block::text(summary)],
                created_at: Utc::now(),
                ..Message::default()
            })
            .await
            .map_err(|_| "delivery source notification could not be recorded".into())
    }

    async fn finish_run(
        &self,
        receipt: &crate::delivery::Receipt,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let prompt = crate::delivery::render_handoff_prompt(&receipt.preview.handoff);
        let runtime = self
            .runtimes
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .remove(&receipt.id)
            .ok_or("delivery runtime is unavailable")?;
        let outcome = runtime.run_frozen(&prompt, cancel).await;
        let (state, result, error) = match outcome {
            Ok(result) => (crate::delivery::TaskState::Succeeded, result, String::new()),
            Err(error) if cancel.is_cancelled() => {
                (crate::delivery::TaskState::Canceled, String::new(), error)
            }
            Err(error) => (crate::delivery::TaskState::Failed, String::new(), error),
        };
        runtime.close().await;
        let task = self
            .receipts
            .finish_task(&receipt.id, state, &result, &error)?;
        self.notify_source(&task).await
    }

    #[cfg(test)]
    fn inject_ready_run(&self, approval_id: &str, run: Box<dyn DeliveryRun>) -> Result<(), String> {
        self.ready_runs
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .insert(approval_id.into(), run);
        Ok(())
    }

    async fn start_worktree_runner(
        &self,
        receipt: &crate::delivery::Receipt,
        cancel: &CancellationToken,
    ) -> Result<Box<dyn DeliveryRun>, String> {
        #[cfg(test)]
        if let Some(run) = self
            .ready_runs
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .remove(&receipt.id)
        {
            return Ok(run);
        }
        let prepared = self
            .prepared
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .remove(&receipt.id)
            .ok_or("delivery receipt has no prepared target capability")?;
        prepared
            .executor
            .add(&receipt.preview.plan, &receipt.base, cancel)
            .await?;

        let shared = self
            .broker
            .shared
            .upgrade()
            .ok_or("delivery broker is no longer available")?;
        let mut stderr = std::io::sink();
        let (builder, sandbox) = load_workspace(
            shared,
            &receipt.preview.plan.path,
            &Disabled::default(),
            cancel,
            &mut stderr,
        )
        .await?;
        let builder = Arc::new(builder);
        if builder.workspace_path != receipt.preview.plan.path.to_string_lossy() {
            return Err("delivery worktree builder root is invalid".into());
        }
        let runtime = crate::runtime::builder::resolve_initial_runtime(
            &builder.config,
            &builder.environment,
            None,
            &builder.overrides,
        )
        .map_err(|error| error.to_string())?;
        let session = builder
            .create_session(&runtime)
            .map_err(|error| error.to_string())?;
        let runner = Arc::new(
            builder
                .build_runner_quiet(&session, &runtime)
                .await
                .map_err(|error| error.to_string())?,
        );
        self.receipts.bind_session(
            &receipt.id,
            &session.header().id,
            Path::new(&builder.workspace_path),
        )?;
        Ok(Box::new(DeliveryRuntime {
            _primary_host: prepared.primary_host,
            worktree_sandbox: sandbox,
            _worktree_builder: builder,
            worktree_session: session,
            worktree_runner: runner,
        }))
    }

    async fn shutdown(&self) {
        let runtimes = self
            .runtimes
            .lock()
            .map(|mut runtimes| std::mem::take(&mut *runtimes))
            .unwrap_or_default();
        for (_, runtime) in runtimes {
            runtime.close().await;
        }
    }
}

#[async_trait]
impl crate::delivery::Service for DeliveryService {
    async fn prepare(
        &self,
        approval_id: &str,
        preview: &crate::delivery::Preview,
    ) -> Result<crate::delivery::Receipt, String> {
        let binding = crate::delivery::ReceiptBinding {
            approval_id: approval_id.into(),
            source_workspace: preview.source_workspace.clone(),
            target_repository: preview
                .plan
                .target
                .repository
                .to_string_lossy()
                .into_owned(),
            target_common_dir: preview
                .plan
                .target
                .common_dir
                .to_string_lossy()
                .into_owned(),
            worktree: preview.plan.path.to_string_lossy().into_owned(),
            branch: preview.plan.branch.clone(),
            base: preview.base.clone(),
            handoff_digest: preview.handoff.digest.clone(),
        };
        let stored = self.receipts.prepare(binding)?;
        if stored.state == crate::delivery::ReceiptState::Unknown {
            return Err(
                "delivery receipt outcome is unknown; inspect the worktree before retrying".into(),
            );
        }
        let receipt = crate::delivery::Receipt {
            id: approval_id.into(),
            preview: preview.clone(),
            base: stored.binding.base,
        };
        #[cfg(test)]
        let has_ready_run = self
            .ready_runs
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .contains_key(approval_id);
        #[cfg(not(test))]
        let has_ready_run = false;
        if !has_ready_run {
            let prepared = self
                .prepare_target(&receipt, &CancellationToken::new())
                .await?;
            self.prepared
                .lock()
                .map_err(|_| "delivery runtime store unavailable")?
                .insert(approval_id.into(), prepared);
        }
        Ok(receipt)
    }

    async fn start(
        &self,
        receipt: &crate::delivery::Receipt,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let stored = self.receipts.begin(&receipt.id)?;
        if stored.binding.worktree != receipt.preview.plan.path.to_string_lossy()
            || stored.binding.base != receipt.base
        {
            let _ = self.receipts.unknown(&receipt.id);
            return Err("delivery receipt binding changed before worktree setup".into());
        }
        if self.source_session(&receipt.id).is_err() {
            let _ = self.receipts.unknown(&receipt.id);
            return Err("delivery receipt has no source session binding".into());
        }
        let runtime = match self.start_worktree_runner(receipt, cancel).await {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = self.receipts.unknown(&receipt.id);
                return Err(error);
            }
        };
        let source_session = self.source_session(&receipt.id)?.header().id;
        let target_session = runtime.target_session();
        let worktree = runtime.worktree();
        self.receipts.begin_task(
            &receipt.id,
            &source_session,
            &target_session,
            Path::new(&worktree),
        )?;
        self.runtimes
            .lock()
            .map_err(|_| "delivery runtime store unavailable")?
            .insert(receipt.id.clone(), runtime);
        self.receipts.started(&receipt.id)?;
        self.finish_run(receipt, cancel).await?;
        Ok(format!("delivery task completed: {}", receipt.id))
    }
}

/// The deliberately narrow delivery status projection suitable for a future
/// server/ACP read-only aggregation. Paths and execution capabilities never
/// cross this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeliveryStatus {
    approval_id: String,
    state: crate::delivery::TaskState,
    result: String,
    error: String,
}

/// Serve-owned lifecycle owner for delivery receipts and their private target
/// runtimes. It is not a tool factory and has no model-visible API.
struct DeliveryManager {
    service: Arc<DeliveryService>,
    receipts: Arc<crate::delivery::ReceiptStore>,
    /// Retained for late workspace attachment; the service owns a clone.
    broker: DeliveryBroker,
}

impl DeliveryManager {
    fn open(
        home: &str,
        shared: &Arc<crate::runtime::builder::Shared>,
        runtime: Runtime,
        cancel: CancellationToken,
    ) -> Result<Self, String> {
        let path = Path::new(home).join(".otto/delivery-receipts.db");
        let receipts = Arc::new(crate::delivery::ReceiptStore::open(&path)?);
        let broker = DeliveryBroker::new(shared, runtime, cancel);
        let service = Arc::new(DeliveryService::new(broker.clone(), Arc::clone(&receipts))?);
        Ok(Self {
            service,
            receipts,
            broker,
        })
    }

    /// Publishes the workspace registry the acceptance broker targets. Called
    /// once the registry exists and before the server accepts a request.
    fn attach_workspaces(&self, workspaces: &Arc<Workspaces>) {
        self.broker.attach_workspaces(workspaces);
    }

    fn bind_source_session(
        &self,
        approval_id: &str,
        source: crate::runtime::builder::SharedSession,
    ) -> Result<(), String> {
        self.service.bind_source_session(approval_id, source)
    }

    fn status(
        &self,
        source_session: &str,
        approval_id: &str,
    ) -> Result<Option<DeliveryStatus>, String> {
        self.receipts.task(approval_id).map(|task| {
            task.filter(|task| task.source_session == source_session)
                .map(|task| DeliveryStatus {
                    approval_id: task.approval_id,
                    state: task.state,
                    result: task.result,
                    error: task.error,
                })
        })
    }

    fn service_for(
        self: &Arc<Self>,
        source: crate::runtime::builder::SharedSession,
    ) -> Arc<dyn crate::delivery::Service> {
        Arc::new(SourceDeliveryService {
            manager: Arc::clone(self),
            source,
        })
    }

    async fn shutdown(&self) {
        self.service.shutdown().await;
    }
}

/// Per-parent source binding for a delivery receipt. It has no target host,
/// filesystem root, sandbox environment, or command-execution API.
struct SourceDeliveryService {
    manager: Arc<DeliveryManager>,
    source: crate::runtime::builder::SharedSession,
}

#[async_trait]
impl crate::delivery::Service for SourceDeliveryService {
    async fn prepare(
        &self,
        approval_id: &str,
        preview: &crate::delivery::Preview,
    ) -> Result<crate::delivery::Receipt, String> {
        self.manager
            .bind_source_session(approval_id, self.source.clone())?;
        self.manager.service.prepare(approval_id, preview).await
    }

    async fn start(
        &self,
        receipt: &crate::delivery::Receipt,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        self.manager.service.start(receipt, cancel).await
    }
}

/// Parent-only serve tool factory. The returned tool gets only its source
/// identity/transcript and delivery trait ports; target capabilities remain in
/// the manager and broker.
struct DeliveryToolFactory {
    manager: Arc<DeliveryManager>,
    preflight: Arc<dyn crate::delivery::Preflight>,
}

impl DeliveryToolFactory {
    fn new(manager: Arc<DeliveryManager>) -> Self {
        // The broker is shared with the manager, so the late
        // `attach_workspaces` call is visible to this preflight.
        let preflight: Arc<dyn crate::delivery::Preflight> = Arc::new(manager.broker.clone());
        Self { manager, preflight }
    }
}

impl crate::runtime::builder::ParentToolFactory for DeliveryToolFactory {
    fn tools(
        &self,
        context: &crate::runtime::builder::ParentToolContext,
    ) -> Result<Vec<Box<dyn crate::tool::Tool + Send + Sync>>, String> {
        let service = self.manager.service_for(context.source_session.clone());
        Ok(vec![Box::new(crate::delivery::StartDeliveryTask::new(
            context.workspace.clone(),
            service,
            Arc::clone(&self.preflight),
        ))])
    }
}

// ---- the session factory ----

/// Builds one [`Controller`] per server-side session on top of the same
/// replacement plumbing the CLI's `/new` and `/resume` use.
struct ServeFactory {
    workspaces: Arc<Workspaces>,
    /// Optional read-only delivery status owner; it does not grant any delivery
    /// effect. Test/frontends without delivery return no delivery status.
    delivery: Option<Arc<DeliveryManager>>,
    runtime: Runtime,
    /// Cancels a workspace load in progress when the server shuts down;
    /// otherwise never fired during normal operation.
    cancel: CancellationToken,
}

/// The sessions for one workspace's builder, with a missing session root
/// reported as no sessions.
fn listed_for(builder: &Builder) -> Result<ListResult, String> {
    if !builder.session_root.exists() {
        return Ok(ListResult::default());
    }
    sessionfs::list(
        &builder.session_root,
        &builder.workspace_path,
        "",
        MAX_LIST_SESSIONS,
    )
    .map_err(|error| builder.redact_error(&error.to_string(), None))
}

impl ServeFactory {
    fn builder(&self) -> &Arc<Builder> {
        &self.workspaces.startup_host().builder
    }

    fn wire(&self, controller: Controller, host: &WorkspaceHost) -> Controller {
        match &host.reloader {
            Some(control) => {
                controller.with_sandbox_control(Arc::clone(control) as Arc<dyn SandboxControl>)
            }
            None => controller,
        }
    }

    /// The hosts `open`/`list` search: just `workspace`'s host when named,
    /// every loaded host otherwise. `None` when a named workspace is not
    /// loaded.
    async fn hosts_for(&self, workspace: Option<&str>) -> Option<Vec<Arc<WorkspaceHost>>> {
        match workspace {
            Some(path) => Some(vec![self.workspaces.host(path).await?]),
            None => Some(
                self.workspaces
                    .list()
                    .await
                    .into_iter()
                    .map(|(_, host)| host)
                    .collect(),
            ),
        }
    }
}

#[async_trait::async_trait]
impl Factory for ServeFactory {
    fn delivery_status(
        &self,
        source_session: &str,
        approval_id: &str,
    ) -> Result<Option<crate::server::delivery::Status>, String> {
        let Some(delivery) = &self.delivery else {
            return Ok(None);
        };
        delivery.status(source_session, approval_id).map(|status| {
            status.map(|status| crate::server::delivery::Status {
                approval_id: status.approval_id,
                state: match status.state {
                    crate::delivery::TaskState::Running => "running",
                    crate::delivery::TaskState::Succeeded => "succeeded",
                    crate::delivery::TaskState::Failed => "failed",
                    crate::delivery::TaskState::Canceled => "canceled",
                    crate::delivery::TaskState::Unknown => "unknown",
                }
                .into(),
                result: status.result,
                error: status.error,
            })
        })
    }

    fn approval_service(&self) -> Option<Arc<crate::approval::Service>> {
        Some(self.workspaces.startup_host().builder.approvals.clone())
    }
    async fn create(&self, workspace: Option<&str>) -> Result<Controller, String> {
        let host = match workspace {
            Some(path) => self
                .workspaces
                .host(path)
                .await
                .ok_or_else(|| format!("workspace not loaded: {path}"))?,
            None => Arc::clone(self.workspaces.startup_host()),
        };
        let runtime = crate::runtime::builder::resolve_initial_runtime(
            &host.builder.config,
            &host.builder.environment,
            None,
            &host.builder.overrides,
        )
        .map_err(|error| error.to_string())?;
        let controller = Controller::create(Arc::clone(&host.builder), &runtime).await?;
        Ok(self.wire(controller, &host))
    }

    async fn create_with_receipt(
        &self,
        workspace: Option<&str>,
        id: String,
        request_id: &str,
    ) -> Result<Controller, String> {
        let host = match workspace {
            Some(path) => self
                .workspaces
                .host(path)
                .await
                .ok_or_else(|| format!("workspace not loaded: {path}"))?,
            None => Arc::clone(self.workspaces.startup_host()),
        };
        let runtime = crate::runtime::builder::resolve_initial_runtime(
            &host.builder.config,
            &host.builder.environment,
            None,
            &host.builder.overrides,
        )
        .map_err(|error| error.to_string())?;
        let controller =
            Controller::create_with_receipt(Arc::clone(&host.builder), &runtime, id, request_id)
                .await?;
        Ok(self.wire(controller, &host))
    }

    async fn open(&self, id: &str, workspace: Option<&str>) -> Result<Controller, String> {
        let hosts = self
            .hosts_for(workspace)
            .await
            .ok_or_else(|| SESSION_NOT_FOUND.to_string())?;
        if !sessionfs::is_session_id(id) {
            return Err(SESSION_NOT_FOUND.to_string());
        }
        for host in &hosts {
            let path = sessionfs::session_directory(
                &host.builder.session_root,
                &host.builder.workspace_path,
            )
            .map_err(|error| host.builder.redact_error(&error.to_string(), None))?
            .join(format!("{id}.jsonl"));
            if path.is_file() {
                // The repair warnings the CLI prints have no channel here;
                // the web UI reads the repaired history like any other.
                let (controller, _warnings) =
                    Controller::open(Arc::clone(&host.builder), &path).await?;
                return Ok(self.wire(controller, host));
            }
        }
        Err(SESSION_NOT_FOUND.to_string())
    }

    async fn list(&self, workspace: Option<&str>) -> Option<Result<ListResult, String>> {
        let hosts = self.hosts_for(workspace).await?;
        let mut sessions = Vec::new();
        let mut skipped = 0i64;
        for host in &hosts {
            match listed_for(&host.builder) {
                Ok(result) => {
                    sessions.extend(result.sessions);
                    skipped += result.skipped;
                }
                Err(error) => return Some(Err(error)),
            }
        }
        if workspace.is_none() {
            // Each host's own list is already newest-first and capped; the
            // merge needs the same cap re-applied across all of them.
            sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
            sessions.truncate(MAX_LIST_SESSIONS);
        }
        Some(Ok(ListResult { sessions, skipped }))
    }

    fn sandbox_reload_available(&self) -> bool {
        self.workspaces.startup_host().reloader.is_some()
    }

    async fn reload_sandbox(&self) -> Option<Result<SandboxInfo, String>> {
        let host = self.workspaces.startup_host();
        let control = host.reloader.as_ref()?;
        Some(SandboxControl::reload(control.as_ref()).await)
    }

    async fn reload_other_sandboxes(&self) -> Vec<(String, Result<SandboxInfo, String>)> {
        let mut results = Vec::new();
        for (path, host) in self.workspaces.list().await {
            if path == self.workspaces.startup {
                continue;
            }
            if let Some(control) = &host.reloader {
                let result = SandboxControl::reload(control.as_ref())
                    .await
                    .map_err(|error| self.builder().redact_error(&error, Some(&self.runtime)));
                results.push((path, result));
            }
        }
        results
    }

    async fn diff_runner(
        &self,
        workspace: &str,
    ) -> Option<(Arc<dyn crate::sandbox::CommandExecutor>, Vec<String>)> {
        let host = self.workspaces.host(workspace).await?;
        let executor = host.builder.command_executor.clone()?;
        let environment = host.builder.sandbox_environment.clone()?;
        Some((executor, environment))
    }

    fn usage_summary(&self, session_id: Option<&str>) -> Result<crate::usage::Summary, String> {
        self.builder().usage_summary(session_id)
    }

    fn usage_analysis(
        &self,
        days: u16,
        session_id: Option<&str>,
    ) -> Result<crate::usage::Analysis, String> {
        self.builder().usage_analysis(days, session_id)
    }

    fn tasks_list(
        &self,
        query: &crate::subagent::record::ListQuery,
    ) -> Result<crate::subagent::record::ListResult, String> {
        self.builder().tasks_list(query)
    }

    fn tasks_get(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Option<crate::subagent::record::TaskRow>, String> {
        self.builder().tasks_get(parent_session, task_id)
    }

    async fn workspaces(&self) -> server::WorkspaceList {
        let loaded = self.workspaces.list().await;
        server::WorkspaceList {
            startup: self.workspaces.startup.clone(),
            roots: self
                .workspaces
                .roots
                .iter()
                .map(|root| root.to_string_lossy().into_owned())
                .collect(),
            loaded: loaded
                .into_iter()
                .map(|(path, host)| server::WorkspaceInfo {
                    path,
                    workflows: host.workflows.is_some(),
                })
                .collect(),
        }
    }

    async fn trust_workspace(&self, path: &str) -> Result<(), server::WorkspaceLoadError> {
        // Relative paths are refused for the same reason `admit_workspace`
        // refuses them: they would resolve against serve's working directory.
        if !Path::new(path).is_absolute() || canonical_directory(Path::new(path)).is_err() {
            return Err(server::WorkspaceLoadError::Invalid(format!(
                "{path}: not an existing directory"
            )));
        }
        super::trust::trust_directory(&self.workspaces.config_path, Path::new(path))
            .map(|_| ())
            .map_err(server::WorkspaceLoadError::Failed)
    }

    fn browse_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = canonical_directory(Path::new(&self.builder().shared.home))
            .into_iter()
            .collect();
        for root in &self.workspaces.roots {
            if !roots.contains(root) {
                roots.push(root.clone());
            }
        }
        roots
    }

    async fn load_workspace(
        &self,
        path: &str,
    ) -> Result<(server::WorkspaceInfo, bool), server::WorkspaceLoadError> {
        let canonical = self
            .workspaces
            .admit(path)
            .map_err(|admission| match admission {
                Admission::Invalid => server::WorkspaceLoadError::Invalid(format!(
                    "{path}: not an existing directory"
                )),
                Admission::NotAdmitted => server::WorkspaceLoadError::NotAdmitted(format!(
                    "{path}: outside the startup workspace and configured roots"
                )),
            })?;
        let shared = Arc::clone(&self.builder().shared);
        // ponytail: no writer is threaded through `Factory::load_workspace`,
        // so a workflow-lock warning goes straight to the process stderr,
        // matching `runtime/builder.rs`'s own `build_runner` warnings.
        let mut stderr = std::io::stderr();
        let (host, newly_loaded) = self
            .workspaces
            .load(
                &canonical,
                &shared,
                &self.runtime,
                &self.cancel,
                &mut stderr,
                true,
            )
            .await
            .map_err(|error| {
                server::WorkspaceLoadError::Failed(
                    self.builder().redact_error(&error, Some(&self.runtime)),
                )
            })?;
        Ok((
            server::WorkspaceInfo {
                path: canonical.to_string_lossy().into_owned(),
                workflows: host.workflows.is_some(),
            },
            newly_loaded,
        ))
    }

    async fn remove_workspace(&self, path: &str) -> Result<(), server::WorkspaceRemoveError> {
        let canonical = canonical_or_literal(path);
        let home = self.builder().shared.home.clone();
        // ponytail: no writer is threaded through `Factory::remove_workspace`,
        // matching `load_workspace`'s own stderr fallback.
        let mut stderr = std::io::stderr();
        self.workspaces.remove(&canonical, &home, &mut stderr).await
    }

    async fn workflow_controller(
        &self,
        workspace: Option<&str>,
    ) -> Option<Arc<crate::workflow::Controller>> {
        let host = match workspace {
            Some(path) => self.workspaces.host(path).await?,
            None => Arc::clone(self.workspaces.startup_host()),
        };
        host.workflows.clone()
    }

    async fn workflow_controllers(&self) -> Vec<Arc<crate::workflow::Controller>> {
        self.workspaces
            .list()
            .await
            .into_iter()
            .filter_map(|(_, host)| host.workflows.clone())
            .collect()
    }
}

// ---- the command ----

/// What [`run`] needs from `cli::run`'s composition root.
pub struct ServeOptions<'a> {
    pub builder: Builder,
    pub runtime: Runtime,
    /// At least one of its two fields is set; both set binds both listeners.
    pub listen: ServerRuntime,
    pub acp_listen: String,
    pub acp_token: String,
    /// The process sandbox the composition root opened, already behind its
    /// switch. [`run`] owns closing it.
    pub control: Arc<SandboxSwitch>,
    /// `None` when bash never came up, so `POST /v1/sandbox/reload` answers 501
    /// rather than a failure.
    pub reloader: Option<Arc<SandboxReloader>>,
    /// Elevated target-local executor and its resolved environment, retained
    /// privately by the startup host for constrained delivery worktree setup.
    pub delivery_setup_executor: Option<Arc<crate::sandbox::Executor>>,
    pub delivery_setup_environment: Option<Vec<String>>,
    /// Set to "serving" before bind so a SIGTERM with no held lease cancels
    /// serve instead of killing the process; read after the listen loop
    /// returns to decide whether to run the migration.
    pub terminate: &'a super::terminate::Terminate,
}

pub async fn run(
    options: ServeOptions<'_>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> i32 {
    let ServeOptions {
        builder,
        runtime,
        listen,
        acp_listen,
        acp_token,
        control,
        reloader,
        delivery_setup_executor,
        delivery_setup_environment,
        terminate,
    } = options;
    terminate.set_serve();

    let workspace_roots = match canonicalize_workspace_roots(&listen.workspace_roots) {
        Ok(roots) => roots,
        Err(message) => {
            let _ = control.close().await;
            return fail(stderr, &builder.redact_error(&message, Some(&runtime)));
        }
    };

    let acp = if acp_listen.is_empty() {
        None
    } else {
        match crate::acp::tcp::bind(&acp_listen, &acp_token) {
            Ok(listener) => Some((listener, acp_token)),
            Err(message) => {
                let _ = control.close().await;
                return fail(stderr, &builder.redact_error(&message, Some(&runtime)));
            }
        }
    };
    let serve_cancel = cancel.child_token();
    let bound = match bind(&listen) {
        Ok(bound) => bound,
        Err(message) => {
            let _ = control.close().await;
            return fail(stderr, &builder.redact_error(&message, Some(&runtime)));
        }
    };
    let Bound { tcp, socket, token } = bound;
    if let Some((listener, _)) = &acp {
        let _ = writeln!(
            stdout,
            "otto serve: ACP {}",
            listener.local_addr().expect("bound address")
        );
    }
    if let Some(tcp) = &tcp {
        announce_listen(stdout, &tcp.address(), &token);
    }

    let builder = Arc::new(builder);
    // The delivery manager opens before the workflow controller is built, and
    // the parent-tool slot is installed before that, so every parent runner
    // this process builds -- workflow steps included -- composes the same
    // tools. `Workspaces` is attached once it exists: it owns the workflow
    // controller through its host, so the broker cannot take it as an
    // argument here.
    let delivery = match DeliveryManager::open(
        &builder.shared.home,
        &builder.shared,
        runtime.clone(),
        serve_cancel.clone(),
    ) {
        Ok(manager) => Arc::new(manager),
        Err(error) => {
            let _ = control.close().await;
            return fail(stderr, &builder.redact_error(&error, Some(&runtime)));
        }
    };
    let _ = builder
        .shared
        .parent_tool_factory
        .install(Arc::new(DeliveryToolFactory::new(Arc::clone(&delivery))));
    let workflows =
        match super::workflow::build_controller(Arc::clone(&builder), &runtime, stderr).await {
            Ok(controller) => Some(controller),
            Err(error) => {
                let redacted = builder.redact_error(&error, Some(&runtime));
                if !redacted.is_empty() {
                    let _ = writeln!(stderr, "warning: workflows disabled: {redacted}");
                }
                None
            }
        };
    let info = builder.runtime_info(&runtime);
    let mut profiles: Vec<String> = builder.config.profiles.keys().cloned().collect();
    profiles.sort();
    let workspace_path = builder.workspace_path.clone();
    let host = Arc::new(WorkspaceHost {
        builder: Arc::clone(&builder),
        workflows,
        delivery_setup: match (delivery_setup_executor, delivery_setup_environment) {
            (Some(executor), Some(environment)) => Some(DeliverySetupCapability {
                executor,
                environment,
            }),
            _ => None,
        },
        reloader,
    });
    let workspaces = Arc::new(Workspaces {
        startup: workspace_path.clone(),
        startup_host: Arc::clone(&host),
        roots: workspace_roots,
        config_path: builder.shared.config_path.clone(),
        loaded: tokio::sync::Mutex::new(BTreeMap::from([(workspace_path, Arc::clone(&host))])),
    });
    load_persisted_workspaces(
        &workspaces,
        &builder.shared,
        &runtime,
        &serve_cancel,
        stderr,
    )
    .await;
    delivery.attach_workspaces(&workspaces);
    let factory: Arc<dyn Factory> = Arc::new(ServeFactory {
        workspaces: Arc::clone(&workspaces),
        delivery: Some(Arc::clone(&delivery)),
        runtime: runtime.clone(),
        cancel: serve_cancel.clone(),
    });
    let server = Server::new(Options {
        info: Info {
            workspace: builder.workspace_path.clone(),
            provider: info.provider.clone(),
            profile: info.profile.clone(),
            model: info.model.clone(),
            thinking: info.thinking.clone(),
            sandbox: info.sandbox.summary().to_string(),
            profiles,
        },
        factory: Arc::clone(&factory),
        config_path: builder.shared.config_path.clone(),
        token,
        logger: Some(Arc::clone(&builder.logger)),
        workflows: host.workflows.clone(),
        schedule: builder.schedule.clone(),
        session_idle_timeout: listen.session_idle_timeout,
        max_open_sessions: listen.max_open_sessions,
    });

    // Runs due scheduled prompts of every origin; delivery is the web routes
    // and the per-connection ACP workers.
    let schedule_worker = builder.schedule.clone().map(|store| {
        tokio::spawn(
            crate::schedule::worker::Worker {
                store,
                run: Some(crate::schedule::worker::RunRole {
                    scope: crate::schedule::Scope::Any,
                    workspace: None,
                    runner: Arc::new(crate::server::schedule::FactoryRunner(factory)),
                }),
                deliver: None,
                logger: Arc::clone(server.logger()),
                metrics: Some(Arc::clone(server.metrics())),
            }
            .run(serve_cancel.child_token()),
        )
    });
    // Closes sessions nobody has used for a while, so a server that stays up
    // keeps only the sessions in play. See `Server::sweep_idle_sessions`.
    server.start_session_sweeper(serve_cancel.child_token());
    let (serve_error, acp_error) = tokio::join!(
        serve_listeners(&server, tcp, socket, &serve_cancel),
        async {
            if let Some((listener, token)) = acp {
                let result = crate::acp::tcp::serve(
                    listener,
                    token,
                    Arc::clone(&server),
                    PathBuf::from(&builder.workspace_path),
                    &serve_cancel,
                )
                .await;
                if result.is_err() {
                    serve_cancel.cancel();
                }
                result.err()
            } else {
                None
            }
        }
    );
    let serve_error = serve_error.or(acp_error);
    serve_cancel.cancel();
    server.cancel_token().cancel();
    if let Some(worker) = schedule_worker {
        let _ = worker.await;
    }
    if terminate.migrating() {
        for warning in server.migrate().await {
            let _ = writeln!(stderr, "warning: {warning}");
        }
    }
    let close_error = server.close().await.err();
    delivery.shutdown().await;
    // Every loaded workspace owns its own controller, not only startup.
    let hosts: Vec<_> = workspaces.loaded.lock().await.values().cloned().collect();
    for host in hosts {
        if let Some(controller) = &host.workflows {
            controller.close().await;
        }
    }
    let sandbox_error = control.close().await.err();

    let redact = |message: &str| -> String { format!("serve: {message}") };
    if let Some(message) = serve_error {
        return fail(stderr, &redact(&message));
    }
    if let Some(message) = close_error {
        return fail(stderr, &redact(&message));
    }
    if sandbox_error.is_some() {
        return fail(stderr, "close sandbox: sandbox runtime close failed");
    }
    0
}

/// Serves the TCP listener (token required) and the socket listener (no
/// token) from one server. A failure on either stops both; the first error is
/// returned.
async fn serve_listeners(
    server: &Arc<Server>,
    tcp: Option<Listener>,
    socket: Option<Listener>,
    cancel: &CancellationToken,
) -> Option<String> {
    let stop = cancel.child_token();
    let tcp_task = tcp.map(|listener| server::serve(listener, server.router(), stop.clone()));
    let socket_task =
        socket.map(|listener| server::serve(listener, server.socket_router(), stop.clone()));
    let guard = |result: Result<(), String>| {
        if result.is_err() {
            stop.cancel();
        }
        result
    };
    let (tcp_result, socket_result) = tokio::join!(
        async {
            match tcp_task {
                Some(task) => guard(task.await),
                None => Ok(()),
            }
        },
        async {
            match socket_task {
                Some(task) => guard(task.await),
                None => Ok(()),
            }
        },
    );
    tcp_result.err().or(socket_result.err())
}

fn announce_listen(stdout: &mut (dyn Write + Send), address: &str, token: &str) {
    let _ = writeln!(stdout, "otto serve: http://{address}/?token={token}");
}

/// The bound listeners. `token` is non-empty exactly when `tcp` is set.
struct Bound {
    tcp: Option<Listener>,
    socket: Option<Listener>,
    token: String,
}

/// Binds the TCP listener, the Unix socket, or both, from the two fields of
/// `listen`. A TCP port is reachable by every local user and every page open
/// in a browser, so the API on it is gated by a per-process token; the token
/// is printed once by the caller and never logged. The socket is protected by
/// its file permissions and needs no token.
fn bind(listen: &ServerRuntime) -> Result<Bound, String> {
    let mut bound = Bound {
        tcp: None,
        socket: None,
        token: String::new(),
    };
    if !listen.listen.is_empty() {
        bound.tcp = Some(listen_tcp(&listen.listen).map_err(|error| format!("serve: {error}"))?);
        bound.token =
            crate::runtime::builder::random_id().map_err(|error| format!("serve: {error}"))?;
    }
    if !listen.socket.is_empty() {
        let socket = match Path::new(&listen.socket).is_absolute() {
            true => listen.socket.clone(),
            false => std::path::absolute(&listen.socket)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .into_owned(),
        };
        bound.socket = Some(listen_unix(&socket).map_err(|error| format!("serve: {error}"))?);
    }
    Ok(bound)
}

#[cfg(test)]
mod tests;
