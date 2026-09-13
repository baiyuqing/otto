//! Process composition: the port of `run`/`runWithDependencies` in
//! `cmd/otto/main.go`.
//!
//! The order of operations, the exact stderr text and the exit codes match
//! Go, because `cmd/otto/main_test.go` pins them. What is deliberately absent
//! is named by a "not yet ported" message rather than silently skipped: the
//! `login`, `logout`, `memory` and `sandbox` subcommands, `serve`, the TUI,
//! memory wiring, skills, sub-agents and `/sandbox reload`.
//!
//! Safety: every diagnostic that could carry a host path, an environment name
//! or a provider URL goes through the redaction boundary before it is
//! written. A boundary that cannot prove it collected every secret renders
//! the empty string, exactly as Go's `errRedactedRuntimeBoundary` does.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto_core::config::resolve::{Overrides, Runtime};
use otto_core::config::{
    File, SandboxSettings, UiMode, resolve_agents, resolve_memory, resolve_sandbox, resolve_server,
    resolve_skills, resolve_ui_mode,
};
use otto_core::session::{CURRENT_VERSION, Header, RuntimeMetadata};
use tokio_util::sync::CancellationToken;

use crate::sandbox::environment::{EnvironmentOptions, EnvironmentSnapshot, resolve_environment};
use crate::session;

use super::boundary::{self, BoundaryInputs};
use super::controller::{Controller, SESSION_OPERATION_UNAVAILABLE};
use super::flags::{CliOptions, ParseFailure, Parsed, parse_flags};
use super::repl::{self, Repl};
use super::runtime_builder::{
    Builder, SharedSession, leaked_workspace, resolve_initial_runtime, validate_session_workspace,
};
use super::sandbox_runtime::{
    OpenOptions, canonical_directory, canonical_executable_file, normalize_sandbox_runtime,
    open_sandbox_runtime, sandbox_runtime_warning, settings_from_config,
};
use super::serve;

/// Go's `maxApprovePromptBytes`.
const MAX_APPROVE_PROMPT_BYTES: usize = 1 << 20;

/// Darwin's process argument/environment budget is about 1 MiB. These
/// ceilings are deliberately larger while still bounding injected snapshots.
const MAX_LOOKUP_ENVIRONMENT_NAME_BYTES: usize = 4 << 10;
const MAX_LOOKUP_ENVIRONMENT_ENTRY_BYTES: usize = 1 << 20;
const MAX_LOOKUP_ENVIRONMENT_ENTRIES: usize = 1 << 18;
const MAX_LOOKUP_ENVIRONMENT_BYTES: usize = 8 << 20;
const MAX_CAPTURED_ENVIRONMENT_ENTRIES: usize = 1 << 19;
const MAX_CAPTURED_ENVIRONMENT_BYTES: usize = 16 << 20;

const ENVIRONMENT_SNAPSHOT_TOO_LARGE: &str = "process environment snapshot is too large";

/// Phase 8 owns the terminal frontend; until then an explicit `--ui tui` on a
/// real terminal is refused rather than silently answered with the REPL.
const TUI_NOT_PORTED: &str = "--ui tui is not yet ported; use --ui repl";

/// Which frontend the resolved UI mode selected. Port of `frontendKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    Repl,
    Once,
}

/// The process environment, parsed into names and values.
type EnvironmentLookup = HashMap<String, String>;

/// Writes `otto: {message}\n` and returns Go's exit code 1.
fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}

/// Runs one `otto` invocation and returns its exit code.
///
/// `environment_entries` is the raw process environment (`KEY=VALUE` byte
/// strings); `terminal` says whether stdin and stdout are both a terminal.
/// Both are parameters rather than reads of process state so the whole
/// startup path is reachable from tests.
pub async fn run(
    args: &[String],
    stdin: Box<dyn BufRead + Send + 'static>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    environment_entries: Vec<Vec<u8>>,
    terminal: bool,
    cancel: &CancellationToken,
) -> i32 {
    // Go dispatches these before flag parsing, because their argument
    // grammars are their own.
    if let Some(first) = args.first()
        && matches!(first.as_str(), "sandbox" | "memory" | "login" | "logout")
    {
        return fail(stderr, &format!("{first} is not yet ported"));
    }

    let options = match parse_flags(args, stdout) {
        Ok(Parsed::Help) => return 0,
        Ok(Parsed::Options(options)) => *options,
        Err(ParseFailure::Unsafe) => {
            let _ = stderr.write_all(b"otto: invalid command-line arguments\n");
            return 2;
        }
        Err(ParseFailure::Rejected(message)) => {
            let _ = stderr.write_all(message.as_bytes());
            return 2;
        }
    };
    let host_entries = match capture_environment(environment_entries) {
        Ok(entries) => entries,
        Err(message) => return fail(stderr, &message),
    };
    let lookup = match environment_lookup(&host_entries) {
        Ok(lookup) => lookup,
        Err(message) => return fail(stderr, &message),
    };

    let process_snapshot = environment_snapshot(&host_entries, vec!["OTTO_API_KEY".to_string()]);
    let empty_config = File::default();
    let empty_environment = HashMap::new();
    let mut startup = StartupBoundary {
        sandbox_secrets: process_snapshot.redaction_values().to_vec(),
        complete: process_snapshot.redactions_complete(),
        config: empty_config,
        environment: empty_environment,
        base_url: options.base_url.clone(),
    };

    let home = match resolve_home(&lookup) {
        Ok(home) => home,
        Err(message) => return fail(stderr, &message),
    };
    let (config_path, config_file) = match load_config(&options, &home) {
        Ok(loaded) => loaded,
        Err(()) => {
            return fail(
                stderr,
                "load config: configuration is invalid or unavailable",
            );
        }
    };
    let mut environment = config_environment(&config_file, &lookup);
    environment.insert("HOME".to_string(), home.clone());

    let configured_snapshot = environment_snapshot(
        &host_entries,
        sandbox_provider_environment_names(&config_file, ""),
    );
    startup.config = config_file.clone();
    startup.environment = environment.clone();
    // Phase 5 seam: Go also folds in the four `auth.Credentials` values here.
    let (merged, merged_complete) = merge_redactions(
        &startup.sandbox_secrets,
        configured_snapshot.redaction_values(),
    );
    startup.sandbox_secrets = merged;
    startup.complete =
        startup.complete && configured_snapshot.redactions_complete() && merged_complete;

    if (!options.archive_path.is_empty()
        || !options.resume_path.is_empty()
        || options.continue_last)
        && !startup.allows_dynamic()
    {
        return fail(stderr, SESSION_OPERATION_UNAVAILABLE);
    }

    let mut approve_prompt = options.approve.clone();
    if options.approve_set
        && let Some(path) = approve_prompt.strip_prefix('@')
    {
        match std::fs::read(path) {
            Ok(data) if data.len() > MAX_APPROVE_PROMPT_BYTES => {
                return fail(
                    stderr,
                    &format!(
                        "read approve prompt: file is too large ({} bytes); maximum is {} bytes",
                        data.len(),
                        MAX_APPROVE_PROMPT_BYTES
                    ),
                );
            }
            Ok(data) => approve_prompt = String::from_utf8_lossy(&data).into_owned(),
            Err(error) => {
                return fail(
                    stderr,
                    &startup.redact(&format!("read approve prompt: {error}")),
                );
            }
        }
    }

    let workspace_path = match canonical_directory(Path::new(&options.cwd)) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(error) => return fail(stderr, &startup.redact(&format!("resolve cwd: {error}"))),
    };
    let workspace = match leaked_workspace(Path::new(&workspace_path)) {
        Ok(workspace) => workspace,
        Err(error) => {
            return fail(
                stderr,
                &startup.redact(&format!("create workspace: {error}")),
            );
        }
    };
    let session_root: PathBuf = [home.as_str(), ".otto", "sessions"].iter().collect();

    if !options.archive_path.is_empty() {
        return match session::archive(
            &session_root,
            &workspace_path,
            Path::new(&options.archive_path),
        ) {
            Ok(result) => {
                let _ = writeln!(
                    stdout,
                    "Archived: {}",
                    startup.redactor().redact_string(&result.path)
                );
                0
            }
            Err(_) => fail(stderr, "archive session: archive session failed"),
        };
    }

    let mut session_path = options.resume_path.clone();
    let mut listed_session_path = false;
    if options.continue_last {
        let listed = match session::list(&session_root, &workspace_path, "", 1) {
            Ok(listed) => listed,
            Err(error) => return fail(stderr, &startup.redact(&error.to_string())),
        };
        let Some(first) = listed.sessions.first() else {
            return fail(
                stderr,
                &startup.redact(&format!("no session found for workspace {workspace_path}")),
            );
        };
        session_path = first.path.clone();
        listed_session_path = true;
    }

    let ui_mode = match resolve_ui_mode(&config_file, &environment, &options.ui) {
        Ok(mode) => mode,
        Err(error) => return fail(stderr, &startup.redact(&error.to_string())),
    };
    let mut frontend = Frontend::Once;
    if !options.approve_set {
        frontend = match select_frontend(ui_mode, terminal) {
            Ok(frontend) => frontend,
            Err(message) => return fail(stderr, &startup.redact(&message)),
        };
    }

    let mut shell = lookup.get("SHELL").cloned().unwrap_or_default();
    if shell.is_empty() {
        shell = "/bin/sh".to_string();
    }
    if let Ok(canonical) = canonical_executable_file(Path::new(&shell)) {
        shell = canonical.to_string_lossy().into_owned();
    }

    let sandbox_driver_override = options.sandbox_set.then(|| options.sandbox.clone());
    let sandbox_settings = match resolve_sandbox_settings(
        &config_file,
        &environment,
        &workspace_path,
        sandbox_driver_override.as_deref(),
    ) {
        Ok(settings) => settings,
        Err(message) => return fail(stderr, &startup.redact(&message)),
    };
    // Resolved for its validation only: the memory service arrives in phase 6.
    if let Err(error) = resolve_memory(&config_file, &environment) {
        return fail(stderr, &error.to_string());
    }

    let mut builder = Builder {
        config_path: PathBuf::from(&config_path),
        config: config_file.clone(),
        environment: environment.clone(),
        workspace,
        workspace_path: workspace_path.clone(),
        session_root: session_root.clone(),
        shell: shell.clone(),
        no_session: options.no_session,
        overrides: overrides_from(&options),
        command_executor: None,
        sandbox_environment: None,
        sandbox_info: super::info::SandboxInfo::default(),
        sandbox_secrets: startup.sandbox_secrets.clone(),
        sandbox_secrets_complete: startup.complete,
    };

    let mut prepared_initial = None;
    let mut metadata = None;
    if !session_path.is_empty() {
        let prepared = if listed_session_path {
            session::Prepared::prepare_listed(
                &session_root,
                &workspace_path,
                Path::new(&session_path),
            )
            .map_err(|error| error.to_string())
        } else {
            prepare_session(Path::new(&session_path), &workspace_path)
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return fail(stderr, &builder.redact_error(&error, None));
            }
        };
        let info = prepared.info();
        metadata = Some(RuntimeMetadata {
            profile: info.profile.clone(),
            provider: info.provider.clone(),
            model: info.model.clone(),
        });
        prepared_initial = Some(prepared);
    }

    let resolved = match resolve_initial_runtime(
        &config_file,
        &environment,
        metadata.as_ref(),
        &builder.overrides,
    ) {
        Ok(runtime) => runtime,
        Err(error) => return fail(stderr, &builder.redact_error(&error.to_string(), None)),
    };

    let open_options = OpenOptions {
        settings: settings_from_config(&sandbox_settings),
        workspace: workspace_path.clone(),
        shell: shell.clone(),
        home: home.clone(),
        host_entries: host_entries.clone(),
        provider_names: sandbox_provider_environment_names(&config_file, &resolved.api_key_env),
    };
    let sandbox = normalize_sandbox_runtime(open_sandbox_runtime(&open_options, cancel).await);
    if cancel.is_cancelled() {
        let _ = sandbox.close();
        return 130;
    }
    if let Some(executor) = sandbox.executor.clone() {
        builder.command_executor = Some(executor);
    }
    builder.sandbox_environment = sandbox.environment.clone();
    builder.sandbox_info = sandbox.info;
    let (merged, merged_complete) =
        merge_redactions(&builder.sandbox_secrets, &sandbox.redaction_values);
    builder.sandbox_secrets = merged;
    builder.sandbox_secrets_complete =
        builder.sandbox_secrets_complete && sandbox.redactions_complete && merged_complete;
    if let Some(warning) = sandbox_runtime_warning(builder.effective_sandbox_info()) {
        let _ = stderr.write_all(warning.as_bytes());
    }
    if cancel.is_cancelled() {
        let _ = sandbox.close();
        return 130;
    }

    let dynamic_content = builder.boundary_allows_dynamic(Some(&resolved));
    if (prepared_initial.is_some() || options.serve) && !dynamic_content {
        let _ = sandbox.close();
        return fail(stderr, SESSION_OPERATION_UNAVAILABLE);
    }
    if cancel.is_cancelled() {
        let _ = sandbox.close();
        return 130;
    }

    if options.serve {
        let listen =
            match resolve_server(&config_file, &environment, &options.socket, &options.listen) {
                Ok(listen) => listen,
                Err(error) => {
                    let _ = sandbox.close();
                    return fail(stderr, &builder.redact_error(&error.to_string(), None));
                }
            };
        let api_key_env = resolved.api_key_env.clone();
        return serve::run(
            serve::ServeOptions {
                builder,
                runtime: resolved,
                listen,
                sandbox,
                reopen: open_options,
                config_path: PathBuf::from(&config_path),
                explicit_config: options.explicit_config,
                driver_override: sandbox_driver_override,
                environment,
                api_key_env,
            },
            stdout,
            stderr,
            cancel,
        )
        .await;
    }

    let (initial_session, warnings) =
        match activate_initial_session(&builder, prepared_initial, dynamic_content, &resolved) {
            Ok(activated) => activated,
            Err(message) => {
                let _ = sandbox.close();
                return fail(stderr, &message);
            }
        };
    for warning in &warnings {
        let _ = writeln!(stderr, "warning: {warning}");
    }
    if cancel.is_cancelled() {
        let _ = initial_session.close();
        let _ = sandbox.close();
        return 130;
    }

    let runner = match builder.build_runner(&initial_session, &resolved).await {
        Ok(runner) => runner,
        Err(message) => {
            let _ = initial_session.close();
            let _ = sandbox.close();
            if cancel.is_cancelled() {
                return 130;
            }
            return fail(stderr, &message);
        }
    };
    if let Err(message) = builder.update_session_runtime(&initial_session, &resolved) {
        runner.close();
        let _ = initial_session.close();
        let _ = sandbox.close();
        if cancel.is_cancelled() {
            return 130;
        }
        return fail(stderr, &message);
    }
    if cancel.is_cancelled() {
        runner.close();
        let _ = initial_session.close();
        let _ = sandbox.close();
        return 130;
    }

    // The builder moves into the controller, so the redactor the tail needs
    // is taken while it is still borrowable.
    let tail_redactor = boundary::secret_redactor(&builder.boundary_inputs(), Some(&resolved));
    let info = builder.runtime_info(&resolved);
    let controller = Controller::new(builder, dynamic_content, initial_session, runner, info);

    let run_error = {
        let mut console = Repl::new(&controller, Box::new(&mut *stdout), Box::new(&mut *stderr));
        match frontend {
            Frontend::Once => console.run_once(&approve_prompt, cancel).await,
            Frontend::Repl => console.run(stdin, cancel).await,
        }
    };

    let cancelled_before_exit = cancel.is_cancelled();
    let frontend_cancelled = matches!(run_error, Err(repl::Error::Cancelled));
    cancel.cancel();
    let controller_error = controller.close();
    let sandbox_error = sandbox.close();
    if let Err(message) = controller_error {
        return fail(
            stderr,
            &format!("close session: {}", redact_with(&tail_redactor, &message)),
        );
    }
    if sandbox_error.is_err() {
        return fail(stderr, "close sandbox: sandbox runtime close failed");
    }
    if cancelled_before_exit || frontend_cancelled {
        return 130;
    }
    let Err(error) = run_error else {
        return 0;
    };
    if frontend == Frontend::Once {
        // `run_once` already rendered the error to stderr.
        return 1;
    }
    if repl::is_command_error(&error, "/new") {
        return fail(stderr, &redact_with(&tail_redactor, &error.to_string()));
    }
    fail(
        stderr,
        &format!("REPL: {}", redact_with(&tail_redactor, &error.to_string())),
    )
}

/// Port of `prepareSession`: a directly named session file must belong to the
/// current workspace.
fn prepare_session(path: &Path, workspace: &str) -> Result<session::Prepared, String> {
    let prepared = session::Prepared::prepare(path).map_err(|error| error.to_string())?;
    match validate_session_workspace(&prepared.info().cwd, workspace) {
        Ok(()) => Ok(prepared),
        Err(message) => {
            let _ = prepared.close();
            Err(message)
        }
    }
}

/// Port of the `activatePrepared` / `newSession` / `session.NewMemory` fork.
fn activate_initial_session(
    builder: &Builder,
    prepared: Option<session::Prepared>,
    dynamic_content: bool,
    runtime: &Runtime,
) -> Result<(SharedSession, Vec<String>), String> {
    if let Some(prepared) = prepared {
        let (store, warnings) = prepared
            .activate()
            .map_err(|error| builder.redact_error(&error.to_string(), Some(runtime)))?;
        let messages = warnings
            .into_iter()
            .map(|warning| builder.redact_error(&warning.message, Some(runtime)))
            .collect();
        return Ok((SharedSession::new(Arc::new(store)), messages));
    }
    if !dynamic_content {
        let header = Header {
            version: CURRENT_VERSION,
            ..Header::default()
        };
        return Ok((SharedSession::memory(header), Vec::new()));
    }
    builder
        .create_session(runtime)
        .map(|session| (session, Vec::new()))
}

fn overrides_from(options: &CliOptions) -> Overrides {
    Overrides {
        profile: options.profile.clone(),
        provider: options.provider.clone(),
        base_url: options.base_url.clone(),
        model: options.model.clone(),
        thinking: options.thinking.clone(),
        shell_timeout: options.shell_timeout,
        max_output_bytes: options.max_output_bytes,
    }
}

/// The boundary that exists before a profile is resolved. Port of Go's
/// `startupBoundary` `runtimeBuilder` value.
struct StartupBoundary {
    sandbox_secrets: Vec<String>,
    complete: bool,
    config: File,
    environment: HashMap<String, String>,
    base_url: String,
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

    fn redactor(&self) -> otto_core::agent::redactor::Redactor {
        boundary::secret_redactor(&self.inputs(), None)
    }

    fn allows_dynamic(&self) -> bool {
        self.redactor().allows_dynamic_content()
    }

    fn redact(&self, message: &str) -> String {
        redact_with(&self.redactor(), message)
    }
}

/// A closed boundary renders nothing at all, matching Go's deliberately
/// empty `errRedactedRuntimeBoundary.Error()`.
fn redact_with(redactor: &otto_core::agent::redactor::Redactor, message: &str) -> String {
    if !redactor.allows_dynamic_content() {
        return String::new();
    }
    redactor.redact_string(message)
}

/// Port of `mergeSandboxRuntimeRedactions` for two groups.
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

/// Port of `captureEnvironment`: bounds the snapshot before anything reads it.
fn capture_environment(entries: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, String> {
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

/// Port of `newEnvironmentLookup`: parses `KEY=VALUE` entries, skipping every
/// malformed one and refusing an oversized set outright.
fn environment_lookup(entries: &[Vec<u8>]) -> Result<EnvironmentLookup, String> {
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

/// Port of `resolveHome`: `$HOME`, else the passwd entry, made absolute.
fn resolve_home(lookup: &EnvironmentLookup) -> Result<String, String> {
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

/// The current user's home directory from the passwd database. Go reads the
/// same source through `os/user`.
fn current_user_home() -> Option<String> {
    let user = nix::unistd::User::from_uid(nix::unistd::getuid()).ok()??;
    let home = user.dir.to_string_lossy().into_owned();
    (!home.is_empty()).then_some(home)
}

/// Port of `loadConfig`: a missing file at an implicit path is an empty one.
fn load_config(options: &CliOptions, home: &str) -> Result<(String, File), ()> {
    let path: PathBuf = if options.config_path.is_empty() {
        [home, ".config", "otto", "config.toml"].iter().collect()
    } else {
        PathBuf::from(&options.config_path)
    };
    match crate::config::load_required(&path) {
        Ok(file) => Ok((path.to_string_lossy().into_owned(), file)),
        Err(error) if error.is_not_found() && !options.explicit_config => {
            Ok((path.to_string_lossy().into_owned(), File::default()))
        }
        Err(_) => Err(()),
    }
}

/// Port of `configEnvironment`.
///
/// Unlike `crate::config::resolution_environment`, every fixed key is always
/// present, even when the process has no such variable: Go inserts the empty
/// string, and resolution distinguishes "absent" from "empty" nowhere.
fn config_environment(file: &File, lookup: &EnvironmentLookup) -> HashMap<String, String> {
    const FIXED_KEYS: [&str; 7] = [
        "HOME",
        "OTTO_PROVIDER",
        "OTTO_PROFILE",
        "OTTO_MODEL",
        "OTTO_API_KEY",
        "OTTO_UI",
        "OTTO_TRACE",
    ];
    let mut keys: BTreeSet<&str> = FIXED_KEYS.into_iter().collect();
    for profile in file.profiles.values() {
        if !profile.api_key_env.is_empty() {
            keys.insert(profile.api_key_env.as_str());
        }
    }
    keys.into_iter()
        .map(|key| {
            (
                key.to_string(),
                lookup.get(key).cloned().unwrap_or_default(),
            )
        })
        .collect()
}

/// Port of `sandboxProviderEnvironmentNames`: `OTTO_API_KEY`, the selected
/// key name and every configured one, deduplicated and sorted.
pub(super) fn sandbox_provider_environment_names(file: &File, selected: &str) -> Vec<String> {
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
    names.into_iter().collect()
}

/// Port of `resolveSandboxSettings`: existing skill and agent roots become
/// read paths so discovery can reach them from inside the sandbox.
pub(super) fn resolve_sandbox_settings(
    file: &File,
    environment: &HashMap<String, String>,
    workspace_path: &str,
    driver_override: Option<&str>,
) -> Result<SandboxSettings, String> {
    let mut raw = file.sandbox.clone();
    let agents =
        resolve_agents(file, environment, workspace_path).map_err(|error| error.to_string())?;
    let roots = resolve_skills(file, environment, workspace_path)
        .roots
        .into_iter()
        .chain(agents.roots);
    for root in roots {
        if std::fs::metadata(&root).is_ok_and(|info| info.is_dir()) {
            raw.read_paths.push(root);
        }
    }
    resolve_sandbox(&raw, driver_override).map_err(|error| error.to_string())
}

/// Port of `selectFrontend`. `auto` on a terminal selects the REPL rather
/// than the TUI for as long as the TUI is unported.
fn select_frontend(mode: UiMode, terminal: bool) -> Result<Frontend, String> {
    match mode {
        UiMode::Auto | UiMode::Repl => Ok(Frontend::Repl),
        UiMode::Tui if terminal => Err(TUI_NOT_PORTED.to_string()),
        UiMode::Tui => Err(
            "--ui tui requires terminal stdin and stdout; use --ui repl for redirected input"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Cursor;

    fn entries(values: &[&str]) -> Vec<Vec<u8>> {
        values
            .iter()
            .map(|value| value.as_bytes().to_vec())
            .collect()
    }

    #[test]
    fn the_lookup_keeps_only_well_formed_entries() {
        let lookup = environment_lookup(&entries(&[
            "HOME=/home/user",
            "no-equals",
            "1BAD=x",
            "OK_2=value",
            "EMPTY=",
        ]))
        .expect("lookup");
        assert_eq!(lookup.get("HOME").map(String::as_str), Some("/home/user"));
        assert_eq!(lookup.get("OK_2").map(String::as_str), Some("value"));
        assert_eq!(lookup.get("EMPTY").map(String::as_str), Some(""));
        assert!(!lookup.contains_key("1BAD"));
        assert!(!lookup.contains_key("no-equals"));
    }

    #[test]
    fn an_oversized_environment_is_refused() {
        let huge = vec![b'A'; MAX_CAPTURED_ENVIRONMENT_BYTES + 1];
        assert!(capture_environment(vec![huge]).is_err());
        assert!(
            capture_environment(vec![
                b"HOME=/x".to_vec();
                MAX_CAPTURED_ENVIRONMENT_ENTRIES + 1
            ])
            .is_err()
        );
    }

    #[test]
    fn the_config_environment_always_holds_every_fixed_key() {
        let mut file = otto_core::config::File::default();
        file.profiles.insert(
            "alpha".to_string(),
            otto_core::config::Profile {
                api_key_env: "ALPHA_KEY".to_string(),
                ..Default::default()
            },
        );
        let mut lookup = HashMap::new();
        lookup.insert("ALPHA_KEY".to_string(), "sk-alpha".to_string());
        let environment = config_environment(&file, &lookup);
        for key in [
            "HOME",
            "OTTO_PROVIDER",
            "OTTO_PROFILE",
            "OTTO_MODEL",
            "OTTO_API_KEY",
            "OTTO_UI",
            "OTTO_TRACE",
        ] {
            assert_eq!(environment.get(key).map(String::as_str), Some(""), "{key}");
        }
        assert_eq!(
            environment.get("ALPHA_KEY").map(String::as_str),
            Some("sk-alpha")
        );
    }

    #[test]
    fn provider_environment_names_are_sorted_and_deduplicated() {
        let mut file = otto_core::config::File::default();
        file.profiles.insert(
            "alpha".to_string(),
            otto_core::config::Profile {
                api_key_env: "ZED_KEY".to_string(),
                ..Default::default()
            },
        );
        file.profiles.insert(
            "beta".to_string(),
            otto_core::config::Profile {
                api_key_env: "OTTO_API_KEY".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(
            sandbox_provider_environment_names(&file, "BETA_KEY"),
            vec![
                "BETA_KEY".to_string(),
                "OTTO_API_KEY".to_string(),
                "ZED_KEY".to_string()
            ]
        );
    }

    #[test]
    fn home_comes_from_the_environment_first() {
        let mut lookup = HashMap::new();
        lookup.insert("HOME".to_string(), "/home/user".to_string());
        assert_eq!(resolve_home(&lookup).as_deref(), Ok("/home/user"));
        assert!(resolve_home(&HashMap::new()).is_ok());
    }

    #[test]
    fn the_frontend_follows_the_ui_mode_and_the_terminal() {
        use otto_core::config::UiMode;
        assert_eq!(select_frontend(UiMode::Auto, false), Ok(Frontend::Repl));
        assert_eq!(select_frontend(UiMode::Auto, true), Ok(Frontend::Repl));
        assert_eq!(select_frontend(UiMode::Repl, true), Ok(Frontend::Repl));
        assert_eq!(
            select_frontend(UiMode::Tui, false),
            Err(
                "--ui tui requires terminal stdin and stdout; use --ui repl for redirected input"
                    .to_string()
            )
        );
        assert_eq!(
            select_frontend(UiMode::Tui, true),
            Err(TUI_NOT_PORTED.to_string())
        );
    }

    #[test]
    fn sandbox_settings_pick_up_existing_skill_and_agent_roots() {
        let directory = tempfile::tempdir().expect("tempdir");
        let workspace = directory.path().to_string_lossy().to_string();
        std::fs::create_dir(directory.path().join(".otto")).expect("otto dir");
        std::fs::create_dir(directory.path().join(".otto/skills")).expect("skills dir");
        let file = otto_core::config::File::default();
        let settings =
            resolve_sandbox_settings(&file, &HashMap::new(), &workspace, None).expect("settings");
        let skills = directory
            .path()
            .join(".otto/skills")
            .to_string_lossy()
            .to_string();
        assert!(settings.read_paths.contains(&skills), "{settings:?}");
    }

    #[tokio::test]
    async fn unported_subcommands_exit_non_zero() {
        for command in ["login", "logout", "memory", "sandbox"] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let code = run(
                &[command.to_string()],
                Box::new(Cursor::new(Vec::new())),
                &mut stdout,
                &mut stderr,
                Vec::new(),
                false,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await;
            assert_eq!(code, 1, "{command}");
            assert_eq!(
                String::from_utf8_lossy(&stderr),
                format!("otto: {command} is not yet ported\n")
            );
        }
    }

    #[tokio::test]
    async fn an_unsafe_command_line_exits_two() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &["--cwd".to_string()],
            Box::new(Cursor::new(Vec::new())),
            &mut stdout,
            &mut stderr,
            Vec::new(),
            false,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert_eq!(code, 2);
        assert_eq!(
            String::from_utf8_lossy(&stderr),
            "otto: invalid command-line arguments\n"
        );
    }

    #[tokio::test]
    async fn help_writes_usage_and_exits_zero() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &["--help".to_string()],
            Box::new(Cursor::new(Vec::new())),
            &mut stdout,
            &mut stderr,
            Vec::new(),
            false,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert_eq!(code, 0);
        assert!(String::from_utf8_lossy(&stdout).starts_with("Usage: otto [options]"));
        assert!(stderr.is_empty());
    }
}
