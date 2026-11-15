//! Memory service opening and the straight-through `start` phase.

use super::*;

/// Opens the memory service and the workspace memory scope when the boundary
/// allows dynamic content, then the advice audit log and the recall limits.
/// The error is already redacted; the caller closes what it opened.
pub fn open_memory(
    builder: &mut Builder,
    memory_config: &MemoryRuntime,
    runtime: &Runtime,
    dynamic_content: bool,
    home: &str,
    stderr: &mut (dyn Write + Send),
) -> Result<(), String> {
    let resolved = runtime;
    let workspace_path = builder.workspace_path.clone();
    if let (Some(_), Some(package)) = (&builder.memory.extension, memory_config.plugin_package()) {
        return Err(format!(
            "an injected memory extension cannot be combined with backend = \"plugin:{package}\""
        ));
    }
    if dynamic_content && memory_config.enabled {
        builder.workspace_scope =
            crate::runtime::wiring::workspace_memory_scope(memory_config, &workspace_path)?;
        if let Some(extension) = builder.memory.extension.clone() {
            extension
                .user_scope
                .validate()
                .map_err(|error| error.to_string())?;
            let guard = crate::memory::guard::content_guard(&builder.secret_values(Some(runtime)))?;
            let client = extension
                .client()
                .map_err(|error| error.to_string())?
                .with_guard(Arc::new(guard))
                .map_err(|error| builder.redact_error(&error.to_string(), Some(runtime)))?;
            if memory_config.require_encryption
                && client.describe().encryption_at_rest != Some(true)
            {
                return Err("selected memory service does not guarantee encryption at rest".into());
            }
            let shared = builder.shared_mut();
            shared.memory.client = Some(Arc::new(client));
            shared.memory.usable = true;
        } else if let Some(package) = memory_config.plugin_package() {
            // The client is built for each runner, where the plugin runtime exists.
            let shared = builder.shared_mut();
            shared.memory.plugin = Some(crate::runtime::wiring::PluginMemorySelection {
                package: package.to_string(),
                required: memory_config.required,
                require_encryption: memory_config.require_encryption,
            });
            shared.memory.user_scope = crate::memory::Scope::new("user", "default");
            shared.memory.usable = true;
        } else {
            let secrets = builder.secret_values(Some(resolved));
            match crate::runtime::wiring::open_memory_service_with_storage(
                &builder.storage,
                memory_config,
                &secrets,
                stderr,
            ) {
                Ok((service, user_scope, usable)) => {
                    let shared = builder.shared_mut();
                    shared.memory.service = service;
                    shared.memory.user_scope = user_scope;
                    shared.memory.usable = usable;
                    if usable {
                        let adapter = crate::memory::adapter::BuiltinAdapter::new(
                            shared.memory.service.clone(),
                        )
                        .map_err(|error| error.to_string())?;
                        shared.memory.client = Some(Arc::new(
                            crate::memory::client::Client::new(Arc::new(adapter))
                                .map_err(|error| error.to_string())?,
                        ));
                    }
                }
                Err(error) => {
                    return Err(builder.redact_error(&error, Some(resolved)));
                }
            }
        }
    }
    let advice_audit = if builder.memory.usable && !builder.memory.external() {
        match builder
            .storage
            .memory_advice(&Path::new(home).join(".otto/memory-advice.db"))
        {
            Ok(store) => Some(store),
            Err(_) => {
                let _ = writeln!(
                    stderr,
                    "warning: memory advice audit log unavailable, continuing without it"
                );
                None
            }
        }
    } else {
        None
    };
    {
        let shared = builder.shared_mut();
        shared.memory.recall_limit = memory_config.max_results;
        shared.memory.recall_token_budget = memory_config.recall_tokens;
        shared.memory.advice_audit = advice_audit;
    }
    Ok(())
}

// ---- straight-through start ----

pub struct StartOptions {
    pub memory_extension: Option<super::super::wiring::MemoryExtension>,
    pub host_entries: Vec<Vec<u8>>,
    pub config_path: String,
    pub explicit_config: bool,
    pub cwd: String,
    pub overrides: Overrides,
    pub logger: Arc<otto_platform::log::Logger>,
}

pub struct Started {
    pub builder: Builder,
    pub runtime: Runtime,
    pub workspace_path: String,
    pub sandbox: WorkspaceSandbox,
}

/// Builds the runtime `otto acp` runs on, from environment capture to the
/// memory service, without a frontend. Warnings go to `stderr` as each step
/// emits them; `Err` is the text the CLI would pass to `fail`.
pub async fn start(
    options: StartOptions,
    cancel: &CancellationToken,
    stderr: &mut (dyn Write + Send),
) -> Result<Started, String> {
    let host_entries = capture_environment(options.host_entries)?;
    let lookup = environment_lookup(&host_entries)?;
    let mut loaded = load(
        &host_entries,
        &lookup,
        &options.config_path,
        options.explicit_config,
        &options.cwd,
        &options.overrides.base_url,
    )?;
    let storage = Arc::new(crate::runtime::storage::Storage::default());
    let configured = configure(
        &mut loaded,
        &host_entries,
        &lookup,
        &options.overrides,
        &mut *stderr,
    )?;
    let workspace_path = canonical_workspace(&options.cwd, &loaded.boundary)?;
    let prepared = prepare_workspace(&loaded, &configured.environment, &workspace_path)?;
    let composed = compose(
        ComposeOptions {
            memory_extension: options.memory_extension,
            sandbox_driver_override: None,
            no_session: false,
            headless: false,
            overrides: options.overrides.clone(),
            explicit_config: options.explicit_config,
            logger: options.logger,
        },
        &loaded,
        &configured,
        &host_entries,
        &lookup,
        &storage,
        &prepared,
        &workspace_path,
        &mut *stderr,
    )?;
    let mut builder = composed.builder;
    let runtime = initial_runtime(&builder, None)?;
    let sandbox = compose_workspace_sandbox(
        &mut builder,
        &composed.sandbox_settings,
        Path::new(&loaded.config_path),
        options.explicit_config,
        None,
        &runtime.api_key_env,
        true,
        cancel,
        &mut *stderr,
    )
    .await;
    if cancel.is_cancelled() {
        let _ = sandbox.control.close().await;
        return Err("startup cancelled".to_string());
    }
    if !builder.boundary_allows_dynamic(Some(&runtime)) {
        let _ = sandbox.control.close().await;
        return Err(SESSION_OPERATION_UNAVAILABLE.to_string());
    }
    if let Err(message) = open_memory(
        &mut builder,
        &prepared.memory_config,
        &runtime,
        true,
        &loaded.home,
        &mut *stderr,
    ) {
        let _ = sandbox.control.close().await;
        return Err(message);
    }
    Ok(Started {
        builder,
        runtime,
        workspace_path,
        sandbox,
    })
}
