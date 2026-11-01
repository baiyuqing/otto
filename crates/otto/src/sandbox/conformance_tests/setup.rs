//! Fixture construction, policies and driver opening for the checks.

use super::*;

fn new_fixture() -> Fixture {
    // The tree is pinned to `/tmp`: the Unix socket below must stay inside the
    // 104-byte `sun_path`, which a `$TMPDIR` under `/var/folders` exhausts on
    // its own.
    let root = tempfile::Builder::new()
        .prefix("otto-sandbox-contract-")
        .tempdir_in("/tmp")
        .expect("conformance fixture");
    let base = std::fs::canonicalize(root.path()).expect("canonical fixture base");
    let workspace = base.join("workspace space ü 'quote;()[]");
    std::fs::create_dir(&workspace).expect("workspace");
    set_private(&workspace);
    let denied_dir = base.join("denied dir");
    std::fs::create_dir(&denied_dir).expect("denied directory");
    set_private(&denied_dir);
    let outside_file = denied_dir.join("outside file");
    let allowed_read = denied_dir.join("allowed read file");
    let visible_file = base.join("visible file");
    write_private(&outside_file, "outside-data");
    write_private(&allowed_read, "allowed-data");
    write_private(&visible_file, "visible-data");
    let special = workspace.join("special space ü 'quote;()[] file");
    Fixture {
        environment: vec![
            "PATH=/usr/bin:/bin".to_string(),
            "LC_ALL=C".to_string(),
            "SANDBOX_CONFORMANCE_VISIBLE=visible-value".to_string(),
            format!("SANDBOX_CONFORMANCE_SPECIAL_PATH={}", special.display()),
        ],
        unix_socket: workspace.join("contract.sock"),
        policy: Policy {
            filesystem: FilesystemMode::WorkspaceWrite,
            network: NetworkMode::Allow,
        },
        base,
        workspace,
        denied_dir,
        outside_file,
        allowed_read,
        visible_file,
        _root: Arc::new(root),
    }
}

fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("chmod 0700");
}

pub(super) fn write_private(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, contents).expect("fixture file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod 0600");
}

/// Builds the fixture and reads the driver's capabilities from a probe driver
/// that is closed again.
pub(super) async fn setup(case: &dyn Contract) -> (Fixture, Capabilities) {
    let mut fixture = new_fixture();
    let probe = case.new_driver(&fixture).await;
    let capabilities = probe.capabilities();
    probe.close().expect("probe close");
    fixture.policy = policy_for_capabilities(capabilities);
    (fixture, capabilities)
}

fn policy_for_capabilities(capabilities: Capabilities) -> Policy {
    policy_for_network(capabilities, NetworkMode::Allow)
        .or_else(|| policy_for_network(capabilities, NetworkMode::Deny))
        .expect("driver capabilities cannot satisfy a conformance policy")
}

pub(super) fn policy_for_network(
    capabilities: Capabilities,
    network: NetworkMode,
) -> Option<Policy> {
    let confined = capabilities.read_confinement
        && capabilities.write_confinement
        && capabilities.unix_socket_deny;
    match network {
        NetworkMode::Allow => capabilities.network_allow.then_some(Policy {
            filesystem: if confined {
                FilesystemMode::WorkspaceWrite
            } else {
                FilesystemMode::Unconfined
            },
            network,
        }),
        NetworkMode::Deny => (capabilities.network_deny && confined).then_some(Policy {
            filesystem: FilesystemMode::WorkspaceWrite,
            network,
        }),
    }
}

pub(super) async fn open(
    case: &dyn Contract,
    fixture: &Fixture,
) -> (Arc<dyn Driver>, Arc<Executor>) {
    open_with_policy(case, fixture, fixture.policy).await
}

pub(super) async fn open_with_policy(
    case: &dyn Contract,
    fixture: &Fixture,
    policy: Policy,
) -> (Arc<dyn Driver>, Arc<Executor>) {
    let driver = case.new_driver(fixture).await;
    let executor =
        Executor::new(driver.clone(), policy, &fixture.workspace).expect("sandbox executor");
    (driver, Arc::new(executor))
}

pub(super) fn shell_request(case: &dyn Contract, fixture: &Fixture, script: &str) -> Request {
    case.request(fixture, case.shell_command(script))
}

pub(super) async fn run(
    executor: &Executor,
    request: Request,
) -> (ExitStatus, Result<(), Error>, String) {
    let (recorder, _lines) = Recorder::new();
    let (errors, _) = Recorder::new();
    let mut stdout = recorder.clone();
    let mut stderr = errors.clone();
    let cancel = CancellationToken::new();
    let (status, result) = executor
        .execute(
            request,
            Streams {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            &cancel,
        )
        .await;
    (status, result, recorder.text())
}
