//! The process sandbox switch and the `[sandbox]` reloader.
//!
//! The composition root in [`super::run`] opens one sandbox and hands it to a
//! [`SandboxSwitch`]; the bash tool captures the switch as its executor, so
//! `/sandbox reload`, `POST /v1/sandbox/reload` and the TUI all replace the
//! runtime underneath a live session instead of restarting the process.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kite_core::config::File;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::app::SandboxControl;
use crate::sandbox::{CommandExecutor, Error as SandboxError, ExitStatus, Request, Streams};

use super::info::SandboxInfo;
use super::sandbox_runtime::{
    CloseError, OpenOptions, SandboxRuntime, normalize_sandbox_runtime, open_sandbox_runtime,
    settings_from_config,
};

const RELOAD_UNAVAILABLE: &str = "sandbox reload requires a usable sandbox; restart kite";
const RELOAD_ENVIRONMENT: &str = "sandbox reload cannot apply allow_env changes; restart kite";
const RELOAD_FAILED: &str = "sandbox reload failed";

// ---- the sandbox switch ----

/// Owns the process sandbox runtime behind a stable [`CommandExecutor`].
///
/// The bash tool captures its executor when a runner is built, so replacing
/// the runtime here applies new sandbox configuration without rebuilding the
/// session, the runner, or the tool set.
///
/// Concurrency: [`SandboxSwitch::execute`] holds the read half for the whole
/// command, so a reload that arrives mid-command waits for it rather than
/// closing the executor underneath it. [`SandboxSwitch::info`] reads a
/// separate mirror instead, because a controller calls it while holding its
/// own lock and must never wait on a reload.
pub struct SandboxSwitch {
    /// `None` once closed.
    current: RwLock<Option<SandboxRuntime>>,
    info: Mutex<SandboxInfo>,
}

impl SandboxSwitch {
    pub fn new(runtime: SandboxRuntime) -> Arc<Self> {
        let info = runtime.info;
        Arc::new(Self {
            current: RwLock::new(Some(runtime)),
            info: Mutex::new(info),
        })
    }

    /// The sandbox state now in effect.
    pub fn info(&self) -> SandboxInfo {
        *self
            .info
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Installs `next` when it can replace the current runtime in place. The
    /// rejected runtime is always closed and the current one left untouched, so
    /// a failed reload leaves bash working exactly as it did before.
    pub async fn reload(&self, next: SandboxRuntime) -> Result<SandboxInfo, String> {
        let mut guard = self.current.write().await;
        let rejected = match guard.as_ref() {
            None => Some(RELOAD_UNAVAILABLE.to_string()),
            Some(current) if !usable(current) => Some(RELOAD_UNAVAILABLE.to_string()),
            Some(_) if !usable(&next) => {
                Some(format!("{RELOAD_FAILED}: {}", reload_reason(&next.info)))
            }
            Some(current)
                if next.environment != current.environment
                    || next.redaction_values != current.redaction_values =>
            {
                Some(RELOAD_ENVIRONMENT.to_string())
            }
            Some(_) => None,
        };
        if let Some(message) = rejected {
            drop(guard);
            let _ = next.close();
            return Err(message);
        }
        let info = next.info;
        let previous = guard.replace(next).expect("a usable current runtime");
        *self
            .info
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = info;
        drop(guard);
        previous.close().map_err(|error| error.to_string())?;
        Ok(info)
    }

    /// Shuts the current runtime down. Idempotent.
    pub async fn close(&self) -> Result<(), CloseError> {
        match self.current.write().await.take() {
            Some(runtime) => runtime.close(),
            None => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl CommandExecutor for SandboxSwitch {
    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), SandboxError>) {
        let guard = self.current.read().await;
        let Some(executor) = guard.as_ref().and_then(|runtime| runtime.executor.as_ref()) else {
            // ponytail: reusing `Error::Closed` keeps the shared sandbox error
            // set untouched for one unreachable-in-practice branch.
            return (ExitStatus::default(), Err(SandboxError::Closed));
        };
        executor.execute(request, streams, cancel).await
    }
}

fn usable(runtime: &SandboxRuntime) -> bool {
    runtime.info.bash_available
        && runtime.executor.is_some()
        && runtime.environment.is_some()
        && runtime.redactions_complete
}

/// Names why a replacement runtime is unusable. An otherwise-available runtime
/// only reaches here with incomplete redactions, which the reason codes report
/// as a runtime failure.
fn reload_reason(info: &SandboxInfo) -> &'static str {
    match info.reason_code() {
        "" => super::info::SandboxReason::RuntimeFailure.as_str(),
        code => code,
    }
}

// ---- the reloader ----

/// Re-reads the configuration file and replaces the process sandbox with the
/// result. Everything except the `[sandbox]` table is fixed at startup: the
/// workspace, shell, home, host environment, and the provider key name the
/// sandbox environment was resolved from.
pub struct SandboxReloader {
    pub(super) control: Arc<SandboxSwitch>,
    pub(super) config_path: PathBuf,
    pub(super) explicit_config: bool,
    pub(super) driver_override: Option<String>,
    pub(super) environment: HashMap<String, String>,
    pub(super) api_key_env: String,
    pub(super) reopen: OpenOptions,
    pub(super) cancel: CancellationToken,
}

#[async_trait::async_trait]
impl SandboxControl for SandboxReloader {
    fn info(&self) -> SandboxInfo {
        self.control.info()
    }

    async fn reload(&self) -> Result<SandboxInfo, String> {
        let file = self.load_config()?;
        let settings = super::run::resolve_sandbox_settings(
            &file,
            &self.environment,
            &self.reopen.workspace,
            self.driver_override.as_deref(),
        )?;
        let next = normalize_sandbox_runtime(
            open_sandbox_runtime(
                &OpenOptions {
                    settings: settings_from_config(&settings),
                    provider_names: super::run::sandbox_provider_environment_names(
                        &file,
                        &self.api_key_env,
                    ),
                    ..self.reopen.clone()
                },
                &self.cancel,
            )
            .await,
        );
        self.control.reload(next).await
    }
}

impl SandboxReloader {
    fn load_config(&self) -> Result<File, String> {
        match crate::config::load_required(&self.config_path) {
            Ok(file) => Ok(file),
            Err(error) if error.is_not_found() && !self.explicit_config => Ok(File::default()),
            Err(_) => Err("load config: configuration is invalid or unavailable".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{DriverMode, NetworkMode, Settings};
    use tempfile::TempDir;

    /// Open options for a real unconfined runtime: the `off` driver opens no
    /// child process, so these tests stay offline and deterministic.
    ///
    /// `GH_TOKEN` is a sensitive name, so it reaches the child only when
    /// `allow_env` names it: that is the one input a reload may not change.
    fn options(home: &TempDir, allow_env: &[&str]) -> OpenOptions {
        let path = home.path().to_string_lossy().into_owned();
        OpenOptions {
            settings: Settings {
                driver: DriverMode::Off,
                network: Some(NetworkMode::Allow),
                read_paths: Vec::new(),
                allow_env: allow_env.iter().map(|name| name.to_string()).collect(),
            },
            workspace: path.clone(),
            shell: "/bin/sh".to_string(),
            home: path,
            host_entries: vec![
                b"PATH=/usr/bin:/bin".to_vec(),
                b"GH_TOKEN=token-value".to_vec(),
            ],
            provider_names: vec!["KITE_API_KEY".to_string()],
        }
    }

    async fn open_runtime(options: &OpenOptions) -> SandboxRuntime {
        normalize_sandbox_runtime(open_sandbox_runtime(options, &CancellationToken::new()).await)
    }

    async fn usable_runtime(home: &TempDir, allow_env: &[&str]) -> SandboxRuntime {
        open_runtime(&options(home, allow_env)).await
    }

    /// A runtime that cannot run bash: an absent workspace is a policy the
    /// driver cannot enforce, and the open fails closed with no executor.
    async fn unavailable_runtime(home: &TempDir) -> SandboxRuntime {
        let mut options = options(home, &[]);
        options.workspace = home.path().join("missing").to_string_lossy().into_owned();
        open_runtime(&options).await
    }

    /// Runs `printf ok` through the switch and returns what the child wrote.
    async fn printf_ok(
        control: &SandboxSwitch,
        home: &TempDir,
    ) -> (Result<(), SandboxError>, String) {
        let mut stdout = Vec::new();
        let (_, result) = control
            .execute(
                Request {
                    argv: vec![
                        "/bin/sh".to_string(),
                        "-c".to_string(),
                        "printf ok".to_string(),
                    ],
                    dir: std::fs::canonicalize(home.path()).expect("canonical home"),
                    env: vec!["PATH=/usr/bin:/bin".to_string()],
                },
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut Vec::new(),
                },
                &CancellationToken::new(),
            )
            .await;
        (result, String::from_utf8_lossy(&stdout).into_owned())
    }

    #[tokio::test]
    async fn commands_run_through_whichever_runtime_is_installed() {
        let home = TempDir::new().expect("home");
        let control = SandboxSwitch::new(usable_runtime(&home, &[]).await);
        assert!(control.info().bash_available);
        let (result, output) = printf_ok(&control, &home).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "ok");

        let info = control
            .reload(usable_runtime(&home, &[]).await)
            .await
            .expect("reload");
        assert_eq!(info, control.info());

        // The replacement is the one now serving; the previous one is closed.
        let (result, output) = printf_ok(&control, &home).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "ok");
        control.close().await.expect("close");
    }

    /// The `allow_env` case and the redaction-value case share one rejection,
    /// and through the real open path one input moves both: a name the child
    /// gains is also a value the redactor must learn, so a single test covers
    /// the pair.
    #[tokio::test]
    async fn a_replacement_that_changes_the_child_environment_is_refused() {
        let home = TempDir::new().expect("home");
        let control = SandboxSwitch::new(usable_runtime(&home, &[]).await);
        let before = control.info();

        let next = usable_runtime(&home, &["GH_TOKEN"]).await;
        let error = control.reload(next).await.expect_err("a refused reload");
        assert_eq!(error, RELOAD_ENVIRONMENT);
        assert_eq!(control.info(), before, "the current runtime is untouched");

        let (result, output) = printf_ok(&control, &home).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "ok", "the previous runtime still serves");
        control.close().await.expect("close");
    }

    #[tokio::test]
    async fn an_unusable_replacement_is_refused_and_names_its_reason() {
        let home = TempDir::new().expect("home");
        let control = SandboxSwitch::new(usable_runtime(&home, &[]).await);
        let before = control.info();

        let error = control
            .reload(unavailable_runtime(&home).await)
            .await
            .expect_err("a refused reload");
        assert_eq!(error, format!("{RELOAD_FAILED}: policy-unsupported"));
        assert_eq!(control.info(), before);
        control.close().await.expect("close");
    }

    #[tokio::test]
    async fn a_reload_needs_a_usable_current_runtime() {
        let home = TempDir::new().expect("home");
        let control = SandboxSwitch::new(unavailable_runtime(&home).await);

        let error = control
            .reload(usable_runtime(&home, &[]).await)
            .await
            .expect_err("a refused reload");
        assert_eq!(error, RELOAD_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_closed_switch_runs_nothing_and_reloads_nothing() {
        let home = TempDir::new().expect("home");
        let control = SandboxSwitch::new(usable_runtime(&home, &[]).await);
        control.close().await.expect("close");
        control.close().await.expect("close is idempotent");

        let (result, _) = printf_ok(&control, &home).await;
        assert!(result.is_err(), "a closed switch must not run commands");

        let error = control
            .reload(usable_runtime(&home, &[]).await)
            .await
            .expect_err("a refused reload");
        assert_eq!(error, RELOAD_UNAVAILABLE);
    }

    fn reloader(home: &TempDir, config: &str, control: Arc<SandboxSwitch>) -> SandboxReloader {
        let path = home.path().join("config.toml");
        std::fs::write(&path, config).expect("write config");
        SandboxReloader {
            control,
            config_path: path,
            explicit_config: true,
            driver_override: Some("off".to_string()),
            environment: HashMap::from([(
                "HOME".to_string(),
                home.path().to_string_lossy().into_owned(),
            )]),
            api_key_env: "KITE_API_KEY".to_string(),
            reopen: options(home, &[]),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn a_reload_reopens_the_sandbox_from_the_configuration_file() {
        let home = TempDir::new().expect("home");
        let control = SandboxSwitch::new(usable_runtime(&home, &[]).await);
        let reloader = reloader(
            &home,
            "[sandbox]\nnetwork = 'allow'\n",
            Arc::clone(&control),
        );

        let info = SandboxControl::reload(&reloader).await.expect("reload");
        assert!(info.bash_available);
        assert_eq!(control.info(), info);

        let (result, output) = printf_ok(&control, &home).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "ok");
        control.close().await.expect("close");
    }

    /// The `[sandbox]` table really reaches the reopened runtime: an
    /// `allow_env` entry that was not there at startup changes the child
    /// environment, which is exactly what a reload may not do.
    #[tokio::test]
    async fn the_sandbox_table_is_re_read_on_every_reload() {
        let home = TempDir::new().expect("home");
        let control = SandboxSwitch::new(usable_runtime(&home, &[]).await);
        let reloader = reloader(
            &home,
            "[sandbox]\nnetwork = 'allow'\nallow_env = ['GH_TOKEN']\n",
            Arc::clone(&control),
        );

        let error = SandboxControl::reload(&reloader)
            .await
            .expect_err("a refused reload");
        assert_eq!(error, RELOAD_ENVIRONMENT);
        control.close().await.expect("close");
    }

    #[tokio::test]
    async fn an_invalid_configuration_leaves_the_sandbox_alone() {
        let home = TempDir::new().expect("home");
        let control = SandboxSwitch::new(usable_runtime(&home, &[]).await);
        let before = control.info();
        let reloader = reloader(
            &home,
            "[sandbox]\nnetwork = 'sometimes'\n",
            Arc::clone(&control),
        );

        SandboxControl::reload(&reloader)
            .await
            .expect_err("an invalid configuration");
        assert_eq!(control.info(), before);

        let (result, output) = printf_ok(&control, &home).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "ok", "the runtime from before the reload serves");
        control.close().await.expect("close");
    }
}
