//! `otto serve`: the HTTP composition root.
//!
//! Everything the REPL path builds is already in place when [`run`] is called;
//! this module only picks the listener and wires one [`Factory`] over the
//! shared [`Builder`]. The process sandbox arrives already behind the
//! [`SandboxSwitch`](super::sandbox_switch::SandboxSwitch) that `POST
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

use crate::app::{Controller, SandboxControl};
use crate::server::listen::{Listener, listen_tcp, listen_unix};
use crate::server::{self, Factory, Info, Options, SESSION_NOT_FOUND, Server};
use crate::session::{self as sessionfs, MAX_LIST_SESSIONS};

use super::sandbox_switch::{SandboxReloader, SandboxSwitch};
use crate::runtime::builder::Builder;
use crate::runtime::info::SandboxInfo;
use crate::runtime::sandbox_runtime::canonical_directory;
use crate::runtime::startup::{Disabled, load_workspace};
use crate::runtime::workspace_admission::{
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

/// What [`run`] needs from `cli::run`'s composition root.
pub struct ServeOptions<'a> {
    pub builder: Builder,
    pub runtime: Runtime,
    /// At least one of its two fields is set; both set binds both listeners.
    pub listen: ServerRuntime,
    pub acp_listen: String,
    pub acp_token: String,
    /// The process sandbox the composition root opened, already behind its
    /// switch. [`run`] owns closing it.
    pub control: Arc<SandboxSwitch>,
    /// `None` when bash never came up, so `POST /v1/sandbox/reload` answers 501
    /// rather than a failure.
    pub reloader: Option<Arc<SandboxReloader>>,
    /// Elevated target-local executor and its resolved environment, retained
    /// privately by the startup host for constrained delivery worktree setup.
    pub delivery_setup_executor: Option<Arc<crate::sandbox::Executor>>,
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
        match crate::acp::tcp::bind(&acp_listen, &acp_token) {
            Ok(listener) => Some((listener, acp_token)),
            Err(message) => {
                let _ = control.close().await;
                return fail(stderr, &builder.redact_error(&message, Some(&runtime)));
            }
        }
    };
    let serve_cancel = cancel.child_token();
    let bound = match bind(&listen) {
        Ok(bound) => bound,
        Err(message) => {
            let _ = control.close().await;
            return fail(stderr, &builder.redact_error(&message, Some(&runtime)));
        }
    };
    let Bound { tcp, socket, token } = bound;
    if let Some((listener, _)) = &acp {
        let _ = writeln!(
            stdout,
            "otto serve: ACP {}",
            listener.local_addr().expect("bound address")
        );
    }
    if let Some(tcp) = &tcp {
        announce_listen(stdout, &tcp.address(), &token);
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
    let workflows =
        match super::workflow::build_controller(Arc::clone(&builder), &runtime, stderr).await {
            Ok(controller) => Some(controller),
            Err(error) => {
                let redacted = builder.redact_error(&error, Some(&runtime));
                if !redacted.is_empty() {
                    let _ = writeln!(stderr, "warning: workflows disabled: {redacted}");
                }
                None
            }
        };
    let info = builder.runtime_info(&runtime);
    let mut profiles: Vec<String> = builder.config.profiles.keys().cloned().collect();
    profiles.sort();
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
            profile: info.profile.clone(),
            model: info.model.clone(),
            thinking: info.thinking.clone(),
            sandbox: info.sandbox.summary().to_string(),
            profiles,
        },
        factory: Arc::clone(&factory),
        config_path: builder.shared.config_path.clone(),
        token,
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
            crate::schedule::worker::Worker {
                store,
                run: Some(crate::schedule::worker::RunRole {
                    scope: crate::schedule::Scope::Any,
                    workspace: None,
                    runner: Arc::new(crate::server::schedule::FactoryRunner(factory)),
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
                let result = crate::acp::tcp::serve(
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
    let tcp_task = tcp.map(|listener| server::serve(listener, server.router(), stop.clone()));
    let socket_task =
        socket.map(|listener| server::serve(listener, server.socket_router(), stop.clone()));
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

fn announce_listen(stdout: &mut (dyn Write + Send), address: &str, token: &str) {
    let _ = writeln!(stdout, "otto serve: http://{address}/?token={token}");
}

/// The bound listeners. `token` is non-empty exactly when `tcp` is set.
struct Bound {
    tcp: Option<Listener>,
    socket: Option<Listener>,
    token: String,
}

/// Binds the TCP listener, the Unix socket, or both, from the two fields of
/// `listen`. A TCP port is reachable by every local user and every page open
/// in a browser, so the API on it is gated by a per-process token; the token
/// is printed once by the caller and never logged. The socket is protected by
/// its file permissions and needs no token.
fn bind(listen: &ServerRuntime) -> Result<Bound, String> {
    let mut bound = Bound {
        tcp: None,
        socket: None,
        token: String::new(),
    };
    if !listen.listen.is_empty() {
        bound.tcp = Some(listen_tcp(&listen.listen).map_err(|error| format!("serve: {error}"))?);
        bound.token =
            crate::runtime::builder::random_id().map_err(|error| format!("serve: {error}"))?;
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
