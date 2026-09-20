//! Sandboxed command execution.
//!
//! A [`Driver`] confines one child process; [`Executor`] validates every
//! request against a [`Policy`] and the driver's [`Capabilities`] before the
//! driver ever sees it.
//!
//! Ownership: a [`Request`] is moved into the executor and cloned once for the
//! driver, so the caller's copy is never observed after the call starts. The
//! writers in [`Streams`] stay borrowed for the duration of one execution.
//!
//! Concurrency and cancellation: [`Executor`] is `Send + Sync` and its methods
//! take `&self`; several executions may run at once. Cancellation is a
//! [`CancellationToken`]: a token cancelled before or during an execution
//! produces [`Error::Cancelled`] and the child's whole process group is killed.
//!
//! Errors: every failure is one [`Error`] variant and carries no detail beyond
//! its kind, so a sandbox failure can never leak host paths or environment
//! values into model-visible text.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub mod direct;
pub mod environment;
pub mod process;
pub mod seatbelt;

#[cfg(test)]
pub(crate) mod conformance;

/// The identifier a driver reports for diagnostics and settings.
///
/// Valid ids are 1..=32 characters of `[a-z0-9-]`; [`Executor::new`] rejects
/// anything else with [`Error::Unavailable`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DriverId(String);

impl DriverId {
    /// Wraps `id` without validating it. [`Executor::new`] performs the check.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reports whether the id is 1..=32 characters of `[a-z0-9-]`.
    pub fn is_valid(&self) -> bool {
        let bytes = self.0.as_bytes();
        (1..=32).contains(&bytes.len())
            && bytes
                .iter()
                .all(|c| *c == b'-' || c.is_ascii_lowercase() || c.is_ascii_digit())
    }
}

impl std::fmt::Display for DriverId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The sandbox implementation a session asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverMode {
    /// Confine when the host supports it, otherwise fail closed.
    #[default]
    Auto,
    /// Require the Seatbelt driver.
    Seatbelt,
    /// Run unconfined. Only this explicit value disables confinement.
    Off,
}

impl DriverMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Seatbelt => "seatbelt",
            Self::Off => "off",
        }
    }
}

/// How the child may reach the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemMode {
    /// Reads are confined to reviewed roots; writes to the workspace and the
    /// private state directories.
    WorkspaceWrite,
    /// No filesystem confinement at all.
    Unconfined,
}

/// How the child may reach the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    Deny,
    Allow,
}

/// The confinement a session requires of its driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub filesystem: FilesystemMode,
    pub network: NetworkMode,
}

/// What a driver can actually enforce. [`Executor::new`] refuses a policy the
/// driver cannot cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub read_confinement: bool,
    pub write_confinement: bool,
    pub network_deny: bool,
    pub network_allow: bool,
    pub unix_socket_deny: bool,
}

/// One command to run.
///
/// `dir` must be absolute, already canonical, an existing directory, and
/// inside the executor's workspace. `env` entries are `NAME=VALUE` with a
/// POSIX-ish name and no duplicate names. Neither may contain a NUL byte.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub argv: Vec<String>,
    pub dir: PathBuf,
    pub env: Vec<String>,
}

/// Where the child's output goes.
///
/// Ownership: both writers are borrowed for the duration of one execution and
/// are written from the executing task only; they are never retained.
pub struct Streams<'a> {
    pub stdout: &'a mut (dyn std::io::Write + Send),
    pub stderr: &'a mut (dyn std::io::Write + Send),
}

impl std::fmt::Debug for Streams<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Streams { .. }")
    }
}

/// How the child finished. `code` is `-1` whenever `signaled` is set, and
/// `signal` is the signal description such as `killed` or `terminated`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: i32,
    pub signaled: bool,
    pub signal: String,
}

/// The persisted sandbox configuration of a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub driver: DriverMode,
    /// `None` keeps the driver's default.
    pub network: Option<NetworkMode>,
    #[serde(default)]
    pub read_paths: Vec<String>,
    #[serde(default)]
    pub allow_env: Vec<String>,
}

/// The private per-session directories a confined child sees as `HOME`,
/// `TMPDIR` and the cache roots. All are created at mode 0700 and owned by the
/// driver that created them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivateDirectories {
    pub root: PathBuf,
    pub home: PathBuf,
    pub temp: PathBuf,
    pub cache: PathBuf,
}

/// Why a driver cannot confine on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    UnsupportedPlatform,
    SeatbeltMissing,
    SelfTestFailed,
    RuntimeFailure,
    InvalidShell,
    EnvironmentRejected,
    PolicyUnsupported,
}

impl UnavailableReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "unsupported-platform",
            Self::SeatbeltMissing => "seatbelt-missing",
            Self::SelfTestFailed => "self-test-failed",
            Self::RuntimeFailure => "runtime-failure",
            Self::InvalidShell => "invalid-shell",
            Self::EnvironmentRejected => "environment-rejected",
            Self::PolicyUnsupported => "policy-unsupported",
        }
    }
}

impl std::fmt::Display for UnavailableReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every sandbox failure.
///
/// Variants carry no detail: the text reaches the model through the bash tool,
/// so a failure must not disclose host paths, environment values, or the
/// reason a specific probe failed beyond its [`UnavailableReason`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("sandbox executor is closed")]
    Closed,
    #[error("invalid sandbox request")]
    InvalidRequest,
    #[error("sandbox policy is unsupported")]
    UnsupportedPolicy,
    #[error("sandbox driver is unavailable: {0}")]
    Unavailable(UnavailableReason),
    #[error("sandbox environment is unsafe")]
    EnvironmentUnsafe,
    #[error("sandbox child launch failed")]
    ChildLaunch,
    #[error("sandbox child wait failed")]
    ChildWait,
    #[error("sandbox child termination failed")]
    ChildTerminate,
    /// The caller's [`CancellationToken`] fired.
    #[error("sandbox execution was cancelled")]
    Cancelled,
}

impl Error {
    /// [`Error::Unavailable`] carrying `reason`.
    pub fn unavailable(reason: UnavailableReason) -> Self {
        Self::Unavailable(reason)
    }
}

/// One confinement mechanism.
///
/// Concurrency: implementations must tolerate concurrent `execute` calls and a
/// `close` racing them; `close` is idempotent and returns the same result to
/// every caller.
#[async_trait]
pub trait Driver: Send + Sync {
    fn id(&self) -> DriverId;
    fn capabilities(&self) -> Capabilities;
    /// Runs `request`, returning how the child finished alongside whether the
    /// execution itself failed.
    ///
    /// The two values are independent: a cancelled execution still reports the
    /// signal that killed the child, and the `bash` tool prints both. A failure
    /// before any child existed pairs the error with a default [`ExitStatus`].
    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>);
    fn close(&self) -> Result<(), Error>;
}

/// What the bash tool depends on. Implemented by [`Executor`].
#[async_trait]
pub trait CommandExecutor: Send + Sync {
    /// Runs `request`, streaming output into `streams`.
    ///
    /// Cancelling `cancel` kills the child's whole process group and returns
    /// [`Error::Cancelled`]. Timeouts are the caller's business: the sandbox
    /// imposes none.
    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>);
}

/// Validates requests, enforces the policy, and owns the driver's lifetime.
///
/// Concurrency: `&self` methods may be called from any task. [`Executor::close`]
/// runs the driver's close exactly once; concurrent callers block until it
/// finishes and observe the same result.
pub struct Executor {
    driver: Arc<dyn Driver>,
    id: DriverId,
    capabilities: Capabilities,
    policy: Policy,
    workspace: PathBuf,
    closed: AtomicBool,
    close_result: OnceLock<Result<(), Error>>,
}

impl Executor {
    /// Binds `driver` to `policy` and `workspace`.
    ///
    /// Errors: [`Error::Unavailable`] with [`UnavailableReason::RuntimeFailure`]
    /// for an invalid driver id, [`Error::UnsupportedPolicy`] when the driver
    /// cannot enforce the policy, [`Error::InvalidRequest`] when the workspace
    /// is not an existing directory.
    pub fn new(driver: Arc<dyn Driver>, policy: Policy, workspace: &Path) -> Result<Self, Error> {
        let id = driver.id();
        if !id.is_valid() {
            return Err(Error::unavailable(UnavailableReason::RuntimeFailure));
        }
        let capabilities = driver.capabilities();
        if !supports_policy(capabilities, policy) {
            return Err(Error::UnsupportedPolicy);
        }
        let workspace = canonical_directory(workspace)?;
        Ok(Self {
            driver,
            id,
            capabilities,
            policy,
            workspace,
            closed: AtomicBool::new(false),
            close_result: OnceLock::new(),
        })
    }

    pub fn id(&self) -> DriverId {
        self.id.clone()
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Closes the driver once. Later and concurrent calls wait for the first
    /// close and return its result.
    pub fn close(&self) -> Result<(), Error> {
        self.closed.store(true, Ordering::SeqCst);
        self.close_result
            .get_or_init(|| self.driver.close())
            .clone()
    }

    fn validated_request(&self, request: Request) -> Result<Request, Error> {
        if request.argv.is_empty() || request.argv.iter().any(|arg| arg.contains('\0')) {
            return Err(Error::InvalidRequest);
        }
        if !self.valid_directory(&request.dir) || !valid_environment(&request.env) {
            return Err(Error::InvalidRequest);
        }
        Ok(request)
    }

    fn valid_directory(&self, dir: &Path) -> bool {
        let Some(text) = dir.to_str() else {
            return false;
        };
        if text.is_empty() || text.contains('\0') || !dir.is_absolute() {
            return false;
        }
        // `components()` drops `.` and collapses trailing separators, so an
        // unclean spelling differs from its own rebuild.
        if dir.components().collect::<PathBuf>().as_os_str() != dir.as_os_str() {
            return false;
        }
        let Ok(canonical) = std::fs::canonicalize(dir) else {
            return false;
        };
        if canonical.as_os_str() != dir.as_os_str() {
            return false;
        }
        canonical.is_dir() && canonical.starts_with(&self.workspace)
    }
}

#[async_trait]
impl CommandExecutor for Executor {
    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>) {
        if cancel.is_cancelled() {
            return (ExitStatus::default(), Err(Error::Cancelled));
        }
        if self.closed.load(Ordering::SeqCst) {
            return (ExitStatus::default(), Err(Error::Closed));
        }
        let request = match self.validated_request(request) {
            Ok(request) => request,
            Err(error) => return (ExitStatus::default(), Err(error)),
        };
        if cancel.is_cancelled() {
            return (ExitStatus::default(), Err(Error::Cancelled));
        }
        if self.closed.load(Ordering::SeqCst) {
            return (ExitStatus::default(), Err(Error::Closed));
        }
        let (mut status, result) = self.driver.execute(request, streams, cancel).await;
        if status.signaled {
            status.code = -1;
        }
        (status, result)
    }
}

/// Canonicalizes `path` and requires it to name an existing directory.
fn canonical_directory(path: &Path) -> Result<PathBuf, Error> {
    let Some(text) = path.to_str() else {
        return Err(Error::InvalidRequest);
    };
    if text.is_empty() || text.contains('\0') {
        return Err(Error::InvalidRequest);
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| Error::InvalidRequest)?;
    if !canonical.is_dir() {
        return Err(Error::InvalidRequest);
    }
    Ok(canonical)
}

/// Reports whether every entry is `NAME=VALUE` with a unique POSIX-ish name
/// and no NUL byte.
fn valid_environment(environment: &[String]) -> bool {
    let mut names = std::collections::HashSet::with_capacity(environment.len());
    for entry in environment {
        if entry.contains('\0') {
            return false;
        }
        let Some((name, _)) = entry.split_once('=') else {
            return false;
        };
        if !valid_environment_name(name) || !names.insert(name) {
            return false;
        }
    }
    true
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if first != b'_' && !first.is_ascii_alphabetic() {
        return false;
    }
    bytes.all(|c| c == b'_' || c.is_ascii_alphanumeric())
}

fn supports_policy(capabilities: Capabilities, policy: Policy) -> bool {
    match policy.filesystem {
        FilesystemMode::WorkspaceWrite => {
            if !capabilities.read_confinement
                || !capabilities.write_confinement
                || !capabilities.unix_socket_deny
            {
                return false;
            }
            match policy.network {
                NetworkMode::Deny => capabilities.network_deny,
                NetworkMode::Allow => capabilities.network_allow,
            }
        }
        FilesystemMode::Unconfined => {
            policy.network == NetworkMode::Allow && capabilities.network_allow
        }
    }
}

#[cfg(test)]
mod tests {
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
        let executor =
            Arc::new(Executor::new(driver.clone(), workspace_policy(), &workspace).unwrap());

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
        let executor =
            Arc::new(Executor::new(driver.clone(), workspace_policy(), &workspace).unwrap());

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
}
