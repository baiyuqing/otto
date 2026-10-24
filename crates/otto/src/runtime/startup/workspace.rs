//! Configuration loading, storage and workspace preparation phases.

use super::*;

/// Builds a `Builder` for a workspace admitted after startup: `serve.rs`'s
/// workspace registry calls this the first time a path is loaded.
///
/// Reuses the startup sequence's pieces rather than duplicating them:
/// `Workspace::new` and `resolve_mcp` resolve against `canonical_path`
/// instead of the ones `cli::run` resolved for the startup workspace, then
/// `compose_workspace_sandbox` opens this workspace's own sandbox, elevated
/// bash-approval executor, and `/sandbox reload` state the same way startup
/// does for its own workspace — always elevating (`elevate: true`) since
/// every workspace `otto serve` loads needs `/approve` the same as the
/// startup workspace, unlike the CLI's `frontend != Frontend::Once` gate,
/// which only matters for the interactive REPL/TUI path. The memory service
/// stays the one already on `shared`; only this workspace's `workspace_scope`
/// is computed. Returns the reloader alongside the `Builder` so
/// `serve.rs`'s `WorkspaceHost` can wire its own `/sandbox reload`.
pub(crate) async fn load_workspace(
    shared: Arc<Shared>,
    canonical_path: &Path,
    disabled: &Disabled,
    cancel: &CancellationToken,
    stderr: &mut (dyn Write + Send),
) -> Result<(Builder, WorkspaceSandbox), String> {
    let mut scoped = (*shared).clone();
    if shared.config.user_values.is_some() {
        let loaded = crate::config::layered::for_workspace(
            Path::new(&shared.home),
            &shared.config_path,
            &shared.config,
            canonical_path,
        )?;
        for diagnostic in &loaded.file.diagnostics {
            let _ = writeln!(
                stderr,
                "warning: {}: ignored {}: {}",
                diagnostic.source, diagnostic.key, diagnostic.reason
            );
        }
        scoped.config = loaded.file;
        disabled.apply(&mut scoped.config);
    }
    let workspace_path = canonical_path.to_string_lossy().into_owned();
    let memory_config =
        resolve_memory(&scoped.config, &scoped.environment).map_err(|error| error.to_string())?;
    let memory_override = memory_config
        .workspace_ids
        .get(&workspace_path)
        .map(String::as_str)
        .unwrap_or_default();
    crate::workspace::initialize(canonical_path, memory_override)
        .map_err(|error| error.to_string())?;
    let workspace_scope =
        crate::runtime::wiring::workspace_memory_scope(&memory_config, &workspace_path)?;
    let shared = Arc::new(scoped);
    let workspace = Arc::new(Workspace::new(canonical_path).map_err(|error| error.to_string())?);
    let mcp_config = resolve_mcp(&shared.config, &shared.environment, &workspace_path)
        .map_err(|error| error.to_string())?;
    let mut builder = Builder::for_workspace(
        Arc::clone(&shared),
        workspace,
        workspace_path.clone(),
        mcp_config,
    );

    let driver_override = shared.sandbox_driver_override.clone();
    let sandbox_settings = resolve_sandbox_settings(
        &shared.config,
        &shared.environment,
        &workspace_path,
        driver_override.as_deref(),
    )?;
    let resolved =
        resolve_initial_runtime(&shared.config, &shared.environment, None, &shared.overrides)
            .map_err(|error| error.to_string())?;
    let composed = compose_workspace_sandbox(
        &mut builder,
        &sandbox_settings,
        &shared.config_path,
        shared.explicit_config,
        driver_override.as_deref(),
        &resolved.api_key_env,
        true,
        disabled,
        cancel,
        stderr,
    )
    .await;

    builder.workspace_scope = workspace_scope;

    Ok((builder, composed))
}

// ---- components ----

/// Components the embedding API switched off. Each disabled state is one the
/// CLI already reaches through a config key, a flag or a failed store open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Disabled {
    pub(crate) bash: bool,
    pub(crate) mcp: bool,
    pub(crate) memory: bool,
    pub(crate) skills: bool,
    pub(crate) plugins: bool,
    pub(crate) subagents: bool,
    pub(crate) schedule: bool,
    pub(crate) session_persistence: bool,
    pub(crate) usage: bool,
    pub(crate) task_history: bool,
}

impl Disabled {
    /// Sets `[mcp]`, `[memory]`, `[skills]`, `[plugins]`, `[agents]` and
    /// `enabled = false` in memory for each disabled one.
    pub(crate) fn apply(&self, file: &mut File) {
        if self.mcp {
            file.mcp.enabled = Some(false);
        }
        if self.memory {
            file.memory.enabled = Some(false);
        }
        if self.skills {
            file.skills.enabled = Some(false);
        }
        if self.plugins {
            file.plugins.enabled = Some(false);
        }
        if self.subagents {
            file.agents.enabled = Some(false);
        }
    }
}

// ---- startup phases ----

/// The process environment snapshot, the boundary that exists before a
/// profile is resolved, the home directory and the layered configuration.
pub(crate) struct Loaded {
    pub(crate) boundary: StartupBoundary,
    pub(crate) home: String,
    pub(crate) config_path: String,
    pub(crate) config: File,
}

/// Process snapshot, boundary, home and layered config. `Err` is the exact
/// text the CLI passes to `fail`.
pub(crate) fn load(
    host_entries: &[Vec<u8>],
    lookup: &EnvironmentLookup,
    config_path: &str,
    explicit_config: bool,
    cwd: &str,
    base_url: &str,
) -> Result<Loaded, String> {
    let process_snapshot = environment_snapshot(host_entries, vec!["OTTO_API_KEY".to_string()]);
    let empty_config = File::default();
    let empty_environment = HashMap::new();
    let boundary = StartupBoundary {
        sandbox_secrets: process_snapshot.redaction_values().to_vec(),
        complete: process_snapshot.redactions_complete(),
        config: empty_config,
        environment: empty_environment,
        base_url: base_url.to_string(),
    };

    let home = resolve_home(lookup)?;
    let (config_path, config) = match load_config(config_path, explicit_config, cwd, &home) {
        Ok(loaded) => loaded,
        Err(message) => return Err(format!("load config: {message}")),
    };
    Ok(Loaded {
        boundary,
        home,
        config_path,
        config,
    })
}

/// Opens the storage backend the configuration selects. The error is already
/// redacted by the boundary.
pub(crate) async fn open_storage(
    loaded: &Loaded,
    lookup: &EnvironmentLookup,
    backends: &crate::runtime::storage::BackendRegistry,
) -> Result<Arc<crate::runtime::storage::Storage>, String> {
    match crate::runtime::storage::Storage::open_home_with(
        Path::new(&loaded.home),
        backends,
        &loaded.config.storage,
        lookup,
    )
    .await
    {
        Ok(storage) => Ok(Arc::new(storage)),
        Err(error) => Err(loaded.boundary.redact(&error)),
    }
}

/// The configuration environment and the captured ChatGPT credentials.
pub(crate) struct Configured {
    pub(crate) environment: HashMap<String, String>,
    pub(crate) auth: CapturedAuth,
}

/// Prints the configuration diagnostics to `stderr`, builds the configuration
/// environment, completes the redaction boundary and captures the ChatGPT
/// credentials. Infallible.
pub(crate) fn configure(
    loaded: &mut Loaded,
    host_entries: &[Vec<u8>],
    lookup: &EnvironmentLookup,
    stderr: &mut (dyn Write + Send),
) -> Configured {
    let config_file = &loaded.config;
    let home = &loaded.home;
    let startup = &mut loaded.boundary;
    for diagnostic in &config_file.diagnostics {
        let _ = writeln!(
            stderr,
            "warning: {} in {}: {}",
            diagnostic.key, diagnostic.source, diagnostic.reason
        );
    }
    let mut environment = config_environment(config_file, lookup);
    environment.insert("HOME".to_string(), home.clone());

    let configured_snapshot = environment_snapshot(
        host_entries,
        sandbox_provider_environment_names(config_file, ""),
    );
    startup.config = config_file.clone();
    startup.environment = environment.clone();
    let (merged, merged_complete) = merge_redactions(
        &startup.sandbox_secrets,
        configured_snapshot.redaction_values(),
    );
    startup.sandbox_secrets = merged;
    startup.complete =
        startup.complete && configured_snapshot.redactions_complete() && merged_complete;
    let captured_auth = capture_auth_credentials(&crate::auth::path_for_home(Path::new(home)));
    let (merged, merged_complete) =
        merge_redactions(&startup.sandbox_secrets, &captured_auth.redaction_values);
    startup.sandbox_secrets = merged;
    startup.complete = startup.complete && captured_auth.complete && merged_complete;
    Configured {
        environment,
        auth: captured_auth,
    }
}

/// The canonical workspace directory. The error is already redacted.
pub(crate) fn canonical_workspace(cwd: &str, boundary: &StartupBoundary) -> Result<String, String> {
    match canonical_directory(Path::new(cwd)) {
        Ok(path) => Ok(path.to_string_lossy().into_owned()),
        Err(error) => Err(boundary.redact(&format!("resolve cwd: {error}"))),
    }
}

/// The workspace initialized on disk and the session root.
pub(crate) struct PreparedWorkspace {
    pub(crate) memory_config: MemoryRuntime,
    pub(crate) workspace: Arc<Workspace>,
    pub(crate) session_root: PathBuf,
}

/// Resolves the memory configuration, initializes the workspace directory and
/// the session root. Errors are already redacted.
pub(crate) fn prepare_workspace(
    loaded: &Loaded,
    environment: &HashMap<String, String>,
    storage: &crate::runtime::storage::Storage,
    workspace_path: &str,
) -> Result<PreparedWorkspace, String> {
    let startup = &loaded.boundary;
    let memory_config = match resolve_memory(&loaded.config, environment) {
        Ok(config) => config,
        Err(error) => return Err(error.to_string()),
    };
    let memory_override = memory_config
        .workspace_ids
        .get(workspace_path)
        .map(String::as_str)
        .unwrap_or_default();
    if let Err(error) = crate::workspace::initialize(Path::new(workspace_path), memory_override) {
        return Err(startup.redact(&error.to_string()));
    }
    let workspace = match Workspace::new(Path::new(workspace_path)) {
        Ok(workspace) => Arc::new(workspace),
        Err(error) => {
            return Err(startup.redact(&format!("create workspace: {error}")));
        }
    };
    let default_session_root: PathBuf =
        [loaded.home.as_str(), ".otto", "sessions"].iter().collect();
    let session_root = match storage.session_root(&default_session_root) {
        Ok(root) => root,
        Err(error) => return Err(startup.redact(&error)),
    };
    Ok(PreparedWorkspace {
        memory_config,
        workspace,
        session_root,
    })
}
