//! The workspace registry, admission and the persisted workspace list.

use super::*;

// ---- the workspace registry ----

/// One loaded workspace: its composition root, its own `/sandbox reload`
/// state, and, when the workflow lock was acquired, its workflow controller.
pub(super) struct DeliverySetupCapability {
    pub(super) executor: Arc<otto::sandbox::Executor>,
    pub(super) environment: Vec<String>,
}

impl DeliverySetupCapability {
    pub(super) fn worktree_executor(&self) -> otto::delivery::WorktreeExecutor {
        otto::delivery::WorktreeExecutor::new(self.executor.clone(), self.environment.clone())
    }
}

pub(super) struct WorkspaceHost {
    pub(super) builder: Arc<Builder>,
    pub(super) workflows: Option<Arc<otto::workflow::Controller>>,
    /// The elevated executor plus its matching environment, retained solely
    /// for constrained delivery worktree setup. It is intentionally private:
    /// ordinary server routes and source runners cannot obtain either a target
    /// executor or the environment needed to run it.
    pub(super) delivery_setup: Option<DeliverySetupCapability>,
    /// This workspace's own sandbox reloader (`None` when it never got a
    /// usable sandbox). `POST /v1/sandbox/reload` reloads the startup
    /// workspace through `ServeFactory::reload_sandbox` and every other
    /// loaded workspace through `reload_other_sandboxes`.
    pub(super) reloader: Option<Arc<SandboxReloader>>,
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
pub(super) struct Workspaces {
    pub(super) startup: String,
    pub(super) startup_host: Arc<WorkspaceHost>,
    /// Canonical `[server].workspace_roots`.
    pub(super) roots: Vec<PathBuf>,
    /// Where `otto trust` writes `[projects]`, re-read on every [`admit`]
    /// call so a directory trusted after startup is admitted without a
    /// restart.
    ///
    /// [`admit`]: Self::admit
    pub(super) config_path: PathBuf,
    pub(super) loaded: tokio::sync::Mutex<BTreeMap<String, Arc<WorkspaceHost>>>,
}

impl Workspaces {
    pub(super) fn startup_host(&self) -> &Arc<WorkspaceHost> {
        &self.startup_host
    }

    /// Whether `requested` may be opened: the startup workspace, a
    /// descendant of one of `self.roots`, or a trusted directory (or its
    /// descendant) from the config file's current `[projects]` tables.
    pub(super) fn admit(&self, requested: &str) -> Result<PathBuf, Admission> {
        let trusted = trusted_projects(&self.config_path);
        admit_workspace(requested, Path::new(&self.startup), &self.roots, &trusted)
    }

    /// The host loaded for `path`, if any.
    pub(super) async fn host(&self, path: &str) -> Option<Arc<WorkspaceHost>> {
        if path == self.startup {
            return Some(Arc::clone(&self.startup_host));
        }
        self.loaded.lock().await.get(path).cloned()
    }

    /// Every loaded workspace, startup first then by path.
    pub(super) async fn list(&self) -> Vec<(String, Arc<WorkspaceHost>)> {
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
    pub(super) async fn load(
        &self,
        canonical: &Path,
        shared: &Arc<otto::runtime::builder::Shared>,
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
            load_workspace(Arc::clone(shared), canonical, cancel, stderr).await?;
        let builder = Arc::new(builder);
        let workspace_runtime = otto::runtime::builder::resolve_initial_runtime(
            &builder.config,
            &builder.environment,
            None,
            &builder.overrides,
        )
        .map_err(|error| builder.redact_error(&error.to_string(), Some(runtime)))?;
        let workflows = match crate::workflow::build_controller(
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
    pub(super) async fn remove(
        &self,
        path: &str,
        home: &str,
        stderr: &mut (dyn Write + Send),
    ) -> Result<(), otto_server::WorkspaceRemoveError> {
        if path == self.startup {
            return Err(otto_server::WorkspaceRemoveError::IsStartup);
        }
        let mut loaded = self.loaded.lock().await;
        let host = loaded.get(path).cloned();
        if let Some(host) = &host
            && let Some(controller) = &host.workflows
            && controller.active_runs() > 0
        {
            return Err(otto_server::WorkspaceRemoveError::InUse(
                "an active workflow run".to_string(),
            ));
        }
        if host.is_none() {
            let in_list = read_workspace_list(home)
                .unwrap_or_default()
                .iter()
                .any(|entry| entry == path);
            if !in_list {
                return Err(otto_server::WorkspaceRemoveError::NotFound);
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
pub(super) fn canonical_or_literal(path: &str) -> String {
    canonical_directory(Path::new(path))
        .map(|canonical| canonical.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

// ---- persisted workspace list ----

/// `~/.otto/serve-workspaces.json`: every workspace ever added via `POST
/// /v1/workspaces`, so a restarted `otto serve` reloads them. One file per
/// user, shared by every `otto serve` process.
pub(super) fn workspace_list_path(home: &str) -> PathBuf {
    Path::new(home).join(".otto/serve-workspaces.json")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct WorkspaceListFile {
    pub(super) workspaces: Vec<String>,
}

/// Reads the persisted workspace list. A missing file reads as an empty
/// list; an unparsable file is `Err` so the caller can warn without touching
/// it, leaving it for the next successful [`add_to_workspace_list`] to
/// overwrite.
pub(super) fn read_workspace_list(home: &str) -> Result<Vec<String>, String> {
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
pub(super) fn write_workspace_list(home: &str, workspaces: Vec<String>) -> std::io::Result<()> {
    let directory = Path::new(home).join(".otto");
    std::fs::create_dir_all(&directory)?;
    let json = serde_json::to_vec(&WorkspaceListFile { workspaces })
        .expect("a list of strings always serializes");
    let suffix = otto::random::random_id()?;
    let temp_path = directory.join(format!(".serve-workspaces-{suffix}.json"));
    std::fs::write(&temp_path, &json)?;
    std::fs::rename(&temp_path, workspace_list_path(home))
}

/// Reads the list, applies `change` and writes it back while holding an
/// exclusive lock on `~/.otto`, the lock `otto::workspace` takes for
/// `workspace.json`. The lock is across processes, so two `otto serve`
/// processes cannot overwrite each other's change. A currently unparsable
/// file is treated as an empty list rather than preserved, matching the
/// spec's "the next successful add ... rewrites it from the empty list plus
/// the new path".
fn update_workspace_list(home: &str, change: impl FnOnce(&mut Vec<String>)) -> std::io::Result<()> {
    let directory = Path::new(home).join(".otto");
    std::fs::create_dir_all(&directory)?;
    let handle = otto::fsops::open_dir_no_follow(&directory)?;
    let _lock = otto::fsops::lock_directory(&handle)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut workspaces = read_workspace_list(home).unwrap_or_default();
    change(&mut workspaces);
    write_workspace_list(home, workspaces)
}

/// Adds `path` to the persisted workspace list (sorted, deduplicated).
pub(super) fn add_to_workspace_list(home: &str, path: &str) -> std::io::Result<()> {
    update_workspace_list(home, |workspaces| {
        if !workspaces.iter().any(|entry| entry == path) {
            workspaces.push(path.to_string());
            workspaces.sort();
        }
    })
}

/// Removes `path` from the persisted workspace list, preserving the order of
/// the remaining entries. A path not in the list is a no-op.
pub(super) fn remove_from_workspace_list(home: &str, path: &str) -> std::io::Result<()> {
    update_workspace_list(home, |workspaces| workspaces.retain(|entry| entry != path))
}

/// Loads every path in the persisted workspace list into `workspaces`, after
/// the startup workspace is already loaded. A path that fails admission or
/// fails to load is skipped with a stderr warning naming the path and the
/// reason; it stays in the file for the next start to retry. Called once at
/// startup, so it never persists what it loads back to the file (those paths
/// are already in it).
pub(super) async fn load_persisted_workspaces(
    workspaces: &Workspaces,
    shared: &Arc<otto::runtime::builder::Shared>,
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
