//! The Seatbelt [`Driver`].
//!
//! Every child is run as `/usr/bin/sandbox-exec -f <profile> -- <argv>` with a
//! profile generated from the session's workspace, its private state tree and
//! the reviewed read roots.
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
//! Two error shapes worth noting: [`Error`] is a closed enum with no join, so
//! a cancellation that arrives alongside a secondary failure wins and the
//! secondary failure is dropped; and a private-state cleanup failure has no
//! variant of its own, surfacing as [`UnavailableReason::RuntimeFailure`].

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{profile, selftest};
use super::{state, violations};
use crate::sandbox::process::{Manager, Outcome, Spec};
use crate::sandbox::{
    Capabilities, DEFAULT_CANCELLATION_GRACE, Driver, DriverId, Error, ExitStatus, NetworkMode,
    PrivateDirectories, Request, Streams, UnavailableReason,
};

mod checks;
mod driver_impl;
mod startup_probe;
mod stderr_filter;
use checks::*;
use stderr_filter::*;

/// How long a failed command waits for the log stream to deliver denials.
const DENIAL_DELIVERY_WAIT: Duration = Duration::from_millis(300);
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
    /// `[sandbox] read_paths`: exceptions inside the read denylist.
    pub read_paths: Vec<String>,
    /// `[sandbox] deny_read_paths`: additions to the built-in read denylist.
    pub deny_read_paths: Vec<String>,
    pub workspaces: Vec<String>,
    /// `None` is rejected; the mode must be explicit.
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

/// The profile options in effect and the text generated from them.
#[derive(Debug)]
struct Active {
    options: profile::Options,
    text: String,
    /// `read_paths` entries the profile omitted because they lie below no
    /// denied path, as written.
    inert_read_paths: Vec<String>,
    /// Built-in denylist entries the profile omitted because they contain a
    /// writable root, as `~/...` text.
    skipped_deny_paths: Vec<String>,
}

/// Runs commands under `/usr/bin/sandbox-exec`.
pub struct SeatbeltDriver {
    workspace: String,
    /// The profile options and text now in the profile file.
    profile: Mutex<Active>,
    state: state::State,
    profile_path: String,
    processes: Manager,
    inner: Mutex<Inner>,
    progress: Condvar,
    /// Streams the sandbox's denial events; `None` when `log` cannot start.
    monitor: Option<violations::Monitor>,
}

impl std::fmt::Debug for SeatbeltDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SeatbeltDriver")
            .field("network", &self.network())
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
    /// workspace, home or network mode the profile cannot express, which
    /// includes a workspace containing the cache base that holds the private
    /// state tree, and [`UnavailableReason::SelfTestFailed`] when the state,
    /// the profile or any startup probe fails. A cancelled token yields
    /// [`Error::Cancelled`].
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
        // `state::create` requires the cache base to lie outside the workspace
        // so no workspace-relative symlink can reach the private state tree.
        // A workspace that contains the cache base cannot satisfy that, which
        // is the shape of `$HOME`: it contains `$HOME/Library/Caches`.
        if resolve_directory(&cache_base)
            .is_some_and(|cache| profile::path_within(&workspace, &cache))
        {
            return Err(Error::unavailable(UnavailableReason::PolicyUnsupported));
        }
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
        let profile_options = profile::Options {
            workspace: workspace.clone(),
            workspaces: options.workspaces.clone(),
            directories: private.directories.clone(),
            shell,
            home,
            host_entries: Vec::new(),
            read_paths: options.read_paths.clone(),
            deny_read_paths: options.deny_read_paths.clone(),
            network: Some(network),
        };
        let generated = profile::generate(&profile_options);
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
        let monitor = private
            .directories
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(violations::Monitor::start);
        let driver = Self {
            workspace,
            profile: Mutex::new(Active {
                options: profile_options,
                text,
                inert_read_paths: Vec::new(),
                skipped_deny_paths: Vec::new(),
            }),
            state: private,
            profile_path,
            processes: Manager::default(),
            inner: Mutex::new(Inner::default()),
            progress: Condvar::new(),
            monitor,
        };
        match driver.run_startup_self_test(cancel).await {
            Ok(()) => Ok(driver),
            Err(error) => {
                let _ = driver.processes.close();
                driver.close_monitor();
                Err((driver.state, error))
            }
        }
    }

    /// The read paths in the profile now in effect.
    pub fn read_paths(&self) -> Vec<String> {
        self.profile
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .options
            .read_paths
            .clone()
    }

    /// The configured denylist additions in the profile now in effect.
    pub fn deny_read_paths(&self) -> Vec<String> {
        self.profile
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .options
            .deny_read_paths
            .clone()
    }

    /// The `read_paths` entries the profile now in effect omitted because
    /// they lie below no denied path, as written.
    pub fn inert_read_paths(&self) -> Vec<String> {
        self.profile
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .inert_read_paths
            .clone()
    }

    /// The built-in denylist entries the profile now in effect omitted
    /// because they contain a writable root, as `~/...` text.
    pub fn skipped_deny_paths(&self) -> Vec<String> {
        self.profile
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .skipped_deny_paths
            .clone()
    }

    /// The canonical Git worktree roots in the profile now in effect.
    pub fn workspaces(&self) -> Vec<String> {
        self.profile
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .options
            .workspaces
            .clone()
    }

    /// The network mode in the profile now in effect.
    pub fn network(&self) -> Option<NetworkMode> {
        self.profile
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .options
            .network
    }

    /// Rewrites the profile for new read paths and network mode, keeping the
    /// private state tree. The private directories, and so
    /// [`SeatbeltDriver::private_directories`], do not change.
    ///
    /// The profile is generated first, so a rejected input writes nothing. The
    /// new profile is then written and the startup self-test runs under it; a
    /// failure writes the previous text back, and poisons the driver if that
    /// write fails too.
    ///
    /// The caller must guarantee that no `execute` runs concurrently and that
    /// calls to this method are not concurrent with each other.
    /// `sandbox-exec` reads the profile file when each child starts, so a child
    /// that starts during the truncate-and-write can read a partial profile,
    /// and the resulting `sandbox-exec:` diagnostic poisons the driver.
    /// `SandboxSwitch` guarantees this by holding its write lock for the whole
    /// call.
    pub async fn reconfigure(
        &self,
        read_paths: Vec<String>,
        deny_read_paths: Vec<String>,
        workspaces: Vec<String>,
        network: NetworkMode,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        {
            let inner = self.inner.lock().expect("seatbelt driver state");
            if inner.closed {
                return Err(Error::Closed);
            }
            if inner.poisoned {
                return Err(Error::unavailable(UnavailableReason::RuntimeFailure));
            }
        }
        check_cancelled(cancel)?;

        let (previous_options, previous_text) = {
            let active = self
                .profile
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            (active.options.clone(), active.text.clone())
        };
        let next_options = profile::Options {
            read_paths,
            deny_read_paths,
            workspaces,
            network: Some(network),
            ..previous_options
        };
        let next_text = profile::generate(&next_options)
            .map_err(|_| Error::unavailable(UnavailableReason::SelfTestFailed))?;
        if next_text != previous_text {
            let restore = |driver: &Self| {
                if driver
                    .state
                    .write_profile(previous_text.as_bytes())
                    .is_err()
                {
                    driver.poison();
                }
            };
            if self.state.write_profile(next_text.as_bytes()).is_err() {
                restore(self);
                return Err(Error::unavailable(UnavailableReason::SelfTestFailed));
            }
            if let Err(error) = self.run_startup_self_test(cancel).await {
                restore(self);
                return Err(error);
            }
        }
        *self
            .profile
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Active {
            options: next_options,
            text: next_text,
            inert_read_paths: Vec::new(),
            skipped_deny_paths: Vec::new(),
        };
        Ok(())
    }

    /// The private directories the child sees as `HOME` and `TMPDIR`.
    pub fn private_directories(&self) -> PrivateDirectories {
        self.state.directories.clone()
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

    fn close_monitor(&self) {
        if let Some(monitor) = &self.monitor {
            monitor.close();
        }
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
const WRITE_PROBE_SCRIPT: &str = concat!(
    r#"my $value = $ARGV[0]; for my $base (1, 8) { "#,
    r#"my ($path, $device, $inode, $type, $uid, $mode, $links) = @ARGV[$base..$base+6]; "#,
    r#"sysopen(my $fh, $path, O_WRONLY|O_NOFOLLOW) or exit 65; "#,
    r#"my @stat = stat($fh); @stat or exit 66; "#,
    r#"$stat[0] == $device && $stat[1] == $inode && ($stat[2] & 0170000) == $type "#,
    r#"&& $stat[4] == $uid && ($stat[2] & 07777) == $mode && $stat[3] == $links or exit 67; "#,
    r#"truncate($fh, 0) or exit 68; my $written = 0; "#,
    r#"while ($written < length($value)) { "#,
    r#"my $count = syswrite($fh, $value, length($value) - $written, $written); "#,
    r#"defined($count) && $count > 0 or exit 69; $written += $count; } "#,
    r#"close($fh) or exit 70; }"#,
);

#[cfg(test)]
/// The availability probe: a nested or Linux host has `sandbox-exec` absent
/// or refusing the most permissive profile there is.
pub(crate) fn unavailable_reason() -> Option<String> {
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

/// The shared driver contract, run against Seatbelt in both network modes.
///
/// Every check skips with a printed reason when the host cannot run
/// `sandbox-exec`, so the suite stays green off macOS and inside a nested
/// sandbox.
#[cfg(test)]
mod contract_tests;

#[cfg(test)]
mod tests;
