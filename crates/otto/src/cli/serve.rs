//! `otto serve`: the HTTP composition root.
//!
//! Everything the REPL path builds is already in place when [`run`] is called;
//! this module only picks the listener and wires one [`Factory`] over the
//! shared [`Builder`]. The process sandbox arrives already behind the
//! [`SandboxSwitch`](super::sandbox_switch::SandboxSwitch) that `POST
//! /v1/sandbox/reload` replaces.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use otto_core::config::ServerRuntime;
use otto_core::config::resolve::Runtime;
use otto_core::config::resolve_feishu;
use otto_core::session::ListResult;
use tokio_util::sync::CancellationToken;

use crate::app::{Controller, SandboxControl};
use crate::inbound;
use crate::server::listen::{Listener, listen_tcp, listen_unix};
use crate::server::{self, Factory, Info, Options, SESSION_NOT_FOUND, Server};
use crate::session::{self as sessionfs, MAX_LIST_SESSIONS};

use super::info::SandboxInfo;
use super::runtime_builder::Builder;
use super::sandbox_runtime::canonical_directory;
use super::sandbox_switch::{SandboxReloader, SandboxSwitch};

/// Writes `otto: {message}\n` and returns exit code 1.
fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}

// ---- the workspace registry ----

/// One loaded workspace: its composition root, its own `/sandbox reload`
/// state, and, when the workflow lock was acquired, its workflow controller.
struct WorkspaceHost {
    builder: Arc<Builder>,
    workflows: Option<Arc<crate::workflow::Controller>>,
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
    loaded: tokio::sync::Mutex<BTreeMap<String, Arc<WorkspaceHost>>>,
}

impl Workspaces {
    fn startup_host(&self) -> &Arc<WorkspaceHost> {
        &self.startup_host
    }

    /// Whether `requested` may be opened: the startup workspace or a
    /// descendant of one of `self.roots`.
    fn admit(&self, requested: &str) -> Result<PathBuf, Admission> {
        admit_workspace(requested, Path::new(&self.startup), &self.roots)
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
        shared: &Arc<super::runtime_builder::Shared>,
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
        let (builder, reloader) =
            super::run::load_workspace(Arc::clone(shared), canonical, cancel, stderr).await?;
        let builder = Arc::new(builder);
        let workflows =
            match super::workflow::build_controller(Arc::clone(&builder), runtime, stderr).await {
                Ok(controller) => Some(controller),
                Err(error) => {
                    let redacted = builder.redact_error(&error, Some(runtime));
                    if !redacted.is_empty() {
                        let _ =
                            writeln!(stderr, "warning: workflows disabled for {path}: {redacted}");
                    }
                    None
                }
            };
        let host = Arc::new(WorkspaceHost {
            builder,
            workflows,
            reloader,
        });
        loaded.insert(path.clone(), Arc::clone(&host));
        if persist && let Err(error) = add_to_workspace_list(&shared.home, &path) {
            let _ = writeln!(stderr, "warning: cannot save workspace list: {error}");
        }
        Ok((host, true))
    }
}

// ---- admission ----

/// Why a requested workspace path was rejected.
#[derive(Debug, PartialEq, Eq)]
enum Admission {
    /// The path does not resolve to an existing directory.
    Invalid,
    /// A real directory, but neither the startup workspace nor a descendant
    /// of any `workspace_roots` entry.
    NotAdmitted,
}

/// Canonicalizes `requested` and admits it when it is `startup` or a
/// descendant of one entry in `roots` (a root itself is admitted).
/// `canonical_directory` resolves symlinks before the comparison, so a link
/// inside a root that points outside it is rejected as `NotAdmitted`, and a
/// missing path or a file is rejected as `Invalid`. The comparison is by path
/// component (`Path::starts_with`), so `/root-x` is not a descendant of
/// `/root`. `requested` must already be absolute: it arrives over HTTP, so
/// resolving a relative path against the server process's working directory
/// would let a client name a directory it never typed.
fn admit_workspace(
    requested: &str,
    startup: &Path,
    roots: &[PathBuf],
) -> Result<PathBuf, Admission> {
    if !Path::new(requested).is_absolute() {
        return Err(Admission::Invalid);
    }
    let canonical = canonical_directory(Path::new(requested)).map_err(|_| Admission::Invalid)?;
    if canonical == startup || roots.iter().any(|root| canonical.starts_with(root)) {
        Ok(canonical)
    } else {
        Err(Admission::NotAdmitted)
    }
}

/// Canonicalizes `[server].workspace_roots` once at startup. A root that does
/// not resolve to an existing directory is a startup error naming the config
/// key and the offending path, not a per-request admission failure.
fn canonicalize_workspace_roots(raw_roots: &[String]) -> Result<Vec<PathBuf>, String> {
    raw_roots
        .iter()
        .map(|root| {
            canonical_directory(Path::new(root))
                .map_err(|error| format!("[server] workspace_roots: \"{root}\": {error}"))
        })
        .collect()
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

/// Adds `path` to the persisted workspace list (sorted, deduplicated) and
/// writes it through a temporary file in `~/.otto` followed by `rename`, so a
/// reader never observes a partial file. A currently unparsable file is
/// treated as an empty list rather than preserved, matching the spec's "the
/// next successful add ... rewrites it from the empty list plus the new
/// path".
fn add_to_workspace_list(home: &str, path: &str) -> std::io::Result<()> {
    let mut workspaces = read_workspace_list(home).unwrap_or_default();
    if !workspaces.iter().any(|entry| entry == path) {
        workspaces.push(path.to_string());
        workspaces.sort();
    }
    let directory = Path::new(home).join(".otto");
    std::fs::create_dir_all(&directory)?;
    let json = serde_json::to_vec(&WorkspaceListFile { workspaces })
        .expect("a list of strings always serializes");
    let suffix = super::runtime_builder::random_id()?;
    let temp = directory.join(format!(".serve-workspaces-{suffix}.json"));
    std::fs::write(&temp, &json)?;
    std::fs::rename(&temp, workspace_list_path(home))
}

/// Loads every path in the persisted workspace list into `workspaces`, after
/// the startup workspace is already loaded. A path that fails admission or
/// fails to load is skipped with a stderr warning naming the path and the
/// reason; it stays in the file for the next start to retry. Called once at
/// startup, so it never persists what it loads back to the file (those paths
/// are already in it).
async fn load_persisted_workspaces(
    workspaces: &Workspaces,
    shared: &Arc<super::runtime_builder::Shared>,
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

// ---- the session factory ----

/// Builds one [`Controller`] per server-side session on top of the same
/// replacement plumbing the CLI's `/new` and `/resume` use.
struct ServeFactory {
    workspaces: Workspaces,
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

    fn wire(&self, controller: Controller) -> Controller {
        match &self.workspaces.startup_host().reloader {
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
    async fn create(&self, workspace: Option<&str>) -> Result<Controller, String> {
        let host = match workspace {
            Some(path) => self
                .workspaces
                .host(path)
                .await
                .ok_or_else(|| format!("workspace not loaded: {path}"))?,
            None => Arc::clone(self.workspaces.startup_host()),
        };
        let controller = Controller::create(Arc::clone(&host.builder), &self.runtime).await?;
        Ok(self.wire(controller))
    }

    async fn open(&self, id: &str, workspace: Option<&str>) -> Result<Controller, String> {
        let hosts = self
            .hosts_for(workspace)
            .await
            .ok_or_else(|| SESSION_NOT_FOUND.to_string())?;
        for host in &hosts {
            let listed = listed_for(&host.builder)?;
            if let Some(entry) = listed.sessions.into_iter().find(|entry| entry.id == id) {
                // The repair warnings the CLI prints have no channel here;
                // the web UI reads the repaired history like any other.
                let (controller, _warnings) =
                    Controller::open(Arc::clone(&host.builder), Path::new(&entry.path)).await?;
                return Ok(self.wire(controller));
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
        // matching `runtime_builder.rs`'s own `build_runner` warnings.
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
pub struct ServeOptions {
    pub builder: Builder,
    pub runtime: Runtime,
    /// Exactly one of its two fields is set.
    pub listen: ServerRuntime,
    /// The process sandbox the composition root opened, already behind its
    /// switch. [`run`] owns closing it.
    pub control: Arc<SandboxSwitch>,
    /// `None` when bash never came up, so `POST /v1/sandbox/reload` answers 501
    /// rather than a failure.
    pub reloader: Option<Arc<SandboxReloader>>,
    /// Open the printed TCP URL in the default browser. Unix listeners have
    /// no URL; [`run`] rejects that combination before bind.
    pub open: bool,
}

pub async fn run(
    options: ServeOptions,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> i32 {
    let ServeOptions {
        builder,
        runtime,
        listen,
        control,
        reloader,
        open,
    } = options;

    if let Err(message) = require_tcp_for_open(open, &listen) {
        let _ = control.close().await;
        return fail(stderr, &message);
    }

    let workspace_roots = match canonicalize_workspace_roots(&listen.workspace_roots) {
        Ok(roots) => roots,
        Err(message) => {
            let _ = control.close().await;
            return fail(stderr, &builder.redact_error(&message, Some(&runtime)));
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
    let (listener, token) = bound;
    if !token.is_empty() {
        announce_listen(stdout, &listener.address(), &token, open);
    }

    let builder = Arc::new(builder);
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
    let feishu = resolve_feishu(&builder.config);
    let mut profiles: Vec<String> = builder.config.profiles.keys().cloned().collect();
    profiles.sort();
    let workspace_path = builder.workspace_path.clone();
    let host = Arc::new(WorkspaceHost {
        builder: Arc::clone(&builder),
        workflows,
        reloader,
    });
    let workspaces = Workspaces {
        startup: workspace_path.clone(),
        startup_host: Arc::clone(&host),
        roots: workspace_roots,
        loaded: tokio::sync::Mutex::new(BTreeMap::from([(workspace_path, Arc::clone(&host))])),
    };
    load_persisted_workspaces(
        &workspaces,
        &builder.shared,
        &runtime,
        &serve_cancel,
        stderr,
    )
    .await;
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
        factory: Arc::new(ServeFactory {
            workspaces,
            runtime: runtime.clone(),
            cancel: serve_cancel.clone(),
        }),
        token,
        // ponytail: `Logger` owns its sink, so the request log goes to the
        // process stderr. Thread a shared writer through if a test ever has to
        // read it.
        logger: None,
        workflows: host.workflows.clone(),
    });
    let inbound = inbound::maybe_start(Arc::clone(&server), feishu, serve_cancel.clone());

    // SIGTERM is how a long-running `otto serve` is asked to shut down; the
    // process token covers SIGINT already.
    let terminate = spawn_terminate(serve_cancel.clone());
    let serve_error = server::serve(listener, server.router(), serve_cancel.clone())
        .await
        .err();
    serve_cancel.cancel();
    if let Some(handle) = inbound {
        let _ = handle.await;
    }
    if let Some(handle) = terminate {
        let _ = handle.await;
    }
    server.cancel_token().cancel();
    let close_error = server.close().await.err();
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

fn require_tcp_for_open(open: bool, listen: &ServerRuntime) -> Result<(), String> {
    if open && listen.listen.is_empty() {
        return Err("--open requires a TCP listener".into());
    }
    Ok(())
}

fn announce_listen(stdout: &mut (dyn Write + Send), address: &str, token: &str, open: bool) {
    let url = format!("http://{address}/?token={token}");
    let _ = writeln!(stdout, "otto serve: {url}");
    if open {
        launch_browser(&url);
    }
}

/// The browser launcher. macOS is given as an absolute path because `PATH` is
/// attacker-influenced input at this point; every other target resolves the
/// freedesktop `xdg-open` through `PATH`, which is the only way it is ever
/// installed. A failed launch is not fatal: the URL was already printed.
#[cfg(all(not(test), target_os = "macos"))]
const OPEN_BINARY: &str = "/usr/bin/open";
/// See the macOS launcher above.
#[cfg(all(not(test), not(target_os = "macos")))]
const OPEN_BINARY: &str = "xdg-open";

#[cfg(test)]
static LAUNCH_TEST: Mutex<()> = Mutex::new(());
#[cfg(test)]
static LAUNCHES: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[cfg(test)]
fn take_launches() -> Vec<String> {
    std::mem::take(&mut *LAUNCHES.lock().unwrap_or_else(|poison| poison.into_inner()))
}

#[cfg(test)]
fn launch_browser(url: &str) {
    LAUNCHES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(url.to_owned());
}

#[cfg(not(test))]
fn launch_browser(url: &str) {
    let _ = std::process::Command::new(OPEN_BINARY).arg(url).spawn();
}

/// Picks the listener. A TCP port is reachable by every local user and every
/// page open in a browser, so the API is gated by a per-process token; the
/// token is printed once by the caller and never logged.
fn bind(listen: &ServerRuntime) -> Result<(Listener, String), String> {
    if !listen.listen.is_empty() {
        let listener = listen_tcp(&listen.listen).map_err(|error| format!("serve: {error}"))?;
        let token =
            super::runtime_builder::random_id().map_err(|error| format!("serve: {error}"))?;
        return Ok((listener, token));
    }
    let socket = match Path::new(&listen.socket).is_absolute() {
        true => listen.socket.clone(),
        false => std::path::absolute(&listen.socket)
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .into_owned(),
    };
    let listener = listen_unix(&socket).map_err(|error| format!("serve: {error}"))?;
    Ok((listener, String::new()))
}

/// Cancels `token` on SIGTERM, ending when the token is cancelled from anywhere
/// else.
fn spawn_terminate(token: CancellationToken) -> Option<tokio::task::JoinHandle<()>> {
    let mut signals =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()?;
    Some(tokio::spawn(async move {
        tokio::select! {
            _ = signals.recv() => token.cancel(),
            _ = token.cancelled() => {}
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::config::ServerRuntime;

    #[test]
    fn open_without_a_tcp_listener_is_rejected() {
        let unix = ServerRuntime {
            socket: "/tmp/otto.sock".into(),
            listen: String::new(),
            ..Default::default()
        };
        assert_eq!(
            require_tcp_for_open(true, &unix).unwrap_err(),
            "--open requires a TCP listener"
        );
        assert!(require_tcp_for_open(false, &unix).is_ok());
        let tcp = ServerRuntime {
            socket: String::new(),
            listen: "127.0.0.1:0".into(),
            ..Default::default()
        };
        assert!(require_tcp_for_open(true, &tcp).is_ok());
    }

    fn with_launch_test<T>(body: impl FnOnce() -> T) -> T {
        let _guard = LAUNCH_TEST
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _ = take_launches();
        body()
    }

    #[test]
    fn open_launches_the_printed_url() {
        with_launch_test(|| {
            let mut stdout = Vec::new();
            announce_listen(&mut stdout, "127.0.0.1:8787", "tok", true);
            assert_eq!(
                String::from_utf8(stdout).expect("utf-8"),
                "otto serve: http://127.0.0.1:8787/?token=tok\n"
            );
            assert_eq!(
                take_launches(),
                vec!["http://127.0.0.1:8787/?token=tok".to_string()]
            );
        });
    }

    #[test]
    fn a_tcp_listener_without_open_does_not_launch() {
        with_launch_test(|| {
            let mut stdout = Vec::new();
            announce_listen(&mut stdout, "127.0.0.1:8787", "tok", false);
            assert!(take_launches().is_empty());
            assert_eq!(
                String::from_utf8(stdout).expect("utf-8"),
                "otto serve: http://127.0.0.1:8787/?token=tok\n"
            );
        });
    }

    // ---- admission ----

    #[test]
    fn the_startup_workspace_is_admitted_with_no_roots() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let admitted =
            admit_workspace(&startup_path.to_string_lossy(), &startup_path, &[]).expect("admitted");
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
        let admitted =
            admit_workspace(&child.to_string_lossy(), &startup_path, &[root_path]).expect("ok");
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
            admit_workspace(&sibling.to_string_lossy(), &startup_path, &[root_path]),
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
            admit_workspace(&lookalike.to_string_lossy(), &startup_path, &[root_path]),
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
            admit_workspace(&link.to_string_lossy(), &startup_path, &[root_path]),
            Err(Admission::NotAdmitted)
        );
    }

    #[test]
    fn a_missing_path_is_invalid() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let missing = startup.path().join("does-not-exist");
        assert_eq!(
            admit_workspace(&missing.to_string_lossy(), &startup_path, &[]),
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
            admit_workspace(&file.to_string_lossy(), &startup_path, &[]),
            Err(Admission::Invalid)
        );
    }

    #[test]
    fn a_relative_path_is_invalid() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        assert_eq!(
            admit_workspace(".", &startup_path, &[]),
            Err(Admission::Invalid)
        );
        assert_eq!(
            admit_workspace("rel/dir", &startup_path, &[]),
            Err(Admission::Invalid)
        );
    }

    #[test]
    fn empty_roots_admits_only_the_startup_path() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let other = tempfile::tempdir().expect("other");
        let other_path = canonical_directory(other.path()).expect("canonical");
        assert!(admit_workspace(&startup_path.to_string_lossy(), &startup_path, &[]).is_ok());
        assert_eq!(
            admit_workspace(&other_path.to_string_lossy(), &startup_path, &[]),
            Err(Admission::NotAdmitted)
        );
    }

    /// A `WorkspaceHost` for tests that need one but never open a real
    /// sandbox or workflow controller.
    fn dummy_host(workspace_root: &Path, session_root: &Path) -> Arc<WorkspaceHost> {
        Arc::new(WorkspaceHost {
            builder: Arc::new(crate::cli::testutil::builder(workspace_root, session_root)),
            workflows: None,
            reloader: None,
        })
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
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        assert!(workspaces.admit(&startup_path.to_string_lossy()).is_ok());
        assert_eq!(
            workspaces.admit(&root_path.to_string_lossy()),
            Ok(root_path)
        );
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
        let runtime = crate::cli::runtime_builder::resolve_initial_runtime(
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
        let runtime = crate::cli::runtime_builder::resolve_initial_runtime(
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
        let other_host = dummy_host(&other_path, other_sessions.path());

        let mut loaded = BTreeMap::new();
        loaded.insert(
            other_path.to_string_lossy().into_owned(),
            Arc::clone(&other_host),
        );
        let runtime = crate::cli::testutil::initial_runtime(&other_host.builder);
        let factory = ServeFactory {
            workspaces: Workspaces {
                startup: startup_path.to_string_lossy().into_owned(),
                startup_host,
                roots: Vec::new(),
                loaded: tokio::sync::Mutex::new(loaded),
            },
            runtime,
            cancel: CancellationToken::new(),
        };

        let controller = factory
            .create(Some(&other_path.to_string_lossy()))
            .await
            .expect("create in workspace B");

        assert_eq!(controller.workspace(), other_path.to_string_lossy());
        assert_eq!(other_host.builder.workspace.root(), other_path);
    }

    /// A host whose session directory listing fails (here: `session_root` is
    /// a file, not a directory) must report that failure, not a plain
    /// "session not found" — a caller reading 404 would otherwise conclude
    /// there is no such session, when the real problem is a broken session
    /// directory.
    #[tokio::test]
    async fn opening_by_id_in_a_named_workspace_whose_listing_fails_propagates_the_error() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let startup_sessions = tempfile::tempdir().expect("startup sessions");
        let startup_host = dummy_host(&startup_path, startup_sessions.path());

        let other = tempfile::tempdir().expect("other workspace");
        let other_path = canonical_directory(other.path()).expect("canonical");
        let broken_session_root = other.path().join("sessions-is-a-file");
        std::fs::write(&broken_session_root, b"not a directory").expect("write file");
        let other_host = dummy_host(&other_path, &broken_session_root);

        let mut loaded = BTreeMap::new();
        loaded.insert(other_path.to_string_lossy().into_owned(), other_host);
        let runtime = crate::cli::testutil::initial_runtime(&startup_host.builder);
        let factory = ServeFactory {
            workspaces: Workspaces {
                startup: startup_path.to_string_lossy().into_owned(),
                startup_host,
                roots: Vec::new(),
                loaded: tokio::sync::Mutex::new(loaded),
            },
            runtime,
            cancel: CancellationToken::new(),
        };

        let error = match factory
            .open("some-id", Some(&other_path.to_string_lossy()))
            .await
        {
            Ok(_) => panic!("a broken session directory must not read as an open session"),
            Err(error) => error,
        };
        assert_ne!(error, SESSION_NOT_FOUND, "{error}");
    }

    /// The same failure, met while searching every loaded workspace for a
    /// resumed id (no `workspace` given): a broken host earlier in the search
    /// order must not be silently skipped in favor of a later host that
    /// happens to hold the id.
    #[tokio::test]
    async fn opening_by_id_across_workspaces_propagates_one_hosts_listing_error() {
        let startup = tempfile::tempdir().expect("startup");
        let startup_path = canonical_directory(startup.path()).expect("canonical");
        let broken_session_root = startup.path().join("sessions-is-a-file");
        std::fs::write(&broken_session_root, b"not a directory").expect("write file");
        let startup_host = dummy_host(&startup_path, &broken_session_root);

        let other = tempfile::tempdir().expect("other workspace");
        let other_path = canonical_directory(other.path()).expect("canonical");
        let other_sessions = tempfile::tempdir().expect("other sessions");
        let other_host = dummy_host(&other_path, other_sessions.path());

        let runtime = crate::cli::testutil::initial_runtime(&other_host.builder);
        let created = Controller::create(Arc::clone(&other_host.builder), &runtime)
            .await
            .expect("create a real session in the working workspace");
        let id = created.info().session_id;

        let mut loaded = BTreeMap::new();
        loaded.insert(other_path.to_string_lossy().into_owned(), other_host);
        let factory = ServeFactory {
            workspaces: Workspaces {
                startup: startup_path.to_string_lossy().into_owned(),
                startup_host,
                roots: Vec::new(),
                loaded: tokio::sync::Mutex::new(loaded),
            },
            runtime,
            cancel: CancellationToken::new(),
        };

        let error = match factory.open(&id, None).await {
            Ok(_) => panic!("the broken startup workspace must not be skipped over"),
            Err(error) => error,
        };
        assert_ne!(error, SESSION_NOT_FOUND, "{error}");
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

    fn runtime_for(shared: &Arc<crate::cli::runtime_builder::Shared>) -> Runtime {
        crate::cli::runtime_builder::resolve_initial_runtime(
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
        let restarted = Workspaces {
            startup: startup_path.to_string_lossy().into_owned(),
            startup_host: dummy_host(&startup_path, sessions.path()),
            roots: vec![roots_path],
            loaded: tokio::sync::Mutex::new(BTreeMap::new()),
        };
        load_persisted_workspaces(&restarted, &shared, &runtime, &cancel, &mut stderr).await;
        let factory = ServeFactory {
            workspaces: restarted,
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
}
