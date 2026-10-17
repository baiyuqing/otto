//! The startup composition `otto` and an embedding host share: environment
//! capture, layered configuration, the redaction boundary, storage, the
//! workspace, the sandbox and the memory service.
//!
//! `cli::run` calls the phase functions in order and keeps its own steps
//! between them (flag-specific checks, session resume, frontend selection).
//! [`start`] runs the same phases straight through, as `otto acp` does. Every
//! warning goes to the `stderr` writer at the moment its step emits it, and
//! every error string is already redacted by the boundary.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto_core::config::resolve::{Overrides, Runtime};
use otto_core::config::{
    File, MemoryRuntime, SandboxSettings, resolve_agents, resolve_mcp, resolve_memory,
    resolve_reflection, resolve_sandbox, resolve_skills,
};
use otto_core::session::RuntimeMetadata;
use tokio_util::sync::CancellationToken;

use crate::app::SESSION_OPERATION_UNAVAILABLE;
use crate::auth::{CapturedAuth, capture_auth_credentials};
use crate::sandbox::direct::DirectDriver;
use crate::sandbox::environment::{EnvironmentOptions, EnvironmentSnapshot, resolve_environment};
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy};

use crate::tool::workspace::Workspace;

use super::boundary::{self, BoundaryInputs};
use super::builder::{Builder, Shared, leaked_workspace, resolve_initial_runtime};
use super::sandbox_runtime::{
    OpenOptions, canonical_directory, canonical_executable_file, normalize_sandbox_runtime,
    open_sandbox_runtime, sandbox_runtime_warning, settings_from_config,
};
use super::sandbox_switch::{SandboxReloader, SandboxSwitch};

/// Darwin's process argument/environment budget is about 1 MiB. These
/// ceilings are deliberately larger while still bounding injected snapshots.
const MAX_LOOKUP_ENVIRONMENT_NAME_BYTES: usize = 4 << 10;
const MAX_LOOKUP_ENVIRONMENT_ENTRY_BYTES: usize = 1 << 20;
const MAX_LOOKUP_ENVIRONMENT_ENTRIES: usize = 1 << 18;
const MAX_LOOKUP_ENVIRONMENT_BYTES: usize = 8 << 20;
pub(crate) const MAX_CAPTURED_ENVIRONMENT_ENTRIES: usize = 1 << 19;
pub(crate) const MAX_CAPTURED_ENVIRONMENT_BYTES: usize = 16 << 20;

const ENVIRONMENT_SNAPSHOT_TOO_LARGE: &str = "process environment snapshot is too large";

/// The process environment, parsed into names and values.
pub(crate) type EnvironmentLookup = HashMap<String, String>;

/// Builds the automatic skill contract checker when skills and the check are
/// enabled and `TYPESAFE_API_KEY` is non-empty. The check defaults to enabled;
/// the legacy `[experimental]` setting remains a fallback for compatibility.
/// A database open failure prints one warning here, before any UI starts, and
/// leaves the checker off for the process; `TYPESAFE_API_KEY` never appears in
/// that warning.
pub(crate) fn skill_checker(
    storage: &crate::runtime::storage::Storage,
    config: &File,
    environment: &HashMap<String, String>,
    home: &str,
    stderr: &mut (dyn Write + Send),
) -> Option<Arc<crate::skill::check::Checker>> {
    if config.skills.enabled == Some(false) {
        return None;
    }
    if !config
        .skills
        .typesafe_skill_check
        .or(config.experimental.typesafe_skill_check)
        .unwrap_or(true)
    {
        return None;
    }
    let api_key = environment
        .get("TYPESAFE_API_KEY")
        .map(String::as_str)
        .unwrap_or("");
    if api_key.is_empty() {
        return None;
    }
    let db_path = Path::new(home).join(".otto/skill-checks.db");
    match storage.skill_checks(&db_path) {
        Ok(store) => Some(Arc::new(crate::skill::check::Checker::with_store(
            store,
            crate::typesafe::Client::new(crate::typesafe::BASE_URL, api_key),
        ))),
        Err(message) => {
            let _ = writeln!(
                stderr,
                "warning: skill check store unavailable ({message}), continuing without automatic contract checks"
            );
            None
        }
    }
}

/// The boundary that exists before a profile is resolved.
pub(crate) struct StartupBoundary {
    pub(crate) sandbox_secrets: Vec<String>,
    pub(crate) complete: bool,
    pub(crate) config: File,
    pub(crate) environment: HashMap<String, String>,
    pub(crate) base_url: String,
}

impl StartupBoundary {
    fn inputs(&self) -> BoundaryInputs<'_> {
        BoundaryInputs {
            sandbox_secrets: &self.sandbox_secrets,
            sandbox_secrets_complete: self.complete,
            config: &self.config,
            environment: &self.environment,
            overrides_base_url: &self.base_url,
        }
    }

    pub(crate) fn redactor(&self) -> otto_core::agent::redactor::Redactor {
        boundary::secret_redactor(&self.inputs(), None)
    }

    pub(crate) fn allows_dynamic(&self) -> bool {
        self.redactor().allows_dynamic_content()
    }

    pub(crate) fn redact(&self, message: &str) -> String {
        redact_with(&self.redactor(), message)
    }
}

/// A closed boundary renders nothing at all, deliberately: an incomplete
/// redaction set must not let a diagnostic escape.
pub(crate) fn redact_with(
    redactor: &otto_core::agent::redactor::Redactor,
    message: &str,
) -> String {
    if !redactor.allows_dynamic_content() {
        return String::new();
    }
    redactor.redact_string(message)
}

fn merge_redactions(first: &[String], second: &[String]) -> (Vec<String>, bool) {
    let mut collector = otto_core::safetext::SecretCollector::new();
    for group in [first, second] {
        for value in group {
            if !collector.add_form(value) {
                return (collector.values(), false);
            }
        }
    }
    (collector.values(), true)
}

fn environment_snapshot(
    host_entries: &[Vec<u8>],
    provider_names: Vec<String>,
) -> EnvironmentSnapshot {
    let options = EnvironmentOptions {
        host_entries: host_entries.to_vec(),
        provider_names,
        ..EnvironmentOptions::default()
    };
    match resolve_environment(&options) {
        Ok(snapshot) => snapshot,
        Err(rejected) => rejected.snapshot().clone(),
    }
}

/// Bounds the snapshot before anything reads it.
pub(crate) fn capture_environment(entries: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, String> {
    if entries.len() > MAX_CAPTURED_ENVIRONMENT_ENTRIES {
        return Err(ENVIRONMENT_SNAPSHOT_TOO_LARGE.to_string());
    }
    let mut total = 0usize;
    for entry in &entries {
        if entry.len() > MAX_CAPTURED_ENVIRONMENT_BYTES - total {
            return Err(ENVIRONMENT_SNAPSHOT_TOO_LARGE.to_string());
        }
        total += entry.len();
    }
    Ok(entries)
}

/// Parses `KEY=VALUE` entries, skipping every malformed one and refusing an
/// oversized set outright.
pub(crate) fn environment_lookup(entries: &[Vec<u8>]) -> Result<EnvironmentLookup, String> {
    let mut parsed: EnvironmentLookup = HashMap::new();
    let mut total = 0usize;
    for entry in entries {
        if entry.len() > MAX_LOOKUP_ENVIRONMENT_ENTRY_BYTES || entry.contains(&0) {
            continue;
        }
        let Ok(entry) = std::str::from_utf8(entry) else {
            continue;
        };
        let Some((name, value)) = entry.split_once('=') else {
            continue;
        };
        if name.len() > MAX_LOOKUP_ENVIRONMENT_NAME_BYTES || !valid_environment_name(name) {
            continue;
        }
        let previous = parsed.get(name);
        if previous.is_none() && parsed.len() >= MAX_LOOKUP_ENVIRONMENT_ENTRIES {
            return Err(ENVIRONMENT_SNAPSHOT_TOO_LARGE.to_string());
        }
        let mut next_total = total + name.len() + value.len();
        if let Some(previous) = previous {
            next_total -= name.len() + previous.len();
        }
        if next_total > MAX_LOOKUP_ENVIRONMENT_BYTES {
            return Err(ENVIRONMENT_SNAPSHOT_TOO_LARGE.to_string());
        }
        parsed.insert(name.to_string(), value.to_string());
        total = next_total;
    }
    Ok(parsed)
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return false;
    }
    bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

/// `$HOME`, else the passwd entry, made absolute.
pub(crate) fn resolve_home_for(lookup: &EnvironmentLookup) -> Result<String, String> {
    resolve_home(lookup)
}

/// The `otto memory` flag grammar names its own config path, so it cannot
/// build a [`CliOptions`].
pub(crate) fn load_config_for(
    config_path: &str,
    explicit: bool,
    home: &str,
) -> Result<(String, File), ()> {
    let path = if config_path.is_empty() {
        PathBuf::from(home).join(".config/otto/config.toml")
    } else {
        PathBuf::from(config_path)
    };
    let file =
        crate::config::layered::load_user(Path::new(home), &path, explicit).map_err(|_| ())?;
    Ok((path.display().to_string(), file))
}

pub(crate) fn config_environment_for(
    file: &File,
    lookup: &EnvironmentLookup,
) -> HashMap<String, String> {
    config_environment(file, lookup)
}

pub(crate) fn resolve_home(lookup: &EnvironmentLookup) -> Result<String, String> {
    const FAILED: &str = "resolve home directory";
    let mut home = lookup.get("HOME").cloned().unwrap_or_default();
    if home.is_empty() {
        home = current_user_home().ok_or_else(|| FAILED.to_string())?;
    }
    if home.is_empty() {
        return Err(FAILED.to_string());
    }
    let path = PathBuf::from(&home);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|_| FAILED.to_string())?
            .join(path)
    };
    Ok(absolute.to_string_lossy().into_owned())
}

/// The current user's home directory from the passwd database.
fn current_user_home() -> Option<String> {
    let user = nix::unistd::User::from_uid(nix::unistd::getuid()).ok()??;
    let home = user.dir.to_string_lossy().into_owned();
    (!home.is_empty()).then_some(home)
}

/// Opens `~/.otto/schedule.db`. Without it the process runs with no schedule
/// store and no delivery worker, after one warning.
pub(crate) fn open_schedule(
    storage: &crate::runtime::storage::Storage,
    home: &str,
    stderr: &mut (dyn Write + Send),
) -> Option<Arc<dyn crate::schedule::ScheduleStore>> {
    match storage.schedule(&Path::new(home).join(".otto/schedule.db")) {
        Ok(store) => Some(store),
        Err(_) => {
            let _ = writeln!(
                stderr,
                "warning: schedule store unavailable, continuing without scheduled deliveries"
            );
            None
        }
    }
}

/// A missing file at an implicit path is an empty one.
fn load_config(
    config_path: &str,
    explicit: bool,
    cwd: &str,
    home: &str,
) -> Result<(String, File), String> {
    let path: PathBuf = if config_path.is_empty() {
        [home, ".config", "otto", "config.toml"].iter().collect()
    } else {
        PathBuf::from(config_path)
    };
    let workspace = if cwd.is_empty() {
        Path::new(".")
    } else {
        Path::new(cwd)
    };
    let loaded = crate::config::layered::load(Path::new(home), &path, explicit, workspace)?;
    Ok((path.to_string_lossy().into_owned(), loaded.file))
}

/// Unlike `crate::config::resolution_environment`, every fixed key is always
/// present, even when the process has no such variable: the empty string is
/// inserted, and resolution distinguishes "absent" from "empty" nowhere. The
/// map also holds each profile's `api_key_env` and the `web_search` key names.
pub(crate) fn config_environment(
    file: &File,
    lookup: &EnvironmentLookup,
) -> HashMap<String, String> {
    const FIXED_KEYS: [&str; 7] = [
        "HOME",
        "OTTO_PROVIDER",
        "OTTO_PROFILE",
        "OTTO_MODEL",
        "OTTO_API_KEY",
        "OTTO_UI",
        "OTTO_TRACE",
    ];
    use crate::tool::web::{BRAVE_KEY_ENV, GOOGLE_ENGINE_ENV, GOOGLE_KEY_ENV};
    let mut keys: BTreeSet<&str> = FIXED_KEYS.into_iter().collect();
    keys.extend([GOOGLE_KEY_ENV, GOOGLE_ENGINE_ENV, BRAVE_KEY_ENV]);
    for profile in file.profiles.values() {
        if !profile.api_key_env.is_empty() {
            keys.insert(profile.api_key_env.as_str());
        }
    }
    let credential_variables = file.plugins.credential_variables();
    keys.extend(credential_variables.iter().map(String::as_str));
    keys.extend(file.storage.credential_variables());
    keys.extend(file.memory.credential_variable());
    keys.extend(["PATH", "TMPDIR", "LANG", "TERM"]);
    keys.into_iter()
        .map(|key| {
            (
                key.to_string(),
                lookup.get(key).cloned().unwrap_or_default(),
            )
        })
        .collect()
}

/// `OTTO_API_KEY`, the selected key name and every configured one, deduplicated
/// and sorted.
pub(crate) fn sandbox_provider_environment_names(file: &File, selected: &str) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    names.insert("OTTO_API_KEY".to_string());
    if !selected.is_empty() {
        names.insert(selected.to_string());
    }
    for profile in file.profiles.values() {
        if !profile.api_key_env.is_empty() {
            names.insert(profile.api_key_env.clone());
        }
    }
    names.extend(file.plugins.credential_variables());
    names.extend(file.storage.credential_variables().map(str::to_owned));
    names.extend(file.memory.credential_variable().map(str::to_owned));
    names.into_iter().collect()
}

/// Existing skill and agent roots become read paths so discovery can reach them
/// from inside the sandbox. A root is only promoted when it is a real
/// directory: `read_paths` are canonicalized into `(subpath <target>)` grants,
/// so promoting a symlink would widen the sandbox to whatever the link points
/// at, and the default roots include the workspace-relative `.otto/skills`,
/// which anything that writes to the workspace controls.
pub(crate) fn resolve_sandbox_settings(
    file: &File,
    environment: &HashMap<String, String>,
    workspace_path: &str,
    driver_override: Option<&str>,
) -> Result<SandboxSettings, String> {
    let mut raw = file.sandbox.clone();
    let agents =
        resolve_agents(file, environment, workspace_path).map_err(|error| error.to_string())?;
    let home = Path::new(environment.get("HOME").map(String::as_str).unwrap_or(""));
    let workspace = Path::new(workspace_path);
    let skill_roots = crate::config::layered::catalog_roots(
        file,
        home,
        workspace,
        &resolve_skills(file, environment, workspace_path).roots,
        "skills",
    )?;
    let agent_roots =
        crate::config::layered::catalog_roots(file, home, workspace, &agents.roots, "agents")?;
    let plugin_roots = if file.plugins.enabled != Some(false) {
        let mut discovery = file.clone();
        discovery.mcp.enabled = Some(false);
        let service = crate::plugin::Service::new(home);
        crate::plugin::Snapshot::load(home, &discovery, environment)
            .packages
            .into_iter()
            .map(|p| {
                service
                    .package_path(&p.digest)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let roots = skill_roots
        .into_iter()
        .chain(agent_roots)
        .chain(plugin_roots);
    for root in roots {
        if std::fs::symlink_metadata(&root).is_ok_and(|info| info.is_dir()) {
            raw.read_paths.push(root);
        }
    }
    let settings = resolve_sandbox(&raw, driver_override).map_err(|error| error.to_string())?;
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
pub(crate) struct WorkspaceSandbox {
    pub(crate) control: Arc<SandboxSwitch>,
    pub(crate) approval_executor: Option<Arc<Executor>>,
    /// The exact resolved environment paired with `approval_executor`. A
    /// constrained delivery setup must use this snapshot, never borrow the
    /// target's ordinary sandbox environment.
    pub(crate) approval_environment: Option<Vec<String>>,
    pub(crate) reloader: Option<Arc<SandboxReloader>>,
}

/// Opens one workspace's sandbox and, when eligible, its elevated
/// bash-approval executor, filling in `builder`'s sandbox-derived fields
/// (`command_executor`, `sandbox_environment`, `sandbox_info`,
/// `sandbox_secrets*`, `excluded_commands`, `bash_approvals`) along the way.
///
/// Shared by `run`'s startup composition and `load_workspace`, so a workspace
/// admitted after startup gets the same `--sandbox` override, `/approve`
/// support, and `/sandbox reload` wiring as the startup workspace — see
/// `docs/specs/2026-09-26-serve-multiple-workspaces.md` ("Server structure").
/// `elevate` gates `bash_approvals`; `sandbox_settings`, `config_path`,
/// `explicit_config`, `driver_override`, and `api_key_env` are inputs the two
/// callers resolve differently (startup from CLI flags and a resumed
/// session's metadata, `load_workspace` from `Shared` alone), so they stay
/// parameters rather than being resolved inside this function.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn compose_workspace_sandbox(
    builder: &mut Builder,
    sandbox_settings: &SandboxSettings,
    config_path: &Path,
    explicit_config: bool,
    driver_override: Option<&str>,
    api_key_env: &str,
    elevate: bool,
    disabled: &Disabled,
    cancel: &CancellationToken,
    stderr: &mut (dyn Write + Send),
) -> WorkspaceSandbox {
    if disabled.bash {
        // No sandbox opens and bash stays unavailable; the switch still
        // exists because every frontend reads sandbox state through it.
        let runtime = crate::runtime::sandbox_runtime::policy_disabled_runtime();
        builder.sandbox_info = runtime.info;
        return WorkspaceSandbox {
            control: SandboxSwitch::new(runtime),
            approval_executor: None,
            approval_environment: None,
            reloader: None,
        };
    }
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
    let sandbox_info = sandbox.info;
    let redaction_values = sandbox.redaction_values.clone();
    let redactions_complete = sandbox.redactions_complete;
    let control = SandboxSwitch::new(sandbox);
    if had_executor {
        builder.command_executor =
            Some(Arc::clone(&control) as Arc<dyn crate::sandbox::CommandExecutor>);
    }
    builder.sandbox_environment = sandbox_environment;
    builder.sandbox_info = sandbox_info;
    let (merged, merged_complete) = merge_redactions(&builder.sandbox_secrets, &redaction_values);
    builder.sandbox_secrets = merged;
    builder.sandbox_secrets_complete =
        builder.sandbox_secrets_complete && redactions_complete && merged_complete;

    let mut approval_executor: Option<Arc<Executor>> = None;
    let mut approval_environment: Option<Vec<String>> = None;
    if sandbox_info.mode == crate::runtime::info::SandboxMode::Seatbelt {
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
    if let Some(warning) = sandbox_runtime_warning(builder.effective_sandbox_info()) {
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
            environment: builder.environment.clone(),
            api_key_env: api_key_env.to_string(),
            reopen: open_options,
            cancel: cancel.child_token(),
            excluded: builder.excluded_commands.clone(),
        })
    });

    builder.grant_control = reloader
        .as_ref()
        .map(|control| control.clone() as Arc<dyn crate::app::SandboxControl>);

    WorkspaceSandbox {
        control,
        approval_executor,
        approval_environment,
        reloader,
    }
}

/// Builds a `Builder` for a workspace admitted after startup: `serve.rs`'s
/// workspace registry calls this the first time a path is loaded.
///
/// Reuses the startup sequence's pieces rather than duplicating them:
/// `leaked_workspace` and `resolve_mcp` resolve against `canonical_path`
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
    if memory_config.remote.is_some()
        && let (Some(client), Some(extension)) = (&scoped.memory.client, &scoped.memory.extension)
    {
        let grant = remote_learning_grant(extension.user_scope.clone(), &workspace_scope);
        let client = client
            .with_grant(grant)
            .map_err(|error| error.to_string())?;
        scoped.memory.client = Some(Arc::new(client));
    }
    let shared = Arc::new(scoped);
    let workspace = leaked_workspace(canonical_path).map_err(|error| error.to_string())?;
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
    pub(crate) reflection: bool,
    pub(crate) schedule: bool,
    pub(crate) session_persistence: bool,
    pub(crate) usage: bool,
    pub(crate) task_history: bool,
}

impl Disabled {
    /// Sets `[mcp]`, `[memory]`, `[skills]`, `[plugins]`, `[agents]` and
    /// `[reflection]` `enabled = false` in memory for each disabled one.
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
        if self.reflection {
            file.reflection.enabled = false;
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
    pub(crate) workspace: &'static Workspace,
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
    let workspace = match leaked_workspace(Path::new(workspace_path)) {
        Ok(workspace) => workspace,
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

pub(crate) struct ComposeOptions {
    pub(crate) memory_extension: Option<super::wiring::MemoryExtension>,
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
        memory: super::wiring::MemoryWiring {
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

/// Opens the memory service and the workspace memory scope when the boundary
/// allows dynamic content, then the advice audit log and the recall limits.
/// The error is already redacted; the caller closes what it opened.
pub(crate) fn open_memory(
    builder: &mut Builder,
    memory_config: &MemoryRuntime,
    runtime: &Runtime,
    dynamic_content: bool,
    home: &str,
    stderr: &mut (dyn Write + Send),
) -> Result<(), String> {
    let resolved = runtime;
    let workspace_path = builder.workspace_path.clone();
    if dynamic_content && memory_config.enabled {
        builder.workspace_scope =
            crate::runtime::wiring::workspace_memory_scope(memory_config, &workspace_path)?;
        if let Some(remote) = &memory_config.remote {
            if builder.memory.extension.is_some() {
                return Err(
                    "an injected memory extension cannot be combined with backend = \"remote\""
                        .into(),
                );
            }
            let token = builder
                .environment
                .get(&remote.token_env)
                .cloned()
                .unwrap_or_default();
            if token.is_empty() {
                let message = format!(
                    "memory service token variable {} is unset or empty",
                    remote.token_env
                );
                if memory_config.required {
                    return Err(message);
                }
                let _ = writeln!(stderr, "warning: {message}; continuing without memory");
            } else {
                let service =
                    crate::memory::remote::RemoteService::new(crate::memory::remote::Config {
                        url: remote.url.clone(),
                        token,
                        recall_timeout: remote.recall_timeout,
                        logger: builder.logger.clone(),
                    })?;
                let user_scope = otto_core::memory::Scope {
                    namespace: "user".into(),
                    id: remote.user.clone(),
                };
                let grant = remote_learning_grant(user_scope.clone(), &builder.workspace_scope);
                builder.shared_mut().memory.extension = Some(
                    crate::runtime::wiring::MemoryExtension::owned(Arc::new(service), user_scope)
                        .with_learning(grant),
                );
            }
        }
        if let Some(extension) = builder.memory.extension.clone() {
            extension
                .user_scope
                .validate()
                .map_err(|error| error.to_string())?;
            let guard_values: Vec<String> = builder
                .secret_values(Some(runtime))
                .into_iter()
                .filter(|value| value.len() <= crate::memory::MAX_EXACT_GUARD_VALUE_BYTES)
                .collect();
            let exact = crate::memory::guard::ExactGuard::new(&guard_values)
                .map_err(|_| "build memory secret guard".to_string())?;
            let guard = crate::memory::guard::CompositeGuard::new(vec![
                Box::new(crate::memory::guard::DefaultGuard),
                Box::new(exact),
            ]);
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
        } else if memory_config.remote.is_none() {
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
    let advice_audit = if builder.memory.usable && builder.memory.extension.is_none() {
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

/// The remote backend's grant: explicit writes to the user scope and one
/// workspace scope. The client checks every request's scopes against it, so
/// each workspace `otto serve` loads needs its own (`load_workspace`).
fn remote_learning_grant(
    user_scope: otto_core::memory::Scope,
    workspace_scope: &crate::memory::Scope,
) -> otto_core::memory::LearningGrant {
    otto_core::memory::LearningGrant {
        scopes: vec![
            user_scope,
            otto_core::memory::Scope {
                namespace: workspace_scope.namespace.clone(),
                id: workspace_scope.id.clone(),
            },
        ],
        input_classes: vec!["explicit_memory".into()],
        background: false,
    }
}

// ---- straight-through start ----

pub(crate) struct StartOptions {
    pub(crate) memory_extension: Option<super::wiring::MemoryExtension>,
    pub(crate) host_entries: Vec<Vec<u8>>,
    pub(crate) config_path: String,
    pub(crate) explicit_config: bool,
    pub(crate) cwd: String,
    pub(crate) overrides: Overrides,
    pub(crate) logger: Arc<crate::log::Logger>,
    pub(crate) disabled: Disabled,
}

pub(crate) struct Started {
    pub(crate) builder: Builder,
    pub(crate) runtime: Runtime,
    pub(crate) workspace_path: String,
    pub(crate) sandbox: WorkspaceSandbox,
}

/// Builds the runtime `otto acp` runs on, from environment capture to the
/// memory service, without a frontend. Warnings go to `stderr` as each step
/// emits them; `Err` is the text the CLI would pass to `fail`.
pub(crate) async fn start(
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
    let storage = open_storage(
        &loaded,
        &lookup,
        &crate::runtime::storage::BackendRegistry::default(),
    )
    .await?;
    let configured = configure(&mut loaded, &host_entries, &lookup, &mut *stderr);
    // After the boundary and the credential capture, so the redaction set
    // does not depend on the disabled components.
    options.disabled.apply(&mut loaded.config);
    let workspace_path = canonical_workspace(&options.cwd, &loaded.boundary)?;
    let prepared = prepare_workspace(&loaded, &configured.environment, &storage, &workspace_path)?;
    let composed = compose(
        ComposeOptions {
            memory_extension: options.memory_extension,
            sandbox_driver_override: None,
            no_session: false,
            headless: false,
            overrides: options.overrides.clone(),
            explicit_config: options.explicit_config,
            logger: options.logger,
            disabled: options.disabled,
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
        &options.disabled,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `start` composes the `otto acp` runtime without a frontend, and a
    /// disabled component stays off: MCP is switched off in the resolved
    /// configuration and the usage store is never opened.
    #[tokio::test]
    async fn start_honors_the_disabled_components() {
        let directory = tempfile::tempdir().expect("tempdir");
        let home = directory.path().join("home");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        std::fs::write(
            home.join(".config/otto/config.toml"),
            r#"
default_profile = "alpha"

[profiles.alpha]
provider = "openai-compatible"
base_url = "https://example.com/v1"
model = "gpt-test"
api_key_env = "ALPHA_KEY"

[memory]
enabled = false

[sandbox]
driver = "off"
"#,
        )
        .expect("write config");
        let cancel = CancellationToken::new();
        let mut stderr = Vec::new();

        let started = start(
            StartOptions {
                memory_extension: None,
                host_entries: vec![
                    format!("HOME={}", home.to_string_lossy()).into_bytes(),
                    b"ALPHA_KEY=sk-alpha".to_vec(),
                ],
                config_path: String::new(),
                explicit_config: false,
                cwd: workspace.to_string_lossy().into_owned(),
                overrides: Overrides::default(),
                logger: Arc::new(crate::log::Logger::discard()),
                disabled: Disabled {
                    mcp: true,
                    usage: true,
                    ..Disabled::default()
                },
            },
            &cancel,
            &mut stderr,
        )
        .await
        .unwrap_or_else(|message| panic!("start: {message}"));

        assert!(!started.builder.mcp.enabled);
        assert!(!home.join(".otto/usage.db").exists());
        assert!(
            home.join(".otto/tasks.db").exists(),
            "an enabled store still opens"
        );
        started
            .sandbox
            .control
            .close()
            .await
            .expect("close sandbox");
    }

    /// The component switches write `enabled = false` for exactly the
    /// disabled components.
    #[test]
    fn disabled_applies_only_the_chosen_switches() {
        let mut file = File::default();
        Disabled {
            skills: true,
            subagents: true,
            reflection: true,
            ..Disabled::default()
        }
        .apply(&mut file);
        assert_eq!(file.skills.enabled, Some(false));
        assert_eq!(file.agents.enabled, Some(false));
        assert!(!file.reflection.enabled);
        assert_eq!(file.mcp.enabled, None);
        assert_eq!(file.memory.enabled, None);
        assert_eq!(file.plugins.enabled, None);
    }

    /// Starts `otto acp` with `backend = "remote"` at `url`. `memory` is added
    /// to `[memory]`; `token`, when set, is the `OTTO_MEM_TOKEN` value.
    async fn start_remote(
        url: &str,
        memory: &str,
        token: Option<&str>,
        extension: Option<super::super::wiring::MemoryExtension>,
    ) -> (Result<Started, String>, String, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("tempdir");
        let home = directory.path().join("home");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        std::fs::write(
            home.join(".config/otto/config.toml"),
            format!(
                r#"
default_profile = "alpha"

[profiles.alpha]
provider = "openai-compatible"
base_url = "https://example.com/v1"
model = "gpt-test"
api_key_env = "ALPHA_KEY"

[memory]
enabled = true
backend = "remote"
{memory}

[memory.remote]
url = "{url}"
user = "u1"

[sandbox]
driver = "off"
"#
            ),
        )
        .expect("write config");
        let mut host_entries = vec![
            format!("HOME={}", home.to_string_lossy()).into_bytes(),
            b"ALPHA_KEY=sk-alpha".to_vec(),
        ];
        if let Some(token) = token {
            host_entries.push(format!("OTTO_MEM_TOKEN={token}").into_bytes());
        }
        let mut stderr = Vec::new();
        let started = start(
            StartOptions {
                memory_extension: extension,
                host_entries,
                config_path: String::new(),
                explicit_config: false,
                cwd: workspace.to_string_lossy().into_owned(),
                overrides: Overrides::default(),
                logger: Arc::new(crate::log::Logger::discard()),
                disabled: Disabled {
                    mcp: true,
                    usage: true,
                    ..Disabled::default()
                },
            },
            &CancellationToken::new(),
            &mut stderr,
        )
        .await;
        (
            started,
            String::from_utf8_lossy(&stderr).into_owned(),
            directory,
        )
    }

    #[tokio::test]
    async fn remote_memory_without_a_token_runs_without_memory() {
        let (started, stderr, directory) =
            start_remote("https://mem.example.test", "", None, None).await;
        let started = started.expect("start");
        assert!(
            stderr.contains("OTTO_MEM_TOKEN is unset or empty; continuing without memory"),
            "{stderr}"
        );
        assert!(started.builder.memory.extension.is_none());
        assert!(started.builder.memory.client.is_none());
        assert!(!started.builder.memory.usable);
        assert!(
            !directory.path().join("home/.otto/memory").exists(),
            "the files store is not opened"
        );
        started.sandbox.control.close().await.expect("close");
    }

    #[tokio::test]
    async fn remote_memory_without_a_token_fails_when_required() {
        let (started, _, _directory) =
            start_remote("https://mem.example.test", "required = true", None, None).await;
        let message = started.err().expect("startup fails");
        assert!(message.contains("OTTO_MEM_TOKEN is unset"), "{message}");
    }

    #[tokio::test]
    async fn remote_memory_rejects_an_injected_extension() {
        let service = crate::memory::remote::RemoteService::new(crate::memory::remote::Config {
            url: "https://other.example.test".into(),
            token: "t".into(),
            recall_timeout: std::time::Duration::from_secs(1),
            logger: Arc::new(crate::log::Logger::discard()),
        })
        .expect("service");
        let extension = super::super::wiring::MemoryExtension::new(
            Arc::new(service),
            otto_core::memory::Scope {
                namespace: "user".into(),
                id: "u2".into(),
            },
        );
        let (started, _, _directory) = start_remote(
            "https://mem.example.test",
            "",
            Some("mem-token"),
            Some(extension),
        )
        .await;
        let message = started.err().expect("startup fails");
        assert!(message.contains("injected memory extension"), "{message}");
    }

    /// The token is a secret like a provider key: it is redacted and its
    /// variable is kept out of sandboxed commands. A workspace `otto serve`
    /// loads later gets a grant for its own scope, so the client does not
    /// refuse its writes.
    #[tokio::test(flavor = "multi_thread")]
    async fn remote_memory_with_a_token_selects_the_service() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", closed.local_addr().expect("addr"));
        drop(closed);
        let (started, _, directory) = start_remote(&url, "", Some("mem-secret-token"), None).await;
        let started = started.expect("start");
        let builder = &started.builder;
        assert!(builder.memory.usable);
        let client = builder.memory.client.as_ref().expect("client");
        assert_eq!(client.describe().identity, "remote");
        assert_eq!(
            client.mode(),
            otto_core::memory::AuthorizationMode::DelegatedLearning
        );
        let secrets = builder.secret_values(Some(&started.runtime));
        assert!(secrets.iter().any(|value| value == "mem-secret-token"));
        assert!(secrets.iter().any(|value| value == "OTTO_MEM_TOKEN"));
        assert!(
            sandbox_provider_environment_names(&builder.config, "")
                .contains(&"OTTO_MEM_TOKEN".to_string())
        );

        let second = directory.path().join("second");
        std::fs::create_dir_all(&second).expect("second workspace");
        let second = second.canonicalize().expect("canonical");
        let mut stderr = Vec::new();
        let (loaded, sandbox) = load_workspace(
            Arc::clone(&builder.shared),
            &second,
            &Disabled::default(),
            &CancellationToken::new(),
            &mut stderr,
        )
        .await
        .expect("load workspace");
        assert_ne!(loaded.workspace_scope, builder.workspace_scope);
        let request = otto_core::memory::RememberRequest {
            context: loaded.memory_context(otto_core::memory::Origin::Model, None),
            request_id: crate::memory::new_id().expect("id"),
            content: otto_core::memory::Content::Text("fact".into()),
            target: None,
            metadata: Default::default(),
        };
        let error = loaded
            .memory
            .client
            .as_ref()
            .expect("client")
            .remember(&request, &CancellationToken::new())
            .await
            .expect_err("the port is closed");
        assert_ne!(error.kind, otto_core::memory::ErrorKind::Unauthorized);
        sandbox.control.close().await.expect("close");
        started.sandbox.control.close().await.expect("close");
    }
}
