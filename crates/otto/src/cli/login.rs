//! `otto login`, `otto logout`, and the REPL's `/login` and `/logout`.
//!
//! Port of `cmd/otto/login_command.go`, `internal/repl/login.go`, the
//! `captureAuthCredentials` helper in `cmd/otto/runtime_builder.go` and the
//! `chatgpt` branch of `runtimeBuilder.buildProvider`. All of it lives in one
//! file because every piece reads or writes the same credential file and
//! nothing else in `cli` touches it.
//!
//! Safety: no failure path formats an underlying cause. Every diagnostic is
//! one of the fixed `auth::AuthError` strings, so a callback query value, a
//! token endpoint body or a host path can never reach stdout or stderr.
//! `capture_auth_credentials` is the only reader of token material here and it
//! returns the values solely so the redaction boundary can mask them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use otto_core::safetext::{self, SecretCollector};
use tokio_util::sync::CancellationToken;

use crate::auth::service::Service;
use crate::auth::{self, AuthError, Credentials};

use super::controller::Controller;
use super::repl::Error as ReplError;
use crate::memory::MAX_EXACT_GUARD_VALUE_BYTES;

/// Go runs `exec.Command("open", url)`, a `PATH` lookup. The absolute path is
/// used here because `PATH` is attacker-influenced input at this point.
#[cfg(not(test))]
const OPEN_BINARY: &str = "/usr/bin/open";

const SIGNED_IN: &str = "Signed in to ChatGPT.";
const SIGNED_OUT: &str = "Signed out of ChatGPT.";
const NOT_SIGNED_IN: &str = "Not signed in to ChatGPT.";
const REPL_SIGNED_IN: &str = "Signed in to ChatGPT. Restart Otto to use the new credentials.";
const LOGIN_USAGE: &str = "usage: /login [status]";

/// What `captureAuthCredentials` returns: the path it read, the credentials
/// when they loaded, and the values the redaction boundary must mask.
#[derive(Debug, Default)]
pub struct CapturedAuth {
    pub path: String,
    pub credentials: Credentials,
    pub loaded: bool,
    pub complete: bool,
    pub redaction_values: Vec<String>,
}

/// Port of `captureAuthCredentials`.
pub fn capture_auth_credentials(path: &Path) -> CapturedAuth {
    let mut capture = CapturedAuth {
        path: path.to_string_lossy().into_owned(),
        complete: true,
        ..CapturedAuth::default()
    };
    if capture.path.is_empty() {
        capture.complete = false;
        return capture;
    }
    let credentials = match auth::load(path) {
        Ok(credentials) => credentials,
        // A missing file is the normal state before `otto login`: nothing is
        // secret, so the boundary stays open.
        Err(AuthError::NoCredentials) => return capture,
        Err(_) => {
            capture.complete = false;
            return capture;
        }
    };
    let mut redactions = SecretCollector::new();
    let mut exact = SecretCollector::new();
    for value in [
        &credentials.access_token,
        &credentials.refresh_token,
        &credentials.id_token,
        &credentials.account_id,
    ] {
        if value.is_empty() {
            continue;
        }
        if !redactions.add(value)
            || !exact.add_form(value)
            || value.len() > MAX_EXACT_GUARD_VALUE_BYTES
        {
            capture.complete = false;
            return capture;
        }
    }
    let values = redactions.values();
    if safetext::dynamic_redaction_marker(&values).is_none() {
        capture.complete = false;
        return capture;
    }
    capture.credentials = credentials;
    capture.loaded = true;
    capture.redaction_values = values;
    capture
}

/// Records what the browser launcher was asked to open. Go swaps the
/// `replOpenBrowser` package variable; a compile-time seam is used here so no
/// test can reach `/usr/bin/open`.
#[cfg(test)]
static LAUNCHES: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[cfg(test)]
fn launched(url: &str) -> bool {
    LAUNCHES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .iter()
        .any(|held| held == url)
}

#[cfg(test)]
fn launch_browser(url: &str) {
    LAUNCHES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(url.to_owned());
}

/// A failed launch is not fatal: the URL was already printed, so the callback
/// flow can still complete. Go ignores the `Start` error for the same reason.
#[cfg(not(test))]
fn launch_browser(url: &str) {
    let _ = std::process::Command::new(OPEN_BINARY).arg(url).spawn();
}

/// The writer the opener shares with its caller. A plain `&mut` cannot cross
/// the `Send + Sync` bound on [`Opener`], and the lock is only ever held
/// inside the synchronous closure body, never across an await point.
///
/// `pub(crate)` so `repl_commands.rs`'s `/mcp login` can reuse the same
/// URL-printing convention instead of duplicating it.
pub(crate) type SharedWriter<'a> = Mutex<&'a mut (dyn Write + Send)>;

/// Port of `browserOpener`: print the URL, which is the reliable path, then
/// also try to launch the default browser. `pub(crate)` for the same reason
/// as [`SharedWriter`].
pub(crate) fn browser_opener<'a, 'w: 'a>(
    stdout: &'a SharedWriter<'w>,
) -> impl Fn(&str) -> Result<(), String> + Send + Sync + 'a {
    move |url: &str| {
        if let Ok(mut writer) = stdout.lock() {
            let _ = write!(writer, "Open this URL to sign in:\n\n  {url}\n\n");
            let _ = writer.flush();
        }
        launch_browser(url);
        Ok(())
    }
}

/// Port of the `chatgpt` branch of `runtimeBuilder.buildProvider`.
pub fn chatgpt_client(
    path: &str,
    credentials: &Credentials,
    loaded: bool,
) -> Result<crate::provider::chatgpt::Client, String> {
    if !loaded {
        return Err(AuthError::NoCredentials.to_string());
    }
    let path = path.trim();
    if path.is_empty() {
        return Err(AuthError::CredentialsUnavailable.to_string());
    }
    // Divergence from Go: `Credentials.TokenSource(ctx, path)` carries the
    // process context. `build_runner` has no token to pass, so the source gets
    // a fresh never-cancelled base and every request still aborts on the
    // per-call token the agent hands `Provider::complete`.
    let tokens = crate::auth::token::TokenSource::new(
        credentials.clone(),
        PathBuf::from(path),
        CancellationToken::new(),
    );
    Ok(crate::provider::chatgpt::Client::new(
        tokens,
        &credentials.account_id,
    ))
}

/// Writes `otto: {message}\n` and returns Go's exit code 1.
fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}

/// Port of `runAuthCommand`: `otto login [--status]` and `otto logout`.
pub async fn run_auth_command(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    home: &str,
    cancel: &CancellationToken,
) -> i32 {
    auth_command(args, stdout, stderr, home, cancel, &Service::new).await
}

async fn auth_command(
    args: &[String],
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    home: &str,
    cancel: &CancellationToken,
    build: &(dyn Fn(PathBuf) -> Service + Sync),
) -> i32 {
    let mut status = false;
    for argument in &args[1..] {
        match argument.as_str() {
            "--status" | "-status" => status = true,
            // Go's `flag` stops at the first non-flag argument and ignores the
            // rest; only an unknown flag is a parse failure.
            _ if argument.starts_with('-') => {
                let _ = writeln!(stderr, "flag provided but not defined: {argument}");
                return 2;
            }
            _ => break,
        }
    }
    let service = build(auth::path_for_home(Path::new(home)));
    // Go passes `context.Background()` to Status and Logout so that reading or
    // clearing the file still works after the process context is cancelled.
    let uncancelled = CancellationToken::new();
    match args[0].as_str() {
        "logout" => match service.logout(&uncancelled) {
            Err(_) => fail(stderr, &AuthError::CredentialsRemoval.to_string()),
            Ok(false) => {
                let _ = writeln!(stdout, "{NOT_SIGNED_IN}");
                0
            }
            Ok(true) => {
                let _ = writeln!(stdout, "{SIGNED_OUT}");
                0
            }
        },
        "login" if status => {
            let (line, signed_in) = service.status(&uncancelled);
            let _ = writeln!(stdout, "{line}");
            i32::from(!signed_in)
        }
        "login" => run_login(&service, stdout, stderr, cancel).await,
        other => fail(stderr, &format!("unknown command {other:?}")),
    }
}

/// Port of `runLogin`.
async fn run_login(
    service: &Service,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> i32 {
    let shared: SharedWriter<'_> = Mutex::new(stdout);
    let opener = browser_opener(&shared);
    let result = service.login(cancel, Some(&opener)).await;
    drop(opener);
    let stdout = shared
        .into_inner()
        .unwrap_or_else(|poison| poison.into_inner());
    match result {
        Ok(()) => {
            let _ = writeln!(stdout, "{SIGNED_IN}");
            0
        }
        Err(AuthError::Cancelled) => fail(stderr, &AuthError::Cancelled.to_string()),
        Err(AuthError::CredentialsPersistence) => {
            fail(stderr, &AuthError::CredentialsPersistence.to_string())
        }
        Err(_) => fail(stderr, &format!("login: {}", AuthError::LoginFailed)),
    }
}

/// The service the REPL commands act on, or `None` when the boundary is closed
/// or no credential path was captured. Port of the `dynamicContent` and
/// `authentication == nil` guards in `app.Controller`.
fn repl_service(
    controller: &Controller,
    build: &(dyn Fn(PathBuf) -> Service + Sync),
) -> Option<Service> {
    let path = controller.auth_path();
    (controller.dynamic_content() && !path.is_empty()).then(|| build(PathBuf::from(path)))
}

/// Port of `REPL.loginCommand`.
pub async fn repl_login(
    controller: &Controller,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    args: &str,
    cancel: &CancellationToken,
) -> Result<(), ReplError> {
    repl_login_with(controller, stdout, stderr, args, cancel, &Service::new).await
}

async fn repl_login_with(
    controller: &Controller,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    args: &str,
    cancel: &CancellationToken,
    build: &(dyn Fn(PathBuf) -> Service + Sync),
) -> Result<(), ReplError> {
    let Some(service) = repl_service(controller, build) else {
        let _ = writeln!(stdout, "{}", AuthError::InteractiveUnavailable);
        return Ok(());
    };
    match args {
        "status" => {
            let (line, _) = service.status(cancel);
            let _ = writeln!(stdout, "{line}");
            Ok(())
        }
        "" => {
            // Go reaches this as `app.ErrAuthenticationUnsupported` from
            // `Controller.Login`; the provider is checked here instead so the
            // controller keeps no authentication state.
            let provider = controller.info().provider;
            if provider != otto_core::config::PROVIDER_CHATGPT {
                let _ = writeln!(
                    stdout,
                    "The {provider} provider authenticates with an API key from the environment; there is nothing to sign in to."
                );
                return Ok(());
            }
            let shared: SharedWriter<'_> = Mutex::new(stdout);
            let opener = browser_opener(&shared);
            let result = service.login(cancel, Some(&opener)).await;
            drop(opener);
            let stdout = shared
                .into_inner()
                .unwrap_or_else(|poison| poison.into_inner());
            match result {
                Ok(()) => {
                    let _ = writeln!(stdout, "{REPL_SIGNED_IN}");
                    Ok(())
                }
                Err(error) => Err(ReplError::Command {
                    command: "/login".to_owned(),
                    message: error.to_string(),
                }),
            }
        }
        _ => {
            let _ = writeln!(stderr, "{LOGIN_USAGE}");
            Ok(())
        }
    }
}

/// Port of `REPL.logoutCommand`.
pub fn repl_logout(
    controller: &Controller,
    stdout: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> Result<(), ReplError> {
    let Some(service) = repl_service(controller, &Service::new) else {
        let _ = writeln!(stdout, "{}", AuthError::InteractiveUnavailable);
        return Ok(());
    };
    match service.logout(cancel) {
        Ok(true) => {
            let _ = writeln!(stdout, "{SIGNED_OUT}");
            Ok(())
        }
        Ok(false) => {
            let _ = writeln!(stdout, "{NOT_SIGNED_IN}");
            Ok(())
        }
        Err(error) => Err(ReplError::Command {
            command: "/logout".to_owned(),
            message: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::login::{LoginError, Opener};
    use crate::cli::testutil;
    use std::sync::{Arc, Mutex as StdMutex};

    /// Builds a service whose sign-in flow returns `credentials` after calling
    /// the opener once, the Rust form of Go's `authLogin` test seam.
    fn flow_returning(credentials: Credentials, url: &'static str) -> impl Fn(PathBuf) -> Service {
        move |path| {
            let credentials = credentials.clone();
            Service::with_flow(path, move |open: &Opener<'_>| {
                let _ = open(url);
                Ok(credentials.clone())
            })
        }
    }

    fn failing_flow(path: PathBuf) -> Service {
        Service::with_flow(path, |_open: &Opener<'_>| Err(LoginError::ExchangeFailed))
    }

    fn credentials(account: &str) -> Credentials {
        Credentials {
            account_id: account.to_owned(),
            ..Credentials::default()
        }
    }

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|value| (*value).to_owned()).collect()
    }

    async fn cli(
        home: &Path,
        args: &[&str],
        build: &(dyn Fn(PathBuf) -> Service + Sync),
    ) -> (i32, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = auth_command(
            &words(args),
            &mut stdout,
            &mut stderr,
            &home.to_string_lossy(),
            &CancellationToken::new(),
            build,
        )
        .await;
        (
            code,
            String::from_utf8(stdout).expect("stdout"),
            String::from_utf8(stderr).expect("stderr"),
        )
    }

    /// A controller whose captured auth path, provider and boundary state the
    /// test chooses. The runtime stays `openai-compatible` so no provider
    /// client is ever built; only the reported provider changes.
    async fn controller_with(
        workspace: &Path,
        sessions: &Path,
        auth_path: &Path,
        provider: &str,
        dynamic: bool,
    ) -> Controller {
        let mut builder = testutil::builder(workspace, sessions);
        builder.auth_path = auth_path.to_string_lossy().into_owned();
        let runtime = testutil::initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        let mut info = builder.runtime_info(&runtime);
        info.provider = provider.to_owned();
        Controller::new(builder, dynamic, session, runner, info)
    }

    async fn repl_call(
        controller: &Controller,
        args: &str,
        build: &(dyn Fn(PathBuf) -> Service + Sync),
    ) -> (String, String, Result<(), ReplError>) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = repl_login_with(
            controller,
            &mut stdout,
            &mut stderr,
            args,
            &CancellationToken::new(),
            build,
        )
        .await;
        (
            String::from_utf8(stdout).expect("stdout"),
            String::from_utf8(stderr).expect("stderr"),
            result,
        )
    }

    /// Port of `TestLoginStatusNotSignedIn`.
    #[tokio::test]
    async fn login_status_reports_that_no_credentials_are_stored() {
        let home = tempfile::tempdir().expect("home");
        let (code, stdout, _) = cli(home.path(), &["login", "--status"], &Service::new).await;
        assert_ne!(code, 0);
        assert!(stdout.contains("Not signed in"), "{stdout}");
    }

    /// Port of `TestLoginStatusSignedIn`.
    #[tokio::test]
    async fn login_status_reports_a_stored_credential_without_the_account_id() {
        let home = tempfile::tempdir().expect("home");
        credentials("acct-xyz")
            .save(&auth::path_for_home(home.path()))
            .expect("save");
        let (code, stdout, stderr) = cli(home.path(), &["login", "--status"], &Service::new).await;
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains(SIGNED_IN), "{stdout}");
        assert!(!stdout.contains("acct-xyz"), "{stdout}");
    }

    /// Port of `TestLogoutRemovesCredentials`.
    #[tokio::test]
    async fn logout_removes_the_credential_file() {
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        credentials("acct-xyz").save(&path).expect("save");
        let (code, stdout, stderr) = cli(home.path(), &["logout"], &Service::new).await;
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains(SIGNED_OUT), "{stdout}");
        assert!(!path.exists(), "credential file still present");
    }

    /// Port of `TestLogoutWhenNotSignedIn`.
    #[tokio::test]
    async fn logout_without_credentials_reports_that_none_were_stored() {
        let home = tempfile::tempdir().expect("home");
        let (code, stdout, stderr) = cli(home.path(), &["logout"], &Service::new).await;
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.contains(NOT_SIGNED_IN), "{stdout}");
    }

    /// Port of `TestLoginSavesCredentials`.
    #[tokio::test]
    async fn login_saves_the_credentials_the_flow_returned() {
        let home = tempfile::tempdir().expect("home");
        let mut saved = credentials("acct-new");
        saved.access_token = "tok".to_owned();
        saved.refresh_token = "ref".to_owned();
        let build = flow_returning(saved, "https://auth.example/authorize?cli=1");
        let (code, stdout, stderr) = cli(home.path(), &["login"], &build).await;
        assert_eq!(code, 0, "{stderr}");
        let loaded = auth::load(&auth::path_for_home(home.path())).expect("load");
        assert_eq!(loaded.account_id, "acct-new");
        assert_eq!(loaded.access_token, "tok");
        assert!(stdout.contains(SIGNED_IN), "{stdout}");
        assert!(
            stdout.contains("https://auth.example/authorize?cli=1"),
            "{stdout}"
        );
        assert!(!stdout.contains("acct-new"), "{stdout}");
        assert!(!stdout.contains("tok\n"), "{stdout}");
    }

    /// Port of `TestLoginFailureIsBounded`.
    #[tokio::test]
    async fn a_login_failure_reports_only_the_fixed_message() {
        let home = tempfile::tempdir().expect("home");
        let (code, _, stderr) = cli(home.path(), &["login"], &failing_flow).await;
        assert_ne!(code, 0);
        assert_eq!(
            stderr,
            format!("otto: login: {}\n", AuthError::LoginFailed),
            "{stderr}"
        );
        assert!(!stderr.contains("exchange"), "{stderr}");
    }

    /// Port of `TestLogoutFailureIsBounded`: the credential path is a
    /// non-empty directory, so removal fails with an OS error that must not
    /// reach stderr.
    #[tokio::test]
    async fn a_logout_failure_reports_only_the_fixed_message() {
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        std::fs::create_dir_all(&path).expect("dir");
        std::fs::write(path.join("child"), b"x").expect("child");
        let (code, _, stderr) = cli(home.path(), &["logout"], &Service::new).await;
        assert_ne!(code, 0);
        assert_eq!(
            stderr,
            format!("otto: {}\n", AuthError::CredentialsRemoval),
            "{stderr}"
        );
        assert!(
            !stderr.contains(&path.to_string_lossy().into_owned()),
            "{stderr}"
        );
    }

    /// Go's `flag.ContinueOnError` returns exit code 2 for an unknown flag.
    #[tokio::test]
    async fn an_unknown_flag_exits_two() {
        let home = tempfile::tempdir().expect("home");
        let (code, _, stderr) = cli(home.path(), &["login", "--nope"], &Service::new).await;
        assert_eq!(code, 2);
        assert!(stderr.contains("--nope"), "{stderr}");
    }

    /// Port of `TestREPLLoginStatusReportsNotSignedIn`.
    #[tokio::test]
    async fn the_repl_reports_no_stored_credential() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        let controller =
            controller_with(workspace.path(), sessions.path(), &path, "chatgpt", true).await;
        let (stdout, _, result) = repl_call(&controller, "status", &Service::new).await;
        assert!(result.is_ok(), "{:?}", result.err());
        assert!(stdout.contains("Not signed in"), "{stdout}");
    }

    /// Port of `TestREPLLoginStatusReportsSignedIn`.
    #[tokio::test]
    async fn the_repl_reports_a_stored_credential_without_the_account_id() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        credentials("acct-9").save(&path).expect("save");
        let controller =
            controller_with(workspace.path(), sessions.path(), &path, "chatgpt", true).await;
        let (stdout, _, result) = repl_call(&controller, "status", &Service::new).await;
        assert!(result.is_ok(), "{:?}", result.err());
        assert!(stdout.contains(SIGNED_IN), "{stdout}");
        assert!(!stdout.contains("acct-9"), "{stdout}");
    }

    /// Port of `TestREPLLoginSavesCredentials`.
    #[tokio::test]
    async fn the_repl_login_saves_credentials_and_shows_the_url() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        let controller =
            controller_with(workspace.path(), sessions.path(), &path, "chatgpt", true).await;
        let mut saved = credentials("acct-7");
        saved.access_token = "secret-token".to_owned();
        let url = "https://auth.example/authorize?repl=1";
        let build = flow_returning(saved, url);
        let (stdout, _, result) = repl_call(&controller, "", &build).await;
        assert!(result.is_ok(), "{:?}", result.err());
        assert!(stdout.contains(REPL_SIGNED_IN), "{stdout}");
        assert!(stdout.contains(url), "{stdout}");
        assert!(!stdout.contains("acct-7"), "{stdout}");
        assert!(!stdout.contains("secret-token"), "{stdout}");
        assert_eq!(auth::load(&path).expect("load").account_id, "acct-7");
        assert!(
            launched(url),
            "the browser launcher did not receive the url"
        );
    }

    /// Port of `TestREPLLoginNonChatGPTProviderExplainsAPIKey`.
    #[tokio::test]
    async fn the_repl_explains_that_a_non_chatgpt_provider_uses_an_api_key() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        let controller = controller_with(
            workspace.path(),
            sessions.path(),
            &path,
            "openai-compatible",
            true,
        )
        .await;
        let (stdout, _, result) = repl_call(&controller, "", &failing_flow).await;
        assert!(result.is_ok(), "{:?}", result.err());
        assert!(stdout.contains("API key"), "{stdout}");
        assert!(!path.exists(), "the refused command still wrote a file");
    }

    /// Port of `TestREPLLogoutRemovesCredentials`.
    #[tokio::test]
    async fn the_repl_logout_removes_credentials() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        credentials("acct-1").save(&path).expect("save");
        let controller = controller_with(
            workspace.path(),
            sessions.path(),
            &path,
            "openai-compatible",
            true,
        )
        .await;
        let mut stdout = Vec::new();
        repl_logout(&controller, &mut stdout, &CancellationToken::new()).expect("logout");
        let stdout = String::from_utf8(stdout).expect("stdout");
        assert!(stdout.contains(SIGNED_OUT), "{stdout}");
        assert!(matches!(auth::load(&path), Err(AuthError::NoCredentials)));
    }

    /// Port of `TestREPLLogoutWhenNotSignedIn`.
    #[tokio::test]
    async fn the_repl_logout_without_credentials_reports_none() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        let controller = controller_with(
            workspace.path(),
            sessions.path(),
            &path,
            "openai-compatible",
            true,
        )
        .await;
        let mut stdout = Vec::new();
        repl_logout(&controller, &mut stdout, &CancellationToken::new()).expect("logout");
        assert!(
            String::from_utf8(stdout)
                .expect("stdout")
                .contains(NOT_SIGNED_IN),
            "missing the not-signed-in line"
        );
    }

    /// Port of `TestREPLLoginFailureIsBounded`.
    #[tokio::test]
    async fn a_repl_login_failure_reports_only_the_fixed_message() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        let controller =
            controller_with(workspace.path(), sessions.path(), &path, "chatgpt", true).await;
        let (_, _, result) = repl_call(&controller, "", &failing_flow).await;
        let error = result.expect_err("login");
        assert!(super::super::repl::is_command_error(&error, "/login"));
        assert_eq!(error.to_string(), AuthError::LoginFailed.to_string());
    }

    /// Port of `TestREPLLoginCommandsUnavailableWhenDynamicContentIsSuppressed`
    /// and of `TestRunSuppressedREPLAuthCommandsStayUnavailable`.
    #[tokio::test]
    async fn a_closed_boundary_makes_every_repl_auth_command_unavailable() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        credentials("acct-1").save(&path).expect("save");
        let controller =
            controller_with(workspace.path(), sessions.path(), &path, "chatgpt", false).await;
        let unavailable = AuthError::InteractiveUnavailable.to_string();

        let (status_out, _, status) = repl_call(&controller, "status", &failing_flow).await;
        let (login_out, _, login) = repl_call(&controller, "", &failing_flow).await;
        let mut logout_out = Vec::new();
        let logout = repl_logout(&controller, &mut logout_out, &CancellationToken::new());
        let logout_out = String::from_utf8(logout_out).expect("stdout");

        assert!(status.is_ok() && login.is_ok() && logout.is_ok());
        for line in [&status_out, &login_out, &logout_out] {
            assert_eq!(line.trim_end(), unavailable, "{line}");
        }
        assert!(auth::load(&path).is_ok(), "credentials were mutated");
    }

    /// Go prints the usage line for any other `/login` argument.
    #[tokio::test]
    async fn an_unexpected_login_argument_prints_the_usage_line() {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        let controller =
            controller_with(workspace.path(), sessions.path(), &path, "chatgpt", true).await;
        let (stdout, stderr, result) = repl_call(&controller, "nonsense", &failing_flow).await;
        assert!(result.is_ok());
        assert!(stdout.is_empty(), "{stdout}");
        assert_eq!(stderr, format!("{LOGIN_USAGE}\n"));
    }

    /// The `/login` and `/logout` dispatch arms in `repl.rs` reach this
    /// module, and `/help` lists both commands.
    #[tokio::test]
    async fn the_repl_dispatch_reaches_the_login_commands() {
        #[derive(Clone, Default)]
        struct Buffer(Arc<StdMutex<Vec<u8>>>);
        impl Write for Buffer {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("lock").extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        let controller =
            controller_with(workspace.path(), sessions.path(), &path, "chatgpt", true).await;
        let stdout = Buffer::default();
        let mut repl = super::super::repl::Repl::new(
            &controller,
            Box::new(stdout.clone()),
            Box::new(Buffer::default()),
        );
        let result = repl
            .run(
                std::io::Cursor::new("/help\n/login status\n/logout\n/exit\n".to_owned()),
                &CancellationToken::new(),
            )
            .await;
        assert!(result.is_ok(), "{:?}", result.err());
        let out = String::from_utf8(stdout.0.lock().expect("lock").clone()).expect("stdout");
        assert!(out.contains("/login [status]"), "{out}");
        assert!(out.contains("/logout   sign out"), "{out}");
        assert!(out.contains("Not signed in"), "{out}");
        assert!(out.contains(NOT_SIGNED_IN), "{out}");
    }

    /// Port of the credential half of
    /// `TestRunTypicalOAuthCredentialsKeepSandboxAndSessionPersistenceEnabled`:
    /// a real-sized credential set keeps the boundary open and supplies four
    /// redaction values.
    #[test]
    fn captured_credentials_supply_the_redaction_values() {
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        let stored = Credentials {
            access_token: "a".repeat(1708),
            refresh_token: "r".repeat(196),
            id_token: "i".repeat(1774),
            account_id: "c".repeat(36),
            ..Credentials::default()
        };
        stored.save(&path).expect("save");

        let captured = capture_auth_credentials(&path);

        assert!(captured.loaded && captured.complete);
        assert_eq!(captured.path, path.to_string_lossy());
        assert_eq!(captured.credentials.account_id, stored.account_id);
        for value in [
            &stored.access_token,
            &stored.refresh_token,
            &stored.id_token,
            &stored.account_id,
        ] {
            assert!(
                captured.redaction_values.iter().any(|held| held == value),
                "a credential value is missing from the redaction set"
            );
        }
    }

    #[test]
    fn a_missing_credential_file_leaves_the_boundary_open() {
        let home = tempfile::tempdir().expect("home");
        let captured = capture_auth_credentials(&auth::path_for_home(home.path()));
        assert!(captured.complete && !captured.loaded);
        assert!(captured.redaction_values.is_empty());
    }

    #[test]
    fn an_unreadable_credential_file_closes_the_boundary() {
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, b"{ not json").expect("write");
        let captured = capture_auth_credentials(&path);
        assert!(!captured.complete && !captured.loaded);
    }

    #[test]
    fn an_empty_path_closes_the_boundary() {
        let captured = capture_auth_credentials(Path::new(""));
        assert!(!captured.complete && !captured.loaded);
    }

    #[test]
    fn a_credential_value_over_the_exact_guard_bound_closes_the_boundary() {
        let home = tempfile::tempdir().expect("home");
        let path = auth::path_for_home(home.path());
        Credentials {
            access_token: "a".repeat(MAX_EXACT_GUARD_VALUE_BYTES + 1),
            ..Credentials::default()
        }
        .save(&path)
        .expect("save");
        let captured = capture_auth_credentials(&path);
        assert!(!captured.complete && !captured.loaded);
    }

    #[test]
    fn the_chatgpt_client_needs_loaded_credentials_and_a_path() {
        let stored = credentials("acct-1");
        assert_eq!(
            chatgpt_client("/tmp/x.json", &stored, false).err(),
            Some(AuthError::NoCredentials.to_string())
        );
        assert_eq!(
            chatgpt_client("   ", &stored, true).err(),
            Some(AuthError::CredentialsUnavailable.to_string())
        );
        assert!(chatgpt_client("/tmp/x.json", &stored, true).is_ok());
    }
}
