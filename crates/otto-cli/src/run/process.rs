//! The process composition flow: `run_with_extensions`.

use super::*;

/// Process entry point for host-provided memory services.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_extensions(
    args: &[String],
    mut stdin: Box<dyn BufRead + Send + 'static>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    environment_entries: Vec<Vec<u8>>,
    terminal: bool,
    cancel: &CancellationToken,
    terminate: &crate::terminate::Terminate,
    memory_extension: Option<otto::runtime::wiring::MemoryExtension>,
) -> i32 {
    if let Some(code) = subcommands::run_subcommand(
        args,
        &mut stdin,
        stdout,
        stderr,
        &environment_entries,
        cancel,
        memory_extension.is_some(),
    )
    .await
    {
        return code;
    }
    let startup_started = Instant::now();
    let mut startup_trace: StartupTrace;

    let workflow_command = if args.first().is_some_and(|first| first == "workflow") {
        match crate::workflow::parse(&args[1..]) {
            Ok(command) => Some(command),
            Err(message) => {
                let _ = writeln!(stderr, "{message}");
                return 2;
            }
        }
    } else {
        None
    };
    let eval_command = if args.first().is_some_and(|first| first == "eval") {
        match crate::eval_command::parse_command(&args[1..]) {
            Ok(command) => Some(command),
            Err(message) => {
                let _ = writeln!(stderr, "{message}");
                return 2;
            }
        }
    } else {
        None
    };
    let flag_args: Vec<String> = if workflow_command.is_some() {
        Vec::new()
    } else if let Some(command) = &eval_command {
        command.startup_flags()
    } else {
        args.to_vec()
    };

    let options = match parse_flags(&flag_args, stdout) {
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
    for warning in options.deprecation_warnings() {
        let _ = stderr.write_all(warning.as_bytes());
    }
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
    let storage = Arc::new(otto::runtime::storage::Storage::default());
    let configured = match startup::configure(
        &mut loaded,
        &host_entries,
        &lookup,
        &overrides_from(&options),
        &mut *stderr,
    ) {
        Ok(configured) => configured,
        Err(message) => return fail(stderr, &message),
    };

    if (!options.archive_path.is_empty()
        || options.delete_target.is_some()
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
        return attach::run_attach(
            &options,
            &loaded,
            &configured,
            &storage,
            &workspace_path,
            terminal,
            stdin,
            stdout,
            stderr,
            cancel,
        )
        .await;
    }
    let prepared =
        match startup::prepare_workspace(&loaded, &configured.environment, &workspace_path) {
            Ok(prepared) => prepared,
            Err(message) => return fail(stderr, &message),
        };
    let session_root = &prepared.session_root;
    let eval_roots = eval_command.as_ref().map(|_| crate::eval_command::Roots {
        session_root: session_root.clone(),
        workspace: workspace_path.clone(),
        home: loaded.home.clone(),
    });
    if let Some(command) = &eval_command
        && command.is_dry_run()
    {
        return crate::eval_command::run_dry_command(command, stdout, stderr);
    }

    if let Some(target) = &options.delete_target {
        return delete::run(
            storage.sessions(),
            session_root,
            &workspace_path,
            target,
            options.delete_force,
            &mut *stdin,
            stdout,
            stderr,
            |text| loaded.boundary.redact(text),
        );
    }

    if !options.archive_path.is_empty() {
        return match storage.sessions().archive(
            session_root,
            &workspace_path,
            &options.archive_path,
        ) {
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
            .list(session_root, &workspace_path, "", 1)
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
        if options.serve || options.acp || workflow_command.is_some() || eval_command.is_some() {
            otto::log::Logger::stderr()
        } else {
            otto::log::Logger::discard()
        },
    );
    let composed = match startup::compose(
        startup::ComposeOptions {
            memory_extension,
            sandbox_driver_override: sandbox_driver_override.clone(),
            no_session: options.no_session,
            // serve, acp and workflow commands also keep `Frontend::Once`.
            headless: frontend == Frontend::Once
                && !options.serve
                && !options.acp
                && workflow_command.is_none()
                && eval_command.is_none(),
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
        let repository = builder.storage.sessions();
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

    let initial_runtime = if options.serve {
        otto_core::config::resolve::resolve_server_bootstrap(
            &builder.config,
            &builder.environment,
            &builder.overrides,
        )
        .map_err(|error| builder.redact_error(&error.to_string(), None))
    } else {
        startup::initial_runtime(&builder, metadata.as_ref())
    };
    let resolved = match initial_runtime {
        Ok(runtime) => runtime,
        Err(message) => return fail(stderr, &message),
    };

    // The evaluation needs the provider and the redactor only: no sandbox and no memory.
    if let (Some(command), Some(roots)) = (&eval_command, &eval_roots) {
        return crate::eval_command::run_command(
            command, &builder, &resolved, roots, stdout, stderr, cancel,
        )
        .await;
    }

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
    let memory_client = builder.memory.client.clone();
    if cancel.is_cancelled() {
        let _ = control.close().await;
        return 130;
    }

    if let Some(command) = workflow_command {
        let builder = Arc::new(builder);
        let controller =
            crate::workflow::build_controller(Arc::clone(&builder), &resolved, stderr).await;
        let result = match controller {
            Ok(controller) => {
                let result = crate::workflow::run_command(command, &controller, stdout).await;
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
        let memory_error = match &memory_client {
            Some(client) => client.close(&CancellationToken::new()).await.err(),
            None => None,
        };
        let _ = memory_service.close();
        if let Some(error) = memory_error {
            return fail(stderr, &format!("close memory: {error}"));
        }
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
        let exit = crate::acp::run(
            crate::acp::AcpOptions {
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
                server_token: lookup.get("OTTO_SERVER_TOKEN").cloned().unwrap_or_default(),
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
        runner.close();
        initial_session.close_resources().await;
        let _ = initial_session.close();
        let _ = control.close().await;
        if cancel.is_cancelled() {
            return 130;
        }
        return fail(stderr, &message);
    }
    if cancel.is_cancelled() {
        runner.close();
        initial_session.close_resources().await;
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
        Some(reloader) => controller.with_sandbox_control(
            Arc::clone(reloader) as Arc<dyn otto::sandbox::control::SandboxControl>
        ),
        None => controller,
    };
    let controller = Arc::new(controller);
    // `--yolo` is the startup form of `/permissions yolo`: it stores the mode of the
    // session this process opens, exactly as the command does.
    if options.yolo {
        let scope = controller.approval_scope();
        if let Err(error) = controller
            .approval_service()
            .set_mode(&scope, otto_core::approval::ApprovalMode::Yolo)
        {
            let _ = control.close().await;
            return fail(stderr, &redact_with(&tail_redactor, &error.to_string()));
        }
    }
    // Only the frontend that *is* the agent in a Herdr pane reports: `otto
    // serve`, `--acp`, `--attach` and headless runs are not it. Reporting is
    // inert unless Herdr started this process.
    let herdr = match frontend {
        Frontend::Tui | Frontend::Repl => {
            otto::herdr::Config::from_environment(&lookup).map(otto::herdr::Reporter::start)
        }
        Frontend::Once => None,
    };
    if let Some(reporter) = &herdr {
        controller.install_herdr(Arc::clone(reporter));
    }
    let schedule_worker = match (&frontend, controller.builder().schedule.clone()) {
        (Frontend::Repl | Frontend::Tui, Some(store)) => Some(tokio::spawn(
            otto::schedule::worker::Worker {
                store,
                run: Some(otto::schedule::worker::RunRole {
                    scope: otto::schedule::Scope::Terminal,
                    workspace: Some(controller.builder().workspace_path.clone()),
                    admit: None,
                    runner: Arc::new(otto::app::scheduled::ControllerRunner(Arc::clone(
                        &controller,
                    ))),
                }),
                deliver: Some(otto::schedule::worker::DeliverRole {
                    scope: otto::schedule::Scope::Terminal,
                    sink: Arc::new(otto::app::scheduled::NoticeSink(Arc::clone(&controller))),
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
            let mut console = if terminal {
                Repl::with_program_status(
                    &controller,
                    Box::new(&mut *stdout),
                    Box::new(&mut *stderr),
                )
            } else {
                Repl::new(&controller, Box::new(&mut *stdout), Box::new(&mut *stderr))
            };
            console.run_once(&prompt, cancel).await
        }
        Frontend::Repl => {
            let mut console = if terminal {
                Repl::with_program_status(
                    &controller,
                    Box::new(&mut *stdout),
                    Box::new(&mut *stderr),
                )
            } else {
                Repl::new(&controller, Box::new(&mut *stdout), Box::new(&mut *stderr))
            };
            console.run(stdin, cancel).await
        }
        Frontend::Tui => {
            otto::tui::run_with_observer_bindings(&controller, cancel, &loaded.config.ui.observer)
                .await
        }
    };

    let cancelled_before_exit = cancel.is_cancelled();
    let frontend_cancelled = matches!(run_error, Err(repl::Error::Cancelled));
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
    controller.close_resources().await;
    if let Some(reporter) = &herdr {
        // Tell Herdr this pane no longer holds an agent before the session
        // closes, so it keeps no stale row.
        reporter.release().await;
    }
    controller.close_workflows().await;
    let controller_error = controller.close();
    let sandbox_error = control.close().await;
    let approval_error = approval_executor
        .as_ref()
        .map(|executor| executor.close())
        .transpose()
        .err();
    let memory_error = match &memory_client {
        Some(client) => client.close(&CancellationToken::new()).await.err(),
        None => None,
    };
    let _ = memory_service.close();
    if let Some(error) = memory_error {
        return fail(stderr, &format!("close memory: {error}"));
    }
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
