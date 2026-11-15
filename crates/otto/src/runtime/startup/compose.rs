//! Builder composition and initial runtime selection.

use super::*;

pub struct ComposeOptions {
    pub memory_extension: Option<super::super::wiring::MemoryExtension>,
    pub sandbox_driver_override: Option<String>,
    pub no_session: bool,
    /// `otto --prompt`: every turn is unattended.
    pub headless: bool,
    pub overrides: Overrides,
    pub explicit_config: bool,
    pub logger: Arc<otto_platform::log::Logger>,
}

pub struct Composed {
    pub builder: Builder,
    pub sandbox_settings: SandboxSettings,
}

/// Resolves the shell and the sandbox settings, opens MCP configuration, the
/// usage, task, schedule, skill-check stores and the approval
/// service, then builds `Shared` and the workspace `Builder`. Warnings go to
/// `stderr` as each store fails to open; errors are already redacted.
#[allow(clippy::too_many_arguments)]
pub fn compose(
    options: ComposeOptions,
    loaded: &Loaded,
    configured: &Configured,
    host_entries: &[Vec<u8>],
    lookup: &EnvironmentLookup,
    storage: &Arc<crate::runtime::storage::Storage>,
    prepared: &PreparedWorkspace,
    workspace_path: &str,
    stderr: &mut (dyn Write + Send),
) -> Result<Composed, String> {
    let startup = &loaded.boundary;
    let config_file = &loaded.config;
    let environment = &configured.environment;
    let home = &loaded.home;
    if options
        .logger
        .enable_diagnostics(Path::new(home), crate::VERSION)
        .is_err()
    {
        let _ = writeln!(stderr, "warning: diagnostic file logging unavailable");
    }

    let mut shell = lookup.get("SHELL").cloned().unwrap_or_default();
    if shell.is_empty() {
        shell = "/bin/sh".to_string();
    }
    if let Ok(canonical) = canonical_executable_file(Path::new(&shell)) {
        shell = canonical.to_string_lossy().into_owned();
    }

    let sandbox_driver_override = options.sandbox_driver_override;
    let sandbox_settings =
        match resolve_sandbox_settings(config_file, sandbox_driver_override.as_deref()) {
            Ok(settings) => settings,
            Err(message) => return Err(startup.redact(&message)),
        };
    let mcp_config = match resolve_mcp(config_file, environment, workspace_path) {
        Ok(config) => config,
        Err(error) => return Err(error.to_string()),
    };
    let usage = match storage.usage(&Path::new(home).join(".otto/usage.db")) {
        Ok(store) => Some(store),
        Err(_) => {
            let _ = writeln!(
                stderr,
                "warning: usage store unavailable, continuing without usage history"
            );
            None
        }
    };
    let task_recorder = match storage.tasks(&Path::new(home).join(".otto/tasks.db")) {
        Ok(store) => Some(store),
        Err(_) => {
            let _ = writeln!(
                stderr,
                "warning: task history store unavailable, continuing without task recording"
            );
            None
        }
    };
    let schedule = open_schedule(storage, home, &mut *stderr);
    let skill_checker = skill_checker(storage, config_file, environment, home, &mut *stderr);
    let approvals = match storage
        .approvals(&Path::new(home).join(".otto/approvals.db"))
        .and_then(crate::approval::Service::from_store)
    {
        Ok(service) => Arc::new(service),
        Err(error) => return Err(error.to_string()),
    };
    approvals.start();
    let captured_auth = &configured.auth;
    let shared = Arc::new(Shared {
        storage: Arc::clone(storage),
        approvals,
        config_path: PathBuf::from(&loaded.config_path),
        config: config_file.clone(),
        environment: environment.clone(),
        home: home.clone(),
        session_root: prepared.session_root.clone(),
        shell: shell.clone(),
        no_session: options.no_session,
        headless: options.headless,
        overrides: options.overrides,
        host_entries: host_entries.to_vec(),
        sandbox_secrets_baseline: startup.sandbox_secrets.clone(),
        sandbox_secrets_baseline_complete: startup.complete,
        sandbox_driver_override: sandbox_driver_override.clone(),
        explicit_config: options.explicit_config,
        auth_path: captured_auth.path.clone(),
        auth_credentials: captured_auth.credentials.clone(),
        auth_credentials_loaded: captured_auth.loaded,
        usage,
        task_recorder,
        schedule,
        skill_checker,
        memory: super::super::wiring::MemoryWiring {
            extension: options.memory_extension,
            ..Default::default()
        },
        logger: options.logger,
        parent_tool_factory: crate::runtime::builder::ParentToolSlot::default(),
    });
    let builder = Builder::for_workspace(
        shared,
        Arc::clone(&prepared.workspace),
        workspace_path.to_string(),
        mcp_config,
    );
    Ok(Composed {
        builder,
        sandbox_settings,
    })
}

/// The process runtime from CLI, environment and configuration, independent of
/// the conversation history being resumed.
pub fn initial_runtime(
    builder: &Builder,
    metadata: Option<&RuntimeMetadata>,
) -> Result<Runtime, String> {
    match resolve_initial_runtime(
        &builder.config,
        &builder.environment,
        metadata,
        &builder.overrides,
    ) {
        Ok(runtime) => Ok(runtime),
        Err(error) if error.to_string().starts_with("missing model:") => Err(
            "no model configured; run `otto setup` to create one, or pass --model <provider>/<model_id>"
                .to_string(),
        ),
        Err(error) => Err(builder.redact_error(&error.to_string(), None)),
    }
}
