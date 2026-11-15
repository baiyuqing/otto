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
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub mod config_edit;
pub mod control;
pub mod direct;
pub mod environment;
pub mod git;
pub mod info;
pub mod process;
pub mod read_denylist;
/// macOS only: the driver shells out to `/usr/bin/sandbox-exec`, and its
/// profile language has no meaning anywhere else. Keeping it off other
/// targets is what lets the rest of the crate build for them.
#[cfg(target_os = "macos")]
pub mod seatbelt;
pub mod worktrees;

#[cfg(test)]
pub(crate) mod conformance_tests;

/// Default grace afforded after `SIGTERM` before process cleanup escalates to
/// `SIGKILL`.
pub(crate) const DEFAULT_CANCELLATION_GRACE: Duration = Duration::from_secs(5);

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
    /// Optional bounded protocol input; ordinary bash keeps stdin closed.
    pub stdin: Option<Vec<u8>>,
    pub dir: PathBuf,
    pub env: Vec<String>,
    /// Asks the driver to report what the sandbox denied when the command
    /// fails. Drivers that cannot report ignore it.
    pub report_denials: bool,
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
    /// Operations the sandbox refused while the command ran, in first-seen
    /// order. Empty unless the driver reports denials and the request asked.
    pub denials: Vec<Denial>,
    /// Unique denials beyond those in `denials`.
    pub denials_omitted: usize,
}

/// One refused sandbox operation: the operation name (for example
/// `file-read-data`) and its target (a path, or an address for the network).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Denial {
    pub operation: String,
    pub target: String,
}

/// The persisted sandbox configuration of a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub driver: DriverMode,
    /// `None` keeps the driver's default.
    pub network: Option<NetworkMode>,
    #[serde(default)]
    pub read_paths: Vec<String>,
    /// Paths added to the built-in read denylist; see
    /// `read_denylist::BUILT_IN_DENY_PATHS`.
    #[serde(default)]
    pub deny_read_paths: Vec<String>,
    #[serde(default)]
    pub allow_env: Vec<String>,
}

/// The private per-session directories a confined child sees as `TMPDIR` and
/// the cache roots. All are created at mode 0700 and owned by the driver that
/// created them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PrivateDirectories {
    pub root: PathBuf,
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

    /// Runs `request` with a caller-selected process termination grace.
    ///
    /// The default preserves compatibility for drivers that only implement
    /// [`Driver::execute`].
    async fn execute_with_grace(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
        _cancellation_grace: Duration,
    ) -> (ExitStatus, Result<(), Error>) {
        self.execute(request, streams, cancel).await
    }

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

    /// Runs `request` with a caller-selected process termination grace.
    ///
    /// The default preserves compatibility for executors that only implement
    /// [`CommandExecutor::execute`].
    async fn execute_with_grace(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
        _cancellation_grace: Duration,
    ) -> (ExitStatus, Result<(), Error>) {
        self.execute(request, streams, cancel).await
    }
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
        if request
            .stdin
            .as_ref()
            .is_some_and(|input| input.len() > 1 << 20)
        {
            return Err(Error::InvalidRequest);
        }
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
        self.execute_with_grace(request, streams, cancel, DEFAULT_CANCELLATION_GRACE)
            .await
    }

    async fn execute_with_grace(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
        cancellation_grace: Duration,
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
        let (mut status, result) = self
            .driver
            .execute_with_grace(request, streams, cancel, cancellation_grace)
            .await;
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
mod tests;
