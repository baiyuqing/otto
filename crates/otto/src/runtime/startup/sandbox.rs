//! Sandbox settings and the per-workspace sandbox composition.

use super::*;

/// `OTTO_API_KEY`, the selected key name and every configured one, deduplicated
/// and sorted.
pub fn sandbox_provider_environment_names(file: &File, selected: &str) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    names.insert("OTTO_API_KEY".to_string());
    names.insert("ANTHROPIC_API_KEY".to_string());
    if !selected.is_empty() {
        names.insert(selected.to_string());
    }
    for provider in file.providers.values() {
        if !provider.api_key_env.is_empty() {
            names.insert(provider.api_key_env.clone());
        }
        if !provider.workspace_id_env.is_empty() {
            names.insert(provider.workspace_id_env.clone());
        }
    }
    names.extend(file.plugins.credential_variables());
    names.into_iter().collect()
}

/// Resolves `[sandbox]` with the CLI driver override and checks the result
/// against the managed policy.
pub(crate) fn resolve_sandbox_settings(
    file: &File,
    driver_override: Option<&str>,
) -> Result<SandboxSettings, String> {
    let settings =
        resolve_sandbox(&file.sandbox, driver_override).map_err(|error| error.to_string())?;
    if let Some(policy) = &file.policy {
        policy.sandbox(&settings).map_err(|e| e.to_string())?;
    }
    Ok(settings)
}

/// The sandbox artifacts one workspace's composition produces: the switch
/// wired into `builder.command_executor`, the elevated bash-approval
/// executor, also used for `[sandbox].excluded_commands` (`None` when the
/// sandbox is not Seatbelt or the unconfined executor could not be built),
/// and the reloader `/sandbox reload` uses to reopen this workspace's own
/// sandbox (`None` without a usable sandbox).
pub struct WorkspaceSandbox {
    pub control: Arc<SandboxSwitch>,
    pub approval_executor: Option<Arc<Executor>>,
    /// The exact resolved environment paired with `approval_executor`. A
    /// constrained delivery setup must use this snapshot, never borrow the
    /// target's ordinary sandbox environment.
    pub approval_environment: Option<Vec<String>>,
    pub reloader: Option<Arc<SandboxReloader>>,
}

/// Opens one workspace's sandbox and, when eligible, its elevated
/// bash-approval executor, filling in `builder`'s sandbox-derived fields
/// (`command_executor`, `sandbox_environment`, `sandbox_info`,
/// `sandbox_secrets*`, `excluded_commands`, `bash_approvals`) along the way.
///
/// Shared by `run`'s startup composition and `load_workspace`, so a workspace
/// admitted after startup gets the same `--sandbox` override, `/approve`
/// support, and `/sandbox reload` wiring as the startup workspace.
/// `elevate` gates `bash_approvals`; `sandbox_settings`, `config_path`,
/// `explicit_config`, `driver_override`, and `api_key_env` are inputs the two
/// callers resolve differently (startup from CLI flags and a resumed
/// session's metadata, `load_workspace` from `Shared` alone), so they stay
/// parameters rather than being resolved inside this function.
#[allow(clippy::too_many_arguments)]
pub async fn compose_workspace_sandbox(
    builder: &mut Builder,
    sandbox_settings: &SandboxSettings,
    config_path: &Path,
    explicit_config: bool,
    driver_override: Option<&str>,
    api_key_env: &str,
    elevate: bool,
    cancel: &CancellationToken,
    stderr: &mut (dyn Write + Send),
) -> WorkspaceSandbox {
    let open_options = OpenOptions {
        settings: settings_from_config(sandbox_settings),
        workspace: builder.workspace_path.clone(),
        shell: builder.shell.clone(),
        home: builder.home.clone(),
        host_entries: builder.host_entries.clone(),
        provider_names: sandbox_provider_environment_names(&builder.config, api_key_env),
        managed_worktrees: crate::sandbox::worktrees::managed(Path::new(&builder.workspace_path)),
    };
    let sandbox = normalize_sandbox_runtime(open_sandbox_runtime(&open_options, cancel).await);
    // The bash tool captures its executor when a runner is built, so the
    // process sandbox lives behind a switch that `/sandbox reload` can replace
    // without rebuilding the session or the runner.
    let had_executor = sandbox.executor.is_some();
    let sandbox_environment = sandbox.environment.clone();
    let sandbox_info = sandbox.info.clone();
    let redaction_values = sandbox.redaction_values.clone();
    let redactions_complete = sandbox.redactions_complete;
    let control = SandboxSwitch::new(sandbox);
    if had_executor {
        builder.command_executor =
            Some(Arc::clone(&control) as Arc<dyn crate::sandbox::CommandExecutor>);
    }
    builder.sandbox_environment = sandbox_environment;
    builder.sandbox_info = sandbox_info.clone();
    let (merged, merged_complete) = merge_redactions(&builder.sandbox_secrets, &redaction_values);
    builder.sandbox_secrets = merged;
    builder.sandbox_secrets_complete =
        builder.sandbox_secrets_complete && redactions_complete && merged_complete;

    let mut approval_executor: Option<Arc<Executor>> = None;
    let mut approval_environment: Option<Vec<String>> = None;
    if sandbox_info.mode == crate::sandbox::info::SandboxMode::Seatbelt {
        let elevated_environment = resolve_environment(&EnvironmentOptions {
            host_entries: builder.host_entries.clone(),
            provider_names: sandbox_provider_environment_names(&builder.config, api_key_env),
            allow_names: sandbox_settings.allow_env.clone(),
            private_directories: None,
        });
        if let Ok(snapshot) = elevated_environment
            && snapshot.redactions_complete()
            && let Some(entries) = snapshot.entries()
            && let Ok(executor) = Executor::new(
                Arc::new(DirectDriver::new()),
                Policy {
                    filesystem: FilesystemMode::Unconfined,
                    network: NetworkMode::Allow,
                },
                builder.workspace.root(),
            )
        {
            let executor = Arc::new(executor);
            let command_executor: Arc<dyn crate::sandbox::CommandExecutor> = executor.clone();
            builder.excluded_commands = Some(Arc::new(crate::tool::bash::ExcludedCommands::new(
                command_executor.clone(),
                entries.to_vec(),
                sandbox_settings.excluded_commands.clone(),
            )));
            if elevate {
                builder.bash_approvals = Some(Arc::new(crate::tool::bash::BashApprovals::new(
                    command_executor,
                    entries.to_vec(),
                )));
            }
            let (merged, complete) =
                merge_redactions(&builder.sandbox_secrets, snapshot.redaction_values());
            builder.sandbox_secrets = merged;
            builder.sandbox_secrets_complete &= complete;
            approval_environment = Some(entries.to_vec());
            approval_executor = Some(executor);
        }
    }
    if let Some(warning) = sandbox_runtime_warning(&builder.effective_sandbox_info()) {
        let _ = stderr.write_all(warning.as_bytes());
    }
    // No reloader at all without a usable sandbox, because there is then
    // nothing to re-point.
    let reloader = (had_executor && builder.effective_sandbox_info().bash_available).then(|| {
        Arc::new(SandboxReloader {
            control: Arc::clone(&control),
            config_path: config_path.to_path_buf(),
            explicit_config,
            driver_override: driver_override.map(str::to_string),
            api_key_env: api_key_env.to_string(),
            reopen: open_options,
            cancel: cancel.child_token(),
            excluded: builder.excluded_commands.clone(),
        })
    });

    builder.grant_control = reloader
        .as_ref()
        .map(|control| control.clone() as Arc<dyn crate::sandbox::control::SandboxControl>);

    WorkspaceSandbox {
        control,
        approval_executor,
        approval_environment,
        reloader,
    }
}
