// `Arc<dyn Driver>` and `&mut dyn Write` cannot be null, so an invalid
// driver or writer is unrepresentable rather than rejected. The defensive
// copy checks are likewise inherent: an owned `Request` is moved.
use super::*;
use std::sync::Mutex;

/// Records what the executor forwards and can be held open on demand.
struct FakeDriver {
    id: DriverId,
    capabilities: Capabilities,
    requests: Mutex<Vec<Request>>,
    close_calls: Mutex<usize>,
    close_error: Option<Error>,
    release: Option<CancellationToken>,
    started: CancellationToken,
    close_started: CancellationToken,
    close_release: Option<CancellationToken>,
}

impl FakeDriver {
    fn new() -> Self {
        Self {
            id: DriverId::new("fake-driver"),
            capabilities: Capabilities {
                read_confinement: true,
                write_confinement: true,
                network_deny: true,
                network_allow: true,
                unix_socket_deny: true,
            },
            requests: Mutex::new(Vec::new()),
            close_calls: Mutex::new(0),
            close_error: None,
            release: None,
            started: CancellationToken::new(),
            close_started: CancellationToken::new(),
            close_release: None,
        }
    }

    fn execute_calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl Driver for FakeDriver {
    fn id(&self) -> DriverId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn execute(
        &self,
        request: Request,
        _streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>) {
        self.requests.lock().unwrap().push(request);
        self.started.cancel();
        if let Some(release) = &self.release {
            tokio::select! {
                _ = release.cancelled() => {}
                _ = cancel.cancelled() => {
                    return (ExitStatus::default(), Err(Error::Cancelled));
                }
            }
        }
        (ExitStatus::default(), Ok(()))
    }

    fn close(&self) -> Result<(), Error> {
        *self.close_calls.lock().unwrap() += 1;
        self.close_started.cancel();
        if let Some(release) = &self.close_release {
            while !release.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        self.close_error.clone().map_or(Ok(()), Err)
    }
}

fn workspace_policy() -> Policy {
    Policy {
        filesystem: FilesystemMode::WorkspaceWrite,
        network: NetworkMode::Deny,
    }
}

fn valid_request(workspace: &Path) -> Request {
    Request {
        argv: vec!["sh".into(), "-c".into(), "exit 0".into()],
        dir: workspace.to_path_buf(),
        env: vec!["VALUE=original".into(), "EMPTY=".into()],
        ..Default::default()
    }
}

struct Sink;
impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn new_test_executor() -> (Arc<FakeDriver>, Executor, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = std::fs::canonicalize(temp.path()).expect("canonical workspace");
    let driver = Arc::new(FakeDriver::new());
    let executor = Executor::new(driver.clone(), workspace_policy(), &workspace)
        .expect("executor construction");
    (driver, executor, temp)
}

async fn run(executor: &Executor, request: Request) -> Result<ExitStatus, Error> {
    let (mut out, mut err) = (Sink, Sink);
    let (status, result) = executor
        .execute(
            request,
            Streams {
                stdout: &mut out,
                stderr: &mut err,
            },
            &CancellationToken::new(),
        )
        .await;
    result.map(|()| status)
}

#[test]
fn every_error_renders_its_fixed_text() {
    assert_eq!(
        Error::unavailable(UnavailableReason::SeatbeltMissing).to_string(),
        "sandbox driver is unavailable: seatbelt-missing"
    );
    for reason in [
        UnavailableReason::UnsupportedPlatform,
        UnavailableReason::SeatbeltMissing,
        UnavailableReason::SelfTestFailed,
        UnavailableReason::RuntimeFailure,
        UnavailableReason::InvalidShell,
        UnavailableReason::EnvironmentRejected,
        UnavailableReason::PolicyUnsupported,
    ] {
        assert_eq!(
            Error::unavailable(reason).to_string(),
            format!("sandbox driver is unavailable: {}", reason.as_str())
        );
    }
    assert_eq!(Error::Closed.to_string(), "sandbox executor is closed");
    assert_eq!(Error::InvalidRequest.to_string(), "invalid sandbox request");
    assert_eq!(
        Error::UnsupportedPolicy.to_string(),
        "sandbox policy is unsupported"
    );
    assert_eq!(
        Error::EnvironmentUnsafe.to_string(),
        "sandbox environment is unsafe"
    );
    assert_eq!(
        Error::ChildLaunch.to_string(),
        "sandbox child launch failed"
    );
    assert_eq!(Error::ChildWait.to_string(), "sandbox child wait failed");
    assert_eq!(
        Error::ChildTerminate.to_string(),
        "sandbox child termination failed"
    );
}

#[test]
fn driver_ids_are_lowercase_ascii_up_to_32_bytes() {
    for invalid in ["", "UPPER", "has_underscore", "has space", &"a".repeat(33)] {
        assert!(!DriverId::new(invalid).is_valid(), "{invalid:?}");
    }
    for valid in ["direct", "seatbelt", "a", &"a".repeat(32)] {
        assert!(DriverId::new(valid).is_valid(), "{valid:?}");
    }
}

#[test]
fn new_executor_rejects_invalid_driver_id() {
    let temp = tempfile::tempdir().unwrap();
    for id in ["", "UPPER", "has_underscore", "has space"] {
        let mut driver = FakeDriver::new();
        driver.id = DriverId::new(id);
        let error = Executor::new(Arc::new(driver), workspace_policy(), temp.path())
            .err()
            .expect("rejection");
        assert_eq!(error, Error::unavailable(UnavailableReason::RuntimeFailure));
    }
}

#[test]
fn new_executor_rejects_unsupported_policy() {
    let temp = tempfile::tempdir().unwrap();
    let cases: [(&str, Policy, Capabilities); 6] = [
        (
            "workspace write needs read confinement",
            Policy {
                filesystem: FilesystemMode::WorkspaceWrite,
                network: NetworkMode::Allow,
            },
            Capabilities {
                write_confinement: true,
                network_allow: true,
                unix_socket_deny: true,
                ..Capabilities::default()
            },
        ),
        (
            "workspace write needs write confinement",
            Policy {
                filesystem: FilesystemMode::WorkspaceWrite,
                network: NetworkMode::Allow,
            },
            Capabilities {
                read_confinement: true,
                network_allow: true,
                unix_socket_deny: true,
                ..Capabilities::default()
            },
        ),
        (
            "workspace write needs unix socket denial",
            Policy {
                filesystem: FilesystemMode::WorkspaceWrite,
                network: NetworkMode::Allow,
            },
            Capabilities {
                read_confinement: true,
                write_confinement: true,
                network_allow: true,
                ..Capabilities::default()
            },
        ),
        (
            "workspace write needs selected network deny",
            Policy {
                filesystem: FilesystemMode::WorkspaceWrite,
                network: NetworkMode::Deny,
            },
            Capabilities {
                read_confinement: true,
                write_confinement: true,
                network_allow: true,
                unix_socket_deny: true,
                ..Capabilities::default()
            },
        ),
        (
            "unconfined rejects network deny",
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Deny,
            },
            Capabilities {
                network_deny: true,
                network_allow: true,
                ..Capabilities::default()
            },
        ),
        (
            "unconfined needs network allow",
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Allow,
            },
            Capabilities::default(),
        ),
    ];
    for (name, policy, capabilities) in cases {
        let mut driver = FakeDriver::new();
        driver.capabilities = capabilities;
        let error = Executor::new(Arc::new(driver), policy, temp.path())
            .err()
            .unwrap_or_else(|| panic!("{name}: want rejection"));
        assert_eq!(error, Error::UnsupportedPolicy, "{name}");
    }
}

#[tokio::test]
async fn new_executor_canonicalizes_and_binds_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let parent = std::fs::canonicalize(temp.path()).unwrap();
    let real = parent.join("real-workspace");
    std::fs::create_dir(&real).unwrap();
    let link = parent.join("workspace-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let child = real.join("child");
    std::fs::create_dir(&child).unwrap();

    let driver = Arc::new(FakeDriver::new());
    let executor = Executor::new(driver, workspace_policy(), &link).expect("executor");
    for dir in [&real, &child] {
        run(&executor, valid_request(dir)).await.expect("execute");
    }
    let outside = tempfile::tempdir().unwrap();
    let outside = std::fs::canonicalize(outside.path()).unwrap();
    assert_eq!(
        run(&executor, valid_request(&outside)).await,
        Err(Error::InvalidRequest)
    );
}

#[tokio::test]
async fn executor_rejects_noncanonical_or_escaped_directory() {
    let (_driver, executor, temp) = new_test_executor();
    let workspace = std::fs::canonicalize(temp.path()).unwrap();
    let child = workspace.join("child");
    std::fs::create_dir(&child).unwrap();
    let link = workspace.join("child-link");
    std::os::unix::fs::symlink(&child, &link).unwrap();

    let cases: Vec<(&str, PathBuf)> = vec![
        ("relative", PathBuf::from(".")),
        ("unclean", child.join(".")),
        ("symlink", link),
        ("missing", workspace.join("missing")),
        ("escaped", workspace.parent().expect("parent").to_path_buf()),
        ("empty", PathBuf::new()),
    ];
    for (name, dir) in cases {
        let mut request = valid_request(&workspace);
        request.dir = dir;
        assert_eq!(
            run(&executor, request).await,
            Err(Error::InvalidRequest),
            "{name}"
        );
    }
}

#[tokio::test]
async fn executor_rejects_malformed_or_duplicate_environment() {
    let (driver, executor, temp) = new_test_executor();
    let workspace = std::fs::canonicalize(temp.path()).unwrap();
    let cases: [(&str, Vec<String>); 6] = [
        ("missing equals", vec!["NAME".into()]),
        ("empty name", vec!["=value".into()]),
        ("invalid name", vec!["BAD-NAME=value".into()]),
        ("nul in name", vec!["BAD\0NAME=value".into()]),
        ("nul in value", vec!["NAME=bad\0value".into()]),
        ("duplicate", vec!["NAME=one".into(), "NAME=two".into()]),
    ];
    for (name, env) in cases {
        let mut request = valid_request(&workspace);
        request.env = env;
        assert_eq!(
            run(&executor, request).await,
            Err(Error::InvalidRequest),
            "{name}"
        );
    }

    let mut request = valid_request(&workspace);
    request.env = Vec::new();
    run(&executor, request).await.expect("empty environment");
    assert!(
        driver
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .env
            .is_empty()
    );

    let mut request = valid_request(&workspace);
    request.argv = vec!["sh".into(), "bad\0argument".into()];
    assert_eq!(run(&executor, request).await, Err(Error::InvalidRequest));

    let mut request = valid_request(&workspace);
    request.argv = Vec::new();
    assert_eq!(run(&executor, request).await, Err(Error::InvalidRequest));
}

#[tokio::test]
async fn executor_forwards_the_validated_request() {
    let (driver, executor, temp) = new_test_executor();
    let workspace = std::fs::canonicalize(temp.path()).unwrap();
    let request = valid_request(&workspace);
    run(&executor, request.clone()).await.expect("execute");
    assert_eq!(driver.requests.lock().unwrap().as_slice(), &[request]);
}

#[tokio::test]
async fn executor_reports_cancellation_without_calling_the_driver() {
    let (driver, executor, temp) = new_test_executor();
    let workspace = std::fs::canonicalize(temp.path()).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (mut out, mut err) = (Sink, Sink);
    let (status, result) = executor
        .execute(
            valid_request(&workspace),
            Streams {
                stdout: &mut out,
                stderr: &mut err,
            },
            &cancel,
        )
        .await;
    assert_eq!(
        (status, result),
        (ExitStatus::default(), Err(Error::Cancelled))
    );
    assert_eq!(driver.execute_calls(), 0);
}

#[tokio::test]
async fn executor_close_is_concurrent_and_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temp.path()).unwrap();
    let close_release = CancellationToken::new();
    let mut driver = FakeDriver::new();
    driver.close_error = Some(Error::ChildTerminate);
    driver.close_release = Some(close_release.clone());
    let driver = Arc::new(driver);
    let executor = Arc::new(Executor::new(driver.clone(), workspace_policy(), &workspace).unwrap());

    let mut handles = Vec::new();
    for _ in 0..16 {
        let executor = executor.clone();
        handles.push(tokio::task::spawn_blocking(move || executor.close()));
    }
    driver.close_started.cancelled().await;
    close_release.cancel();
    for handle in handles {
        assert_eq!(handle.await.unwrap(), Err(Error::ChildTerminate));
    }
    assert_eq!(executor.close(), Err(Error::ChildTerminate));
    assert_eq!(*driver.close_calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn execute_racing_close_is_refused_after_close_starts() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temp.path()).unwrap();
    let release = CancellationToken::new();
    let mut driver = FakeDriver::new();
    driver.release = Some(release.clone());
    let driver = Arc::new(driver);
    let executor = Arc::new(Executor::new(driver.clone(), workspace_policy(), &workspace).unwrap());

    let active = {
        let executor = executor.clone();
        let workspace = workspace.clone();
        tokio::spawn(async move {
            let (mut out, mut err) = (Sink, Sink);
            executor
                .execute(
                    valid_request(&workspace),
                    Streams {
                        stdout: &mut out,
                        stderr: &mut err,
                    },
                    &CancellationToken::new(),
                )
                .await
        })
    };
    driver.started.cancelled().await;

    let closer = {
        let executor = executor.clone();
        tokio::task::spawn_blocking(move || executor.close())
    };
    driver.close_started.cancelled().await;

    assert_eq!(
        run(&executor, valid_request(&workspace)).await,
        Err(Error::Closed)
    );
    assert_eq!(driver.execute_calls(), 1);

    release.cancel();
    let (_status, result) = active.await.unwrap();
    result.expect("active execution");
    closer.await.unwrap().expect("close");
    assert_eq!(
        run(&executor, valid_request(&workspace)).await,
        Err(Error::Closed)
    );
}
