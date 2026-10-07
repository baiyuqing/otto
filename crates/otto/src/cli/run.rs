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
//! `sandbox`, `memory`, `login`, `logout`, `setup` and `trust` dispatch before flag
//! parsing, because their argument grammars are their own; nothing is left
//! unported.
//!
//! The TUI (`--ui tui`, or `--ui auto` on a terminal) dispatches to
//! [`crate::tui::run`].
//!
//! Safety: every diagnostic that could carry a host path, an environment name
//! or a provider URL goes through the redaction boundary before it is written.
//! A boundary that cannot prove it collected every secret renders the empty
//! string.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use otto_core::config::UiMode;
use otto_core::config::resolve::{Overrides, Runtime};
use otto_core::config::{resolve_server, resolve_ui_mode};
use otto_core::session::{CURRENT_VERSION, Header, RuntimeMetadata};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::session;

use super::controller::{Controller, PROMPT_ACTIVE, SESSION_OPERATION_UNAVAILABLE};
use super::flags::{CliOptions, ParseFailure, Parsed, parse_flags};
use super::repl::{self, Repl};
use super::serve;
use crate::runtime::boundary;
use crate::runtime::builder::{Builder, SharedSession};
use crate::runtime::startup::{
    self, Disabled, EnvironmentLookup, capture_environment, environment_lookup, redact_with,
    resolve_home,
};
// Helpers other `cli` modules and `plugin` call through this path.
pub(crate) use crate::runtime::startup::{
    config_environment_for, load_config_for, resolve_home_for, sandbox_provider_environment_names,
};

const MAX_PROMPT_BYTES: usize = 1 << 20;

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

/// Writes `otto: {message}\n` and returns exit code 1.
pub(crate) fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}

/// Runs `on_exit` reflection before the controller closes, for at most
/// [`crate::app::auto_reflection::EXIT_TIMEOUT`]. Ctrl+C skips it. The wait is the cost of
/// the feature: a process that is exiting cannot finish it in the background.
/// Returns the line to show, or `Err` when it timed out. It takes no writers, so the
/// future stays `Send`.
async fn reflect_on_exit(
    controller: &Controller,
    cancel: &CancellationToken,
) -> Result<Option<String>, ()> {
    let token = cancel.child_token();
    let outcome = tokio::time::timeout(
        crate::app::auto_reflection::EXIT_TIMEOUT,
        controller.reflect_on_exit(&token),
    )
    .await;
    token.cancel();
    outcome.map_err(|_| ())
}

/// Runs one `otto` invocation and returns its exit code.
///
/// `environment_entries` is the raw process environment (`KEY=VALUE` byte
/// strings); `terminal` says whether stdin and stdout are both a terminal.
/// Both are parameters rather than reads of process state so the whole
/// startup path is reachable from tests.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    args: &[String],
    stdin: Box<dyn BufRead + Send + 'static>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    environment_entries: Vec<Vec<u8>>,
    terminal: bool,
    cancel: &CancellationToken,
    terminate: &super::terminate::Terminate,
) -> i32 {
    run_with_backends(
        args,
        stdin,
        stdout,
        stderr,
        environment_entries,
        terminal,
        cancel,
        terminate,
        &crate::runtime::storage::BackendRegistry::default(),
    )
    .await
}

/// Process entry point for statically linked storage extensions.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_backends(
    args: &[String],
    mut stdin: Box<dyn BufRead + Send + 'static>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    environment_entries: Vec<Vec<u8>>,
    terminal: bool,
    cancel: &CancellationToken,
    terminate: &super::terminate::Terminate,
    backends: &crate::runtime::storage::BackendRegistry,
) -> i32 {
    if args
        .first()
        .is_some_and(|a| matches!(a.as_str(), "config" | "plugin"))
    {
        let entries = match capture_environment(environment_entries) {
            Ok(v) => v,
            Err(e) => return fail(stderr, &e),
        };
        let lookup = match environment_lookup(&entries) {
            Ok(v) => v,
            Err(e) => return fail(stderr, &e),
        };
        if args[0] == "plugin" {
            return super::plugin::run(&args[1..], stdout, stderr, &lookup);
        }
        return super::config_command::run(&args[1..], stdout, stderr, &lookup);
    }
    if args.first().is_some_and(|command| command == "workspace") {
        return super::workspace_command::run(&args[1..], stdout, stderr);
    }
    if args.first().is_some_and(|command| command == "storage") {
        if backends.is_empty()
            && args.get(1).is_none_or(|command| command != "init")
            && !args.iter().any(|argument| argument == "--to-config")
        {
            // Preserve legacy storage commands' environment-independent dispatch.
            return super::storage::run_async(&args[1..], stdout, stderr).await;
        }
        let entries = match capture_environment(environment_entries) {
            Ok(entries) => entries,
            Err(message) => return fail(stderr, &message),
        };
        let lookup = match environment_lookup(&entries) {
            Ok(lookup) => lookup,
            Err(message) => return fail(stderr, &message),
        };
        return super::storage::run_async_with_backends(
            &args[1..],
            stdout,
            stderr,
            backends,
            &lookup,
        )
        .await;
    }
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
            let home = match resolve_home(&lookup) {
                Ok(home) => home,
                Err(message) => return fail(stderr, &message),
            };
            let config = match super::memory_command::startup_configuration(&args[1..], &home) {
                Ok(config) => config,
                Err(()) => {
                    return super::memory_command::run_with_storage(
                        &args[1..],
                        stdout,
                        stderr,
                        &lookup,
                        &Default::default(),
                    );
                }
            };
            let storage = match crate::runtime::storage::Storage::open_home_with(
                Path::new(&home),
                backends,
                &config.storage,
                &lookup,
            )
            .await
            {
                Ok(storage) => storage,
                Err(message) => return fail(stderr, &message),
            };
            return super::memory_command::run_with_storage(
                &args[1..],
                stdout,
                stderr,
                &lookup,
                &storage,
            );
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
    if let Some(first) = args.first()
        && first == "setup"
    {
        let host_entries = match capture_environment(environment_entries) {
            Ok(entries) => entries,
            Err(message) => return fail(stderr, &message),
        };
        let lookup = match environment_lookup(&host_entries) {
            Ok(lookup) => lookup,
            Err(message) => return fail(stderr, &message),
        };
        return super::setup::run(&args[1..], &mut stdin, stdout, stderr, &lookup);
    }
    if let Some(first) = args.first()
        && first == "trust"
    {
        let host_entries = match capture_environment(environment_entries) {
            Ok(entries) => entries,
            Err(message) => return fail(stderr, &message),
        };
        let lookup = match environment_lookup(&host_entries) {
            Ok(lookup) => lookup,
            Err(message) => return fail(stderr, &message),
        };
        return super::trust::run(&args[1..], stdout, stderr, &lookup);
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
        Ok(Parsed::Help | Parsed::Version) => return 0,
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

    let mut loaded = match startup::load(
        &host_entries,
        &lookup,
        &options.config_path,
        options.explicit_config,
        &options.cwd,
        &options.base_url,
    ) {
        Ok(loaded) => loaded,
        Err(message) => return fail(stderr, &message),
    };
    startup_trace.mark("config/load");
    let storage = match startup::open_storage(&loaded, &lookup, backends).await {
        Ok(storage) => storage,
        Err(message) => return fail(stderr, &message),
    };
    if !options.acp_listen.is_empty()
        && loaded
            .config
            .policy
            .as_ref()
            .and_then(|p| p.force.get("server"))
            .and_then(toml::Value::as_table)
            .is_some_and(|p| p.contains_key("socket") || p.contains_key("listen"))
    {
        return fail(
            stderr,
            "ACP listener override is prohibited by managed server policy",
        );
    }
    let configured = startup::configure(&mut loaded, &host_entries, &lookup, &mut *stderr);

    if (!options.archive_path.is_empty()
        || !options.resume_path.is_empty()
        || options.continue_last)
        && !loaded.boundary.allows_dynamic()
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
                return fail(
                    stderr,
                    &loaded.boundary.redact(&format!("read prompt: {error}")),
                );
            }
        }
    }

    let workspace_path = match startup::canonical_workspace(&options.cwd, &loaded.boundary) {
        Ok(path) => path,
        Err(message) => return fail(stderr, &message),
    };
    if options.attach {
        // Neither client builds a provider runtime or opens a sandbox or
        // session: both use the `otto serve` on the socket, which is
        // `--socket`, `[server].socket` or the default, never a TCP address.
        let mut file = loaded.config.clone();
        file.server.listen.clear();
        let socket = resolve_server(&file, &configured.environment, &options.socket, "")
            .map_err(|error| error.to_string())
            .and_then(|server| {
                std::path::absolute(&server.socket).map_err(|error| error.to_string())
            });
        let socket = match socket {
            Ok(socket) => socket,
            Err(message) => return fail(stderr, &loaded.boundary.redact(&message)),
        };
        let schedule = startup::open_schedule(&storage, &loaded.home, &mut *stderr);
        if options.acp {
            return crate::acp::attach::run(
                &socket,
                PathBuf::from(&workspace_path),
                schedule,
                Arc::new(crate::log::Logger::stderr()),
                stdin,
                stdout,
                stderr,
                cancel,
            )
            .await;
        }
        if !terminal {
            return fail(stderr, "--attach requires terminal stdin and stdout");
        }
        let start = if options.continue_last {
            crate::tui::attach::Start::Continue
        } else if !options.resume_path.is_empty() {
            crate::tui::attach::Start::Resume(options.resume_path.clone())
        } else {
            crate::tui::attach::Start::New
        };
        return crate::tui::attach::run(&socket, &workspace_path, start, schedule, cancel, stderr)
            .await;
    }
    let prepared = match startup::prepare_workspace(
        &loaded,
        &configured.environment,
        &storage,
        &workspace_path,
    ) {
        Ok(prepared) => prepared,
        Err(message) => return fail(stderr, &message),
    };
    let session_root = &prepared.session_root;

    if !options.archive_path.is_empty() {
        return match storage.sessions().and_then(|repository| {
            repository.archive(session_root, &workspace_path, &options.archive_path)
        }) {
            Ok(result) => {
                let _ = writeln!(
                    stdout,
                    "Archived: {}",
                    loaded.boundary.redactor().redact_string(&result.path)
                );
                0
            }
            Err(_) => fail(stderr, "archive session: archive session failed"),
        };
    }

    let mut session_path = options.resume_path.clone();
    let mut listed_session_path = false;
    if options.continue_last {
        let listed = match storage
            .sessions()
            .and_then(|repository| repository.list(session_root, &workspace_path, "", 1))
        {
            Ok(listed) => listed,
            Err(error) => return fail(stderr, &loaded.boundary.redact(&error.to_string())),
        };
        let Some(first) = listed.sessions.first() else {
            return fail(
                stderr,
                &loaded
                    .boundary
                    .redact(&format!("no session found for workspace {workspace_path}")),
            );
        };
        session_path = first.path.clone();
        listed_session_path = true;
    }

    let ui_mode = match resolve_ui_mode(&loaded.config, &configured.environment, &options.ui) {
        Ok(mode) => mode,
        Err(error) => return fail(stderr, &loaded.boundary.redact(&error.to_string())),
    };
    let mut frontend = Frontend::Once;
    if !options.prompt_set && !options.acp {
        frontend = match select_frontend(ui_mode, terminal) {
            Ok(frontend) => frontend,
            Err(message) => return fail(stderr, &loaded.boundary.redact(&message)),
        };
    }

    let sandbox_driver_override = options.sandbox_set.then(|| options.sandbox.clone());
    // Stderr is free for serve, acp and workflow; the REPL, TUI and one-shot
    // frontends own the terminal or already print warnings there.
    let logger = Arc::new(
        if options.serve || options.acp || workflow_command.is_some() {
            crate::log::Logger::stderr()
        } else {
            crate::log::Logger::discard()
        },
    );
    let composed = match startup::compose(
        startup::ComposeOptions {
            sandbox_driver_override: sandbox_driver_override.clone(),
            no_session: options.no_session,
            overrides: overrides_from(&options),
            explicit_config: options.explicit_config,
            logger,
            disabled: Disabled::default(),
        },
        &loaded,
        &configured,
        &host_entries,
        &lookup,
        &storage,
        &prepared,
        &workspace_path,
        &mut *stderr,
    ) {
        Ok(composed) => composed,
        Err(message) => return fail(stderr, &message),
    };
    let sandbox_settings = composed.sandbox_settings;
    let mut builder = composed.builder;

    let mut prepared_initial: Option<Box<dyn session::PreparedSession>> = None;
    let mut metadata = None;
    if !session_path.is_empty() {
        let create_lease = match builder.create_lease() {
            Ok(create_lease) => create_lease,
            Err(error) => return fail(stderr, &builder.redact_error(&error, None)),
        };
        let repository = match builder.storage.sessions() {
            Ok(repository) => repository,
            Err(error) => return fail(stderr, &loaded.boundary.redact(&error)),
        };
        let prepared = prepare_session_backend(
            repository,
            session_root,
            &workspace_path,
            &session_path,
            create_lease,
            listed_session_path,
        );
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

    let resolved = match startup::initial_runtime(&builder, metadata.as_ref()) {
        Ok(runtime) => runtime,
        Err(message) => return fail(stderr, &message),
    };

    // `elevate` is REPL/TUI-specific here (an interactive frontend or a
    // server both need `/approve`); `load_workspace` always passes `true`,
    // because every workspace `otto serve` loads at runtime needs the same
    // support the startup workspace gets.
    let elevate = options.serve || options.acp || frontend != Frontend::Once;
    let composed = startup::compose_workspace_sandbox(
        &mut builder,
        &sandbox_settings,
        Path::new(&loaded.config_path),
        options.explicit_config,
        sandbox_driver_override.as_deref(),
        &resolved.api_key_env,
        elevate,
        &Disabled::default(),
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
    let approval_environment = composed.approval_environment;
    let reloader = composed.reloader;

    let dynamic_content = builder.boundary_allows_dynamic(Some(&resolved));
    if (prepared_initial.is_some() || options.serve || options.acp || workflow_command.is_some())
        && !dynamic_content
    {
        let _ = control.close().await;
        return fail(stderr, SESSION_OPERATION_UNAVAILABLE);
    }
    if let Err(message) = startup::open_memory(
        &mut builder,
        &prepared.memory_config,
        &resolved,
        dynamic_content,
        &loaded.home,
        stderr,
    ) {
        let _ = control.close().await;
        return fail(stderr, &message);
    }
    startup_trace.mark("memory/open");
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

    if options.acp {
        let builder = Arc::new(builder);
        let exit = super::acp::run(
            super::acp::AcpOptions {
                builder,
                runtime: resolved,
                workspace: PathBuf::from(&workspace_path),
                control,
                reloader,
                terminate,
            },
            stdin,
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

    if options.serve {
        let resolved_listen = resolve_server(
            &loaded.config,
            &configured.environment,
            &options.socket,
            &options.listen,
        )
        .and_then(|mut listen| {
            // Both flags bind both listeners; `resolve_server` keeps
            // only the TCP one when both overrides are given.
            if options.socket_set && options.listen_set {
                listen.socket =
                    resolve_server(&loaded.config, &configured.environment, &options.socket, "")?
                        .socket;
            }
            Ok(listen)
        });
        let listen = match resolved_listen {
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
                acp_listen: options.acp_listen.clone(),
                acp_token: lookup.get("OTTO_ACP_TOKEN").cloned().unwrap_or_default(),
                control,
                reloader,
                delivery_setup_executor: approval_executor.clone(),
                delivery_setup_environment: approval_environment.clone(),
                terminate,
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

    let (initial_session, warnings) = match activate_initial_session(
        &builder,
        prepared_initial,
        dynamic_content,
        &resolved,
    )
    .await
    {
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
    // Only the frontend that *is* the agent in a Herdr pane reports: `otto
    // serve`, `--acp`, `--attach` and headless runs are not it. Reporting is
    // inert unless Herdr started this process.
    let herdr = match frontend {
        Frontend::Tui | Frontend::Repl => {
            crate::herdr::Config::from_environment(&lookup).map(crate::herdr::Reporter::start)
        }
        Frontend::Once => None,
    };
    if let Some(reporter) = &herdr {
        controller.install_herdr(Arc::clone(reporter));
    }
    let schedule_worker = match (&frontend, controller.builder().schedule.clone()) {
        (Frontend::Repl | Frontend::Tui, Some(store)) => Some(tokio::spawn(
            crate::schedule::worker::Worker {
                store,
                run: Some(crate::schedule::worker::RunRole {
                    scope: crate::schedule::Scope::Terminal,
                    workspace: Some(controller.builder().workspace_path.clone()),
                    runner: Arc::new(crate::app::scheduled::ControllerRunner(Arc::clone(
                        &controller,
                    ))),
                }),
                deliver: Some(crate::schedule::worker::DeliverRole {
                    scope: crate::schedule::Scope::Terminal,
                    sink: Arc::new(crate::app::scheduled::NoticeSink(Arc::clone(&controller))),
                }),
                logger: Arc::clone(&controller.builder().logger),
                metrics: None,
            }
            .run(cancel.child_token()),
        )),
        _ => None,
    };
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
    if !cancelled_before_exit
        && run_error.is_ok()
        && !terminate.migrating()
        && controller.reflects_on_exit()
    {
        let _ = writeln!(
            stderr,
            "reflecting on this session before exit (up to {}s, Ctrl+C skips)...",
            crate::app::auto_reflection::EXIT_TIMEOUT.as_secs()
        );
        match reflect_on_exit(&controller, cancel).await {
            Ok(Some(line)) => {
                let _ = writeln!(stdout, "{line}");
            }
            Ok(None) => {}
            Err(()) => {
                let _ = writeln!(
                    stderr,
                    "reflection: timed out; /reflect can cover it next time"
                );
            }
        }
    }
    cancel.cancel();
    if let Some(task) = mcp_swap {
        task.abort();
    }
    if let Some(task) = schedule_worker {
        let _ = task.await;
    }
    if terminate.migrating() {
        for warning in controller.migrate().await {
            let _ = writeln!(stderr, "warning: {warning}");
        }
    }
    controller.close_mcp().await;
    if let Some(reporter) = &herdr {
        // Tell Herdr this pane no longer holds an agent before the session
        // closes, so it keeps no stale row.
        reporter.release().await;
    }
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
        let mut runner = match controller
            .builder()
            .build_runner_quiet(&session, &runtime)
            .await
        {
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
                    runner = match controller
                        .builder()
                        .build_runner_quiet(&session, &runtime)
                        .await
                    {
                        Ok(next) => next,
                        Err(_) => return,
                    };
                }
                Err(_) => return,
            }
        }
    })
}

fn prepare_session_backend(
    repository: &dyn session::SessionRepository,
    root: &Path,
    workspace: &str,
    reference: &str,
    create_lease: Option<u64>,
    listed: bool,
) -> Result<Box<dyn session::PreparedSession>, String> {
    let prepared = if listed {
        repository.prepare_listed(root, workspace, reference, create_lease)
    } else {
        repository.prepare_in(root, reference, create_lease)
    }?;
    if let Err(error) =
        crate::runtime::builder::validate_session_workspace(&prepared.info().cwd, workspace)
    {
        let _ = prepared.close();
        return Err(error);
    }
    Ok(prepared)
}

async fn activate_initial_session(
    builder: &Builder,
    prepared: Option<Box<dyn crate::session::PreparedSession>>,
    dynamic_content: bool,
    runtime: &Runtime,
) -> Result<(SharedSession, Vec<String>), String> {
    if let Some(prepared) = prepared {
        let prepared = prepared.with_replayable(crate::tool::safety::is_replayable);
        let (session, warnings) = tokio::task::spawn_blocking(move || prepared.activate())
            .await
            .map_err(|error| format!("session activation task panicked: {error}"))?
            .map_err(|error| builder.redact_error(&error, Some(runtime)))?;
        let messages = warnings
            .into_iter()
            .map(|warning| builder.redact_error(&warning.message, Some(runtime)))
            .collect();
        return Ok((SharedSession::new(session), messages));
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
    use crate::runtime::sandbox_runtime::canonical_directory;
    use crate::runtime::startup::{
        MAX_CAPTURED_ENVIRONMENT_BYTES, MAX_CAPTURED_ENVIRONMENT_ENTRIES, config_environment,
        load_workspace, resolve_sandbox_settings, skill_checker,
    };
    use otto_core::config::File;
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

        assert!(
            skill_checker(
                &crate::runtime::storage::Storage::default(),
                &file,
                &environment,
                "/nonexistent",
                &mut stderr,
            )
            .is_none()
        );
        assert!(stderr.get_ref().is_empty());
    }

    #[test]
    fn skill_checker_is_none_without_the_experimental_flag() {
        let file = File::default();
        let environment = HashMap::from([("TYPESAFE_API_KEY".to_string(), "key".to_string())]);
        let mut stderr = Cursor::new(Vec::new());

        assert!(
            skill_checker(
                &crate::runtime::storage::Storage::default(),
                &file,
                &environment,
                "/nonexistent",
                &mut stderr,
            )
            .is_none()
        );
        assert!(stderr.get_ref().is_empty());
    }

    #[test]
    fn skill_checker_is_none_without_an_api_key() {
        let file = skill_check_enabled_config();
        let environment = HashMap::new();
        let mut stderr = Cursor::new(Vec::new());

        assert!(
            skill_checker(
                &crate::runtime::storage::Storage::default(),
                &file,
                &environment,
                "/nonexistent",
                &mut stderr,
            )
            .is_none()
        );
        assert!(stderr.get_ref().is_empty());
    }

    #[test]
    fn skill_checker_opens_a_database_when_all_three_conditions_hold() {
        let home = tempfile::tempdir().expect("home");
        let file = skill_check_enabled_config();
        let environment = HashMap::from([("TYPESAFE_API_KEY".to_string(), "key".to_string())]);
        let mut stderr = Cursor::new(Vec::new());

        let checker = skill_checker(
            &crate::runtime::storage::Storage::default(),
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
    fn the_config_environment_carries_the_web_search_keys() {
        let file = otto_core::config::File::default();
        let keyless = |lookup: &[(&str, &str)]| {
            let lookup: HashMap<String, String> = lookup
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let environment = config_environment(&file, &lookup);
            crate::tool::web::search_is_keyless(&|name| environment.get(name).cloned())
        };
        assert!(keyless(&[]));
        assert!(!keyless(&[("BRAVE_API_KEY", "brave")]));
        assert!(!keyless(&[
            ("GOOGLE_API_KEY", "google"),
            ("GOOGLE_SEARCH_ENGINE_ID", "cx"),
        ]));
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
            &crate::cli::terminate::Terminate::new(),
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
    async fn an_empty_implicit_configuration_points_to_setup() {
        let home = tempfile::tempdir().expect("home");
        let workspace = tempfile::tempdir().expect("workspace");
        let args = vec![
            "--cwd".to_string(),
            workspace.path().to_string_lossy().into_owned(),
            "--no-session".to_string(),
        ];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &args,
            Box::new(Cursor::new(Vec::new())),
            &mut stdout,
            &mut stderr,
            entries(&[&format!("HOME={}", home.path().to_string_lossy())]),
            false,
            &tokio_util::sync::CancellationToken::new(),
            &crate::cli::terminate::Terminate::new(),
        )
        .await;

        assert_eq!(code, 1);
        assert_eq!(
            String::from_utf8_lossy(&stderr),
            "otto: no provider configured; run `otto setup` to create one, or pass --provider and --model\n"
        );
    }

    /// Pins every startup warning, in emission order, with the exact bytes.
    #[tokio::test]
    async fn startup_warnings_keep_their_order_and_text() {
        let directory = tempfile::tempdir().expect("tempdir");
        let home = directory.path().join("home");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        for store in ["usage.db", "tasks.db", "schedule.db"] {
            std::fs::create_dir_all(home.join(".otto").join(store)).expect("blocked store");
        }
        std::fs::create_dir_all(workspace.join(".otto")).expect("project dir");
        std::fs::write(
            workspace.join(".otto/config.toml"),
            "[sandbox]\ndriver = \"seatbelt\"\n",
        )
        .expect("write project config");
        let config = home.join(".config/otto/config.toml");
        std::fs::write(
            &config,
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

[sandbox]
driver = "off"
"#,
        )
        .expect("write config");

        let args = vec![
            "acp".to_string(),
            "--cwd".to_string(),
            workspace.to_string_lossy().into_owned(),
        ];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &args,
            Box::new(Cursor::new(Vec::new())),
            &mut stdout,
            &mut stderr,
            entries(&[
                &format!("HOME={}", home.to_string_lossy()),
                "ALPHA_KEY=sk-alpha",
            ]),
            false,
            &tokio_util::sync::CancellationToken::new(),
            &crate::cli::terminate::Terminate::new(),
        )
        .await;

        let stderr = String::from_utf8_lossy(&stderr).replace(
            &*workspace
                .canonicalize()
                .expect("canonical")
                .to_string_lossy(),
            "<workspace>",
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        assert_eq!(
            stderr,
            "warning: sandbox in <workspace>/.otto/config.toml: field is restricted to user or managed configuration\n\
             warning: usage store unavailable, continuing without usage history\n\
             warning: task history store unavailable, continuing without task recording\n\
             warning: schedule store unavailable, continuing without scheduled deliveries\n\
             warning: sandbox is off; bash runs unsandboxed as your user\n"
        );
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
            &crate::cli::terminate::Terminate::new(),
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
            &crate::cli::terminate::Terminate::new(),
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

        let (builder, _reloader) = load_workspace(
            Arc::clone(&shared),
            &canonical,
            &Disabled::default(),
            &cancel,
            &mut stderr,
        )
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

        let (builder, _reloader) = load_workspace(
            Arc::clone(&shared),
            &canonical,
            &Disabled::default(),
            &cancel,
            &mut stderr,
        )
        .await
        .expect("load workspace");

        assert_eq!(
            builder.sandbox_info.mode,
            crate::runtime::info::SandboxMode::Off
        );
        assert!(builder.sandbox_info.bash_available);
        assert!(builder.command_executor.is_some());
    }

    /// `otto serve --sandbox off` must apply to a workspace loaded after
    /// startup too: both read the same `Shared::sandbox_driver_override`
    /// rather than `load_workspace` dropping it.
    #[tokio::test]
    async fn load_workspace_binds_a_delivery_worktree_as_its_own_file_root() {
        let root = tempfile::tempdir().expect("root");
        let primary = root.path().join("primary");
        let worktree = primary.join(".otto/delivery-worktrees/delivery-1");
        std::fs::create_dir_all(&worktree).expect("worktree");
        let primary = canonical_directory(&primary).expect("primary canonical");
        let worktree = canonical_directory(&worktree).expect("worktree canonical");
        let shared = shared_with_offline_sandbox(root.path());
        let (builder, _sandbox) = load_workspace(
            shared,
            &worktree,
            &Disabled::default(),
            &CancellationToken::new(),
            &mut Vec::new(),
        )
        .await
        .expect("load worktree");

        assert_eq!(builder.workspace_path, worktree.to_string_lossy());
        assert_eq!(builder.workspace.root(), worktree);
        assert_ne!(builder.workspace.root(), primary);
        let runtime = crate::runtime::builder::resolve_initial_runtime(
            &builder.config,
            &builder.environment,
            None,
            &builder.overrides,
        )
        .expect("runtime");
        let session = builder.create_session(&runtime).expect("delivery session");
        let runner = builder
            .build_runner_quiet(&session, &runtime)
            .await
            .expect("delivery runner");
        assert_eq!(
            runner.session().header().workspace,
            worktree.to_string_lossy()
        );
        assert_ne!(
            runner.session().header().workspace,
            primary.to_string_lossy()
        );
    }

    #[tokio::test]
    async fn load_workspace_honors_the_shared_sandbox_driver_override() {
        let shared_root = tempfile::tempdir().expect("shared root");
        let workspace = tempfile::tempdir().expect("workspace");
        let canonical = canonical_directory(workspace.path()).expect("canonical");
        let shared = shared_with_sandbox_driver_override(shared_root.path(), "off");
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut stderr = Vec::new();

        let (builder, _reloader) = load_workspace(
            Arc::clone(&shared),
            &canonical,
            &Disabled::default(),
            &cancel,
            &mut stderr,
        )
        .await
        .expect("load workspace");

        assert_eq!(
            builder.sandbox_info.mode,
            crate::runtime::info::SandboxMode::Off
        );
    }

    /// A workspace loaded at runtime under Seatbelt gets the same elevated
    /// `/approve` executor startup builds for its own workspace. Skips only
    /// when `sandbox-exec` itself cannot run on this host; once it can, a
    /// runtime that does not come up is a failure.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn load_workspace_seatbelt_gets_bash_approvals() {
        use std::os::unix::fs::PermissionsExt as _;

        if let Some(reason) = crate::sandbox::seatbelt::driver::unavailable_reason() {
            eprintln!("skipping: {reason}");
            return;
        }
        let shared_root = tempfile::tempdir().expect("shared root");
        // The Seatbelt private tree is created under `$HOME/Library/Caches`;
        // without it the driver does not open.
        let caches = shared_root.path().join("Library/Caches");
        std::fs::create_dir_all(&caches).expect("cache base");
        for directory in [caches.parent().expect("library"), caches.as_path()] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .expect("cache mode");
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let canonical = canonical_directory(workspace.path()).expect("canonical");
        let shared = shared_with_sandbox_driver_override(shared_root.path(), "seatbelt");
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut stderr = Vec::new();

        let (builder, sandbox) = load_workspace(
            Arc::clone(&shared),
            &canonical,
            &Disabled::default(),
            &cancel,
            &mut stderr,
        )
        .await
        .expect("load workspace");

        assert_eq!(
            builder.sandbox_info.mode,
            crate::runtime::info::SandboxMode::Seatbelt,
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(builder.bash_approvals.is_some());
        assert!(sandbox.reloader.is_some());
        assert!(sandbox.approval_executor.is_some());
        assert!(sandbox.approval_environment.is_some());
        assert_eq!(
            sandbox.approval_executor.is_some(),
            sandbox.approval_environment.is_some()
        );
    }
}
