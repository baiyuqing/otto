//! Configuration loading, storage and workspace preparation phases.

#[cfg(test)]
mod tests;

use super::*;

/// Builds a `Builder` for a workspace admitted after startup: `serve.rs`'s
/// workspace registry calls this the first time a path is loaded.
///
/// Reuses the startup sequence's pieces rather than duplicating them:
/// `Workspace::new` and `resolve_mcp` resolve against `canonical_path`
/// instead of the ones `otto_cli::run` resolved for the startup workspace, then
/// `compose_workspace_sandbox` opens this workspace's own sandbox, elevated
/// bash-approval executor, and `/sandbox reload` state the same way startup
/// does for its own workspace — always elevating (`elevate: true`) since
/// every workspace `otto serve` loads needs `/approve` the same as the
/// startup workspace, unlike the CLI's `frontend != Frontend::Once` gate,
/// which only matters for the interactive REPL/TUI path. The memory service
/// stays the one already on `shared`; only this workspace's `workspace_scope`
/// is computed. Returns the reloader alongside the `Builder` so
/// `serve.rs`'s `WorkspaceHost` can wire its own `/sandbox reload`.
pub async fn load_workspace(
    shared: Arc<Shared>,
    canonical_path: &Path,
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
    let sandbox_settings = resolve_sandbox_settings(&shared.config, driver_override.as_deref())?;
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
        cancel,
        stderr,
    )
    .await;

    builder.workspace_scope = workspace_scope;

    Ok((builder, composed))
}

// ---- startup phases ----

/// The process environment snapshot, the boundary that exists before a
/// profile is resolved, the home directory and the layered configuration.
pub struct Loaded {
    pub boundary: StartupBoundary,
    pub home: String,
    pub config_path: String,
    pub config: File,
}

/// Process snapshot, boundary, home and layered config. `Err` is the exact
/// text the CLI passes to `fail`.
pub fn load(
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

/// The configuration environment and the captured ChatGPT credentials.
pub struct Configured {
    pub environment: HashMap<String, String>,
    pub auth: CapturedAuth,
}

/// Prints the configuration diagnostics to `stderr`, builds the configuration
/// environment, completes the redaction boundary and captures the ChatGPT
/// credentials, after checking the active endpoint without credential lookup.
pub fn configure(
    loaded: &mut Loaded,
    host_entries: &[Vec<u8>],
    lookup: &EnvironmentLookup,
    overrides: &Overrides,
    stderr: &mut (dyn Write + Send),
) -> Result<Configured, String> {
    configure_with_readers(
        loaded,
        host_entries,
        lookup,
        overrides,
        stderr,
        config_environment,
        capture_auth_credentials,
    )
}

#[allow(clippy::too_many_arguments)]
fn configure_with_readers(
    loaded: &mut Loaded,
    host_entries: &[Vec<u8>],
    lookup: &EnvironmentLookup,
    overrides: &Overrides,
    stderr: &mut (dyn Write + Send),
    read_environment: impl FnOnce(&File, &EnvironmentLookup) -> HashMap<String, String>,
    read_auth: impl FnOnce(&Path) -> CapturedAuth,
) -> Result<Configured, String> {
    // These are selection inputs, not provider credential bindings. Do not build
    // the configuration environment or capture auth even for redaction until
    // the active endpoint is authorized. Session history does not select it.
    let selection = ["OTTO_MODEL", "OTTO_PROVIDER"]
        .into_iter()
        .filter_map(|name| {
            lookup
                .get(name)
                .map(|value| (name.to_string(), value.clone()))
        })
        .collect();
    if let Err(error) = otto_core::config::resolve::resolve_without_credentials(
        &loaded.config,
        &selection,
        &Default::default(),
        overrides,
    ) {
        // No selection is valid for server first-run configuration. Other
        // frontends retain their existing actionable missing-model error.
        if !error.to_string().starts_with("missing model:") {
            // Config/selection text may itself contain arbitrary secrets. The
            // configured redactor is deliberately not available yet.
            return Err(concat!(
                "startup model configuration rejected before credential lookup; ",
                "check the selected model and endpoint origin grants in user or managed configuration"
            ).to_string());
        }
    }
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
    let mut environment = read_environment(config_file, lookup);
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
    let captured_auth = read_auth(&crate::auth::path_for_home(Path::new(home)));
    let (merged, merged_complete) =
        merge_redactions(&startup.sandbox_secrets, &captured_auth.redaction_values);
    startup.sandbox_secrets = merged;
    startup.complete = startup.complete && captured_auth.complete && merged_complete;
    Ok(Configured {
        environment,
        auth: captured_auth,
    })
}

/// The canonical workspace directory. The error is already redacted.
pub fn canonical_workspace(cwd: &str, boundary: &StartupBoundary) -> Result<String, String> {
    match canonical_directory(Path::new(cwd)) {
        Ok(path) => Ok(path.to_string_lossy().into_owned()),
        Err(error) => Err(boundary.redact(&format!("resolve cwd: {error}"))),
    }
}

/// The workspace initialized on disk and the session root.
pub struct PreparedWorkspace {
    pub memory_config: MemoryRuntime,
    pub workspace: Arc<Workspace>,
    pub session_root: PathBuf,
}

/// Resolves the memory configuration, initializes the workspace directory and
/// the session root. Errors are already redacted.
pub fn prepare_workspace(
    loaded: &Loaded,
    environment: &HashMap<String, String>,
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
    let session_root = default_session_root;
    Ok(PreparedWorkspace {
        memory_config,
        workspace,
        session_root,
    })
}
