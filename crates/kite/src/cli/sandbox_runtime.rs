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

use kite_core::config::sandbox::{
    SandboxDriverMode, SandboxNetworkMode, SandboxSettings as CoreSettings,
};
use kite_core::safetext::SecretCollector;
use tokio_util::sync::CancellationToken;

use crate::sandbox::direct::DirectDriver;
use crate::sandbox::environment::{EnvironmentOptions, EnvironmentSnapshot, resolve_environment};
#[cfg(target_os = "macos")]
use crate::sandbox::seatbelt::{Options as SeatbeltOptions, SeatbeltDriver};
use crate::sandbox::{
    Driver, DriverMode, Error, Executor, FilesystemMode, NetworkMode, Policy, Settings,
};

use super::info::{SandboxInfo, SandboxMode, SandboxNetwork, SandboxReason};

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
    closer: Closer,
}

impl SandboxRuntime {
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
/// `kite-core` must build for wasm32 and so cannot depend on the crate that
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
    let host = resolve_environment(&EnvironmentOptions {
        host_entries: options.host_entries.clone(),
        provider_names: options.provider_names.clone(),
        allow_names: options.settings.allow_env.clone(),
        private_directories: None,
    });
    let (host_snapshot, host_ok) = match &host {
        Ok(snapshot) => (snapshot, true),
        Err(rejected) => (rejected.snapshot(), false),
    };
    let host_redactions = host_snapshot.redaction_values().to_vec();
    let host_complete = host_snapshot.redactions_complete();
    if !host_ok || host_snapshot.entries().is_none() {
        return unavailable(
            SandboxReason::EnvironmentRejected,
            host_redactions,
            host_complete,
        );
    }
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
/// one Kite has, so every other target reports the mode as unsupported and
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
            // The driver reads this only for its single PATH entry, which is
            // useless if it is not text; a non-UTF-8 entry is dropped here
            // rather than lossily rewritten into a path the child would use.
            host_entries: utf8_entries(&options.host_entries),
            read_paths: options.settings.read_paths.clone(),
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

    let private_directories = driver.private_directories();
    let resolved = resolve_environment(&EnvironmentOptions {
        host_entries: options.host_entries.clone(),
        provider_names: options.provider_names.clone(),
        allow_names: options.settings.allow_env.clone(),
        private_directories: Some(private_directories),
    });
    let (snapshot, resolved_ok) = match &resolved {
        Ok(snapshot) => (snapshot, true),
        Err(rejected) => (rejected.snapshot(), false),
    };
    let (redactions, complete) = merged_redactions(&host_redactions, snapshot, host_complete);
    let entries = snapshot.entries().map(<[String]>::to_vec);
    if !resolved_ok || entries.is_none() || cancel.is_cancelled() {
        let cleanup = driver.close();
        let reason = if cancel.is_cancelled() {
            SandboxReason::RuntimeFailure
        } else {
            SandboxReason::EnvironmentRejected
        };
        return after_cleanup(reason, redactions, complete, cleanup);
    }

    let policy = Policy {
        filesystem: FilesystemMode::WorkspaceWrite,
        network,
    };
    finish(
        Arc::new(driver) as Arc<dyn Driver>,
        policy,
        &workspace,
        seatbelt_sandbox_info(network),
        entries,
        redactions,
        complete,
        cancel,
    )
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

    let resolved = resolve_environment(&EnvironmentOptions {
        host_entries: options.host_entries.clone(),
        provider_names: options.provider_names.clone(),
        allow_names: options.settings.allow_env.clone(),
        private_directories: None,
    });
    let (snapshot, resolved_ok) = match &resolved {
        Ok(snapshot) => (snapshot, true),
        Err(rejected) => (rejected.snapshot(), false),
    };
    let (redactions, complete) = merged_redactions(&host_redactions, snapshot, host_complete);
    let entries = snapshot.entries().map(<[String]>::to_vec);
    if !resolved_ok || entries.is_none() {
        return unavailable(SandboxReason::EnvironmentRejected, redactions, complete);
    }
    if cancel.is_cancelled() {
        return unavailable(SandboxReason::RuntimeFailure, redactions, complete);
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
    };
    finish(
        Arc::new(DirectDriver::new()) as Arc<dyn Driver>,
        policy,
        &workspace,
        info,
        entries,
        redactions,
        complete,
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
        closer: Closer::Executor(executor),
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
    };
    runtime.executor = None;
    runtime.environment = None;
    runtime
}

/// The warning printed once at startup when the sandbox is not what the user
/// would assume. A confined runtime prints nothing.
pub fn sandbox_runtime_warning(info: SandboxInfo) -> Option<String> {
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

fn unavailable(reason: SandboxReason, redactions: Vec<String>, complete: bool) -> SandboxRuntime {
    SandboxRuntime {
        executor: None,
        environment: None,
        info: SandboxInfo {
            mode: SandboxMode::Unavailable,
            network: SandboxNetwork::default(),
            bash_available: false,
            reason: safe_sandbox_reason(reason),
        },
        redaction_values: redactions,
        redactions_complete: complete,
        closer: Closer::Nothing,
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

/// Merges the host redaction set with the child's, keeping the union bounded.
/// The result is complete only if both inputs were complete and the merge
/// itself did not hit the collector's ceiling.
fn merged_redactions(
    host: &[String],
    snapshot: &EnvironmentSnapshot,
    host_complete: bool,
) -> (Vec<String>, bool) {
    let mut collector = SecretCollector::new();
    let mut merge_complete = true;
    'outer: for group in [host, snapshot.redaction_values()] {
        for value in group {
            if !collector.add_form(value) {
                merge_complete = false;
                break 'outer;
            }
        }
    }
    let complete = host_complete && snapshot.redactions_complete() && merge_complete;
    (collector.values(), complete)
}

#[cfg(target_os = "macos")]
fn seatbelt_sandbox_info(network: NetworkMode) -> SandboxInfo {
    SandboxInfo {
        mode: SandboxMode::Seatbelt,
        network: match network {
            NetworkMode::Allow => SandboxNetwork::Allowed,
            NetworkMode::Deny => SandboxNetwork::Denied,
        },
        bash_available: true,
        reason: SandboxReason::None,
    }
}

fn open_reason(error: &Error) -> SandboxReason {
    match error {
        Error::Unavailable(reason) => SandboxReason::from(*reason),
        Error::UnsupportedPolicy => SandboxReason::PolicyUnsupported,
        _ => SandboxReason::RuntimeFailure,
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

#[cfg(target_os = "macos")]
fn utf8_entries(entries: &[Vec<u8>]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| String::from_utf8(entry.clone()).ok())
        .collect()
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
mod tests {
    use super::*;
    use kite_core::config::SandboxConfig;
    use kite_core::config::sandbox::resolve_sandbox;

    fn options(workspace: &Path, driver: DriverMode) -> OpenOptions {
        OpenOptions {
            settings: Settings {
                driver,
                network: Some(NetworkMode::Allow),
                read_paths: Vec::new(),
                allow_env: Vec::new(),
            },
            workspace: workspace.to_string_lossy().into_owned(),
            shell: "/bin/sh".to_string(),
            home: workspace.to_string_lossy().into_owned(),
            host_entries: vec![b"PATH=/usr/bin:/bin".to_vec()],
            provider_names: vec!["KITE_API_KEY".to_string()],
        }
    }

    #[test]
    fn resolved_configuration_becomes_native_settings() {
        let config = SandboxConfig {
            driver: Some("off".to_string()),
            network: Some("deny".to_string()),
            read_paths: vec!["/b".to_string(), "/a".to_string()],
            allow_env: vec!["ZED".to_string(), "ABLE".to_string()],
        };
        let resolved = resolve_sandbox(&config, None).expect("resolve");
        let settings = settings_from_config(&resolved);
        assert_eq!(settings.driver, DriverMode::Off);
        assert_eq!(settings.network, Some(NetworkMode::Deny));
        assert_eq!(
            settings.read_paths,
            vec!["/a".to_string(), "/b".to_string()]
        );
        assert_eq!(
            settings.allow_env,
            vec!["ABLE".to_string(), "ZED".to_string()]
        );
    }

    #[test]
    fn the_cli_driver_override_wins_over_the_configuration_file() {
        let config = SandboxConfig {
            driver: Some("off".to_string()),
            ..SandboxConfig::default()
        };
        let resolved = resolve_sandbox(&config, Some("seatbelt")).expect("resolve");
        assert_eq!(settings_from_config(&resolved).driver, DriverMode::Seatbelt);
    }

    #[test]
    fn an_absent_driver_defaults_to_auto_and_the_network_to_allow() {
        let resolved = resolve_sandbox(&SandboxConfig::default(), None).expect("resolve");
        let settings = settings_from_config(&resolved);
        assert_eq!(settings.driver, DriverMode::Auto);
        assert_eq!(settings.network, Some(NetworkMode::Allow));
    }

    #[tokio::test]
    async fn the_off_driver_opens_an_unconfined_runtime() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cancel = CancellationToken::new();
        let runtime = normalize_sandbox_runtime(
            open_sandbox_runtime(&options(dir.path(), DriverMode::Off), &cancel).await,
        );
        assert_eq!(runtime.info.mode, SandboxMode::Off);
        assert_eq!(runtime.info.network, SandboxNetwork::Unconfined);
        assert!(runtime.info.bash_available);
        assert!(runtime.executor.is_some());
        assert!(runtime.environment.is_some());
        assert!(runtime.redactions_complete);
        assert_eq!(
            sandbox_runtime_warning(runtime.info).as_deref(),
            Some("warning: sandbox is off; bash runs unsandboxed as your user\n")
        );
        runtime.close().expect("close");
        runtime.close().expect("close is idempotent");
    }

    #[tokio::test]
    async fn a_missing_workspace_is_an_unsupported_policy() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut options = options(dir.path(), DriverMode::Off);
        options.workspace = dir.path().join("absent").to_string_lossy().into_owned();
        let runtime = open_sandbox_runtime(&options, &CancellationToken::new()).await;
        assert_eq!(runtime.info.mode, SandboxMode::Unavailable);
        assert_eq!(runtime.info.reason_code(), "policy-unsupported");
        assert!(runtime.executor.is_none());
    }

    #[tokio::test]
    async fn a_shell_that_is_not_executable_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let shell = dir.path().join("not-a-shell");
        std::fs::write(&shell, "").expect("write");
        let mut options = options(dir.path(), DriverMode::Off);
        options.shell = shell.to_string_lossy().into_owned();
        let runtime = open_sandbox_runtime(&options, &CancellationToken::new()).await;
        assert_eq!(runtime.info.reason_code(), "invalid-shell");
    }

    #[tokio::test]
    async fn a_settings_value_without_a_network_mode_is_unsupported() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut options = options(dir.path(), DriverMode::Off);
        options.settings.network = None;
        let runtime = open_sandbox_runtime(&options, &CancellationToken::new()).await;
        assert_eq!(runtime.info.reason_code(), "policy-unsupported");
    }

    #[tokio::test]
    async fn a_cancelled_open_reports_a_runtime_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let runtime = open_sandbox_runtime(&options(dir.path(), DriverMode::Off), &cancel).await;
        assert_eq!(runtime.info.reason_code(), "runtime-failure");
        assert!(runtime.executor.is_none());
    }

    #[tokio::test]
    async fn an_unsafe_host_environment_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut options = options(dir.path(), DriverMode::Off);
        options.host_entries = vec![b"PATH=/usr/bin".to_vec(), b"no-equals-sign".to_vec()];
        let runtime = open_sandbox_runtime(&options, &CancellationToken::new()).await;
        assert_eq!(runtime.info.reason_code(), "environment-rejected");
        assert!(runtime.environment.is_none());
    }

    #[test]
    fn an_incomplete_redaction_set_disables_bash() {
        let runtime = SandboxRuntime {
            executor: None,
            environment: Some(vec!["PATH=/usr/bin".to_string()]),
            info: SandboxInfo {
                mode: SandboxMode::Seatbelt,
                network: SandboxNetwork::Denied,
                bash_available: true,
                reason: SandboxReason::None,
            },
            redaction_values: vec!["secret".to_string()],
            redactions_complete: false,
            closer: Closer::Nothing,
        };
        let normalized = normalize_sandbox_runtime(runtime);
        assert_eq!(normalized.info.mode, SandboxMode::Unavailable);
        assert_eq!(normalized.info.reason_code(), "environment-rejected");
        assert!(normalized.environment.is_none());
        // The values are kept: the redactor still hides what was collected.
        assert_eq!(normalized.redaction_values, vec!["secret".to_string()]);
    }

    #[test]
    fn a_runtime_claiming_bash_without_an_executor_is_failed_closed() {
        let runtime = SandboxRuntime {
            executor: None,
            environment: Some(vec!["PATH=/usr/bin".to_string()]),
            info: SandboxInfo {
                mode: SandboxMode::Seatbelt,
                network: SandboxNetwork::Denied,
                bash_available: true,
                reason: SandboxReason::None,
            },
            redaction_values: Vec::new(),
            redactions_complete: true,
            closer: Closer::Nothing,
        };
        let normalized = normalize_sandbox_runtime(runtime);
        assert_eq!(normalized.info.reason_code(), "runtime-failure");
        assert!(!normalized.info.bash_available);
    }

    #[test]
    fn the_unavailable_warning_names_the_reason_but_nothing_else() {
        let info = SandboxInfo::unavailable(SandboxReason::SeatbeltMissing);
        assert_eq!(
            sandbox_runtime_warning(info).as_deref(),
            Some(
                "warning: bash is unavailable because the configured sandbox could not be established (reason: seatbelt-missing); file tools remain available\n"
            )
        );
        let confined = SandboxInfo {
            mode: SandboxMode::Seatbelt,
            network: SandboxNetwork::Denied,
            bash_available: true,
            reason: SandboxReason::None,
        };
        assert_eq!(sandbox_runtime_warning(confined), None);
    }

    #[test]
    fn an_unavailable_runtime_without_a_reason_reports_a_runtime_failure() {
        let runtime = unavailable(SandboxReason::None, Vec::new(), true);
        assert_eq!(runtime.info.reason, SandboxReason::RuntimeFailure);
    }

    #[test]
    fn the_shell_must_be_a_regular_executable_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(canonical_executable_file(Path::new("/bin/sh")).is_ok());
        assert!(canonical_executable_file(Path::new("")).is_err());
        assert!(canonical_executable_file(dir.path()).is_err());
        assert!(canonical_executable_file(Path::new("/bin/does-not-exist")).is_err());
    }

    #[test]
    fn a_canonical_directory_follows_symlinks_and_rejects_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("mkdir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        assert_eq!(
            canonical_directory(&link).expect("resolve"),
            std::fs::canonicalize(&real).expect("canonical")
        );
        let file = dir.path().join("file");
        std::fs::write(&file, "").expect("write");
        assert!(canonical_directory(&file).is_err());
    }
}
