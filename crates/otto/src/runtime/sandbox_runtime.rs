//! Opening the process sandbox and classifying the result.
//!
//! Everything the rest of the binary needs to know about confinement is decided
//! here: which driver ran, whether `bash` may be offered at all, which values
//! the redactor must hide, and whether that redaction set is provably complete.
//!
//! Fail-closed is the rule. Every failure path produces an unavailable runtime
//! with no executor and no environment rather than a usable one with a warning,
//! and [`normalize_sandbox_runtime`] re-checks that invariant so a runtime can
//! never claim `bash` it cannot actually run safely.
//!
//! ponytail: there is no dependency-injection struct for the driver. The tests
//! exercise the same paths with the real `direct` driver (offline, no child
//! process at open time) and the end-to-end test covers Seatbelt for real.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto_core::config::sandbox::{
    SandboxDriverMode, SandboxNetworkMode, SandboxSettings as CoreSettings,
};
use tokio_util::sync::CancellationToken;

use crate::sandbox::direct::DirectDriver;
use crate::sandbox::read_denylist::ReadDenylist;
#[cfg(target_os = "macos")]
use crate::sandbox::seatbelt::{Options as SeatbeltOptions, SeatbeltDriver};
use crate::sandbox::{
    Driver, DriverMode, Error, Executor, FilesystemMode, NetworkMode, Policy, Settings,
};

use super::info::{SandboxInfo, SandboxMode, SandboxNetwork, SandboxReason};

mod child_environment;
use child_environment::{resolve_child, resolve_host};

/// A sandbox that could not be shut down cleanly. The specific cause is
/// deliberately not carried, because it can name host paths the model must
/// never see.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("sandbox runtime cleanup failed")]
pub struct CloseError;

/// What [`open_sandbox_runtime`] needs to establish confinement.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    pub settings: Settings,
    pub workspace: String,
    pub shell: String,
    pub home: String,
    /// The host environment as captured, byte for byte. Entries need not be
    /// valid UTF-8; the environment resolver classifies them as bytes.
    pub host_entries: Vec<Vec<u8>>,
    /// The environment variable names that hold provider API keys. Their
    /// values are redacted whether or not they reach the child.
    pub provider_names: Vec<String>,
    /// Canonical roots of Git-linked worktrees sharing the selected workspace.
    /// They are discovered by the host before opening Seatbelt, never sourced
    /// from model-controlled command input or persistent read grants.
    pub managed_worktrees: Vec<String>,
}

/// What closing the runtime has to do.
enum Closer {
    Nothing,
    /// Closing the executor closes the driver it holds.
    Executor(Arc<Executor>),
    /// A cleanup during a failed open already failed; report it once.
    AlreadyFailed,
}

/// The outcome of one sandbox open.
///
/// `executor` and `environment` are both `Some` exactly when
/// `info.bash_available` is true; [`normalize_sandbox_runtime`] enforces it.
pub struct SandboxRuntime {
    pub executor: Option<Arc<Executor>>,
    pub environment: Option<Vec<String>>,
    pub info: SandboxInfo,
    pub redaction_values: Vec<String>,
    /// When false the caller must suppress child output entirely: a value the
    /// redactor does not know about would otherwise reach the model verbatim.
    pub redactions_complete: bool,
    /// The read denylist the Seatbelt profile enforces. `None` for every
    /// other driver and for an unusable runtime.
    pub read_denylist: Option<ReadDenylist>,
    closer: Closer,
    /// The Seatbelt driver behind `executor`, kept so a reload can rewrite its
    /// profile in place. `None` for every other driver and for an unusable
    /// runtime.
    #[cfg(target_os = "macos")]
    seatbelt: Option<Arc<SeatbeltDriver>>,
}

/// Why [`SandboxRuntime::reconfigure`] did not apply the new settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconfigureError {
    /// The new settings resolve to a different child environment or redaction
    /// set. Consumers hold the old environment by value, so it cannot change.
    EnvironmentChanged,
    Failed(SandboxReason),
}

impl SandboxRuntime {
    /// Applies new `read_paths` and `network` to the current Seatbelt driver
    /// without opening a new one.
    ///
    /// The driver keeps its private state tree, so the child environment and
    /// redaction values stay byte-identical and consumers that copied them at
    /// build time remain valid. A change that alters either is refused with
    /// [`ReconfigureError::EnvironmentChanged`] before the driver is touched.
    ///
    /// `None` means this does not apply: the runtime has no Seatbelt driver, or
    /// `options` asks for another driver. The caller opens a replacement
    /// runtime instead. The caller must hold off every command for the whole
    /// call; see [`SeatbeltDriver::reconfigure`].
    #[cfg(target_os = "macos")]
    pub async fn reconfigure(
        &mut self,
        options: &OpenOptions,
        cancel: &CancellationToken,
    ) -> Option<Result<(), ReconfigureError>> {
        let driver = Arc::clone(self.seatbelt.as_ref()?);
        let network = options.settings.network?;
        if options.settings.driver == DriverMode::Off {
            return None;
        }
        Some(
            self.reconfigure_driver(&driver, options, network, cancel)
                .await,
        )
    }

    /// See the macOS definition above.
    #[cfg(not(target_os = "macos"))]
    pub async fn reconfigure(
        &mut self,
        _options: &OpenOptions,
        _cancel: &CancellationToken,
    ) -> Option<Result<(), ReconfigureError>> {
        None
    }

    #[cfg(target_os = "macos")]
    async fn reconfigure_driver(
        &mut self,
        driver: &Arc<SeatbeltDriver>,
        options: &OpenOptions,
        network: NetworkMode,
        cancel: &CancellationToken,
    ) -> Result<(), ReconfigureError> {
        use ReconfigureError::{EnvironmentChanged, Failed};

        if cancel.is_cancelled() {
            return Err(Failed(SandboxReason::RuntimeFailure));
        }
        let host = resolve_host(options);
        let child = resolve_child(
            options,
            Some(driver.private_directories()),
            &host.redactions,
            host.complete,
        );
        if !host.usable || !child.usable || !child.complete {
            return Err(Failed(SandboxReason::EnvironmentRejected));
        }
        if child.entries != self.environment || child.redactions != self.redaction_values {
            return Err(EnvironmentChanged);
        }

        let previous = (
            driver.read_paths(),
            driver.deny_read_paths(),
            driver.workspaces(),
            driver.network().unwrap_or(NetworkMode::Deny),
        );
        if let Err(error) = driver
            .reconfigure(
                options.settings.read_paths.clone(),
                options.settings.deny_read_paths.clone(),
                options.managed_worktrees.clone(),
                network,
                cancel,
            )
            .await
        {
            return Err(Failed(reconfigure_reason(&error, cancel)));
        }
        let workspace = self
            .executor
            .as_ref()
            .map(|executor| executor.workspace().to_path_buf())
            .unwrap_or_default();
        let policy = Policy {
            filesystem: FilesystemMode::WorkspaceWrite,
            network,
        };
        let executor =
            match Executor::new(Arc::clone(driver) as Arc<dyn Driver>, policy, &workspace) {
                Ok(executor) => Arc::new(executor),
                Err(error) => {
                    let reason = executor_reason(&error);
                    // The driver already runs the new profile; restore the old one
                    // so it matches the executor that stays in service.
                    let _ = driver
                        .reconfigure(
                            previous.0,
                            previous.1,
                            previous.2,
                            previous.3,
                            &CancellationToken::new(),
                        )
                        .await;
                    return Err(Failed(reason));
                }
            };
        self.info = seatbelt_sandbox_info(
            network,
            driver.inert_read_paths(),
            driver.skipped_deny_paths(),
        );
        self.read_denylist = Some(ReadDenylist::new(
            &options.home,
            options.settings.deny_read_paths.clone(),
        ));
        self.closer = Closer::Executor(Arc::clone(&executor));
        // The replaced executor is dropped without `close`: closing it would
        // close the driver both executors share and delete the private tree.
        self.executor = Some(executor);
        Ok(())
    }

    /// Shuts the sandbox down. Idempotent: [`Executor::close`] runs the
    /// driver's close once and returns the same result to every caller.
    pub fn close(&self) -> Result<(), CloseError> {
        match &self.closer {
            Closer::Nothing => Ok(()),
            Closer::Executor(executor) => executor.close().map_err(|_| CloseError),
            Closer::AlreadyFailed => Err(CloseError),
        }
    }
}

/// Converts resolved configuration into the native sandbox settings.
///
/// The two types are field-for-field equivalents that cannot be shared:
/// `otto-core` must build for wasm32 and so cannot depend on the crate that
/// owns the executor.
pub fn settings_from_config(resolved: &CoreSettings) -> Settings {
    Settings {
        driver: match resolved.driver {
            SandboxDriverMode::Auto => DriverMode::Auto,
            SandboxDriverMode::Seatbelt => DriverMode::Seatbelt,
            SandboxDriverMode::Off => DriverMode::Off,
        },
        network: Some(match resolved.network {
            SandboxNetworkMode::Deny => NetworkMode::Deny,
            SandboxNetworkMode::Allow => NetworkMode::Allow,
        }),
        read_paths: resolved.read_paths.clone(),
        deny_read_paths: resolved.deny_read_paths.clone(),
        allow_env: resolved.allow_env.clone(),
    }
}

/// Opens the sandbox described by `options`.
///
/// Never returns an error: an unopenable sandbox is a runtime with
/// `bash_available` false and a reason, so the binary keeps running with file
/// tools only. The caller prints a warning and continues.
pub async fn open_sandbox_runtime(
    options: &OpenOptions,
    cancel: &CancellationToken,
) -> SandboxRuntime {
    if cancel.is_cancelled() {
        return unavailable(SandboxReason::RuntimeFailure, Vec::new(), false);
    }
    if options.settings.network.is_none() {
        return unavailable(SandboxReason::PolicyUnsupported, Vec::new(), false);
    }

    // Classify the host environment before opening anything: a host that
    // cannot be classified safely must not reach a child at all, and the
    // partial redaction set is still worth keeping.
    let host = resolve_host(options);
    if !host.usable {
        return unavailable(
            SandboxReason::EnvironmentRejected,
            host.redactions,
            host.complete,
        );
    }
    let (host_redactions, host_complete) = (host.redactions, host.complete);
    if cancel.is_cancelled() {
        return unavailable(
            SandboxReason::RuntimeFailure,
            host_redactions,
            host_complete,
        );
    }

    match options.settings.driver {
        DriverMode::Auto | DriverMode::Seatbelt => {
            open_confined(options, host_redactions, host_complete, cancel).await
        }
        DriverMode::Off => open_direct(options, host_redactions, host_complete, cancel).await,
    }
}

/// The confined driver `auto` and `seatbelt` ask for. Seatbelt is the only
/// one Otto has, so every other target reports the mode as unsupported and
/// the runtime fails closed with no `bash` tool; `--sandbox off` is the
/// explicit, and only, way to run commands there.
#[cfg(target_os = "macos")]
async fn open_confined(
    options: &OpenOptions,
    host_redactions: Vec<String>,
    host_complete: bool,
    cancel: &CancellationToken,
) -> SandboxRuntime {
    open_seatbelt(options, host_redactions, host_complete, cancel).await
}

/// See the macOS definition above.
#[cfg(not(target_os = "macos"))]
async fn open_confined(
    _options: &OpenOptions,
    host_redactions: Vec<String>,
    host_complete: bool,
    _cancel: &CancellationToken,
) -> SandboxRuntime {
    unavailable(
        SandboxReason::UnsupportedPlatform,
        host_redactions,
        host_complete,
    )
}

#[cfg(target_os = "macos")]
async fn open_seatbelt(
    options: &OpenOptions,
    host_redactions: Vec<String>,
    host_complete: bool,
    cancel: &CancellationToken,
) -> SandboxRuntime {
    let Some(network) = options.settings.network else {
        return unavailable(
            SandboxReason::PolicyUnsupported,
            host_redactions,
            host_complete,
        );
    };
    let Ok(workspace) = canonical_directory(Path::new(&options.workspace)) else {
        return unavailable(
            SandboxReason::PolicyUnsupported,
            host_redactions,
            host_complete,
        );
    };
    let Ok(shell) = canonical_executable_file(Path::new(&options.shell)) else {
        return unavailable(SandboxReason::InvalidShell, host_redactions, host_complete);
    };
    if cancel.is_cancelled() {
        return unavailable(
            SandboxReason::RuntimeFailure,
            host_redactions,
            host_complete,
        );
    }

    let driver = SeatbeltDriver::open(
        SeatbeltOptions {
            workspace: workspace.to_string_lossy().into_owned(),
            shell: shell.to_string_lossy().into_owned(),
            home: options.home.clone(),
            cache_base: Path::new(&options.home)
                .join("Library")
                .join("Caches")
                .to_string_lossy()
                .into_owned(),
            read_paths: options.settings.read_paths.clone(),
            deny_read_paths: options.settings.deny_read_paths.clone(),
            workspaces: options.managed_worktrees.clone(),
            network: Some(network),
        },
        cancel,
    )
    .await;
    let driver = match driver {
        Ok(driver) => driver,
        Err(error) => {
            let reason = if cancel.is_cancelled() {
                SandboxReason::RuntimeFailure
            } else {
                open_reason(&error)
            };
            return unavailable(reason, host_redactions, host_complete);
        }
    };
    if cancel.is_cancelled() {
        let cleanup = driver.close();
        return after_cleanup(
            SandboxReason::RuntimeFailure,
            host_redactions,
            host_complete,
            cleanup,
        );
    }

    let child = resolve_child(
        options,
        Some(driver.private_directories()),
        &host_redactions,
        host_complete,
    );
    if !child.usable || cancel.is_cancelled() {
        let cleanup = driver.close();
        let reason = if cancel.is_cancelled() {
            SandboxReason::RuntimeFailure
        } else {
            SandboxReason::EnvironmentRejected
        };
        return after_cleanup(reason, child.redactions, child.complete, cleanup);
    }

    let policy = Policy {
        filesystem: FilesystemMode::WorkspaceWrite,
        network,
    };
    let driver = Arc::new(driver);
    let mut runtime = finish(
        Arc::clone(&driver) as Arc<dyn Driver>,
        policy,
        &workspace,
        seatbelt_sandbox_info(
            network,
            driver.inert_read_paths(),
            driver.skipped_deny_paths(),
        ),
        child.entries,
        child.redactions,
        child.complete,
        cancel,
    );
    if runtime.executor.is_some() {
        runtime.seatbelt = Some(driver);
        runtime.read_denylist = Some(ReadDenylist::new(
            &options.home,
            options.settings.deny_read_paths.clone(),
        ));
    }
    runtime
}

async fn open_direct(
    options: &OpenOptions,
    host_redactions: Vec<String>,
    host_complete: bool,
    cancel: &CancellationToken,
) -> SandboxRuntime {
    let Ok(workspace) = canonical_directory(Path::new(&options.workspace)) else {
        return unavailable(
            SandboxReason::PolicyUnsupported,
            host_redactions,
            host_complete,
        );
    };
    // The shell is validated even unconfined: bash still executes it, and an
    // unresolvable or non-executable path is a configuration error either way.
    if canonical_executable_file(Path::new(&options.shell)).is_err() {
        return unavailable(SandboxReason::InvalidShell, host_redactions, host_complete);
    }
    if cancel.is_cancelled() {
        return unavailable(
            SandboxReason::RuntimeFailure,
            host_redactions,
            host_complete,
        );
    }

    let child = resolve_child(options, None, &host_redactions, host_complete);
    if !child.usable {
        return unavailable(
            SandboxReason::EnvironmentRejected,
            child.redactions,
            child.complete,
        );
    }
    if cancel.is_cancelled() {
        return unavailable(
            SandboxReason::RuntimeFailure,
            child.redactions,
            child.complete,
        );
    }

    let policy = Policy {
        filesystem: FilesystemMode::Unconfined,
        network: NetworkMode::Allow,
    };
    let info = SandboxInfo {
        mode: SandboxMode::Off,
        network: SandboxNetwork::Unconfined,
        bash_available: true,
        reason: SandboxReason::None,
        inert_read_paths: Vec::new(),
        skipped_deny_paths: Vec::new(),
    };
    finish(
        Arc::new(DirectDriver::new()) as Arc<dyn Driver>,
        policy,
        &workspace,
        info,
        child.entries,
        child.redactions,
        child.complete,
        cancel,
    )
}

/// Binds the driver to an executor, or closes it and reports why not.
#[allow(clippy::too_many_arguments)]
fn finish(
    driver: Arc<dyn Driver>,
    policy: Policy,
    workspace: &Path,
    info: SandboxInfo,
    environment: Option<Vec<String>>,
    redactions: Vec<String>,
    complete: bool,
    cancel: &CancellationToken,
) -> SandboxRuntime {
    let executor = match Executor::new(Arc::clone(&driver), policy, workspace) {
        Ok(executor) => executor,
        Err(error) => {
            let cleanup = driver.close();
            let reason = if cancel.is_cancelled() {
                SandboxReason::RuntimeFailure
            } else {
                executor_reason(&error)
            };
            return after_cleanup(reason, redactions, complete, cleanup);
        }
    };
    let executor = Arc::new(executor);
    if cancel.is_cancelled() {
        let cleanup = executor.close();
        return after_cleanup(SandboxReason::RuntimeFailure, redactions, complete, cleanup);
    }
    SandboxRuntime {
        executor: Some(Arc::clone(&executor)),
        environment,
        info,
        redaction_values: redactions,
        redactions_complete: complete,
        read_denylist: None,
        closer: Closer::Executor(executor),
        #[cfg(target_os = "macos")]
        seatbelt: None,
    }
}

/// Fails a runtime closed when it claims `bash` but cannot run it safely.
///
/// Incomplete redactions would leak secrets into command output, and a
/// missing executor or environment leaves nothing to run. Startup and every
/// later reload classify runtimes here so they agree on what "usable" means.
pub fn normalize_sandbox_runtime(mut runtime: SandboxRuntime) -> SandboxRuntime {
    let reason = if !runtime.redactions_complete {
        SandboxReason::EnvironmentRejected
    } else if runtime.info.bash_available
        && (runtime.executor.is_none() || runtime.environment.is_none())
    {
        SandboxReason::RuntimeFailure
    } else {
        return runtime;
    };
    runtime.info = SandboxInfo {
        mode: SandboxMode::Unavailable,
        network: SandboxNetwork::default(),
        bash_available: false,
        reason,
        inert_read_paths: Vec::new(),
        skipped_deny_paths: Vec::new(),
    };
    runtime.executor = None;
    runtime.environment = None;
    runtime.read_denylist = None;
    #[cfg(target_os = "macos")]
    {
        runtime.seatbelt = None;
    }
    runtime
}

/// The warning printed once at startup when the sandbox is not what the user
/// would assume. A confined runtime prints nothing.
pub fn sandbox_runtime_warning(info: &SandboxInfo) -> Option<String> {
    match info.mode {
        SandboxMode::Unavailable => Some(format!(
            "warning: bash is unavailable because the configured sandbox could not be established (reason: {}); file tools remain available\n",
            safe_sandbox_reason(info.reason).as_str()
        )),
        SandboxMode::Off
            if info.network == SandboxNetwork::Unconfined
                && info.bash_available
                && info.reason == SandboxReason::None =>
        {
            Some("warning: sandbox is off; bash runs unsandboxed as your user\n".to_string())
        }
        _ => None,
    }
}

/// The runtime for a host that switched bash off: no executor, bash
/// unavailable, and a redaction set that is complete because nothing opened.
pub(crate) fn policy_disabled_runtime() -> SandboxRuntime {
    unavailable(SandboxReason::PolicyUnsupported, Vec::new(), true)
}

fn unavailable(reason: SandboxReason, redactions: Vec<String>, complete: bool) -> SandboxRuntime {
    SandboxRuntime {
        executor: None,
        environment: None,
        info: SandboxInfo {
            mode: SandboxMode::Unavailable,
            network: SandboxNetwork::default(),
            bash_available: false,
            reason: safe_sandbox_reason(reason),
            inert_read_paths: Vec::new(),
            skipped_deny_paths: Vec::new(),
        },
        redaction_values: redactions,
        redactions_complete: complete,
        read_denylist: None,
        closer: Closer::Nothing,
        #[cfg(target_os = "macos")]
        seatbelt: None,
    }
}

fn after_cleanup(
    reason: SandboxReason,
    redactions: Vec<String>,
    complete: bool,
    cleanup: Result<(), Error>,
) -> SandboxRuntime {
    let mut runtime = unavailable(reason, redactions, complete);
    if cleanup.is_err() {
        runtime.closer = Closer::AlreadyFailed;
    }
    runtime
}

#[cfg(target_os = "macos")]
fn seatbelt_sandbox_info(
    network: NetworkMode,
    inert_read_paths: Vec<String>,
    skipped_deny_paths: Vec<String>,
) -> SandboxInfo {
    SandboxInfo {
        mode: SandboxMode::Seatbelt,
        network: match network {
            NetworkMode::Allow => SandboxNetwork::Allowed,
            NetworkMode::Deny => SandboxNetwork::Denied,
        },
        bash_available: true,
        reason: SandboxReason::None,
        inert_read_paths,
        skipped_deny_paths,
    }
}

fn open_reason(error: &Error) -> SandboxReason {
    match error {
        Error::Unavailable(reason) => SandboxReason::from(*reason),
        Error::UnsupportedPolicy => SandboxReason::PolicyUnsupported,
        _ => SandboxReason::RuntimeFailure,
    }
}

#[cfg(target_os = "macos")]
fn reconfigure_reason(error: &Error, cancel: &CancellationToken) -> SandboxReason {
    if cancel.is_cancelled() {
        SandboxReason::RuntimeFailure
    } else {
        open_reason(error)
    }
}

fn executor_reason(error: &Error) -> SandboxReason {
    open_reason(error)
}

/// Keeps an out-of-range reason from reaching a user-visible string. Rust's
/// enum already bounds the value, so this only maps the `None` placeholder
/// that an unavailable runtime must never carry.
fn safe_sandbox_reason(reason: SandboxReason) -> SandboxReason {
    match reason {
        SandboxReason::None => SandboxReason::RuntimeFailure,
        reason => reason,
    }
}

/// Resolves `path` to an existing directory with every symlink followed.
pub fn canonical_directory(path: &Path) -> Result<PathBuf, std::io::Error> {
    let canonical = std::fs::canonicalize(path)?;
    if !std::fs::metadata(&canonical)?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("not a directory: {}", canonical.display()),
        ));
    }
    Ok(canonical)
}

/// Resolves `path` to an existing, regular, executable file.
///
/// The final component is re-checked with `lstat` after canonicalization so a
/// symlink swapped in between the two calls cannot pass as the shell.
pub fn canonical_executable_file(path: &Path) -> Result<PathBuf, Error> {
    use std::os::unix::fs::PermissionsExt;

    let Some(text) = path.to_str() else {
        return Err(Error::InvalidRequest);
    };
    if text.is_empty() || text.contains('\0') {
        return Err(Error::InvalidRequest);
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| Error::InvalidRequest)?;
    let metadata = std::fs::symlink_metadata(&canonical).map_err(|_| Error::InvalidRequest)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(Error::InvalidRequest);
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests;
