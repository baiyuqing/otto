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
#[allow(dead_code)] // Used only when the delivery lifecycle is fully installed.
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
        let (builder, sandbox) =
            super::run::load_workspace(Arc::clone(shared), canonical, cancel, stderr).await?;
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
#[allow(dead_code)] // Retained only by the not-yet-installed delivery service.
struct DeliveryRuntime {
    _primary_host: Arc<WorkspaceHost>,
    worktree_sandbox: super::run::WorkspaceSandbox,
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

#[allow(dead_code)] // Held only by the not-yet-installed delivery service.
struct PreparedDelivery {
    primary_host: Arc<WorkspaceHost>,
    executor: crate::delivery::WorktreeExecutor,
}

/// Receipt-owned delivery effect service. It is intentionally private and
/// uninstalled until task execution/status wiring exists. Its only mutable Git
/// operation is `WorktreeExecutor::add`; every target runner is built after
/// that operation around the newly-created worktree root.
#[allow(dead_code)] // Installed only after delivery task/status wiring is complete.
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
        let (builder, sandbox) =
            super::run::load_workspace(shared, &receipt.preview.plan.path, cancel, &mut stderr)
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
        workspaces,
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
mod tests {
    use super::*;

    #[test]
    fn tcp_startup_prints_connection_details() {
        let mut stdout = Vec::new();
        announce_listen(&mut stdout, "127.0.0.1:8787", "tok");
        assert_eq!(
            String::from_utf8(stdout).expect("utf-8"),
            "otto serve: http://127.0.0.1:8787/?token=tok\n"
        );
    }

    // ---- admission ----

    #[test]
    fn the_startup_workspace_is_admitted_with_no_roots() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let admitted = admit_workspace(&startup_path.to_string_lossy(), &startup_path, &[], &[])
            .expect("admitted");
        assert_eq!(admitted, startup_path);
    }

    #[test]
    fn a_root_itself_is_admitted() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let root = tempfile::tempdir().expect("root");
        let root_path = canonical_directory(root.path()).expect("canonical");
        let admitted = admit_workspace(
            &root_path.to_string_lossy(),
            &startup_path,
            std::slice::from_ref(&root_path),
            &[],
        )
        .expect("admitted");
        assert_eq!(admitted, root_path);
    }

    #[test]
    fn a_descendant_of_a_root_is_admitted() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let root = tempfile::tempdir().expect("root");
        let root_path = canonical_directory(root.path()).expect("canonical");
        let child = root_path.join("project");
        std::fs::create_dir(&child).expect("child");
        let admitted = admit_workspace(&child.to_string_lossy(), &startup_path, &[root_path], &[])
            .expect("ok");
        assert_eq!(admitted, child);
    }

    #[test]
    fn a_sibling_of_a_root_is_not_admitted() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("root");
        std::fs::create_dir(&root).expect("root");
        let root_path = canonical_directory(&root).expect("canonical");
        let sibling = parent.path().join("sibling");
        std::fs::create_dir(&sibling).expect("sibling");
        assert_eq!(
            admit_workspace(&sibling.to_string_lossy(), &startup_path, &[root_path], &[]),
            Err(Admission::NotAdmitted)
        );
    }

    #[test]
    fn a_prefix_lookalike_sibling_is_not_admitted() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("root");
        std::fs::create_dir(&root).expect("root");
        let root_path = canonical_directory(&root).expect("canonical");
        let lookalike = parent.path().join("root-x");
        std::fs::create_dir(&lookalike).expect("lookalike");
        assert_eq!(
            admit_workspace(
                &lookalike.to_string_lossy(),
                &startup_path,
                &[root_path],
                &[]
            ),
            Err(Admission::NotAdmitted)
        );
    }

    #[test]
    fn a_symlink_inside_a_root_pointing_outside_is_not_admitted() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let root = tempfile::tempdir().expect("root");
        let root_path = canonical_directory(root.path()).expect("canonical");
        let outside = tempfile::tempdir().expect("outside");
        let outside_path = canonical_directory(outside.path()).expect("canonical");
        let link = root_path.join("escape");
        std::os::unix::fs::symlink(&outside_path, &link).expect("symlink");
        assert_eq!(
            admit_workspace(&link.to_string_lossy(), &startup_path, &[root_path], &[]),
            Err(Admission::NotAdmitted)
        );
    }

    #[test]
    fn a_missing_path_is_invalid() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let missing = startup.path().join("does-not-exist");
        assert_eq!(
            admit_workspace(&missing.to_string_lossy(), &startup_path, &[], &[]),
            Err(Admission::Invalid)
        );
    }

    #[test]
    fn a_file_is_invalid_not_a_directory() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let file = startup.path().join("file.txt");
        std::fs::write(&file, b"hi").expect("write");
        assert_eq!(
            admit_workspace(&file.to_string_lossy(), &startup_path, &[], &[]),
            Err(Admission::Invalid)
        );
    }

    #[test]
    fn a_relative_path_is_invalid() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        assert_eq!(
            admit_workspace(".", &startup_path, &[], &[]),
            Err(Admission::Invalid)
        );
        assert_eq!(
            admit_workspace("rel/dir", &startup_path, &[], &[]),
            Err(Admission::Invalid)
        );
    }

    #[test]
    fn empty_roots_admits_only_the_startup_path() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let other = tempfile::tempdir().expect("other");
        let other_path = canonical_directory(other.path()).expect("canonical");
        assert!(admit_workspace(&startup_path.to_string_lossy(), &startup_path, &[], &[]).is_ok());
        assert_eq!(
            admit_workspace(&other_path.to_string_lossy(), &startup_path, &[], &[]),
            Err(Admission::NotAdmitted)
        );
    }

    #[test]
    fn a_trusted_directory_outside_every_root_is_admitted() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let trusted = tempfile::tempdir().expect("trusted");
        let trusted_path = canonical_directory(trusted.path()).expect("canonical");
        let admitted = admit_workspace(
            &trusted_path.to_string_lossy(),
            &startup_path,
            &[],
            std::slice::from_ref(&trusted_path),
        )
        .expect("admitted");
        assert_eq!(admitted, trusted_path);
    }

    #[test]
    fn a_descendant_of_a_trusted_directory_is_admitted() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let trusted = tempfile::tempdir().expect("trusted");
        let trusted_path = canonical_directory(trusted.path()).expect("canonical");
        let child = trusted_path.join("project");
        std::fs::create_dir(&child).expect("child");
        let admitted = admit_workspace(
            &child.to_string_lossy(),
            &startup_path,
            &[],
            &[trusted_path],
        )
        .expect("ok");
        assert_eq!(admitted, child);
    }

    #[test]
    fn a_prefix_lookalike_sibling_of_a_trusted_directory_is_not_admitted() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let parent = tempfile::tempdir().expect("parent");
        let trusted = parent.path().join("app");
        std::fs::create_dir(&trusted).expect("trusted");
        let trusted_path = canonical_directory(&trusted).expect("canonical");
        let lookalike = parent.path().join("app-x");
        std::fs::create_dir(&lookalike).expect("lookalike");
        assert_eq!(
            admit_workspace(
                &lookalike.to_string_lossy(),
                &startup_path,
                &[],
                &[trusted_path]
            ),
            Err(Admission::NotAdmitted)
        );
    }

    /// A `WorkspaceHost` for tests that need one but never open a real
    /// sandbox or workflow controller.
    fn dummy_host(workspace_root: &Path, session_root: &Path) -> Arc<WorkspaceHost> {
        Arc::new(WorkspaceHost {
            builder: Arc::new(crate::cli::testutil::builder(workspace_root, session_root)),
            workflows: None,
            delivery_setup: None,
            reloader: None,
        })
    }

    #[tokio::test]
    async fn controlled_delivery_run_persists_notifies_and_closes() {
        struct FakeRun {
            prompt: Arc<Mutex<Option<String>>>,
            closed: Arc<std::sync::atomic::AtomicBool>,
        }

        #[async_trait]
        impl DeliveryRun for FakeRun {
            fn target_session(&self) -> String {
                "target-session".into()
            }

            fn worktree(&self) -> String {
                "/target/.otto/delivery-worktrees/delivery-1".into()
            }

            async fn run_frozen(
                &self,
                prompt: &str,
                _cancel: &CancellationToken,
            ) -> Result<String, String> {
                *self.prompt.lock().unwrap() = Some(prompt.into());
                Ok("target completed the frozen handoff".into())
            }

            async fn close(self: Box<Self>) {
                self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let startup_host = dummy_host(&startup_path, sessions.path());
        let shared = Arc::clone(&startup_host.builder.shared);
        let workspaces = Arc::new(Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host,
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        });
        let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
        let manager = Arc::new(
            DeliveryManager::open(
                &home.path().to_string_lossy(),
                &shared,
                runtime,
                CancellationToken::new(),
            )
            .expect("manager"),
        );
        manager.attach_workspaces(&workspaces);
        let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
            id: "source-session".into(),
            workspace: "/source".into(),
            ..otto_core::session::Header::default()
        });
        manager
            .bind_source_session("approval-1", source.clone())
            .expect("bind source");
        let worktree = startup_path.join(".otto/delivery-worktrees/delivery-1");
        let handoff = crate::delivery::handoff(
            "Ship frozen design",
            [("source-ref-1".into(), "Selected design text".into())],
        )
        .unwrap();
        let preview = crate::delivery::Preview {
            source_workspace: "/source".into(),
            plan: crate::delivery::WorktreePlan {
                target: crate::delivery::Target {
                    repository: startup_path.clone(),
                    common_dir: startup_path.join(".git"),
                },
                delivery_id: "delivery-1".into(),
                branch: "agent/test".into(),
                path: worktree.clone(),
            },
            base: "a".repeat(40),
            handoff,
        };
        let receipt = crate::delivery::Receipt {
            id: "approval-1".into(),
            base: preview.base.clone(),
            preview,
        };
        manager
            .receipts
            .prepare(crate::delivery::ReceiptBinding {
                approval_id: receipt.id.clone(),
                source_workspace: receipt.preview.source_workspace.clone(),
                target_repository: receipt
                    .preview
                    .plan
                    .target
                    .repository
                    .to_string_lossy()
                    .into_owned(),
                target_common_dir: receipt
                    .preview
                    .plan
                    .target
                    .common_dir
                    .to_string_lossy()
                    .into_owned(),
                worktree: worktree.to_string_lossy().into_owned(),
                branch: receipt.preview.plan.branch.clone(),
                base: receipt.base.clone(),
                handoff_digest: receipt.preview.handoff.digest.clone(),
            })
            .expect("receipt");
        manager
            .receipts
            .begin_task("approval-1", "source-session", "target-session", &worktree)
            .expect("task");
        let prompt = Arc::new(Mutex::new(None));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        manager.service.runtimes.lock().unwrap().insert(
            receipt.id.clone(),
            Box::new(FakeRun {
                prompt: Arc::clone(&prompt),
                closed: Arc::clone(&closed),
            }),
        );

        manager
            .service
            .finish_run(&receipt, &CancellationToken::new())
            .await
            .expect("terminal lifecycle");

        assert_eq!(
            prompt.lock().unwrap().as_deref(),
            Some(
                "Delivery objective:\nShip frozen design\n\nFrozen source handoff:\n\n## source-ref-1\nSelected design text"
            )
        );
        assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(manager.service.runtimes.lock().unwrap().is_empty());
        let status = manager
            .status("source-session", "approval-1")
            .unwrap()
            .unwrap();
        assert_eq!(status.state, crate::delivery::TaskState::Succeeded);
        assert_eq!(status.result, "target completed the frozen handoff");
        let messages = source.messages();
        assert_eq!(messages.len(), 1);
        let text = messages[0].text();
        assert!(text.contains("target completed the frozen handoff"));
        assert!(!text.contains(&startup_path.to_string_lossy().to_string()));
        assert!(!text.contains(&worktree.to_string_lossy().to_string()));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn delivery_source_notification_is_once_only_and_capability_free() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let startup_host = dummy_host(&startup_path, sessions.path());
        let shared = Arc::clone(&startup_host.builder.shared);
        let workspaces = Arc::new(Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host,
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        });
        let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
        let manager = Arc::new(
            DeliveryManager::open(
                &home.path().to_string_lossy(),
                &shared,
                runtime,
                CancellationToken::new(),
            )
            .expect("manager"),
        );
        manager.attach_workspaces(&workspaces);
        let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
            id: "source-session".into(),
            workspace: "/source".into(),
            ..otto_core::session::Header::default()
        });
        manager
            .bind_source_session("approval-1", source.clone())
            .expect("bind source");
        let worktree = startup_path.join(".otto/delivery-worktrees/delivery-1");
        let binding = crate::delivery::ReceiptBinding {
            approval_id: "approval-1".into(),
            source_workspace: "/source".into(),
            target_repository: startup_path.to_string_lossy().into_owned(),
            target_common_dir: startup_path.join(".git").to_string_lossy().into_owned(),
            worktree: worktree.to_string_lossy().into_owned(),
            branch: "agent/test".into(),
            base: "a".repeat(40),
            handoff_digest: "b".repeat(64),
        };
        manager.receipts.prepare(binding).expect("receipt");
        manager
            .receipts
            .begin_task("approval-1", "source-session", "target-session", &worktree)
            .expect("task");
        let task = manager
            .receipts
            .finish_task(
                "approval-1",
                crate::delivery::TaskState::Succeeded,
                "target work is complete",
                "",
            )
            .expect("terminal task");
        manager
            .service
            .notify_source(&task)
            .await
            .expect("notify once");
        manager
            .service
            .notify_source(&task)
            .await
            .expect("notify twice");
        let messages = source.messages();
        assert_eq!(messages.len(), 1);
        let text = messages[0].text();
        assert!(text.contains("Delivery task approval-1 completed"));
        assert!(text.contains("target work is complete"));
        assert!(!text.contains(&startup_path.to_string_lossy().to_string()));
        assert!(!text.contains(&worktree.to_string_lossy().to_string()));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn delivery_tool_factory_is_fresh_and_unadmitted_preflight_has_no_source_binding() {
        use crate::runtime::builder::ParentToolFactory;

        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let startup_host = dummy_host(&startup_path, sessions.path());
        let shared = Arc::clone(&startup_host.builder.shared);
        let workspaces = Arc::new(Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host,
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        });
        let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
        let manager = Arc::new(
            DeliveryManager::open(
                &home.path().to_string_lossy(),
                &shared,
                runtime,
                CancellationToken::new(),
            )
            .expect("manager"),
        );
        manager.attach_workspaces(&workspaces);
        let factory = DeliveryToolFactory::new(Arc::clone(&manager));
        let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
            id: "source-session".into(),
            workspace: "/source".into(),
            ..otto_core::session::Header::default()
        });
        let context = crate::runtime::builder::ParentToolContext {
            workspace: "/source".into(),
            session_id: "source-session".into(),
            source_session: source,
        };
        let first = factory.tools(&context).expect("first tools");
        let second = factory.tools(&context).expect("second tools");
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].definition().name, crate::delivery::TOOL_NAME);
        assert_eq!(second[0].definition().name, crate::delivery::TOOL_NAME);

        let outside = tempfile::tempdir().expect("outside");
        let arguments = crate::tool::testutil::raw(&format!(
            r#"{{"target":"{}","objective":"ship"}}"#,
            outside.path().to_string_lossy()
        ));
        let error = match first[0]
            .approval_spec_async(&arguments, &CancellationToken::new())
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("unadmitted target must be rejected"),
        };
        assert!(error.content.contains("not an admitted workspace"));
        assert!(manager.service.source_sessions.lock().unwrap().is_empty());
        assert!(workspaces.loaded.lock().await.is_empty());
        manager.shutdown().await;
    }

    fn approval_for_delivery(
        id: &str,
        spec: &crate::tool::ApprovalSpec,
    ) -> crate::approval::Request {
        crate::approval::Request {
            options: Vec::new(),
            schema_version: 1,
            id: id.into(),
            revision: 1,
            kind: crate::approval::Kind::Authorization,
            scope: crate::approval::Scope {
                workspace: "/source".into(),
                session_id: "source-session".into(),
                task_id: String::new(),
                run_id: String::new(),
            },
            subject: crate::approval::Subject {
                tool_call_id: "delivery-call".into(),
                source: "delivery".into(),
                id: "op_delivery".into(),
                version: spec.version.clone(),
                details: spec.details.clone(),
            },
            title: spec.title.clone(),
            reason: spec.reason.clone(),
            persistent: false,
            blocking: true,
            created_at: 1,
            expires_at: Some(2),
            state: crate::approval::State::Approved,
            decision: None,
            application: crate::approval::ApplicationState::Applying,
            application_error: String::new(),
        }
    }

    #[tokio::test]
    async fn delivery_factory_complete_controlled_flow_persists_status_notifies_and_closes() {
        use crate::runtime::builder::ParentToolFactory;

        struct GitReplies(Mutex<Vec<String>>);
        #[async_trait]
        impl crate::sandbox::CommandExecutor for GitReplies {
            async fn execute(
                &self,
                _request: crate::sandbox::Request,
                mut streams: crate::sandbox::Streams<'_>,
                _cancel: &CancellationToken,
            ) -> (
                crate::sandbox::ExitStatus,
                Result<(), crate::sandbox::Error>,
            ) {
                let output = self.0.lock().unwrap().remove(0);
                let _ = std::io::Write::write_all(&mut streams.stdout, output.as_bytes());
                (crate::sandbox::ExitStatus::default(), Ok(()))
            }
        }

        struct ReadyRun {
            prompt: Arc<Mutex<Option<String>>>,
            closed: Arc<std::sync::atomic::AtomicBool>,
            worktree: String,
        }
        #[async_trait]
        impl DeliveryRun for ReadyRun {
            fn target_session(&self) -> String {
                "target-session".into()
            }
            fn worktree(&self) -> String {
                self.worktree.clone()
            }
            async fn run_frozen(
                &self,
                prompt: &str,
                _cancel: &CancellationToken,
            ) -> Result<String, String> {
                *self.prompt.lock().unwrap() = Some(prompt.into());
                Ok("completed controlled target work".into())
            }
            async fn close(self: Box<Self>) {
                self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let home = tempfile::tempdir().expect("home");
        let target = tempfile::tempdir().expect("target");
        let target_path = canonical_directory(target.path()).expect("canonical target");
        let git_dir = target_path.join(".git");
        std::fs::create_dir(&git_dir).expect("git dir");
        let target_text = format!("{}\n", target_path.display());
        let git_text = format!("{}\n", git_dir.display());
        let head = format!("{}\n", "a".repeat(40));
        let replies = Arc::new(GitReplies(Mutex::new(vec![
            target_text.clone(),
            git_text.clone(),
            git_text.clone(),
            head.clone(),
            target_text,
            git_text.clone(),
            git_text,
            head,
        ])));
        let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
        let mut builder = crate::cli::testutil::builder(&target_path, home.path());
        builder.command_executor = Some(replies as Arc<dyn crate::sandbox::CommandExecutor>);
        builder.sandbox_environment = Some(Vec::new());
        let host = Arc::new(WorkspaceHost {
            builder: Arc::new(builder),
            workflows: None,
            delivery_setup: None,
            reloader: None,
        });
        let workspaces = Arc::new(Workspaces {
            startup: target_path.to_string_lossy().into_owned(),
            startup_host: Arc::clone(&host),
            roots: Vec::new(),
            config_path: shared.config_path.clone(),
            loaded: tokio::sync::Mutex::new(BTreeMap::from([(
                target_path.to_string_lossy().into_owned(),
                host,
            )])),
        });
        let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
        let manager = Arc::new(
            DeliveryManager::open(
                &home.path().to_string_lossy(),
                &shared,
                runtime,
                CancellationToken::new(),
            )
            .expect("manager"),
        );
        manager.attach_workspaces(&workspaces);
        let factory = DeliveryToolFactory::new(Arc::clone(&manager));
        let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
            id: "source-session".into(),
            workspace: "/source".into(),
            ..otto_core::session::Header::default()
        });
        let context = crate::runtime::builder::ParentToolContext {
            workspace: "/source".into(),
            session_id: "source-session".into(),
            source_session: source.clone(),
        };
        let tool = factory
            .tools(&context)
            .expect("tool")
            .pop()
            .expect("delivery tool");
        let arguments = crate::tool::testutil::raw(&format!(
            r#"{{"target":"{}","objective":"ship frozen","source_refs":["design"]}}"#,
            target_path.to_string_lossy()
        ));
        let spec = tool
            .approval_spec_async(&arguments, &CancellationToken::new())
            .await
            .expect("preflight")
            .expect("spec");
        let approval = approval_for_delivery("approval-1", &spec);
        let worktree = target_path.join(".otto/delivery-worktrees").join(
            serde_json::from_str::<serde_json::Value>(&spec.details).unwrap()["worktree"]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap(),
        );
        let prompt = Arc::new(Mutex::new(None));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        manager
            .service
            .inject_ready_run(
                "approval-1",
                Box::new(ReadyRun {
                    prompt: Arc::clone(&prompt),
                    closed: Arc::clone(&closed),
                    worktree: worktree.to_string_lossy().into_owned(),
                }),
            )
            .expect("inject ready run");
        tool.apply_approval(&arguments, &CancellationToken::new(), &approval)
            .await
            .expect("apply approval");
        let operation_id = serde_json::from_str(r#""op_delivery""#).unwrap();
        let result = tool
            .execute_approved(
                otto_core::tool::ToolCall {
                    tool_call_id: "call",
                    operation_id: &operation_id,
                    name: crate::delivery::TOOL_NAME,
                    arguments: &arguments,
                    attempt: 1,
                },
                &CancellationToken::new(),
                &approval,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            prompt.lock().unwrap().as_deref(),
            Some(
                "Delivery objective:\nship frozen\n\nFrozen source handoff:\n\n## source-ref-1\ndesign"
            )
        );
        assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
        let status = manager
            .status("source-session", "approval-1")
            .unwrap()
            .unwrap();
        assert_eq!(status.state, crate::delivery::TaskState::Succeeded);
        assert_eq!(status.result, "completed controlled target work");
        let message = source.messages().pop().expect("source notice").text();
        assert!(message.contains("completed controlled target work"));
        assert!(!message.contains(&target_path.to_string_lossy().to_string()));
        assert!(!message.contains(&worktree.to_string_lossy().to_string()));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn delivery_factory_preflight_and_apply_bind_source_before_effect_capability() {
        use crate::runtime::builder::ParentToolFactory;

        struct GitReplies(Mutex<Vec<String>>);
        #[async_trait]
        impl crate::sandbox::CommandExecutor for GitReplies {
            async fn execute(
                &self,
                _request: crate::sandbox::Request,
                streams: crate::sandbox::Streams<'_>,
                _cancel: &CancellationToken,
            ) -> (
                crate::sandbox::ExitStatus,
                Result<(), crate::sandbox::Error>,
            ) {
                let output = self.0.lock().unwrap().remove(0);
                let _ = streams.stdout.write_all(output.as_bytes());
                (crate::sandbox::ExitStatus::default(), Ok(()))
            }
        }

        let home = tempfile::tempdir().expect("home");
        let target = tempfile::tempdir().expect("target");
        let target_path = canonical_directory(target.path()).expect("canonical target");
        let git_dir = target_path.join(".git");
        std::fs::create_dir(&git_dir).expect("git directory");
        let target_text = format!("{}\n", target_path.display());
        let git_text = format!("{}\n", git_dir.display());
        let head = format!("{}\n", "a".repeat(40));
        let replies = Arc::new(GitReplies(Mutex::new(vec![
            target_text.clone(),
            git_text.clone(),
            git_text.clone(),
            head.clone(),
            target_text,
            git_text.clone(),
            git_text,
            head,
        ])));
        let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
        let mut builder = crate::cli::testutil::builder(&target_path, home.path());
        builder.command_executor = Some(replies as Arc<dyn crate::sandbox::CommandExecutor>);
        builder.sandbox_environment = Some(Vec::new());
        let host = Arc::new(WorkspaceHost {
            builder: Arc::new(builder),
            workflows: None,
            delivery_setup: None,
            reloader: None,
        });
        let workspaces = Arc::new(Workspaces {
            startup: target_path.to_string_lossy().into_owned(),
            startup_host: Arc::clone(&host),
            roots: Vec::new(),
            config_path: shared.config_path.clone(),
            loaded: tokio::sync::Mutex::new(BTreeMap::from([(
                target_path.to_string_lossy().into_owned(),
                host,
            )])),
        });
        let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
        let manager = Arc::new(
            DeliveryManager::open(
                &home.path().to_string_lossy(),
                &shared,
                runtime,
                CancellationToken::new(),
            )
            .expect("manager"),
        );
        manager.attach_workspaces(&workspaces);
        let factory = DeliveryToolFactory::new(Arc::clone(&manager));
        let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
            id: "source-session".into(),
            workspace: "/source".into(),
            ..otto_core::session::Header::default()
        });
        let context = crate::runtime::builder::ParentToolContext {
            workspace: "/source".into(),
            session_id: "source-session".into(),
            source_session: source,
        };
        let tool = factory
            .tools(&context)
            .expect("tool")
            .pop()
            .expect("delivery tool");
        let arguments = crate::tool::testutil::raw(&format!(
            r#"{{"target":"{}","objective":"ship fixture"}}"#,
            target_path.to_string_lossy()
        ));
        let spec = tool
            .approval_spec_async(&arguments, &CancellationToken::new())
            .await
            .expect("preflight")
            .expect("approval spec");
        let approval = approval_for_delivery("approval-1", &spec);
        let error = tool
            .apply_approval(&arguments, &CancellationToken::new(), &approval)
            .await
            .expect_err("no elevated effect capability in fixture");
        assert!(error.contains("constrained worktree setup capability"));
        assert!(
            manager
                .service
                .source_sessions
                .lock()
                .unwrap()
                .contains_key("approval-1")
        );
        assert!(
            manager
                .receipts
                .get("approval-1")
                .expect("receipt read")
                .is_some()
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn delivery_manager_owns_private_receipts_status_and_shutdown() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let startup_host = dummy_host(&startup_path, sessions.path());
        let shared = Arc::clone(&startup_host.builder.shared);
        let workspaces = Arc::new(Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host,
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        });
        let runtime = crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder);
        let manager = DeliveryManager::open(
            &home.path().to_string_lossy(),
            &shared,
            runtime,
            CancellationToken::new(),
        )
        .expect("manager");
        manager.attach_workspaces(&workspaces);
        let source = crate::runtime::builder::SharedSession::memory(otto_core::session::Header {
            id: "source-session".into(),
            workspace: "/source".into(),
            ..otto_core::session::Header::default()
        });
        manager
            .bind_source_session("approval-1", source)
            .expect("bind source");
        assert!(
            manager
                .service
                .source_sessions
                .lock()
                .unwrap()
                .contains_key("approval-1")
        );
        let binding = crate::delivery::ReceiptBinding {
            approval_id: "approval-1".into(),
            source_workspace: "/source".into(),
            target_repository: startup_path.to_string_lossy().into_owned(),
            target_common_dir: startup_path.join(".git").to_string_lossy().into_owned(),
            worktree: startup_path
                .join(".otto/delivery-worktrees/delivery-1")
                .to_string_lossy()
                .into_owned(),
            branch: "agent/test".into(),
            base: "a".repeat(40),
            handoff_digest: "b".repeat(64),
        };
        manager.receipts.prepare(binding).expect("receipt");
        manager
            .receipts
            .begin_task(
                "approval-1",
                "source-session",
                "target-session",
                &startup_path.join(".otto/delivery-worktrees/delivery-1"),
            )
            .expect("task");
        assert_eq!(
            manager
                .status("source-session", "approval-1")
                .expect("status")
                .expect("task")
                .state,
            crate::delivery::TaskState::Running
        );
        assert!(home.path().join(".otto/delivery-receipts.db").is_file());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn the_delivery_factory_is_installed_exactly_once_for_later_parent_runners() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let host = dummy_host(&startup_path, sessions.path());
        let shared = Arc::clone(&host.builder.shared);

        assert!(
            !shared.parent_tool_factory.installed(),
            "no frontend tool is present before serve installs one"
        );

        // Serve's order: the manager opens, the slot is installed, and every
        // later parent runner -- the workflow controller included -- composes
        // the same tools.
        let manager = Arc::new(
            DeliveryManager::open(
                &home.path().to_string_lossy(),
                &shared,
                crate::cli::testutil::initial_runtime(&host.builder),
                CancellationToken::new(),
            )
            .expect("manager"),
        );
        assert!(
            shared
                .parent_tool_factory
                .install(Arc::new(DeliveryToolFactory::new(Arc::clone(&manager))))
        );
        assert!(shared.parent_tool_factory.installed());
        assert!(
            !shared
                .parent_tool_factory
                .install(Arc::new(DeliveryToolFactory::new(Arc::clone(&manager)))),
            "a second installation cannot add a second frontend"
        );
    }

    #[tokio::test]
    async fn delivery_broker_refuses_unadmitted_target_before_loading_a_host() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let outside = tempfile::tempdir().expect("outside");
        let startup_host = dummy_host(&startup_path, sessions.path());
        let shared = Arc::clone(&startup_host.builder.shared);
        let workspaces = Arc::new(Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host,
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        });
        let broker = DeliveryBroker::new(
            &shared,
            crate::cli::testutil::initial_runtime(&workspaces.startup_host.builder),
            CancellationToken::new(),
        );
        broker.attach_workspaces(&workspaces);

        let error = crate::delivery::Preflight::preview(
            &broker,
            "/source",
            &crate::delivery::Request {
                target: outside.path().to_string_lossy().into_owned(),
                objective: "ship".into(),
                branch: None,
                source_refs: Vec::new(),
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert_eq!(error, "delivery target is not an admitted workspace");
        assert!(workspaces.loaded.lock().await.is_empty());
    }

    #[test]
    fn workspaces_admit_reads_startup_and_roots() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let root = tempfile::tempdir().expect("root");
        let root_path = canonical_directory(root.path()).expect("canonical");
        let workspaces = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: vec![root_path.clone()],
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        assert!(workspaces.admit(&startup_path.to_string_lossy()).is_ok());
        assert_eq!(
            workspaces.admit(&root_path.to_string_lossy()),
            Ok(root_path)
        );
    }

    #[test]
    fn a_directory_trusted_after_startup_is_admitted_without_a_restart() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let config_dir = tempfile::tempdir().expect("config dir");
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, b"").expect("write empty config");
        let trusted = tempfile::tempdir().expect("trusted");
        let trusted_path = canonical_directory(trusted.path()).expect("canonical");
        let workspaces = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: Vec::new(),
            config_path: config_path.clone(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        assert_eq!(
            workspaces.admit(&trusted_path.to_string_lossy()),
            Err(Admission::NotAdmitted)
        );

        std::fs::write(
            &config_path,
            format!(
                "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                trusted_path.display()
            ),
        )
        .expect("trust it");

        assert_eq!(
            workspaces.admit(&trusted_path.to_string_lossy()),
            Ok(trusted_path)
        );
    }

    fn factory_with(
        startup: &Path,
        sessions: &Path,
        roots: Vec<PathBuf>,
        config_path: PathBuf,
    ) -> ServeFactory {
        let startup_host = dummy_host(startup, sessions);
        let runtime = crate::cli::testutil::initial_runtime(&startup_host.builder);
        ServeFactory {
            workspaces: Arc::new(Workspaces {
                startup: startup.to_string_lossy().into_owned(),
                startup_host,
                roots,
                config_path,
                loaded: tokio::sync::Mutex::new(BTreeMap::new()),
            }),
            delivery: None,
            runtime,
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn trusting_a_workspace_writes_the_config_so_it_is_admitted() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let config_dir = tempfile::tempdir().expect("config dir");
        let config_path = config_dir.path().join("config.toml");
        let picked = tempfile::tempdir().expect("picked");
        let picked_path = canonical_directory(picked.path()).expect("canonical");
        let factory = factory_with(
            &startup_path,
            sessions.path(),
            Vec::new(),
            config_path.clone(),
        );

        factory
            .trust_workspace(&picked_path.to_string_lossy())
            .await
            .expect("trusted");

        assert_eq!(
            factory.workspaces.admit(&picked_path.to_string_lossy()),
            Ok(picked_path)
        );
        assert!(
            std::fs::read_to_string(&config_path)
                .expect("config")
                .contains("trust_level = \"trusted\"")
        );
    }

    #[tokio::test]
    async fn trusting_a_missing_directory_is_invalid() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let config_dir = tempfile::tempdir().expect("config dir");
        let factory = factory_with(
            &startup_path,
            sessions.path(),
            Vec::new(),
            config_dir.path().join("config.toml"),
        );

        let got = factory
            .trust_workspace(&startup_path.join("missing").to_string_lossy())
            .await;

        assert!(
            matches!(got, Err(server::WorkspaceLoadError::Invalid(_))),
            "{got:?}"
        );
    }

    #[test]
    fn browse_roots_are_the_home_directory_then_the_configured_roots() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let root = tempfile::tempdir().expect("root");
        let root_path = canonical_directory(root.path()).expect("canonical");
        // `testutil::builder` uses the workspace directory as home.
        let factory = factory_with(
            &startup_path,
            sessions.path(),
            vec![root_path.clone()],
            PathBuf::new(),
        );

        assert_eq!(factory.browse_roots(), vec![startup_path, root_path]);
    }

    #[test]
    fn a_trusted_path_that_no_longer_exists_is_skipped() {
        let holder = tempfile::tempdir().expect("holder");
        let target = holder.path().join("gone");
        std::fs::create_dir(&target).expect("create");
        let target_path = canonical_directory(&target).expect("canonical");
        let config_dir = tempfile::tempdir().expect("config dir");
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                target_path.display()
            ),
        )
        .expect("write config");
        std::fs::remove_dir(&target).expect("remove");

        assert_eq!(trusted_projects(&config_path), Vec::<PathBuf>::new());
    }

    /// Every concurrent caller loading the same path gets the same host, and
    /// only one of them built it: the registry mutex is held across the
    /// build, so a caller that loses the race sees the path already loaded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_builds_the_workspace_once_under_concurrent_callers() {
        let shared_root = tempfile::tempdir().expect("shared root");
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let canonical = canonical_directory(workspace.path()).expect("canonical");
        let shared = crate::cli::testutil::shared_with_offline_sandbox(shared_root.path());
        let runtime = crate::runtime::builder::resolve_initial_runtime(
            &shared.config,
            &shared.environment,
            None,
            &shared.overrides,
        )
        .expect("runtime");
        let workspaces = Arc::new(Workspaces {
            startup: shared.home.clone(),
            startup_host: dummy_host(Path::new(&shared.home), sessions.path()),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        });
        let cancel = CancellationToken::new();

        let mut handles = Vec::new();
        for _ in 0..4 {
            let workspaces = Arc::clone(&workspaces);
            let shared = Arc::clone(&shared);
            let runtime = runtime.clone();
            let cancel = cancel.clone();
            let canonical = canonical.clone();
            handles.push(tokio::spawn(async move {
                let mut stderr = Vec::new();
                workspaces
                    .load(&canonical, &shared, &runtime, &cancel, &mut stderr, false)
                    .await
                    .expect("load")
            }));
        }
        let mut newly_loaded_count = 0;
        let mut hosts = Vec::new();
        for handle in handles {
            let (host, newly_loaded) = handle.await.expect("join");
            if newly_loaded {
                newly_loaded_count += 1;
            }
            hosts.push(host);
        }
        assert_eq!(newly_loaded_count, 1, "exactly one caller builds the host");
        for host in &hosts[1..] {
            assert!(Arc::ptr_eq(&hosts[0], host));
        }
    }

    /// A failed load (here: the workspace directory does not exist) leaves
    /// the registry untouched, so a later, valid load of the same path is
    /// still `newly_loaded`.
    #[tokio::test]
    async fn a_failed_load_leaves_the_registry_unchanged() {
        let shared_root = tempfile::tempdir().expect("shared root");
        let sessions = tempfile::tempdir().expect("sessions");
        let missing = shared_root.path().join("does-not-exist");
        let shared = crate::cli::testutil::shared_with_offline_sandbox(shared_root.path());
        let runtime = crate::runtime::builder::resolve_initial_runtime(
            &shared.config,
            &shared.environment,
            None,
            &shared.overrides,
        )
        .expect("runtime");
        let workspaces = Workspaces {
            startup: shared.home.clone(),
            startup_host: dummy_host(Path::new(&shared.home), sessions.path()),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        let cancel = CancellationToken::new();
        let mut stderr = Vec::new();

        let error = match workspaces
            .load(&missing, &shared, &runtime, &cancel, &mut stderr, false)
            .await
        {
            Ok(_) => panic!("a missing directory must fail to load"),
            Err(error) => error,
        };
        assert!(!error.is_empty());
        assert!(workspaces.loaded.lock().await.is_empty());

        std::fs::create_dir(&missing).expect("create the workspace directory");
        let (_, newly_loaded) = workspaces
            .load(&missing, &shared, &runtime, &cancel, &mut stderr, false)
            .await
            .expect("load after fixing the path");
        assert!(newly_loaded);
    }

    /// A session created for a named, already-loaded workspace uses that
    /// workspace's own `Builder`: its `Controller::workspace()` and its
    /// file-tool `Workspace` root are both that workspace, not the startup
    /// one.
    #[tokio::test]
    async fn create_in_a_named_workspace_uses_that_workspaces_builder() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let startup_sessions = tempfile::tempdir().expect("startup sessions");
        let startup_host = dummy_host(&startup_path, startup_sessions.path());

        let other = tempfile::tempdir().expect("other workspace");
        let other_path = canonical_directory(other.path()).expect("canonical");
        let other_sessions = tempfile::tempdir().expect("other sessions");
        let shared = crate::cli::testutil::shared_with_offline_sandbox(other_sessions.path());
        std::fs::write(&shared.config_path, "[sandbox]\ndriver='off'\n").expect("config");
        let (mut builder, sandbox) = super::super::run::load_workspace(
            shared,
            &other_path,
            &CancellationToken::new(),
            &mut Vec::new(),
        )
        .await
        .expect("load workspace B");
        Arc::make_mut(&mut builder.shared)
            .config
            .profiles
            .get_mut("alpha")
            .unwrap()
            .model = "workspace-b-model".into();
        assert!(
            builder.grant_control.is_some(),
            "loaded workspace wires persistent read grants"
        );
        let other_host = Arc::new(WorkspaceHost {
            builder: Arc::new(builder),
            workflows: None,
            delivery_setup: match (sandbox.approval_executor, sandbox.approval_environment) {
                (Some(executor), Some(environment)) => Some(DeliverySetupCapability {
                    executor,
                    environment,
                }),
                _ => None,
            },
            reloader: sandbox.reloader,
        });

        let mut loaded = BTreeMap::new();
        loaded.insert(
            other_path.to_string_lossy().into_owned(),
            Arc::clone(&other_host),
        );
        let runtime = crate::cli::testutil::initial_runtime(&startup_host.builder);
        let factory = ServeFactory {
            workspaces: Arc::new(Workspaces {
                startup: startup_path.to_string_lossy().into_owned(),
                startup_host,
                roots: Vec::new(),
                config_path: PathBuf::new(),
                loaded: tokio::sync::Mutex::new(loaded),
            }),
            delivery: None,
            runtime,
            cancel: CancellationToken::new(),
        };

        let controller = factory
            .create(Some(&other_path.to_string_lossy()))
            .await
            .expect("create in workspace B");

        assert_eq!(controller.workspace(), other_path.to_string_lossy());
        assert_eq!(other_host.builder.workspace.root(), other_path);
        assert_eq!(controller.info().model, "workspace-b-model");
        controller
            .reload_sandbox()
            .await
            .expect("reload B when startup has no reloader");
        controller.close_mcp().await;
        controller.close().expect("close");
        other_host
            .reloader
            .as_ref()
            .unwrap()
            .control
            .close()
            .await
            .expect("close sandbox");
    }

    fn factory_over(startup_path: &Path, sessions: &Path) -> (ServeFactory, Arc<WorkspaceHost>) {
        let startup_host = dummy_host(startup_path, sessions);
        let runtime = crate::cli::testutil::initial_runtime(&startup_host.builder);
        let factory = ServeFactory {
            workspaces: Arc::new(Workspaces {
                startup: startup_path.to_string_lossy().into_owned(),
                startup_host: Arc::clone(&startup_host),
                roots: Vec::new(),
                config_path: PathBuf::new(),
                loaded: tokio::sync::Mutex::new(BTreeMap::from([(
                    startup_path.to_string_lossy().into_owned(),
                    Arc::clone(&startup_host),
                )])),
            }),
            delivery: None,
            runtime,
            cancel: CancellationToken::new(),
        };
        (factory, startup_host)
    }

    /// Resume by id reads `<session directory>/<id>.jsonl` directly, so a
    /// session older than the newest `MAX_LIST_SESSIONS` still opens.
    #[tokio::test]
    async fn opening_by_id_finds_a_session_outside_the_newest_listing_window() {
        let workspace = tempfile::tempdir().expect("workspace");
        let workspace_path = canonical_directory(workspace.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let (factory, host) = factory_over(&workspace_path, sessions.path());

        use otto_core::session::Session;
        let runtime = crate::cli::testutil::initial_runtime(&host.builder);
        let mut oldest = String::new();
        for index in 0..=MAX_LIST_SESSIONS {
            let session = host.builder.create_session(&runtime).expect("session");
            session
                .append(crate::cli::testutil::user("hello"))
                .await
                .expect("persist the first message");
            let path = PathBuf::from(session.path());
            if index == 0 {
                oldest = path
                    .file_stem()
                    .expect("file stem")
                    .to_string_lossy()
                    .into_owned();
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .expect("session file")
                    .set_modified(
                        std::time::SystemTime::now() - std::time::Duration::from_secs(86_400),
                    )
                    .expect("age the file");
            }
        }
        let listed = listed_for(&host.builder).expect("listing");
        assert_eq!(listed.sessions.len(), MAX_LIST_SESSIONS);
        assert!(listed.sessions.iter().all(|entry| entry.id != oldest));

        let reopened = factory.open(&oldest, None).await.expect("open by id");
        assert_eq!(reopened.info().session_id, oldest);
    }

    /// An id that is not 32 lowercase hexadecimal characters answers "not
    /// found" before it is used as a file name.
    #[tokio::test]
    async fn opening_a_non_hex_id_is_not_found() {
        let workspace = tempfile::tempdir().expect("workspace");
        let workspace_path = canonical_directory(workspace.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let (factory, _host) = factory_over(&workspace_path, sessions.path());
        for id in [
            "../../etc/passwd",
            "some-id",
            &"A".repeat(32),
            &"a".repeat(31),
            &"a".repeat(33),
        ] {
            let error = match factory.open(id, None).await {
                Ok(_) => panic!("{id} opened"),
                Err(error) => error,
            };
            assert_eq!(error, SESSION_NOT_FOUND, "{id}");
        }
        assert!(sessionfs::is_session_id(&"0123456789abcdef".repeat(2)));
    }

    #[test]
    fn a_nonexistent_root_is_a_startup_error_naming_the_key_and_path() {
        let error = canonicalize_workspace_roots(&["/does/not/exist".to_string()]).unwrap_err();
        assert!(error.contains("workspace_roots"), "{error}");
        assert!(error.contains("/does/not/exist"), "{error}");
    }

    #[test]
    fn valid_roots_canonicalize_in_order() {
        let a = tempfile::tempdir().expect("a");
        let b = tempfile::tempdir().expect("b");
        let canonical_a = canonical_directory(a.path()).expect("canonical");
        let canonical_b = canonical_directory(b.path()).expect("canonical");
        let roots = canonicalize_workspace_roots(&[
            a.path().to_string_lossy().into_owned(),
            b.path().to_string_lossy().into_owned(),
        ])
        .expect("canonicalize");
        assert_eq!(roots, vec![canonical_a, canonical_b]);
    }

    // ---- persisted workspace list ----

    fn runtime_for(shared: &Arc<crate::runtime::builder::Shared>) -> Runtime {
        crate::runtime::builder::resolve_initial_runtime(
            &shared.config,
            &shared.environment,
            None,
            &shared.overrides,
        )
        .expect("runtime")
    }

    /// Adding a workspace persists it, and a fresh registry over the same
    /// home (a restart) reloads it, so it shows up in `Factory::workspaces`
    /// the same way `GET /v1/workspaces` reports it.
    #[tokio::test]
    async fn a_persisted_workspace_reloads_at_startup_and_is_listed() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let roots_dir = tempfile::tempdir().expect("roots");
        let roots_path = canonical_directory(roots_dir.path()).expect("canonical");
        let workspace_path = roots_path.join("project");
        std::fs::create_dir(&workspace_path).expect("workspace directory");
        let sessions = tempfile::tempdir().expect("sessions");
        let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
        let runtime = runtime_for(&shared);
        let cancel = CancellationToken::new();
        let mut stderr = Vec::new();

        let first_run = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: vec![roots_path.clone()],
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        first_run
            .load(
                &workspace_path,
                &shared,
                &runtime,
                &cancel,
                &mut stderr,
                true,
            )
            .await
            .expect("load and persist");

        // The restart: a fresh registry seeded with only the startup
        // workspace, as `run` seeds it before reloading the persisted list.
        let restarted = Arc::new(Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: vec![roots_path],
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        });
        load_persisted_workspaces(&restarted, &shared, &runtime, &cancel, &mut stderr).await;
        let factory = ServeFactory {
            workspaces: restarted,
            delivery: None,
            runtime,
            cancel,
        };

        let listed = factory.workspaces().await.loaded;
        assert!(
            listed
                .iter()
                .any(|info| info.path == workspace_path.to_string_lossy()),
            "{listed:?}"
        );
    }

    /// A path saved while it was inside `workspace_roots` is skipped with a
    /// warning once a config change moves it outside; a path still inside
    /// loads normally.
    #[tokio::test]
    async fn a_listed_path_outside_current_roots_is_skipped_others_load() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let roots_dir = tempfile::tempdir().expect("roots");
        let roots_path = canonical_directory(roots_dir.path()).expect("canonical");
        let admitted = roots_path.join("project");
        std::fs::create_dir(&admitted).expect("admitted child");
        let outside = tempfile::tempdir().expect("outside");
        let outside_path = canonical_directory(outside.path()).expect("canonical");
        let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
        let runtime = runtime_for(&shared);
        let home_str = home.path().to_str().expect("utf-8 home");
        add_to_workspace_list(home_str, &admitted.to_string_lossy()).expect("add admitted");
        add_to_workspace_list(home_str, &outside_path.to_string_lossy()).expect("add outside");

        let workspaces = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: vec![roots_path],
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        let cancel = CancellationToken::new();
        let mut stderr = Vec::new();
        load_persisted_workspaces(&workspaces, &shared, &runtime, &cancel, &mut stderr).await;

        let loaded = workspaces.loaded.lock().await;
        assert!(loaded.contains_key(&admitted.to_string_lossy().into_owned()));
        assert!(!loaded.contains_key(&outside_path.to_string_lossy().into_owned()));
        drop(loaded);
        let warning = String::from_utf8(stderr).expect("utf-8");
        assert!(
            warning.contains(outside_path.to_string_lossy().as_ref()),
            "{warning}"
        );
        assert!(
            warning.contains("outside the startup workspace and configured roots"),
            "{warning}"
        );
    }

    /// A persisted path whose directory was since deleted is skipped with a
    /// warning; startup does not fail because of it.
    #[tokio::test]
    async fn a_deleted_persisted_directory_is_skipped_with_a_warning() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let missing = home.path().join("does-not-exist");
        let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
        let runtime = runtime_for(&shared);
        add_to_workspace_list(
            home.path().to_str().expect("utf-8 home"),
            &missing.to_string_lossy(),
        )
        .expect("add missing path");

        let workspaces = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        let cancel = CancellationToken::new();
        let mut stderr = Vec::new();
        load_persisted_workspaces(&workspaces, &shared, &runtime, &cancel, &mut stderr).await;

        assert!(workspaces.loaded.lock().await.is_empty());
        let warning = String::from_utf8(stderr).expect("utf-8");
        assert!(
            warning.contains(missing.to_string_lossy().as_ref()),
            "{warning}"
        );
        assert!(warning.contains("not an existing directory"), "{warning}");
    }

    /// An unparsable persisted file is a startup warning, not a startup
    /// failure, and startup never rewrites it: only a later successful add
    /// does.
    #[tokio::test]
    async fn an_unparsable_workspace_list_file_warns_and_is_left_unchanged() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
        let runtime = runtime_for(&shared);
        let home_str = home.path().to_str().expect("utf-8 home");
        std::fs::create_dir_all(home.path().join(".otto")).expect(".otto dir");
        let garbage = b"not json".to_vec();
        std::fs::write(workspace_list_path(home_str), &garbage).expect("write garbage");

        let workspaces = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        let cancel = CancellationToken::new();
        let mut stderr = Vec::new();
        load_persisted_workspaces(&workspaces, &shared, &runtime, &cancel, &mut stderr).await;

        assert!(workspaces.loaded.lock().await.is_empty());
        let warning = String::from_utf8(stderr).expect("utf-8");
        assert!(warning.contains("warning:"), "{warning}");
        let after = std::fs::read(workspace_list_path(home_str)).expect("read back");
        assert_eq!(after, garbage, "an unparsable file must be left untouched");
    }

    /// The startup workspace is loaded before `Workspaces` exists and is
    /// already in `loaded`, so `load` never reaches the persist step for it;
    /// adding a different path twice still leaves one entry.
    #[tokio::test]
    async fn the_startup_workspace_is_never_persisted_and_duplicate_adds_collapse() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let shared = crate::cli::testutil::shared_with_offline_sandbox(home.path());
        let runtime = runtime_for(&shared);
        let cancel = CancellationToken::new();
        let mut stderr = Vec::new();
        let startup_host = dummy_host(&startup_path, sessions.path());
        let workspaces = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: Arc::clone(&startup_host),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::from([(
                startup_path.to_string_lossy().into_owned(),
                startup_host,
            )])),
        };

        let (_, newly_loaded) = workspaces
            .load(&startup_path, &shared, &runtime, &cancel, &mut stderr, true)
            .await
            .expect("load the already-loaded startup workspace");
        assert!(!newly_loaded);
        assert!(
            !workspace_list_path(home.path().to_str().expect("utf-8 home")).exists(),
            "the startup workspace must never be written to the persisted list"
        );

        let other = tempfile::tempdir().expect("other");
        let other_path = canonical_directory(other.path()).expect("canonical");
        let other_string = other_path.to_string_lossy().into_owned();
        let home_str = home.path().to_str().expect("utf-8 home");
        add_to_workspace_list(home_str, &other_string).expect("add once");
        add_to_workspace_list(home_str, &other_string).expect("add twice");
        assert_eq!(
            read_workspace_list(home_str).expect("read"),
            vec![other_string]
        );
    }

    // ---- removing a workspace ----

    /// Removing a loaded, persisted workspace drops its host and its
    /// persisted-list entry in one call. The other entries keep their
    /// original order: `remove_from_workspace_list`'s `retain` never
    /// resorts, unlike `add_to_workspace_list`.
    #[tokio::test]
    async fn removing_a_loaded_and_persisted_workspace_unloads_it_and_updates_the_list() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let workspace = tempfile::tempdir().expect("workspace");
        let workspace_path = canonical_directory(workspace.path()).expect("canonical");
        let workspace_string = workspace_path.to_string_lossy().into_owned();
        let home_str = home.path().to_str().expect("utf-8 home");
        write_workspace_list(
            home_str,
            vec![
                "z-other".to_string(),
                workspace_string.clone(),
                "a-other".to_string(),
            ],
        )
        .expect("seed the persisted list");

        let mut loaded = BTreeMap::new();
        loaded.insert(
            workspace_string.clone(),
            dummy_host(&workspace_path, sessions.path()),
        );
        let workspaces = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(loaded),
        };
        let mut stderr = Vec::new();

        workspaces
            .remove(&workspace_string, home_str, &mut stderr)
            .await
            .expect("remove a loaded workspace");

        assert!(
            !workspaces
                .loaded
                .lock()
                .await
                .contains_key(&workspace_string)
        );
        assert_eq!(
            read_workspace_list(home_str).expect("read"),
            vec!["z-other".to_string(), "a-other".to_string()],
            "the other entries keep their original order"
        );
    }

    /// A path only in the persisted list, never loaded, is removed from the
    /// list; `loaded` is untouched.
    #[tokio::test]
    async fn removing_a_persisted_only_workspace_removes_it_from_the_list() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let home_str = home.path().to_str().expect("utf-8 home");
        add_to_workspace_list(home_str, "/some/persisted/path").expect("seed");
        add_to_workspace_list(home_str, "/other/path").expect("seed");

        let workspaces = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        let mut stderr = Vec::new();

        workspaces
            .remove("/some/persisted/path", home_str, &mut stderr)
            .await
            .expect("remove a persisted-only workspace");

        assert!(workspaces.loaded.lock().await.is_empty());
        assert_eq!(
            read_workspace_list(home_str).expect("read"),
            vec!["/other/path".to_string()]
        );
    }

    /// A path neither loaded nor in the persisted list is `NotFound`.
    #[tokio::test]
    async fn removing_an_unknown_path_answers_not_found() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let workspaces = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        let mut stderr = Vec::new();
        let home_str = home.path().to_str().expect("utf-8 home");

        let error = workspaces
            .remove("/never/added", home_str, &mut stderr)
            .await
            .expect_err("an unknown path must not be removable");
        assert!(matches!(error, server::WorkspaceRemoveError::NotFound));
    }

    /// The startup workspace can never be removed, even when it happens to
    /// also be in the persisted list.
    #[tokio::test]
    async fn removing_the_startup_workspace_answers_is_startup() {
        let home = tempfile::tempdir().expect("home");
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let sessions = tempfile::tempdir().expect("sessions");
        let workspaces = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: Vec::new(),
            config_path: PathBuf::new(),
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        let mut stderr = Vec::new();
        let home_str = home.path().to_str().expect("utf-8 home");

        let error = workspaces
            .remove(&startup_path.to_string_lossy(), home_str, &mut stderr)
            .await
            .expect_err("the startup workspace must not be removable");
        assert!(matches!(error, server::WorkspaceRemoveError::IsStartup));
    }
}
