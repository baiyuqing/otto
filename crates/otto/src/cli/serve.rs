//! `otto serve`: the HTTP composition root.
//!
//! Port of `cmd/otto/serve.go`. Everything
//! the REPL path builds is already in place when [`run`] is called; this
//! module only picks the listener and wires one [`Factory`] over the shared
//! [`Builder`]. The process sandbox arrives already behind the
//! [`SandboxSwitch`](super::sandbox_switch::SandboxSwitch) that
//! `POST /v1/sandbox/reload` replaces.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use otto_core::config::ServerRuntime;
use otto_core::config::resolve::Runtime;
use otto_core::config::resolve_feishu;
use otto_core::session::ListResult;
use tokio_util::sync::CancellationToken;

use crate::app::{Controller, SandboxControl};
use crate::inbound;
use crate::server::listen::{Listener, listen_tcp, listen_unix};
use crate::server::{self, Factory, Info, Options, SESSION_NOT_FOUND, Server};
use crate::session::{self as sessionfs, MAX_LIST_SESSIONS};

use super::info::SandboxInfo;
use super::runtime_builder::Builder;
use super::sandbox_switch::{SandboxReloader, SandboxSwitch};

/// Writes `otto: {message}\n` and returns Go's exit code 1.
fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}

// ---- the session factory ----

/// Builds one [`Controller`] per server-side session on top of the same
/// replacement plumbing the CLI's `/new` and `/resume` use. Port of
/// `serveFactories`.
struct ServeFactory {
    builder: Arc<Builder>,
    runtime: Runtime,
    sandbox: Option<Arc<SandboxReloader>>,
}

impl ServeFactory {
    /// Go's `session.List(ctx, root, workspace, "", maxServeListSessions)`,
    /// with a missing session root reported as no sessions.
    fn listed(&self) -> Result<ListResult, String> {
        if !self.builder.session_root.exists() {
            return Ok(ListResult::default());
        }
        sessionfs::list(
            &self.builder.session_root,
            &self.builder.workspace_path,
            "",
            MAX_LIST_SESSIONS,
        )
        .map_err(|error| self.builder.redact_error(&error.to_string(), None))
    }

    fn wire(&self, controller: Controller) -> Controller {
        match &self.sandbox {
            Some(control) => {
                controller.with_sandbox_control(Arc::clone(control) as Arc<dyn SandboxControl>)
            }
            None => controller,
        }
    }
}

#[async_trait::async_trait]
impl Factory for ServeFactory {
    async fn create(&self) -> Result<Controller, String> {
        let controller = Controller::create(Arc::clone(&self.builder), &self.runtime).await?;
        Ok(self.wire(controller))
    }

    async fn open(&self, id: &str) -> Result<Controller, String> {
        let listed = self.listed()?;
        let Some(entry) = listed.sessions.into_iter().find(|entry| entry.id == id) else {
            return Err(SESSION_NOT_FOUND.to_string());
        };
        // The repair warnings the CLI prints have no channel here; the web UI
        // reads the repaired history like any other.
        let (controller, _warnings) =
            Controller::open(Arc::clone(&self.builder), Path::new(&entry.path)).await?;
        Ok(self.wire(controller))
    }

    fn list(&self) -> Option<Result<ListResult, String>> {
        Some(self.listed())
    }

    fn sandbox_reload_available(&self) -> bool {
        self.sandbox.is_some()
    }

    async fn reload_sandbox(&self) -> Option<Result<SandboxInfo, String>> {
        let control = self.sandbox.as_ref()?;
        Some(SandboxControl::reload(control.as_ref()).await)
    }

    fn usage_summary(&self, session_id: Option<&str>) -> Result<crate::usage::Summary, String> {
        self.builder.usage_summary(session_id)
    }

    fn usage_analysis(
        &self,
        days: u16,
        session_id: Option<&str>,
    ) -> Result<crate::usage::Analysis, String> {
        self.builder.usage_analysis(days, session_id)
    }
}

// ---- the command ----

/// What [`run`] needs from `cli::run`'s composition root.
pub struct ServeOptions {
    pub builder: Builder,
    pub runtime: Runtime,
    /// Exactly one of its two fields is set.
    pub listen: ServerRuntime,
    /// The process sandbox the composition root opened, already behind its
    /// switch. [`run`] owns closing it.
    pub control: Arc<SandboxSwitch>,
    /// `None` when bash never came up, so `POST /v1/sandbox/reload` answers
    /// 501 rather than a failure. Port of `runtimeBuilder.sandboxReload`.
    pub reloader: Option<Arc<SandboxReloader>>,
    /// Open the printed TCP URL in the default browser. Unix listeners have
    /// no URL; [`run`] rejects that combination before bind.
    pub open: bool,
}

/// Port of `runtimeBuilder.runServe`.
pub async fn run(
    options: ServeOptions,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> i32 {
    let ServeOptions {
        builder,
        runtime,
        listen,
        control,
        reloader,
        open,
    } = options;

    if let Err(message) = require_tcp_for_open(open, &listen) {
        let _ = control.close().await;
        return fail(stderr, &message);
    }

    let serve_cancel = cancel.child_token();
    let bound = match bind(&listen) {
        Ok(bound) => bound,
        Err(message) => {
            let _ = control.close().await;
            return fail(stderr, &builder.redact_error(&message, Some(&runtime)));
        }
    };
    let (listener, token) = bound;
    if !token.is_empty() {
        announce_listen(stdout, &listener.address(), &token, open);
    }

    let info = builder.runtime_info(&runtime);
    let feishu = resolve_feishu(&builder.config);
    let mut profiles: Vec<String> = builder.config.profiles.keys().cloned().collect();
    profiles.sort();
    let server = Server::new(Options {
        info: Info {
            workspace: builder.workspace_path.clone(),
            provider: info.provider.clone(),
            profile: info.profile.clone(),
            model: info.model.clone(),
            sandbox: info.sandbox.summary().to_string(),
            profiles,
        },
        factory: Arc::new(ServeFactory {
            builder: Arc::new(builder),
            runtime: runtime.clone(),
            sandbox: reloader,
        }),
        token,
        // ponytail: Go threads the caller's stderr into the slog handler;
        // `Logger` owns its sink, so the request log goes to the process
        // stderr instead. Thread a shared writer through if a test ever has
        // to read it.
        logger: None,
    });
    let inbound = inbound::maybe_start(Arc::clone(&server), feishu, serve_cancel.clone());

    // SIGTERM is how a long-running `otto serve` is asked to shut down; the
    // process token covers SIGINT already.
    let terminate = spawn_terminate(serve_cancel.clone());
    let serve_error = server::serve(listener, server.router(), serve_cancel.clone())
        .await
        .err();
    serve_cancel.cancel();
    if let Some(handle) = inbound {
        let _ = handle.await;
    }
    if let Some(handle) = terminate {
        let _ = handle.await;
    }
    server.cancel_token().cancel();
    let close_error = server.close().await.err();
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

fn require_tcp_for_open(open: bool, listen: &ServerRuntime) -> Result<(), String> {
    if open && listen.listen.is_empty() {
        return Err("--open requires a TCP listener".into());
    }
    Ok(())
}

fn announce_listen(stdout: &mut (dyn Write + Send), address: &str, token: &str, open: bool) {
    let url = format!("http://{address}/?token={token}");
    let _ = writeln!(stdout, "otto serve: {url}");
    if open {
        launch_browser(&url);
    }
}

/// Go runs `exec.Command("open", url)`. The absolute path is used because
/// `PATH` is attacker-influenced input at this point. A failed launch is not
/// fatal: the URL was already printed.
#[cfg(not(test))]
const OPEN_BINARY: &str = "/usr/bin/open";

#[cfg(test)]
static LAUNCH_TEST: Mutex<()> = Mutex::new(());
#[cfg(test)]
static LAUNCHES: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[cfg(test)]
fn take_launches() -> Vec<String> {
    std::mem::take(&mut *LAUNCHES.lock().unwrap_or_else(|poison| poison.into_inner()))
}

#[cfg(test)]
fn launch_browser(url: &str) {
    LAUNCHES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(url.to_owned());
}

#[cfg(not(test))]
fn launch_browser(url: &str) {
    let _ = std::process::Command::new(OPEN_BINARY).arg(url).spawn();
}

/// Picks the listener. A TCP port is reachable by every local user and every
/// page open in a browser, so the API is gated by a per-process token; the
/// token is printed once by the caller and never logged.
fn bind(listen: &ServerRuntime) -> Result<(Listener, String), String> {
    if !listen.listen.is_empty() {
        let listener = listen_tcp(&listen.listen).map_err(|error| format!("serve: {error}"))?;
        let token =
            super::runtime_builder::random_id().map_err(|error| format!("serve: {error}"))?;
        return Ok((listener, token));
    }
    let socket = match Path::new(&listen.socket).is_absolute() {
        true => listen.socket.clone(),
        false => std::path::absolute(&listen.socket)
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .into_owned(),
    };
    let listener = listen_unix(&socket).map_err(|error| format!("serve: {error}"))?;
    Ok((listener, String::new()))
}

/// Cancels `token` on SIGTERM, ending when the token is cancelled from
/// anywhere else. Port of `subscribeOSTerminate` plus the goroutine that
/// watches it.
fn spawn_terminate(token: CancellationToken) -> Option<tokio::task::JoinHandle<()>> {
    let mut signals =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()?;
    Some(tokio::spawn(async move {
        tokio::select! {
            _ = signals.recv() => token.cancel(),
            _ = token.cancelled() => {}
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::config::ServerRuntime;

    #[test]
    fn open_without_a_tcp_listener_is_rejected() {
        let unix = ServerRuntime {
            socket: "/tmp/otto.sock".into(),
            listen: String::new(),
        };
        assert_eq!(
            require_tcp_for_open(true, &unix).unwrap_err(),
            "--open requires a TCP listener"
        );
        assert!(require_tcp_for_open(false, &unix).is_ok());
        let tcp = ServerRuntime {
            socket: String::new(),
            listen: "127.0.0.1:0".into(),
        };
        assert!(require_tcp_for_open(true, &tcp).is_ok());
    }

    fn with_launch_test<T>(body: impl FnOnce() -> T) -> T {
        let _guard = LAUNCH_TEST
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _ = take_launches();
        body()
    }

    #[test]
    fn open_launches_the_printed_url() {
        with_launch_test(|| {
            let mut stdout = Vec::new();
            announce_listen(&mut stdout, "127.0.0.1:8787", "tok", true);
            assert_eq!(
                String::from_utf8(stdout).expect("utf-8"),
                "otto serve: http://127.0.0.1:8787/?token=tok\n"
            );
            assert_eq!(
                take_launches(),
                vec!["http://127.0.0.1:8787/?token=tok".to_string()]
            );
        });
    }

    #[test]
    fn a_tcp_listener_without_open_does_not_launch() {
        with_launch_test(|| {
            let mut stdout = Vec::new();
            announce_listen(&mut stdout, "127.0.0.1:8787", "tok", false);
            assert!(take_launches().is_empty());
            assert_eq!(
                String::from_utf8(stdout).expect("utf-8"),
                "otto serve: http://127.0.0.1:8787/?token=tok\n"
            );
        });
    }
}
