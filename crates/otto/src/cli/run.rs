//! Process composition: the entry point `main` calls, from flag parsing to the
//! exit code.
//!
//! The tests pin the order of operations, the exact stderr text and the exit
//! codes.
//!
//! The composition root opens one process sandbox and hands it to a
//! [`SandboxSwitch`], so `/sandbox reload`, `POST /v1/sandbox/reload` and the
//! TUI all re-point bash at a new runtime without restarting the process.
//!
//! `sandbox`, `memory`, `login` and `logout` dispatch before flag parsing,
//! because their argument grammars are their own; nothing is left unported.
//!
//! The TUI (`--ui tui`, or `--ui auto` on a terminal) dispatches to
//! [`crate::tui::run`].
//!
//! Safety: every diagnostic that could carry a host path, an environment name
//! or a provider URL goes through the redaction boundary before it is written.
//! A boundary that cannot prove it collected every secret renders the empty
//! string.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use otto_core::config::resolve::{Overrides, Runtime};
use otto_core::config::{
    File, SandboxSettings, UiMode, resolve_agents, resolve_mcp, resolve_memory, resolve_sandbox,
    resolve_server, resolve_skills, resolve_ui_mode,
};
use otto_core::session::{CURRENT_VERSION, Header, RuntimeMetadata};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::sandbox::direct::DirectDriver;
use crate::sandbox::environment::{EnvironmentOptions, EnvironmentSnapshot, resolve_environment};
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy};
use crate::session;

use super::boundary::{self, BoundaryInputs};
use super::controller::{Controller, PROMPT_ACTIVE, SESSION_OPERATION_UNAVAILABLE};
use super::flags::{CliOptions, ParseFailure, Parsed, parse_flags};
use super::repl::{self, Repl};
use super::runtime_builder::{
    Builder, Shared, SharedSession, leaked_workspace, resolve_initial_runtime,
    validate_session_workspace,
};
use super::sandbox_runtime::{
    OpenOptions, canonical_directory, canonical_executable_file, normalize_sandbox_runtime,
    open_sandbox_runtime, sandbox_runtime_warning, settings_from_config,
};
use super::sandbox_switch::{SandboxReloader, SandboxSwitch};
use super::serve;

const MAX_PROMPT_BYTES: usize = 1 << 20;

/// Darwin's process argument/environment budget is about 1 MiB. These
/// ceilings are deliberately larger while still bounding injected snapshots.
const MAX_LOOKUP_ENVIRONMENT_NAME_BYTES: usize = 4 << 10;
const MAX_LOOKUP_ENVIRONMENT_ENTRY_BYTES: usize = 1 << 20;
const MAX_LOOKUP_ENVIRONMENT_ENTRIES: usize = 1 << 18;
const MAX_LOOKUP_ENVIRONMENT_BYTES: usize = 8 << 20;
const MAX_CAPTURED_ENVIRONMENT_ENTRIES: usize = 1 << 19;
const MAX_CAPTURED_ENVIRONMENT_BYTES: usize = 16 << 20;

const ENVIRONMENT_SNAPSHOT_TOO_LARGE: &str = "process environment snapshot is too large";

struct StartupTrace {
    enabled: bool,
    started: Instant,
    last: Instant,
    entries: Vec<(&'static str, std::time::Duration)>,
}

impl StartupTrace {
    fn from_lookup(lookup: &EnvironmentLookup, started: Instant) -> Self {
        let enabled = matches!(
            lookup.get("OTTO_STARTUP_TRACE").map(String::as_str),
            Some("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
        );
        Self {
            enabled,
            started,
            last: started,
            entries: Vec::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn mark(&mut self, label: &'static str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        self.entries.push((label, now.duration_since(self.last)));
        self.last = now;
    }

    fn extend(&mut self, entries: Vec<(&'static str, std::time::Duration)>) {
        if !self.enabled {
            return;
        }
        self.entries.extend(entries);
    }

    fn finish_ready(&mut self, stderr: &mut (dyn Write + Send)) {
        if !self.enabled {
            return;
        }
        for (label, elapsed) in &self.entries {
            let _ = writeln!(stderr, "startup {label}: {}ms", elapsed.as_millis());
        }
        let _ = writeln!(
            stderr,
            "startup ready: {}ms",
            Instant::now().duration_since(self.started).as_millis()
        );
        self.enabled = false;
    }
}

/// Which frontend the resolved UI mode selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    Repl,
    Once,
    Tui,
}

/// The process environment, parsed into names and values.
type EnvironmentLookup = HashMap<String, String>;

/// Writes `otto: {message}\n` and returns exit code 1.
pub(crate) fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}

/// Builds the automatic skill contract checker (experimental) when its three
/// enabling conditions all hold: `[skills].enabled` is not false,
/// `[experimental].typesafe_skill_check` is true, and `TYPESAFE_API_KEY` is
/// non-empty. A database open failure prints one warning here, before any UI
/// starts, and leaves the checker off for the process; `TYPESAFE_API_KEY`
/// never appears in that warning.
fn skill_checker(
    config: &File,
    environment: &HashMap<String, String>,
    home: &str,
    stderr: &mut (dyn Write + Send),
) -> Option<Arc<crate::skill::check::Checker>> {
    if config.skills.enabled == Some(false) {
        return None;
    }
    if config.experimental.typesafe_skill_check != Some(true) {
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
    match crate::skill::check::Checker::open(
        &db_path,
        crate::skill::check::TYPESAFE_BASE_URL,
        api_key,
    ) {
        Ok(checker) => Some(Arc::new(checker)),
        Err(message) => {
            let _ = writeln!(
                stderr,
                "warning: skill check store unavailable ({message}), continuing without automatic contract checks"
            );
            None
        }
    }
}

/// Runs one `otto` invocation and returns its exit code.
///
/// `environment_entries` is the raw process environment (`KEY=VALUE` byte
/// strings); `terminal` says whether stdin and stdout are both a terminal.
/// Both are parameters rather than reads of process state so the whole
/// startup path is reachable from tests.
pub async fn run(
    args: &[String],
    mut stdin: Box<dyn BufRead + Send + 'static>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    environment_entries: Vec<Vec<u8>>,
    terminal: bool,
    cancel: &CancellationToken,
) -> i32 {
    let startup_started = Instant::now();
    let mut startup_trace: StartupTrace;
    // These dispatch before flag parsing, because their argument grammars are
    // their own.
    if let Some(first) = args.first()
        && matches!(first.as_str(), "sandbox" | "memory")
    {
        let host_entries = match capture_environment(environment_entries) {
            Ok(entries) => entries,
            Err(message) => return fail(stderr, &message),
        };
        let lookup = match environment_lookup(&host_entries) {
            Ok(lookup) => lookup,
            Err(message) => return fail(stderr, &message),
        };
        if first == "memory" {
            return super::memory_command::run(&args[1..], stdout, stderr, &lookup);
        }
        return super::sandbox_setup::run(
            &args[1..],
            &mut stdin,
            stdout,
            stderr,
            &host_entries,
            &lookup,
            cancel,
        )
        .await;
    }
    if let Some(first) = args.first()
        && first == "mcp"
    {
        let host_entries = match capture_environment(environment_entries) {
            Ok(entries) => entries,
            Err(message) => return fail(stderr, &message),
        };
        let lookup = match environment_lookup(&host_entries) {
            Ok(lookup) => lookup,
            Err(message) => return fail(stderr, &message),
        };
        return super::mcp::run(&args[1..], stdout, stderr, &lookup, cancel).await;
    }
    if let Some(first) = args.first()
        && matches!(first.as_str(), "login" | "logout")
    {
        let host_entries = match capture_environment(environment_entries) {
            Ok(entries) => entries,
            Err(message) => return fail(stderr, &message),
        };
        let lookup = match environment_lookup(&host_entries) {
            Ok(lookup) => lookup,
            Err(message) => return fail(stderr, &message),
        };
        let home = match resolve_home(&lookup) {
            Ok(home) => home,
            Err(message) => return fail(stderr, &message),
        };
        return super::login::run_auth_command(args, stdout, stderr, &home, cancel).await;
    }

    let workflow_command = if args.first().is_some_and(|first| first == "workflow") {
        match super::workflow::parse(&args[1..]) {
            Ok(command) => Some(command),
            Err(message) => {
                let _ = writeln!(stderr, "{message}");
                return 2;
            }
        }
    } else {
        None
    };
    let flag_args = if workflow_command.is_some() {
        &[]
    } else {
        args
    };

    let options = match parse_flags(flag_args, stdout) {
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

    startup_trace = StartupTrace::from_lookup(&lookup, startup_started);
    startup_trace.mark("environment");

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
    startup_trace.mark("config/load");
    let mut environment = config_environment(&config_file, &lookup);
    environment.insert("HOME".to_string(), home.clone());

    let configured_snapshot = environment_snapshot(
        &host_entries,
        sandbox_provider_environment_names(&config_file, ""),
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
    let captured_auth =
        super::login::capture_auth_credentials(&crate::auth::path_for_home(Path::new(&home)));
    let (merged, merged_complete) =
        merge_redactions(&startup.sandbox_secrets, &captured_auth.redaction_values);
    startup.sandbox_secrets = merged;
    startup.complete = startup.complete && captured_auth.complete && merged_complete;

    if (!options.archive_path.is_empty()
        || !options.resume_path.is_empty()
        || options.continue_last)
        && !startup.allows_dynamic()
    {
        return fail(stderr, SESSION_OPERATION_UNAVAILABLE);
    }

    let mut prompt = options.prompt.clone();
    if options.prompt_set
        && let Some(path) = prompt.strip_prefix('@')
    {
        match std::fs::read(path) {
            Ok(data) if data.len() > MAX_PROMPT_BYTES => {
                return fail(
                    stderr,
                    &format!(
                        "read prompt: file is too large ({} bytes); maximum is {} bytes",
                        data.len(),
                        MAX_PROMPT_BYTES
                    ),
                );
            }
            Ok(data) => prompt = String::from_utf8_lossy(&data).into_owned(),
            Err(error) => {
                return fail(stderr, &startup.redact(&format!("read prompt: {error}")));
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
    if !options.prompt_set {
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
    let memory_config = match resolve_memory(&config_file, &environment) {
        Ok(config) => config,
        Err(error) => return fail(stderr, &error.to_string()),
    };
    let mcp_config = match resolve_mcp(&config_file, &environment, &workspace_path) {
        Ok(config) => config,
        Err(error) => return fail(stderr, &error.to_string()),
    };
    let usage = match crate::usage::Store::open(&Path::new(&home).join(".otto/usage.db")) {
        Ok(store) => Some(Arc::new(store)),
        Err(_) => {
            let _ = writeln!(
                stderr,
                "warning: usage store unavailable, continuing without usage history"
            );
            None
        }
    };
    let task_recorder =
        match crate::subagent::record::Store::open(&Path::new(&home).join(".otto/tasks.db")) {
            Ok(store) => Some(Arc::new(store)),
            Err(_) => {
                let _ = writeln!(
                    stderr,
                    "warning: task history store unavailable, continuing without task recording"
                );
                None
            }
        };
    let skill_checker = skill_checker(&config_file, &environment, &home, &mut *stderr);

    let shared = Arc::new(Shared {
        config_path: PathBuf::from(&config_path),
        config: config_file.clone(),
        environment: environment.clone(),
        home: home.clone(),
        session_root: session_root.clone(),
        shell: shell.clone(),
        no_session: options.no_session,
        overrides: overrides_from(&options),
        host_entries: host_entries.clone(),
        sandbox_secrets_baseline: startup.sandbox_secrets.clone(),
        sandbox_secrets_baseline_complete: startup.complete,
        sandbox_driver_override: sandbox_driver_override.clone(),
        explicit_config: options.explicit_config,
        auth_path: captured_auth.path.clone(),
        auth_credentials: captured_auth.credentials.clone(),
        auth_credentials_loaded: captured_auth.loaded,
        usage,
        task_recorder,
        skill_checker,
        memory: Default::default(),
    });
    let mut builder = Builder::for_workspace(shared, workspace, workspace_path.clone(), mcp_config);

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

    // `elevate` is REPL/TUI-specific here (an interactive frontend or a
    // server both need `/approve`); `load_workspace` always passes `true`,
    // because every workspace `otto serve` loads at runtime needs the same
    // support the startup workspace gets.
    let elevate = options.serve || frontend != Frontend::Once;
    let composed = compose_workspace_sandbox(
        &mut builder,
        &sandbox_settings,
        Path::new(&config_path),
        options.explicit_config,
        sandbox_driver_override.as_deref(),
        &resolved.api_key_env,
        elevate,
        cancel,
        stderr,
    )
    .await;
    startup_trace.mark("sandbox/open");
    // Composition's only await is the sandbox open above; nothing after it
    // can move `cancel` from unset to set, so one check here covers both the
    // switch and the (synchronous) elevation/reloader construction.
    if cancel.is_cancelled() {
        let _ = composed.control.close().await;
        return 130;
    }
    let control = composed.control;
    let approval_executor = composed.approval_executor;
    let reloader = composed.reloader;

    let dynamic_content = builder.boundary_allows_dynamic(Some(&resolved));
    if (prepared_initial.is_some() || options.serve || workflow_command.is_some())
        && !dynamic_content
    {
        let _ = control.close().await;
        return fail(stderr, SESSION_OPERATION_UNAVAILABLE);
    }
    if dynamic_content {
        let secrets = builder.secret_values(Some(&resolved));
        match super::wiring::open_memory_service(&memory_config, &secrets, stderr) {
            Ok((service, user_scope, usable)) => {
                let shared = builder.shared_mut();
                shared.memory.service = service;
                shared.memory.user_scope = user_scope;
                shared.memory.usable = usable;
            }
            Err(error) => {
                let _ = control.close().await;
                return fail(stderr, &builder.redact_error(&error, Some(&resolved)));
            }
        }
        match super::wiring::workspace_memory_scope(&memory_config, &workspace_path) {
            Ok(scope) => builder.workspace_scope = scope,
            Err(error) => {
                let _ = control.close().await;
                return fail(stderr, &error);
            }
        }
    }
    startup_trace.mark("memory/open");
    {
        let shared = builder.shared_mut();
        shared.memory.recall_limit = memory_config.max_results;
        shared.memory.recall_token_budget = memory_config.recall_tokens;
    }
    let memory_service = Arc::clone(&builder.memory.service);
    if cancel.is_cancelled() {
        let _ = control.close().await;
        return 130;
    }

    if let Some(command) = workflow_command {
        let builder = Arc::new(builder);
        let controller =
            super::workflow::build_controller(Arc::clone(&builder), &resolved, stderr).await;
        let result = match controller {
            Ok(controller) => {
                let result = super::workflow::run_command(command, &controller, stdout).await;
                controller.close().await;
                result
            }
            Err(error) => Err(error),
        };
        let sandbox_error = control.close().await;
        let approval_error = approval_executor
            .as_ref()
            .map(|executor| executor.close())
            .transpose()
            .err();
        let _ = memory_service.close();
        if sandbox_error.is_err() || approval_error.is_some() {
            return fail(stderr, "close sandbox: sandbox runtime close failed");
        }
        if cancel.is_cancelled() {
            return 130;
        }
        return match result {
            Ok(()) => 0,
            Err(error) => fail(stderr, &builder.redact_error(&error, Some(&resolved))),
        };
    }

    if options.serve {
        let listen =
            match resolve_server(&config_file, &environment, &options.socket, &options.listen) {
                Ok(listen) => listen,
                Err(error) => {
                    let _ = control.close().await;
                    return fail(stderr, &builder.redact_error(&error.to_string(), None));
                }
            };
        let exit = serve::run(
            serve::ServeOptions {
                builder,
                runtime: resolved,
                listen,
                control,
                reloader,
                open: options.open,
            },
            stdout,
            stderr,
            cancel,
        )
        .await;
        if approval_executor
            .as_ref()
            .is_some_and(|executor| executor.close().is_err())
        {
            return fail(stderr, "close sandbox: sandbox runtime close failed");
        }
        return exit;
    }

    let (initial_session, warnings) =
        match activate_initial_session(&builder, prepared_initial, dynamic_content, &resolved) {
            Ok(activated) => activated,
            Err(message) => {
                let _ = control.close().await;
                return fail(stderr, &message);
            }
        };
    startup_trace.mark("session/activate");
    for warning in &warnings {
        let _ = writeln!(stderr, "warning: {warning}");
    }
    if cancel.is_cancelled() {
        let _ = initial_session.close();
        let _ = control.close().await;
        return 130;
    }

    let background_mcp = frontend != Frontend::Once
        && dynamic_content
        && builder.mcp.enabled
        && builder.mcp.servers.iter().any(|server| server.enabled);
    let trace_runner_build = startup_trace.is_enabled();
    let runner_result = if background_mcp {
        builder
            .build_runner_without_mcp_with_trace(&initial_session, &resolved, trace_runner_build)
            .await
    } else {
        builder
            .build_runner_with_trace(&initial_session, &resolved, trace_runner_build)
            .await
    };
    let (runner, runner_trace) = match runner_result {
        Ok(runner) => runner,
        Err(message) => {
            let _ = initial_session.close();
            let _ = control.close().await;
            if cancel.is_cancelled() {
                return 130;
            }
            return fail(stderr, &message);
        }
    };
    startup_trace.extend(runner_trace);
    startup_trace.mark("runner/build");
    if let Err(message) = builder.update_session_runtime(&initial_session, &resolved) {
        runner.close_mcp().await;
        runner.close();
        let _ = initial_session.close();
        let _ = control.close().await;
        if cancel.is_cancelled() {
            return 130;
        }
        return fail(stderr, &message);
    }
    if cancel.is_cancelled() {
        runner.close_mcp().await;
        runner.close();
        let _ = initial_session.close();
        let _ = control.close().await;
        return 130;
    }

    // The builder moves into the controller, so the redactor the tail needs
    // is taken while it is still borrowable.
    let tail_redactor = boundary::secret_redactor(&builder.boundary_inputs(), Some(&resolved));
    let info = builder.runtime_info(&resolved);
    let controller = Controller::new(builder, dynamic_content, initial_session, runner, info);
    // One process sandbox serves every controller, so each one reports the live
    // state rather than the value captured when it was built.
    let controller = match &reloader {
        Some(reloader) => controller
            .with_sandbox_control(Arc::clone(reloader) as Arc<dyn crate::app::SandboxControl>),
        None => controller,
    };
    let controller = Arc::new(controller);
    let mcp_swap = background_mcp.then(|| {
        spawn_mcp_runner_swap(
            Arc::clone(&controller),
            resolved.clone(),
            cancel.child_token(),
        )
    });

    startup_trace.finish_ready(stderr);

    let run_error = match frontend {
        Frontend::Once => {
            let mut console =
                Repl::new(&controller, Box::new(&mut *stdout), Box::new(&mut *stderr));
            console.run_once(&prompt, cancel).await
        }
        Frontend::Repl => {
            let mut console =
                Repl::new(&controller, Box::new(&mut *stdout), Box::new(&mut *stderr));
            console.run(stdin, cancel).await
        }
        Frontend::Tui => crate::tui::run(&controller, cancel).await,
    };

    let cancelled_before_exit = cancel.is_cancelled();
    let frontend_cancelled = matches!(run_error, Err(repl::Error::Cancelled));
    cancel.cancel();
    if let Some(task) = mcp_swap {
        task.abort();
    }
    controller.close_mcp().await;
    let controller_error = controller.close();
    let sandbox_error = control.close().await;
    let approval_error = approval_executor
        .as_ref()
        .map(|executor| executor.close())
        .transpose()
        .err();
    let _ = memory_service.close();
    if let Err(message) = controller_error {
        return fail(
            stderr,
            &format!("close session: {}", redact_with(&tail_redactor, &message)),
        );
    }
    if sandbox_error.is_err() || approval_error.is_some() {
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

fn spawn_mcp_runner_swap(
    controller: Arc<Controller>,
    runtime: Runtime,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(session) = controller.current_session_opt() else {
            return;
        };
        let session_id = session.header().id;
        let mut runner = match controller.builder().build_runner(&session, &runtime).await {
            Ok(runner) => runner,
            Err(_) => return,
        };
        let info = controller.builder().runtime_info(&runtime);
        loop {
            if cancel.is_cancelled() {
                runner.close_mcp().await;
                runner.close();
                return;
            }
            match controller.replace_runner_if_current(&session_id, runner, info.clone()) {
                Ok(true) | Ok(false) => return,
                Err(message) if message == PROMPT_ACTIVE => {
                    // Rebuild instead of holding a runner that may have stale
                    // context after a long turn. This keeps the swap simple
                    // and only installs a runner built for the post-turn state.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let Some(session) = controller.current_session_opt() else {
                        return;
                    };
                    if session.header().id != session_id {
                        return;
                    }
                    runner = match controller.builder().build_runner(&session, &runtime).await {
                        Ok(next) => next,
                        Err(_) => return,
                    };
                }
                Err(_) => return,
            }
        }
    })
}

/// A directly named session file must belong to the current workspace.
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

/// The boundary that exists before a profile is resolved.
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

/// A closed boundary renders nothing at all, deliberately: an incomplete
/// redaction set must not let a diagnostic escape.
fn redact_with(redactor: &otto_core::agent::redactor::Redactor, message: &str) -> String {
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

/// Parses `KEY=VALUE` entries, skipping every malformed one and refusing an
/// oversized set outright.
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
    load_config(
        &CliOptions {
            config_path: config_path.to_string(),
            explicit_config: explicit,
            ..CliOptions::default()
        },
        home,
    )
}

pub(crate) fn config_environment_for(
    file: &File,
    lookup: &EnvironmentLookup,
) -> HashMap<String, String> {
    config_environment(file, lookup)
}

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

/// The current user's home directory from the passwd database.
fn current_user_home() -> Option<String> {
    let user = nix::unistd::User::from_uid(nix::unistd::getuid()).ok()??;
    let home = user.dir.to_string_lossy().into_owned();
    (!home.is_empty()).then_some(home)
}

/// A missing file at an implicit path is an empty one.
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

/// Unlike `crate::config::resolution_environment`, every fixed key is always
/// present, even when the process has no such variable: the empty string is
/// inserted, and resolution distinguishes "absent" from "empty" nowhere.
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

/// `OTTO_API_KEY`, the selected key name and every configured one, deduplicated
/// and sorted.
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

/// Existing skill and agent roots become read paths so discovery can reach them
/// from inside the sandbox. A root is only promoted when it is a real
/// directory: `read_paths` are canonicalized into `(subpath <target>)` grants,
/// so promoting a symlink would widen the sandbox to whatever the link points
/// at, and the default roots include the workspace-relative `.otto/skills`,
/// which anything that writes to the workspace controls.
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
        if std::fs::symlink_metadata(&root).is_ok_and(|info| info.is_dir()) {
            raw.read_paths.push(root);
        }
    }
    resolve_sandbox(&raw, driver_override).map_err(|error| error.to_string())
}

/// The sandbox artifacts one workspace's composition produces: the switch
/// wired into `builder.command_executor`, the elevated bash-approval
/// executor (`None` when `elevate` was false, the sandbox is not Seatbelt, or
/// elevation failed), and the reloader `/sandbox reload` uses to reopen this
/// workspace's own sandbox (`None` without a usable sandbox).
pub(super) struct WorkspaceSandbox {
    pub(super) control: Arc<SandboxSwitch>,
    pub(super) approval_executor: Option<Arc<Executor>>,
    pub(super) reloader: Option<Arc<SandboxReloader>>,
}

/// Opens one workspace's sandbox and, when eligible, its elevated
/// bash-approval executor, filling in `builder`'s sandbox-derived fields
/// (`command_executor`, `sandbox_environment`, `sandbox_info`,
/// `sandbox_secrets*`, `bash_approvals`) along the way.
///
/// Shared by `run`'s startup composition and `load_workspace`, so a workspace
/// admitted after startup gets the same `--sandbox` override, `/approve`
/// support, and `/sandbox reload` wiring as the startup workspace — see
/// `docs/specs/2026-09-26-serve-multiple-workspaces.md` ("Server structure").
/// `elevate` gates the approval executor; `sandbox_settings`, `config_path`,
/// `explicit_config`, `driver_override`, and `api_key_env` are inputs the two
/// callers resolve differently (startup from CLI flags and a resumed
/// session's metadata, `load_workspace` from `Shared` alone), so they stay
/// parameters rather than being resolved inside this function.
#[allow(clippy::too_many_arguments)]
pub(super) async fn compose_workspace_sandbox(
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
    if elevate && sandbox_info.mode == super::info::SandboxMode::Seatbelt {
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
            builder.bash_approvals = Some(Arc::new(crate::tool::bash::BashApprovals::new(
                command_executor,
                entries.to_vec(),
            )));
            let (merged, complete) =
                merge_redactions(&builder.sandbox_secrets, snapshot.redaction_values());
            builder.sandbox_secrets = merged;
            builder.sandbox_secrets_complete &= complete;
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
        })
    });

    WorkspaceSandbox {
        control,
        approval_executor,
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
pub(super) async fn load_workspace(
    shared: Arc<Shared>,
    canonical_path: &Path,
    cancel: &CancellationToken,
    stderr: &mut (dyn Write + Send),
) -> Result<(Builder, Option<Arc<SandboxReloader>>), String> {
    let workspace_path = canonical_path.to_string_lossy().into_owned();
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
        cancel,
        stderr,
    )
    .await;

    let memory_config =
        resolve_memory(&shared.config, &shared.environment).map_err(|error| error.to_string())?;
    builder.workspace_scope =
        super::wiring::workspace_memory_scope(&memory_config, &workspace_path)?;

    Ok((builder, composed.reloader))
}

fn select_frontend(mode: UiMode, terminal: bool) -> Result<Frontend, String> {
    match mode {
        UiMode::Auto if terminal => Ok(Frontend::Tui),
        UiMode::Auto => Ok(Frontend::Repl),
        UiMode::Repl => Ok(Frontend::Repl),
        UiMode::Tui if terminal => Ok(Frontend::Tui),
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

    /// A config with `[experimental].typesafe_skill_check = true` and
    /// `[skills].enabled` left at its default (enabled).
    fn skill_check_enabled_config() -> File {
        let mut file = File::default();
        file.experimental.typesafe_skill_check = Some(true);
        file
    }

    #[test]
    fn skill_checker_is_none_when_skills_are_disabled() {
        let mut file = skill_check_enabled_config();
        file.skills.enabled = Some(false);
        let environment = HashMap::from([("TYPESAFE_API_KEY".to_string(), "key".to_string())]);
        let mut stderr = Cursor::new(Vec::new());

        assert!(skill_checker(&file, &environment, "/nonexistent", &mut stderr).is_none());
        assert!(stderr.get_ref().is_empty());
    }

    #[test]
    fn skill_checker_is_none_without_the_experimental_flag() {
        let file = File::default();
        let environment = HashMap::from([("TYPESAFE_API_KEY".to_string(), "key".to_string())]);
        let mut stderr = Cursor::new(Vec::new());

        assert!(skill_checker(&file, &environment, "/nonexistent", &mut stderr).is_none());
        assert!(stderr.get_ref().is_empty());
    }

    #[test]
    fn skill_checker_is_none_without_an_api_key() {
        let file = skill_check_enabled_config();
        let environment = HashMap::new();
        let mut stderr = Cursor::new(Vec::new());

        assert!(skill_checker(&file, &environment, "/nonexistent", &mut stderr).is_none());
        assert!(stderr.get_ref().is_empty());
    }

    #[test]
    fn skill_checker_opens_a_database_when_all_three_conditions_hold() {
        let home = tempfile::tempdir().expect("home");
        let file = skill_check_enabled_config();
        let environment = HashMap::from([("TYPESAFE_API_KEY".to_string(), "key".to_string())]);
        let mut stderr = Cursor::new(Vec::new());

        let checker = skill_checker(
            &file,
            &environment,
            &home.path().to_string_lossy(),
            &mut stderr,
        );

        assert!(checker.is_some());
        assert!(stderr.get_ref().is_empty());
        assert!(home.path().join(".otto/skill-checks.db").exists());
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
        assert_eq!(select_frontend(UiMode::Auto, true), Ok(Frontend::Tui));
        assert_eq!(select_frontend(UiMode::Repl, true), Ok(Frontend::Repl));
        assert_eq!(select_frontend(UiMode::Repl, false), Ok(Frontend::Repl));
        assert_eq!(select_frontend(UiMode::Tui, true), Ok(Frontend::Tui));
        assert_eq!(
            select_frontend(UiMode::Tui, false),
            Err(
                "--ui tui requires terminal stdin and stdout; use --ui repl for redirected input"
                    .to_string()
            )
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

    /// A workspace lives inside the sandbox's writable subtree, so anything
    /// that clones or writes there controls `.otto/skills`. A symlinked root
    /// would be canonicalized into an `(allow file-read* (subpath <target>))`
    /// grant covering the link's target, which is how a cloned repository
    /// could read `~/.ssh`. Existence is therefore checked without following
    /// the link.
    #[test]
    fn a_symlinked_skill_root_is_not_promoted_to_a_sandbox_read_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        let workspace = directory.path().to_string_lossy().to_string();
        let secrets = directory.path().join("secrets");
        std::fs::create_dir(&secrets).expect("secrets dir");
        std::fs::create_dir(directory.path().join(".otto")).expect("otto dir");
        std::os::unix::fs::symlink(&secrets, directory.path().join(".otto/skills"))
            .expect("symlink");

        let file = otto_core::config::File::default();
        let settings =
            resolve_sandbox_settings(&file, &HashMap::new(), &workspace, None).expect("settings");

        let skills = directory
            .path()
            .join(".otto/skills")
            .to_string_lossy()
            .to_string();
        assert!(
            !settings.read_paths.contains(&skills),
            "a symlinked skill root reached the sandbox read paths: {settings:?}"
        );
    }

    #[tokio::test]
    async fn startup_trace_environment_writes_timing_lines() {
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

[mcp]
enabled = false

[skills]
paths = []

[agents]
paths = []

[sandbox]
driver = "off"
"#,
        )
        .expect("write config");

        let args = vec![
            "--cwd".to_string(),
            workspace.to_string_lossy().into_owned(),
            "--no-session".to_string(),
        ];
        let environment = entries(&[
            &format!("HOME={}", home.to_string_lossy()),
            "ALPHA_KEY=sk-alpha",
            "OTTO_STARTUP_TRACE=1",
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &args,
            Box::new(Cursor::new(Vec::new())),
            &mut stdout,
            &mut stderr,
            environment,
            false,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&stderr));
        let stderr = String::from_utf8_lossy(&stderr);
        assert!(stderr.contains("startup ready:"), "{stderr}");
        assert!(stderr.contains("startup sandbox/open:"), "{stderr}");
        assert!(stderr.contains("startup runner/build:"), "{stderr}");
        assert!(stderr.contains("startup runner/mcp:"), "{stderr}");
        assert!(
            stderr.contains("startup runner/workspace-context:"),
            "{stderr}"
        );
        assert!(!stderr.contains("startup total:"), "{stderr}");
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

    // ---- load_workspace ----

    use crate::cli::testutil::{shared_with_offline_sandbox, shared_with_sandbox_driver_override};

    /// Loading a second workspace must not open a second memory service: the
    /// service lives on `Shared`, and `load_workspace` only computes the new
    /// workspace's own scope.
    #[tokio::test]
    async fn load_workspace_reuses_the_shared_memory_service() {
        let shared_root = tempfile::tempdir().expect("shared root");
        let workspace = tempfile::tempdir().expect("workspace");
        let canonical = canonical_directory(workspace.path()).expect("canonical");
        let shared = shared_with_offline_sandbox(shared_root.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut stderr = Vec::new();

        let (builder, _reloader) =
            load_workspace(Arc::clone(&shared), &canonical, &cancel, &mut stderr)
                .await
                .expect("load workspace");

        assert!(Arc::ptr_eq(&builder.memory.service, &shared.memory.service));
        assert_eq!(builder.workspace_path, canonical.to_string_lossy());
    }

    /// The new workspace opens its own sandbox with the shared `host_entries`
    /// and shell, the same as startup does for its own workspace.
    #[tokio::test]
    async fn load_workspace_opens_its_own_sandbox() {
        let shared_root = tempfile::tempdir().expect("shared root");
        let workspace = tempfile::tempdir().expect("workspace");
        let canonical = canonical_directory(workspace.path()).expect("canonical");
        let shared = shared_with_offline_sandbox(shared_root.path());
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut stderr = Vec::new();

        let (builder, _reloader) =
            load_workspace(Arc::clone(&shared), &canonical, &cancel, &mut stderr)
                .await
                .expect("load workspace");

        assert_eq!(
            builder.sandbox_info.mode,
            super::super::info::SandboxMode::Off
        );
        assert!(builder.sandbox_info.bash_available);
        assert!(builder.command_executor.is_some());
    }

    /// `otto serve --sandbox off` must apply to a workspace loaded after
    /// startup too: both read the same `Shared::sandbox_driver_override`
    /// rather than `load_workspace` dropping it.
    #[tokio::test]
    async fn load_workspace_honors_the_shared_sandbox_driver_override() {
        let shared_root = tempfile::tempdir().expect("shared root");
        let workspace = tempfile::tempdir().expect("workspace");
        let canonical = canonical_directory(workspace.path()).expect("canonical");
        let shared = shared_with_sandbox_driver_override(shared_root.path(), "off");
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut stderr = Vec::new();

        let (builder, _reloader) =
            load_workspace(Arc::clone(&shared), &canonical, &cancel, &mut stderr)
                .await
                .expect("load workspace");

        assert_eq!(
            builder.sandbox_info.mode,
            super::super::info::SandboxMode::Off
        );
    }

    /// On a host where Seatbelt actually comes up, a workspace loaded at
    /// runtime gets the same elevated `/approve` executor startup builds for
    /// its own workspace. Skips (rather than failing) when this host cannot
    /// open Seatbelt, the same accommodation `open_sandbox_runtime` itself
    /// makes for a host missing the entitlement or Command Line Tools
    /// (`docs/development.md`).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn load_workspace_seatbelt_gets_bash_approvals() {
        let shared_root = tempfile::tempdir().expect("shared root");
        let workspace = tempfile::tempdir().expect("workspace");
        let canonical = canonical_directory(workspace.path()).expect("canonical");
        let shared = shared_with_sandbox_driver_override(shared_root.path(), "seatbelt");
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut stderr = Vec::new();

        let (builder, reloader) =
            load_workspace(Arc::clone(&shared), &canonical, &cancel, &mut stderr)
                .await
                .expect("load workspace");

        if builder.sandbox_info.mode != super::super::info::SandboxMode::Seatbelt {
            eprintln!(
                "skipping: Seatbelt did not come up on this host: {}",
                String::from_utf8_lossy(&stderr)
            );
            return;
        }
        assert!(builder.bash_approvals.is_some());
        assert!(reloader.is_some());
    }
}
