//! Builder composition and initial runtime selection.

use super::*;

pub(crate) struct ComposeOptions {
    pub(crate) memory_extension: Option<super::super::wiring::MemoryExtension>,
    pub(crate) sandbox_driver_override: Option<String>,
    pub(crate) no_session: bool,
    /// `otto --prompt`: every turn is unattended.
    pub(crate) headless: bool,
    pub(crate) overrides: Overrides,
    pub(crate) explicit_config: bool,
    pub(crate) logger: Arc<crate::log::Logger>,
    pub(crate) disabled: Disabled,
}

pub(crate) struct Composed {
    pub(crate) builder: Builder,
    pub(crate) sandbox_settings: SandboxSettings,
}

/// Resolves the shell and the sandbox settings, opens MCP configuration, the
/// usage, task, schedule, skill-check and reflection stores and the approval
/// service, then builds `Shared` and the workspace `Builder`. Warnings go to
/// `stderr` as each store fails to open; errors are already redacted.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compose(
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
    let disabled = options.disabled;
    if options.logger.enable_diagnostics(Path::new(home)).is_err() {
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
    let sandbox_settings = match resolve_sandbox_settings(
        config_file,
        environment,
        workspace_path,
        sandbox_driver_override.as_deref(),
    ) {
        Ok(settings) => settings,
        Err(message) => return Err(startup.redact(&message)),
    };
    let mcp_config = match resolve_mcp(config_file, environment, workspace_path) {
        Ok(config) => config,
        Err(error) => return Err(error.to_string()),
    };
    let usage = if disabled.usage {
        None
    } else {
        match storage.usage(&Path::new(home).join(".otto/usage.db")) {
            Ok(store) => Some(store),
            Err(_) => {
                let _ = writeln!(
                    stderr,
                    "warning: usage store unavailable, continuing without usage history"
                );
                None
            }
        }
    };
    let task_recorder = if disabled.task_history {
        None
    } else {
        match storage.tasks(&Path::new(home).join(".otto/tasks.db")) {
            Ok(store) => Some(store),
            Err(_) => {
                let _ = writeln!(
                    stderr,
                    "warning: task history store unavailable, continuing without task recording"
                );
                None
            }
        }
    };
    let schedule = if disabled.schedule {
        None
    } else {
        open_schedule(storage, home, &mut *stderr)
    };
    let skill_checker = skill_checker(storage, config_file, environment, home, &mut *stderr);
    let reflection_config = match resolve_reflection(config_file) {
        Ok(config) => config,
        Err(error) => return Err(error.to_string()),
    };
    let reflection_store = if reflection_config.enabled {
        match storage.reflection(&Path::new(home).join(".otto/reflection.db")) {
            Ok(store) => Some(store),
            Err(_) => {
                let _ = writeln!(
                    stderr,
                    "warning: reflection store unavailable, continuing without reflection history"
                );
                None
            }
        }
    } else {
        None
    };
    let mut reflector =
        crate::reflection::Reflector::with_store(reflection_config, reflection_store);
    if !home.is_empty() {
        reflector = reflector.with_log_dir(Path::new(home).join(".otto/reflection/runs"));
    }
    if let Some(key) = environment
        .get("TYPESAFE_API_KEY")
        .filter(|key| !key.is_empty())
    {
        reflector = reflector.with_decision_backend(
            Arc::new(crate::decision::jev::Jev::new(
                crate::typesafe::BASE_URL,
                key,
            )),
            Arc::clone(&options.logger),
        );
    }
    let reflector = Arc::new(reflector);

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
        no_session: options.no_session || disabled.session_persistence,
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
        reflector,
        logger: options.logger,
        parent_tool_factory: crate::runtime::builder::ParentToolSlot::default(),
    });
    let builder = Builder::for_workspace(
        shared,
        prepared.workspace,
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
pub(crate) fn initial_runtime(
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
        Err(error) if error.to_string() == "missing provider" => Err(
            "no provider configured; run `otto setup` to create one, or pass --provider and --model"
                .to_string(),
        ),
        Err(error) => Err(builder.redact_error(&error.to_string(), None)),
    }
}
