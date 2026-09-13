//! The Seatbelt [`Driver`].
//!
//! Port of `internal/sandbox/seatbelt/driver_darwin.go`. Every child is run as
//! `/usr/bin/sandbox-exec -f <profile> -- <argv>` with a profile generated from
//! the session's workspace, its private state tree and the reviewed read roots.
//!
//! Ownership: [`SeatbeltDriver`] owns the private state tree and the process
//! manager, and releases both exactly once in [`SeatbeltDriver::close`]. The
//! writers in [`Streams`] are borrowed for one execution only.
//!
//! Concurrency and cancellation: `execute` takes `&self` and may run
//! concurrently; a cancelled token kills the child's whole process group and
//! yields [`Error::Cancelled`] alongside the status the child died with.
//! `close` is idempotent and concurrent callers observe the first result.
//!
//! Errors: `sandbox-exec` writes its own diagnostics to the child's stderr. A
//! line starting `sandbox-exec:` means the sandbox itself failed rather than
//! the command, so it is withheld from the caller, latches the driver as
//! poisoned, and becomes [`UnavailableReason::RuntimeFailure`]. The single
//! exception is `sandbox-exec: execvp()`, which is an ordinary "command not
//! found" and is suppressed without poisoning anything.
//!
//! Differences from Go, all forced by the type system:
//!
//! * Go's `driverDependencies` injects eleven functions so its tests can
//!   simulate a hostile host. Nothing in this crate consumes that seam, so the
//!   port calls the production operations directly.
//! * Go joins a cancellation with any secondary failure into a multi-error.
//!   [`Error`] is a closed enum with no join, so cancellation wins and the
//!   secondary failure is dropped, which matches what Go's joined error
//!   displays.
//! * A private-state cleanup failure has no Go-equivalent [`Error`] variant; it
//!   surfaces as [`UnavailableReason::RuntimeFailure`].

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::state;
use super::{profile, selftest};
use crate::sandbox::process::{Manager, Outcome, Spec};
use crate::sandbox::{
    Capabilities, Driver, DriverId, Error, ExitStatus, NetworkMode, PrivateDirectories, Request,
    Streams, UnavailableReason,
};

/// The only binary this driver ever executes.
const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

/// How long the whole startup self-test may take.
const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How much probe output is kept before the probe is failed for being noisy.
const SELF_TEST_OUTPUT_LIMIT: usize = 8 * 1024;

/// How much stderr may be buffered while deciding whether it is a
/// `sandbox-exec` diagnostic or ordinary child output.
const STDERR_DECISION_LIMIT: usize = 4 * 1024;

const DIAGNOSTIC_PREFIX: &[u8] = b"sandbox-exec:";
const POLICY_EXEC_PREFIX: &[u8] = b"sandbox-exec: execvp()";

/// What the driver needs to build a profile and a private state tree.
///
/// Every path must already be absolute; `open` canonicalizes and re-checks
/// them. An empty `cache_base` means "use the platform user cache directory".
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub workspace: String,
    pub shell: String,
    pub home: String,
    pub cache_base: String,
    /// The host environment, read only for its single `PATH` entry.
    pub host_entries: Vec<String>,
    pub read_paths: Vec<String>,
    /// `None` is rejected, matching Go's refusal of the zero `NetworkMode`.
    pub network: Option<NetworkMode>,
}

/// Mutable driver state guarded by one lock.
#[derive(Debug, Default)]
struct Inner {
    closed: bool,
    /// Set once the sandbox itself has failed. Every later execution is
    /// refused rather than run unconfined.
    poisoned: bool,
    close_done: bool,
    close_result: Option<Result<(), Error>>,
}

/// Runs commands under `/usr/bin/sandbox-exec`.
pub struct SeatbeltDriver {
    workspace: String,
    network: NetworkMode,
    state: state::State,
    profile_path: String,
    processes: Manager,
    inner: Mutex<Inner>,
    progress: Condvar,
}

impl std::fmt::Debug for SeatbeltDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SeatbeltDriver")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl SeatbeltDriver {
    /// Builds the private state, writes the profile, and proves the profile
    /// works before returning.
    ///
    /// Errors: [`UnavailableReason::SeatbeltMissing`] when `/usr/bin/sandbox-exec`
    /// is not the root-owned executable it must be,
    /// [`UnavailableReason::InvalidShell`] for a shell that is not a canonical
    /// executable file, [`UnavailableReason::PolicyUnsupported`] for a
    /// workspace, home or network mode the profile cannot express, and
    /// [`UnavailableReason::SelfTestFailed`] when the state, the profile or any
    /// startup probe fails. A cancelled token yields [`Error::Cancelled`].
    pub async fn open(options: Options, cancel: &CancellationToken) -> Result<Self, Error> {
        check_cancelled(cancel)?;
        if !valid_sandbox_exec() {
            return Err(Error::unavailable(UnavailableReason::SeatbeltMissing));
        }
        check_cancelled(cancel)?;

        let Some(workspace) = resolve_directory(&options.workspace) else {
            return Err(Error::unavailable(UnavailableReason::PolicyUnsupported));
        };
        check_cancelled(cancel)?;

        let shell = match profile::resolve_path(&options.shell) {
            Ok(shell)
                if profile::valid_resolved(&shell)
                    && shell.kind == profile::PathKind::Regular
                    && shell.executable =>
            {
                shell.path
            }
            _ => return Err(Error::unavailable(UnavailableReason::InvalidShell)),
        };
        check_cancelled(cancel)?;

        let Some(home) = resolve_directory(&options.home) else {
            return Err(Error::unavailable(UnavailableReason::PolicyUnsupported));
        };
        let Some(network) = options.network else {
            return Err(Error::unavailable(UnavailableReason::PolicyUnsupported));
        };
        check_cancelled(cancel)?;

        let cache_base = if options.cache_base.is_empty() {
            let Some(base) = user_cache_directory() else {
                return Err(Error::unavailable(UnavailableReason::SelfTestFailed));
            };
            base
        } else {
            options.cache_base.clone()
        };
        check_cancelled(cancel)?;

        let private = state::create(&workspace, &cache_base)
            .map_err(|_| Error::unavailable(UnavailableReason::SelfTestFailed))?;
        let build = Self::build(&options, workspace, shell, home, network, private, cancel).await;
        match build {
            Ok(driver) => Ok(driver),
            Err((private, error)) => {
                let _ = private.close();
                Err(error)
            }
        }
    }

    /// The part of `open` that must undo the private state tree on failure.
    ///
    /// Returns the state back to the caller on every error so the tree is
    /// removed exactly once, whichever step failed.
    // The large `Err` payload is the point: the state travels back so exactly
    // one caller removes the tree. It is matched immediately, never propagated
    // with `?`, so boxing it would only add an allocation on the failure path.
    #[allow(clippy::too_many_arguments, clippy::result_large_err)]
    async fn build(
        options: &Options,
        workspace: String,
        shell: String,
        home: String,
        network: NetworkMode,
        private: state::State,
        cancel: &CancellationToken,
    ) -> Result<Self, (state::State, Error)> {
        if let Err(error) = check_cancelled(cancel) {
            return Err((private, error));
        }
        let generated = profile::generate(&profile::Options {
            workspace: workspace.clone(),
            directories: private.directories.clone(),
            shell,
            home,
            host_entries: options.host_entries.clone(),
            read_paths: options.read_paths.clone(),
            network: Some(network),
        });
        let Ok(text) = generated else {
            return Err((
                private,
                Error::unavailable(UnavailableReason::SelfTestFailed),
            ));
        };
        if private.write_profile(text.as_bytes()).is_err() {
            return Err((
                private,
                Error::unavailable(UnavailableReason::SelfTestFailed),
            ));
        }
        if let Err(error) = check_cancelled(cancel) {
            return Err((private, error));
        }

        let profile_path = private.profile_path.clone();
        let driver = Self {
            workspace,
            network,
            state: private,
            profile_path,
            processes: Manager::new(),
            inner: Mutex::new(Inner::default()),
            progress: Condvar::new(),
        };
        match driver.run_startup_self_test(cancel).await {
            Ok(()) => Ok(driver),
            Err(error) => {
                let _ = driver.processes.close();
                Err((driver.state, error))
            }
        }
    }

    /// The private directories the child sees as `HOME` and `TMPDIR`.
    pub fn private_directories(&self) -> PrivateDirectories {
        self.state.directories.clone()
    }

    /// Proves the profile allows what it promises and denies what it forbids.
    ///
    /// Five probes run under the real profile: the profile loads at all, a file
    /// under the private home reads back, two writable fixtures accept a write
    /// through a descriptor they re-identify by device, inode, type, owner,
    /// permissions and link count, and a file under the profile directory is
    /// refused for both reading and writing.
    async fn run_startup_self_test(&self, cancel: &CancellationToken) -> Result<(), Error> {
        const ALLOWED_READ: &str = "otto-seatbelt-allowed-read";
        const ALLOWED_WRITE: &str = "otto-seatbelt-allowed-write";
        const DENIED: &str = "otto-seatbelt-denied-fixture";

        let deadline = cancel.child_token();
        let timer = {
            let token = deadline.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SELF_TEST_TIMEOUT).await;
                token.cancel();
            })
        };
        let home = self.state.directories.home.to_string_lossy().into_owned();
        let temp = self.state.directories.temp.to_string_lossy().into_owned();
        let mut fixtures = selftest::Fixtures::prepare(
            &home,
            &self.workspace,
            &temp,
            &self.state.profiles,
            ALLOWED_READ,
            DENIED,
        )
        .map_err(|_| Error::unavailable(UnavailableReason::SelfTestFailed))?;

        let probes = self
            .self_test_probes(
                &fixtures,
                cancel,
                &deadline,
                ALLOWED_READ,
                ALLOWED_WRITE,
                DENIED,
            )
            .await;
        let cleanup = fixtures.cleanup();
        timer.abort();
        if probes.is_err() || cleanup.is_err() {
            return probes.and(Err(Error::unavailable(UnavailableReason::SelfTestFailed)));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn self_test_probes(
        &self,
        fixtures: &selftest::Fixtures,
        caller: &CancellationToken,
        deadline: &CancellationToken,
        allowed_read: &str,
        allowed_write: &str,
        denied: &str,
    ) -> Result<(), Error> {
        let unavailable = || Error::unavailable(UnavailableReason::SelfTestFailed);
        let read_fixture = fixtures
            .get(selftest::Kind::AllowedRead)
            .map_err(|_| unavailable())?;
        let denied_read = fixtures
            .get(selftest::Kind::DeniedRead)
            .map_err(|_| unavailable())?;
        let denied_write = fixtures
            .get(selftest::Kind::DeniedWrite)
            .map_err(|_| unavailable())?;

        let (outcome, stdout) = self
            .run_probe(caller, deadline, &["/bin/sh", "-c", "exit 0"], None)
            .await?;
        if !successful(&outcome) || !stdout.is_empty() {
            return Err(unavailable());
        }

        let (outcome, stdout) = self
            .run_probe(caller, deadline, &["/bin/cat", read_fixture.path()], None)
            .await?;
        if !successful(&outcome)
            || stdout != allowed_read.as_bytes()
            || read_fixture.validate_contents(allowed_read).is_err()
        {
            return Err(unavailable());
        }

        let arguments = fixtures
            .write_probe_arguments()
            .map_err(|_| unavailable())?;
        let mut write_probe: Vec<&str> = vec![
            "/usr/bin/perl",
            "-MFcntl=O_WRONLY,O_NOFOLLOW",
            "-e",
            WRITE_PROBE_SCRIPT,
            allowed_write,
        ];
        write_probe.extend(arguments.iter().map(String::as_str));
        let (outcome, stdout) = self
            .run_probe(caller, deadline, &write_probe, Some(fixtures))
            .await?;
        if !successful(&outcome) || !stdout.is_empty() {
            return Err(unavailable());
        }
        if fixtures.validate_written_contents(allowed_write).is_err() {
            return Err(unavailable());
        }

        let (outcome, stdout) = self
            .run_probe(caller, deadline, &["/bin/cat", denied_read.path()], None)
            .await?;
        if !denied_result(&outcome)
            || !stdout.is_empty()
            || denied_read.validate_contents(denied).is_err()
        {
            return Err(unavailable());
        }

        let (outcome, stdout) = self
            .run_probe(
                caller,
                deadline,
                &[
                    "/bin/sh",
                    "-c",
                    r#"printf changed > "$1""#,
                    "probe",
                    denied_write.path(),
                ],
                None,
            )
            .await?;
        if !denied_result(&outcome)
            || !stdout.is_empty()
            || denied_write.validate_contents(denied).is_err()
        {
            return Err(unavailable());
        }
        Ok(())
    }

    /// Runs one probe under the real profile with a fixed, minimal environment.
    ///
    /// `revalidate` is re-checked immediately before dispatch, closing the
    /// window between building the probe's arguments and the child starting.
    async fn run_probe(
        &self,
        caller: &CancellationToken,
        deadline: &CancellationToken,
        argv: &[&str],
        revalidate: Option<&selftest::Fixtures>,
    ) -> Result<(Outcome, Vec<u8>), Error> {
        let unavailable = || Error::unavailable(UnavailableReason::SelfTestFailed);
        check_cancelled(caller)?;
        check_cancelled(deadline)?;

        let mut stdout = BoundedCollector::new(SELF_TEST_OUTPUT_LIMIT);
        let mut ordinary = BoundedCollector::new(SELF_TEST_OUTPUT_LIMIT);
        let mut stderr = StderrFilter::new(&mut ordinary, None);

        let home = self.state.directories.home.to_string_lossy().into_owned();
        let temp = self.state.directories.temp.to_string_lossy().into_owned();
        let spec = Spec {
            path: SANDBOX_EXEC_PATH.to_string(),
            args: self.sandbox_exec_args(argv.iter().map(|value| (*value).to_string())),
            directory: std::path::PathBuf::from(&self.workspace),
            environment: vec![
                "PATH=/usr/bin:/bin".to_string(),
                format!("HOME={home}"),
                format!("TMPDIR={temp}"),
                "LC_ALL=C".to_string(),
            ],
        };
        if let Some(fixtures) = revalidate {
            fixtures
                .validate_before_write_dispatch()
                .map_err(|_| unavailable())?;
            check_cancelled(caller)?;
            check_cancelled(deadline)?;
        }

        let (outcome, result) = self
            .processes
            .run(
                spec,
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                deadline,
            )
            .await;
        let finish = stderr.finish();
        let infrastructure = stderr.infrastructure_failure();
        if result.is_err()
            || finish.is_err()
            || infrastructure
            || stdout.overflowed()
            || ordinary.overflowed()
        {
            return Err(unavailable());
        }
        Ok((outcome, stdout.into_bytes()))
    }

    /// `-f <profile> -- <argv...>`, the only argument shape this driver uses.
    fn sandbox_exec_args(&self, argv: impl Iterator<Item = String>) -> Vec<String> {
        let mut args = vec![
            "-f".to_string(),
            self.profile_path.clone(),
            "--".to_string(),
        ];
        args.extend(argv);
        args
    }

    /// Marks the sandbox itself as broken so no later execution runs.
    fn poison(&self) {
        let mut inner = self.inner.lock().expect("seatbelt driver state");
        inner.poisoned = true;
    }

    /// Whether `request` is one this driver will run.
    ///
    /// The executor validates too; the driver repeats the check because it is
    /// also reachable directly and the directory must be inside *this*
    /// driver's workspace, which only the driver knows.
    fn valid_request(&self, request: &Request) -> bool {
        if request.argv.is_empty() || request.argv.iter().any(|arg| arg.contains('\0')) {
            return false;
        }
        self.valid_directory(&request.dir) && valid_environment(&request.env)
    }

    fn valid_directory(&self, directory: &std::path::Path) -> bool {
        let Some(text) = directory.to_str() else {
            return false;
        };
        if text.is_empty()
            || !profile::is_absolute(text)
            || profile::clean(text) != text
            || text.contains('\0')
        {
            return false;
        }
        let Ok(canonical) = profile::canonical_filesystem_path(text) else {
            return false;
        };
        if canonical != text {
            return false;
        }
        std::fs::symlink_metadata(text)
            .is_ok_and(|info| info.is_dir() && !info.file_type().is_symlink())
            && profile::path_within(&self.workspace, text)
    }
}

/// The Perl program the allowed-write probe runs.
///
/// It re-opens each writable fixture by path with `O_NOFOLLOW`, checks the
/// descriptor's device, inode, type, owner, permissions and link count against
/// the values the host measured, then truncates and rewrites it. A mismatch
/// exits non-zero rather than writing, so a fixture swapped between the host's
/// last check and the child's open is caught inside the sandbox.
const WRITE_PROBE_SCRIPT: &str = r#"my $value = $ARGV[0]; for my $base (1, 8) { my ($path, $device, $inode, $type, $uid, $mode, $links) = @ARGV[$base..$base+6]; sysopen(my $fh, $path, O_WRONLY|O_NOFOLLOW) or exit 65; my @stat = stat($fh); @stat or exit 66; $stat[0] == $device && $stat[1] == $inode && ($stat[2] & 0170000) == $type && $stat[4] == $uid && ($stat[2] & 07777) == $mode && $stat[3] == $links or exit 67; truncate($fh, 0) or exit 68; my $written = 0; while ($written < length($value)) { my $count = syswrite($fh, $value, length($value) - $written, $written); defined($count) && $count > 0 or exit 69; $written += $count; } close($fh) or exit 70; }"#;

#[async_trait]
impl Driver for SeatbeltDriver {
    fn id(&self) -> DriverId {
        DriverId::new(super::ID)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read_confinement: true,
            write_confinement: true,
            unix_socket_deny: true,
            network_allow: self.network == NetworkMode::Allow,
            network_deny: self.network == NetworkMode::Deny,
        }
    }

    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>) {
        if !self.valid_request(&request) {
            return (ExitStatus::default(), Err(Error::InvalidRequest));
        }
        if let Err(error) = check_cancelled(cancel) {
            return (ExitStatus::default(), Err(error));
        }
        {
            let inner = self.inner.lock().expect("seatbelt driver state");
            if inner.closed {
                return (ExitStatus::default(), Err(Error::Closed));
            }
            if inner.poisoned {
                return (
                    ExitStatus::default(),
                    Err(Error::unavailable(UnavailableReason::RuntimeFailure)),
                );
            }
        }
        if let Err(error) = check_cancelled(cancel) {
            return (ExitStatus::default(), Err(error));
        }

        let spec = Spec {
            path: SANDBOX_EXEC_PATH.to_string(),
            args: self.sandbox_exec_args(request.argv.into_iter()),
            directory: request.dir,
            environment: request.env,
        };
        let latch = || self.poison();
        let mut stderr = StderrFilter::new(streams.stderr, Some(&latch));
        let (outcome, result) = self
            .processes
            .run(
                spec,
                Streams {
                    stdout: streams.stdout,
                    stderr: &mut stderr,
                },
                cancel,
            )
            .await;
        let finish = stderr.finish();
        let filter_failure = stderr.infrastructure_failure();
        drop(stderr);

        let infrastructure = filter_failure
            || matches!(&result, Err(error) if infrastructure_error(error))
            || finish.is_err();
        if infrastructure {
            self.poison();
        }
        let status = ExitStatus {
            code: outcome.code,
            signaled: outcome.signaled,
            signal: outcome.signal,
        };
        if check_cancelled(cancel).is_err() {
            return (status, Err(Error::Cancelled));
        }
        if infrastructure {
            return (
                ExitStatus::default(),
                Err(Error::unavailable(UnavailableReason::RuntimeFailure)),
            );
        }
        (status, bounded_execution_error(result))
    }

    fn close(&self) -> Result<(), Error> {
        let mut inner = self.inner.lock().expect("seatbelt driver state");
        if inner.closed {
            while !inner.close_done {
                inner = self.progress.wait(inner).expect("seatbelt driver state");
            }
            return inner.close_result.clone().unwrap_or(Ok(()));
        }
        inner.closed = true;
        drop(inner);

        let manager = self.processes.close();
        let cleanup = self.state.close();
        let result = match (manager, cleanup) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(_)) => Err(Error::unavailable(UnavailableReason::RuntimeFailure)),
            (Ok(()), Ok(())) => Ok(()),
        };

        let mut inner = self.inner.lock().expect("seatbelt driver state");
        inner.close_result = Some(result.clone());
        inner.close_done = true;
        drop(inner);
        self.progress.notify_all();
        result
    }
}

fn successful(outcome: &Outcome) -> bool {
    outcome.code == 0 && !outcome.signaled && outcome.signal.is_empty()
}

fn denied_result(outcome: &Outcome) -> bool {
    outcome.code > 0 && !outcome.signaled && outcome.signal.is_empty()
}

fn check_cancelled(cancel: &CancellationToken) -> Result<(), Error> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Whether a manager failure means the sandbox machinery broke rather than the
/// command. Cancellation and closure are ordinary; everything else is not.
fn infrastructure_error(error: &Error) -> bool {
    !matches!(error, Error::Cancelled | Error::Closed)
}

/// Reduces a manager failure to the bounded set the caller may see.
fn bounded_execution_error(result: Result<(), Error>) -> Result<(), Error> {
    match result {
        Ok(()) => Ok(()),
        Err(Error::Cancelled) => Err(Error::Cancelled),
        Err(Error::Closed) => Err(Error::Closed),
        Err(_) => Err(Error::unavailable(UnavailableReason::RuntimeFailure)),
    }
}

/// Whether `/usr/bin/sandbox-exec` is the canonical, root-owned, executable
/// regular file it must be. Anything else means the host cannot confine.
fn valid_sandbox_exec() -> bool {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(canonical) = profile::canonical_filesystem_path(SANDBOX_EXEC_PATH) else {
        return false;
    };
    if canonical != SANDBOX_EXEC_PATH {
        return false;
    }
    std::fs::symlink_metadata(SANDBOX_EXEC_PATH).is_ok_and(|info| {
        info.is_file()
            && !info.file_type().is_symlink()
            && info.uid() == 0
            && info.permissions().mode() & 0o111 != 0
    })
}

/// Canonicalizes `path` and requires it to be an existing directory.
fn resolve_directory(path: &str) -> Option<String> {
    match profile::resolve_path(path) {
        Ok(resolved)
            if profile::valid_resolved(&resolved)
                && resolved.kind == profile::PathKind::Directory =>
        {
            Some(resolved.path)
        }
        _ => None,
    }
}

/// The platform user cache directory, Go's `os.UserCacheDir` on macOS.
fn user_cache_directory() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    if home.is_empty() || !std::path::Path::new(&home).is_absolute() {
        return None;
    }
    Some(format!("{home}/Library/Caches"))
}

/// `NAME=VALUE` entries with POSIX-ish names, no NUL and no duplicates.
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
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if first != '_' && !first.is_ascii_alphabetic() {
        return false;
    }
    characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

/// A writer that keeps at most `limit` bytes and remembers if more arrived.
struct BoundedCollector {
    limit: usize,
    buffer: Vec<u8>,
    overflow: bool,
}

impl BoundedCollector {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            buffer: Vec::new(),
            overflow: false,
        }
    }

    fn overflowed(&self) -> bool {
        self.overflow
    }

    fn into_bytes(self) -> Vec<u8> {
        self.buffer
    }
}

impl std::io::Write for BoundedCollector {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.buffer.len());
        if remaining < data.len() {
            self.overflow = true;
        }
        self.buffer
            .extend_from_slice(&data[..remaining.min(data.len())]);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Separates `sandbox-exec`'s own diagnostics from the child's stderr.
///
/// Bytes are buffered only while the first line could still turn out to be a
/// `sandbox-exec:` diagnostic, at most [`STDERR_DECISION_LIMIT`] of them. Once
/// the output is known to be ordinary the buffer is flushed and every later
/// write passes straight through.
struct StderrFilter<'a> {
    destination: &'a mut (dyn std::io::Write + Send),
    pending: Vec<u8>,
    ordinary: bool,
    infrastructure: bool,
    execvp_suppressed: bool,
    write_failed: bool,
    on_infrastructure: Option<&'a (dyn Fn() + Sync)>,
}

impl<'a> StderrFilter<'a> {
    fn new(
        destination: &'a mut (dyn std::io::Write + Send),
        on_infrastructure: Option<&'a (dyn Fn() + Sync)>,
    ) -> Self {
        Self {
            destination,
            pending: Vec::new(),
            ordinary: false,
            infrastructure: false,
            execvp_suppressed: false,
            write_failed: false,
            on_infrastructure,
        }
    }

    /// Flushes whatever is still buffered and reports whether the caller's
    /// writer failed. A withheld diagnostic is discarded, not flushed.
    fn finish(&mut self) -> Result<(), Error> {
        if !self.ordinary
            && !self.infrastructure
            && !self.execvp_suppressed
            && !self.pending.is_empty()
        {
            self.classify_pending();
        }
        if self.infrastructure || self.execvp_suppressed {
            self.pending.clear();
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        if self.write_ordinary(&pending).is_err() || self.write_failed {
            return Err(Error::ChildWait);
        }
        Ok(())
    }

    fn infrastructure_failure(&self) -> bool {
        self.infrastructure
    }

    fn classify_pending(&mut self) {
        if self.pending.starts_with(POLICY_EXEC_PREFIX) {
            self.execvp_suppressed = true;
            self.pending.clear();
            return;
        }
        if self.pending.starts_with(DIAGNOSTIC_PREFIX) {
            if let Some(latch) = self.on_infrastructure {
                latch();
            }
            self.infrastructure = true;
            self.pending.clear();
            return;
        }
        self.ordinary = true;
    }

    fn write_ordinary(&mut self, data: &[u8]) -> std::io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if let Err(error) = self.destination.write_all(data) {
            self.write_failed = true;
            return Err(error);
        }
        Ok(())
    }
}

impl std::io::Write for StderrFilter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let original = data.len();
        if self.infrastructure || self.execvp_suppressed {
            return Ok(original);
        }
        if self.ordinary {
            self.write_ordinary(data)?;
            return Ok(original);
        }

        let mut data = data;
        while !data.is_empty() && !self.ordinary && !self.infrastructure && !self.execvp_suppressed
        {
            let remaining = STDERR_DECISION_LIMIT.saturating_sub(self.pending.len());
            if remaining == 0 {
                self.classify_pending();
                break;
            }
            let mut take = data.len().min(remaining);
            if let Some(newline) = data[..take].iter().position(|byte| *byte == b'\n') {
                take = newline + 1;
            }
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];

            if !could_begin_diagnostic(&self.pending) {
                self.ordinary = true;
            } else if self.pending.contains(&b'\n') || self.pending.len() == STDERR_DECISION_LIMIT {
                self.classify_pending();
            }
        }
        if self.infrastructure || self.execvp_suppressed {
            return Ok(original);
        }
        if self.ordinary {
            let pending = std::mem::take(&mut self.pending);
            self.write_ordinary(&pending)?;
            self.write_ordinary(data)?;
        }
        Ok(original)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Whether `value` is still a possible prefix of a `sandbox-exec:` diagnostic.
fn could_begin_diagnostic(value: &[u8]) -> bool {
    if value.len() <= DIAGNOSTIC_PREFIX.len() {
        value == &DIAGNOSTIC_PREFIX[..value.len()]
    } else {
        value.starts_with(DIAGNOSTIC_PREFIX)
    }
}

/// The shared driver contract, run against Seatbelt in both network modes.
///
/// Port of Go's `TestSeatbeltDriverContract`. Go gates the file with a
/// `darwin` build tag; here every check skips with a printed reason when the
/// host cannot run `sandbox-exec`, so the suite stays green off macOS and
/// inside a nested sandbox.
#[cfg(test)]
mod contract {
    use std::path::Path;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use super::{Options, SANDBOX_EXEC_PATH, SeatbeltDriver};
    use crate::sandbox::conformance::{Contract, Fixture};
    use crate::sandbox::{Driver, NetworkMode, Request};

    /// The probe Go does not need: a nested or Linux host has `sandbox-exec`
    /// absent or refusing the most permissive profile there is.
    fn unavailable_reason() -> Option<String> {
        if !std::path::Path::new(SANDBOX_EXEC_PATH).exists() {
            return Some(format!("{SANDBOX_EXEC_PATH} is absent"));
        }
        let probe = std::process::Command::new(SANDBOX_EXEC_PATH)
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .output();
        match probe {
            Ok(output) if output.status.success() => None,
            Ok(output) => Some(format!(
                "{SANDBOX_EXEC_PATH} rejected the permissive probe profile: {}",
                output.status
            )),
            Err(error) => Some(format!("{SANDBOX_EXEC_PATH} could not run: {error}")),
        }
    }

    /// Creates `name` under the fixture base at mode 0700 and canonicalizes it,
    /// matching Go's `canonicalDriverTestDirectory`.
    fn private_directory(fixture: &Fixture, name: &str) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let path = fixture.base.join(name);
        std::fs::create_dir_all(&path).expect("driver directory");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("driver directory mode");
        std::fs::canonicalize(&path)
            .expect("canonical driver directory")
            .to_str()
            .expect("driver directory text")
            .to_string()
    }

    #[derive(Debug, Clone, Copy)]
    struct SeatbeltContract {
        network: NetworkMode,
    }

    #[async_trait]
    impl Contract for SeatbeltContract {
        fn skip_reason(&self) -> Option<String> {
            unavailable_reason()
        }

        async fn new_driver(&self, fixture: &Fixture) -> Arc<dyn Driver> {
            let options = Options {
                workspace: fixture.workspace.to_str().expect("workspace").to_string(),
                shell: "/bin/sh".to_string(),
                home: private_directory(fixture, "host-home"),
                cache_base: private_directory(fixture, "user-cache"),
                host_entries: fixture.environment.clone(),
                read_paths: vec![
                    fixture
                        .allowed_read
                        .to_str()
                        .expect("allowed read")
                        .to_string(),
                ],
                network: Some(self.network),
            };
            let driver = SeatbeltDriver::open(options, &CancellationToken::new())
                .await
                .expect("seatbelt open");
            Arc::new(driver)
        }

        fn request(&self, fixture: &Fixture, argv: Vec<String>) -> Request {
            Request {
                argv,
                dir: fixture.workspace.clone(),
                env: fixture.environment.clone(),
            }
        }

        fn shell_command(&self, script: &str) -> Vec<String> {
            vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
        }

        fn tcp_client(&self, host: &str, port: &str) -> Vec<String> {
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf contract | /usr/bin/nc -w 5 "$1" "$2""#.to_string(),
                "conformance".to_string(),
                host.to_string(),
                port.to_string(),
            ]
        }

        fn unix_client(&self, path: &Path) -> Vec<String> {
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf contract | /usr/bin/nc -U "$1""#.to_string(),
                "conformance".to_string(),
                path.display().to_string(),
            ]
        }

        /// Go sets the same flag: `sandbox-exec` serialises profile
        /// compilation, so sixteen simultaneous callers add nothing.
        fn skip_concurrent_calls(&self) -> bool {
            true
        }
    }

    mod network_allow {
        use super::SeatbeltContract;
        use crate::sandbox::NetworkMode;

        crate::sandbox::conformance::driver_contract!(SeatbeltContract {
            network: NetworkMode::Allow
        });
    }

    mod network_deny {
        use super::SeatbeltContract;
        use crate::sandbox::NetworkMode;

        crate::sandbox::conformance::driver_contract!(SeatbeltContract {
            network: NetworkMode::Deny
        });
    }
}
