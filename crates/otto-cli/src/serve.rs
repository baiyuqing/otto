//! `otto serve`: the HTTP composition root.
//!
//! Everything the REPL path builds is already in place when [`run`] is called;
//! this module only picks the listener and wires one [`Factory`] over the
//! shared [`Builder`]. The process sandbox arrives already behind the
//! [`SandboxSwitch`](otto::runtime::sandbox_switch::SandboxSwitch) that `POST
//! /v1/sandbox/reload` replaces.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::{Arc, Weak};

use async_trait::async_trait;

use chrono::Utc;
use otto_core::config::ServerRuntime;
use otto_core::config::resolve::Runtime;
use otto_core::model::{Block, Message, Role};
use otto_core::session::{ListResult, Session};
use tokio_util::sync::CancellationToken;

use otto::app::{Controller, SandboxControl};
use otto::log::Logger;
use otto::session::{self as sessionfs, MAX_LIST_SESSIONS};
use otto_server::listen::{AllowedHosts, Listener, listen_tcp, listen_unix};
use otto_server::tls::{TlsListener, load_acceptor};
use otto_server::{self, Factory, Info, Options, SESSION_NOT_FOUND, Server};

use otto::runtime::builder::Builder;
use otto::runtime::info::SandboxInfo;
use otto::runtime::sandbox_runtime::canonical_directory;
use otto::runtime::sandbox_switch::{SandboxReloader, SandboxSwitch};
use otto::runtime::startup::{Disabled, load_workspace};
use otto::runtime::workspace_admission::{
    Admission, admit_workspace, canonicalize_workspace_roots, trusted_projects,
};

mod delivery;
mod factory;
mod workspaces;
use delivery::*;
use factory::*;
use workspaces::*;

/// Writes `otto: {message}\n` and returns exit code 1.
fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}

// ---- the command ----

/// What [`run`] needs from `otto_cli::run`'s composition root.
pub struct ServeOptions<'a> {
    pub builder: Builder,
    pub runtime: Runtime,
    /// At least one of its two fields is set; both set binds both listeners.
    pub listen: ServerRuntime,
    pub acp_listen: String,
    pub acp_token: String,
    /// `OTTO_SERVER_TOKEN` from the process environment; empty when unset.
    pub server_token: String,
    /// The process sandbox the composition root opened, already behind its
    /// switch. [`run`] owns closing it.
    pub control: Arc<SandboxSwitch>,
    /// `None` when bash never came up, so `POST /v1/sandbox/reload` answers 501
    /// rather than a failure.
    pub reloader: Option<Arc<SandboxReloader>>,
    /// Elevated target-local executor and its resolved environment, retained
    /// privately by the startup host for constrained delivery worktree setup.
    pub delivery_setup_executor: Option<Arc<otto::sandbox::Executor>>,
    pub delivery_setup_environment: Option<Vec<String>>,
    /// Set to "serving" before bind so a SIGTERM with no held lease cancels
    /// serve instead of killing the process; read after the listen loop
    /// returns to decide whether to run the migration.
    pub terminate: &'a super::terminate::Terminate,
}

pub async fn run(
    options: ServeOptions<'_>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> i32 {
    let ServeOptions {
        builder,
        runtime,
        listen,
        acp_listen,
        acp_token,
        server_token,
        control,
        reloader,
        delivery_setup_executor,
        delivery_setup_environment,
        terminate,
    } = options;
    terminate.set_serve();

    let workspace_roots = match canonicalize_workspace_roots(&listen.workspace_roots) {
        Ok(roots) => roots,
        Err(message) => {
            let _ = control.close().await;
            return fail(stderr, &builder.redact_error(&message, Some(&runtime)));
        }
    };

    let acp = if acp_listen.is_empty() {
        None
    } else {
        match otto_acp::tcp::bind(&acp_listen, &acp_token) {
            Ok(listener) => Some((listener, acp_token)),
            Err(message) => {
                let _ = control.close().await;
                return fail(stderr, &builder.redact_error(&message, Some(&runtime)));
            }
        }
    };
    let serve_cancel = cancel.child_token();
    let bound = match bind(&listen, &server_token, &builder.logger) {
        Ok(bound) => bound,
        Err(message) => {
            let _ = control.close().await;
            return fail(stderr, &builder.redact_error(&message, Some(&runtime)));
        }
    };
    let Bound {
        tcp,
        socket,
        token,
        token_from_env,
        tls,
        remote,
    } = bound;
    if let Some((listener, _)) = &acp {
        let _ = writeln!(
            stdout,
            "otto serve: ACP {}",
            listener.local_addr().expect("bound address")
        );
    }
    if let Some(tcp) = &tcp {
        let scheme = if tls { "https" } else { "http" };
        let printed_token = if token_from_env {
            None
        } else {
            Some(token.as_str())
        };
        announce_listen(stdout, scheme, &tcp.address(), printed_token);
    }

    let builder = Arc::new(builder);
    // The delivery manager opens before the workflow controller is built, and
    // the parent-tool slot is installed before that, so every parent runner
    // this process builds -- workflow steps included -- composes the same
    // tools. `Workspaces` is attached once it exists: it owns the workflow
    // controller through its host, so the broker cannot take it as an
    // argument here.
    let delivery = match DeliveryManager::open(
        &builder.shared.home,
        &builder.shared,
        runtime.clone(),
        serve_cancel.clone(),
    ) {
        Ok(manager) => Arc::new(manager),
        Err(error) => {
            let _ = control.close().await;
            return fail(stderr, &builder.redact_error(&error, Some(&runtime)));
        }
    };
    let _ = builder
        .shared
        .parent_tool_factory
        .install(Arc::new(DeliveryToolFactory::new(Arc::clone(&delivery))));
    let workflows = if runtime.model.is_empty() {
        None
    } else {
        match super::workflow::build_controller(Arc::clone(&builder), &runtime, stderr).await {
            Ok(controller) => Some(controller),
            Err(error) => {
                let redacted = builder.redact_error(&error, Some(&runtime));
                if !redacted.is_empty() {
                    let _ = writeln!(stderr, "warning: workflows disabled: {redacted}");
                }
                None
            }
        }
    };
    let info = builder.runtime_info(&runtime);
    let models = otto::config::configured_model_selectors(&builder.config);
    let workspace_path = builder.workspace_path.clone();
    let host = Arc::new(WorkspaceHost {
        builder: Arc::clone(&builder),
        workflows,
        delivery_setup: match (delivery_setup_executor, delivery_setup_environment) {
            (Some(executor), Some(environment)) => Some(DeliverySetupCapability {
                executor,
                environment,
            }),
            _ => None,
        },
        reloader,
    });
    let workspaces = Arc::new(Workspaces {
        startup: workspace_path.clone(),
        startup_host: Arc::clone(&host),
        roots: workspace_roots,
        config_path: builder.shared.config_path.clone(),
        loaded: tokio::sync::Mutex::new(BTreeMap::from([(workspace_path, Arc::clone(&host))])),
    });
    load_persisted_workspaces(
        &workspaces,
        &builder.shared,
        &runtime,
        &serve_cancel,
        stderr,
    )
    .await;
    delivery.attach_workspaces(&workspaces);
    let factory: Arc<dyn Factory> = Arc::new(ServeFactory {
        workspaces: Arc::clone(&workspaces),
        delivery: Some(Arc::clone(&delivery)),
        runtime: runtime.clone(),
        cancel: serve_cancel.clone(),
    });
    let server = Server::new(Options {
        info: Info {
            workspace: builder.workspace_path.clone(),
            provider: info.provider.clone(),
            provider_type: info.provider_type.clone(),
            model: info.model.clone(),
            thinking: info.thinking.clone(),
            sandbox: info.sandbox.summary().to_string(),
            models,
        },
        factory: Arc::clone(&factory),
        config_path: builder.shared.config_path.clone(),
        token,
        remote_listener: remote,
        logger: Some(Arc::clone(&builder.logger)),
        workflows: host.workflows.clone(),
        schedule: builder.schedule.clone(),
        session_idle_timeout: listen.session_idle_timeout,
        max_open_sessions: listen.max_open_sessions,
    });

    // Runs due scheduled prompts of every origin; delivery is the web routes
    // and the per-connection ACP workers.
    let schedule_worker = builder.schedule.clone().map(|store| {
        tokio::spawn(
            otto::schedule::worker::Worker {
                store,
                run: Some(otto::schedule::worker::RunRole {
                    scope: otto::schedule::Scope::Any,
                    workspace: None,
                    runner: Arc::new(otto_server::schedule::FactoryRunner(factory)),
                }),
                deliver: None,
                logger: Arc::clone(server.logger()),
                metrics: Some(Arc::clone(server.metrics())),
            }
            .run(serve_cancel.child_token()),
        )
    });
    // Closes sessions nobody has used for a while, so a server that stays up
    // keeps only the sessions in play. See `Server::sweep_idle_sessions`.
    server.start_session_sweeper(serve_cancel.child_token());
    let (serve_error, acp_error) = tokio::join!(
        serve_listeners(&server, tcp, socket, &serve_cancel),
        async {
            if let Some((listener, token)) = acp {
                let result = otto_acp::tcp::serve(
                    listener,
                    token,
                    Arc::clone(&server),
                    PathBuf::from(&builder.workspace_path),
                    &serve_cancel,
                )
                .await;
                if result.is_err() {
                    serve_cancel.cancel();
                }
                result.err()
            } else {
                None
            }
        }
    );
    let serve_error = serve_error.or(acp_error);
    serve_cancel.cancel();
    server.cancel_token().cancel();
    if let Some(worker) = schedule_worker {
        let _ = worker.await;
    }
    if terminate.migrating() {
        for warning in server.migrate().await {
            let _ = writeln!(stderr, "warning: {warning}");
        }
    }
    let close_error = server.close().await.err();
    delivery.shutdown().await;
    // Every loaded workspace owns its own controller, not only startup.
    let hosts: Vec<_> = workspaces.loaded.lock().await.values().cloned().collect();
    for host in hosts {
        if let Some(controller) = &host.workflows {
            controller.close().await;
        }
    }
    let sandbox_error = control.close().await.err();

    let redact = |message: &str| -> String { format!("serve: {message}") };
    if let Some(message) = serve_error {
        return fail(stderr, &redact(&message));
    }
    if let Some(message) = close_error {
        return fail(stderr, &redact(&message));
    }
    if sandbox_error.is_some() {
        return fail(stderr, "close sandbox: sandbox runtime close failed");
    }
    0
}

/// Serves the TCP listener (token required) and the socket listener (no
/// token) from one server. A failure on either stops both; the first error is
/// returned.
async fn serve_listeners(
    server: &Arc<Server>,
    tcp: Option<Listener>,
    socket: Option<Listener>,
    cancel: &CancellationToken,
) -> Option<String> {
    let stop = cancel.child_token();
    let tcp_task = tcp.map(|listener| otto_server::serve(listener, server.router(), stop.clone()));
    let socket_task =
        socket.map(|listener| otto_server::serve(listener, server.socket_router(), stop.clone()));
    let guard = |result: Result<(), String>| {
        if result.is_err() {
            stop.cancel();
        }
        result
    };
    let (tcp_result, socket_result) = tokio::join!(
        async {
            match tcp_task {
                Some(task) => guard(task.await),
                None => Ok(()),
            }
        },
        async {
            match socket_task {
                Some(task) => guard(task.await),
                None => Ok(()),
            }
        },
    );
    tcp_result.err().or(socket_result.err())
}

/// Prints the TCP listener's URL. `token` is `Some` only for a token that
/// `otto serve` generated; a token from `OTTO_SERVER_TOKEN` is never written
/// to stdout.
fn announce_listen(
    stdout: &mut (dyn Write + Send),
    scheme: &str,
    address: &str,
    token: Option<&str>,
) {
    match token {
        Some(token) => {
            let _ = writeln!(stdout, "otto serve: {scheme}://{address}/?token={token}");
        }
        None => {
            let _ = writeln!(
                stdout,
                "otto serve: {scheme}://{address}/ (token from OTTO_SERVER_TOKEN)"
            );
        }
    }
}

/// The bound listeners. `token` is non-empty exactly when `tcp` is set.
struct Bound {
    /// The TCP listener, wrapped in TLS when `[server].tls_cert` is set.
    tcp: Option<Listener>,
    /// The Unix socket listener.
    socket: Option<Listener>,
    /// The token the TCP listener requires.
    token: String,
    /// The token came from `OTTO_SERVER_TOKEN`, so it must not be printed.
    token_from_env: bool,
    /// The TCP listener terminates TLS.
    tls: bool,
    /// The TCP listener is bound to a non-loopback address.
    remote: bool,
}

/// Checks the length and the characters of an `OTTO_SERVER_TOKEN` value:
/// 32 to 4000 printable ASCII bytes without spaces.
fn validate_server_token(token: &str) -> Result<(), String> {
    let printable = token.bytes().all(|byte| (33..=126).contains(&byte));
    if (32..=4000).contains(&token.len()) && printable {
        return Ok(());
    }
    Err("serve: OTTO_SERVER_TOKEN must be 32 to 4000 printable ASCII bytes without spaces".into())
}

/// Binds the TCP listener, the Unix socket, or both, from the two fields of
/// `listen`. The API on a TCP port is gated by a token. The token is
/// `server_token` (`OTTO_SERVER_TOKEN`) when that is non-empty, else a
/// per-process random value that the caller prints once; neither is logged.
/// A non-loopback address is accepted only when `[server].tls_cert` and
/// `tls_key` are set and the token comes from the environment; otherwise the
/// address must be loopback. `server_token` and `tls` are ignored when
/// `listen.listen` is empty. The socket is protected by its file permissions
/// and needs no token.
fn bind(listen: &ServerRuntime, server_token: &str, log: &Arc<Logger>) -> Result<Bound, String> {
    let mut bound = Bound {
        tcp: None,
        socket: None,
        token: String::new(),
        token_from_env: false,
        tls: false,
        remote: false,
    };
    if !listen.listen.is_empty() {
        if !server_token.is_empty() {
            validate_server_token(server_token)?;
            bound.token = server_token.to_string();
            bound.token_from_env = true;
        } else {
            bound.token =
                otto::runtime::builder::random_id().map_err(|error| format!("serve: {error}"))?;
        }
        let acceptor = listen
            .tls
            .as_ref()
            .map(|tls| load_acceptor(&tls.cert_path, &tls.key_path))
            .transpose()
            .map_err(|error| format!("serve: {error}"))?;
        let hosts = if acceptor.is_some() && bound.token_from_env {
            AllowedHosts::Any
        } else {
            AllowedHosts::LoopbackOnly
        };
        let tcp = listen_tcp(&listen.listen, hosts).map_err(|error| format!("serve: {error}"))?;
        let local_address = tcp
            .local_addr()
            .map_err(|error| format!("serve: {error}"))?;
        bound.remote = !local_address.ip().is_loopback();
        bound.tls = acceptor.is_some();
        bound.tcp = Some(match acceptor {
            Some(acceptor) => Listener::Tls(TlsListener::new(tcp, acceptor, Arc::clone(log))),
            None => Listener::Tcp(tcp),
        });
    }
    if !listen.socket.is_empty() {
        let socket = match Path::new(&listen.socket).is_absolute() {
            true => listen.socket.clone(),
            false => std::path::absolute(&listen.socket)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .into_owned(),
        };
        bound.socket = Some(listen_unix(&socket).map_err(|error| format!("serve: {error}"))?);
    }
    Ok(bound)
}

#[cfg(test)]
mod tests;
