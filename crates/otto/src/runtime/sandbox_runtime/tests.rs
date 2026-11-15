use super::*;
use otto_core::config::SandboxConfig;
use otto_core::config::sandbox::resolve_sandbox;

fn options(workspace: &Path, driver: DriverMode) -> OpenOptions {
    OpenOptions {
        settings: Settings {
            driver,
            network: Some(NetworkMode::Allow),
            read_paths: Vec::new(),
            deny_read_paths: Vec::new(),
            allow_env: Vec::new(),
        },
        workspace: workspace.to_string_lossy().into_owned(),
        shell: "/bin/sh".to_string(),
        home: workspace.to_string_lossy().into_owned(),
        host_entries: vec![b"PATH=/usr/bin:/bin".to_vec()],
        provider_names: vec!["OTTO_API_KEY".to_string()],
        managed_worktrees: vec![workspace.to_string_lossy().into_owned()],
    }
}

#[test]
fn resolved_configuration_becomes_native_settings() {
    let config = SandboxConfig {
        driver: Some("off".to_string()),
        network: Some("deny".to_string()),
        read_paths: vec!["/b".to_string(), "/a".to_string()],
        deny_read_paths: Vec::new(),
        allow_env: vec!["ZED".to_string(), "ABLE".to_string()],
        excluded_commands: Vec::new(),
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
        sandbox_runtime_warning(&runtime.info).as_deref(),
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
            ..Default::default()
        },
        redaction_values: vec!["secret".to_string()],
        redactions_complete: false,
        read_denylist: None,
        closer: Closer::Nothing,
        #[cfg(target_os = "macos")]
        seatbelt: None,
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
            ..Default::default()
        },
        redaction_values: Vec::new(),
        redactions_complete: true,
        read_denylist: None,
        closer: Closer::Nothing,
        #[cfg(target_os = "macos")]
        seatbelt: None,
    };
    let normalized = normalize_sandbox_runtime(runtime);
    assert_eq!(normalized.info.reason_code(), "runtime-failure");
    assert!(!normalized.info.bash_available);
}

#[test]
fn the_unavailable_warning_names_the_reason_but_nothing_else() {
    let info = SandboxInfo::unavailable(SandboxReason::SeatbeltMissing);
    assert_eq!(
        sandbox_runtime_warning(&info).as_deref(),
        Some(
            "warning: bash is unavailable because the configured sandbox could not be established (reason: seatbelt-missing); file tools remain available\n"
        )
    );
    let confined = SandboxInfo {
        mode: SandboxMode::Seatbelt,
        network: SandboxNetwork::Denied,
        bash_available: true,
        reason: SandboxReason::None,
        ..Default::default()
    };
    assert_eq!(sandbox_runtime_warning(&confined), None);
}

#[test]
fn an_unavailable_runtime_without_a_reason_reports_a_runtime_failure() {
    let runtime = unavailable(SandboxReason::None, Vec::new(), true);
    assert_eq!(runtime.info.reason, SandboxReason::RuntimeFailure);
}
