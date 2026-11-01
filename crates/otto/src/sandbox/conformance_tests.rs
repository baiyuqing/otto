//! The shared driver conformance suite.
//!
//! Every [`Driver`] implementation runs the same checks through a [`Contract`]
//! adapter, so a new driver cannot advertise a capability it does not enforce.
//!
//! Ownership: [`Fixture`] owns a temporary tree that lives until the last clone
//! is dropped, which also removes the workspace on a panicking check.
//!
//! Concurrency: the checks drive an [`Executor`] from several tasks and threads
//! at once and require a multi-threaded Tokio runtime, because
//! [`Driver::close`] blocks until active executions finish.
//!
//! Shapes the checks are forced into:
//!
//! * The host environment is proved absent by the exact-match assertion alone;
//!   `std::env::set_var` is `unsafe` and this workspace denies `unsafe_code`.
//! * `std::process::Command` keeps the child environment in a `BTreeMap`, so
//!   the dump arrives name-sorted. The check compares sorted vectors and still
//!   requires the exact set.
//! * A `Request` is moved into [`CommandExecutor::execute`], so the caller
//!   holds no alias to mutate after the execution starts; the clone check
//!   keeps only the observable half.
//! * The TCP and Unix clients are `/usr/bin/nc` rather than a re-exec of the
//!   test binary, which removes the helper-process gate and keeps the client
//!   inside the reviewed `/usr/bin` root.
//! * The deny checks poll a non-blocking listener instead of closing it from
//!   another thread, which `std::net` cannot do safely.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use super::{
    Capabilities, CommandExecutor, Driver, DriverId, Error, Executor, ExitStatus, FilesystemMode,
    NetworkMode, Policy, Request, Streams,
};

mod confinement_checks;
mod git_metadata_checks;
mod lifecycle_checks;
mod processes;
mod recorders;
mod setup;
pub(crate) use confinement_checks::*;
pub(crate) use git_metadata_checks::*;
pub(crate) use lifecycle_checks::*;
use processes::*;
pub(crate) use recorders::*;
use setup::*;

/// How long any wait may take before the check fails.
const AWAIT: Duration = Duration::from_secs(10);

/// How long a signalled descendant may take to leave the process table before
/// [`assert_gone`] calls it a survivor.
const SETTLE: Duration = Duration::from_secs(2);
/// How often [`assert_gone`] re-probes within [`SETTLE`].
const SETTLE_INTERVAL: Duration = Duration::from_millis(10);

/// The temporary tree every check runs against.
///
/// Cloning shares the tree; `environment` is the only field a check varies.
#[derive(Debug, Clone)]
pub(crate) struct Fixture {
    pub(crate) base: PathBuf,
    pub(crate) workspace: PathBuf,
    /// A directory the driver is configured to deny reading.
    pub(crate) denied_dir: PathBuf,
    /// A file in `denied_dir` that no read exception covers.
    pub(crate) outside_file: PathBuf,
    /// A file in `denied_dir` that a read exception covers.
    pub(crate) allowed_read: PathBuf,
    /// A file outside every denied path.
    pub(crate) visible_file: PathBuf,
    pub(crate) unix_socket: PathBuf,
    pub(crate) environment: Vec<String>,
    pub(crate) policy: Policy,
    _root: Arc<tempfile::TempDir>,
}

impl Fixture {
    /// Returns a copy whose environment carries one more `name=value` entry.
    fn with_environment(&self, name: &str, value: &Path) -> Self {
        let mut fixture = self.clone();
        fixture
            .environment
            .push(format!("{name}={}", value.display()));
        fixture
    }

    fn environment_value(&self, name: &str) -> String {
        let prefix = format!("{name}=");
        self.environment
            .iter()
            .find_map(|entry| entry.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("environment does not contain {name}"))
            .to_string()
    }
}

/// The driver-specific half of the suite.
///
/// Implementations are cheap value types constructed once per check; every
/// method must be deterministic and offline.
#[async_trait]
pub(crate) trait Contract: Send + Sync {
    /// `Some(reason)` skips every check and prints the reason.
    fn skip_reason(&self) -> Option<String> {
        None
    }

    /// Opens a driver for `fixture`. Panics rather than returning an error, so
    /// a failed check fails the test.
    async fn new_driver(&self, fixture: &Fixture) -> Arc<dyn Driver>;

    fn request(&self, fixture: &Fixture, argv: Vec<String>) -> Request;

    fn shell_command(&self, script: &str) -> Vec<String>;

    fn tcp_client(&self, host: &str, port: &str) -> Vec<String>;

    fn unix_client(&self, path: &Path) -> Vec<String>;

    /// Set by drivers whose external helper serialises executions.
    fn skip_concurrent_calls(&self) -> bool {
        false
    }
}

/// Declares one conformance check as a `#[tokio::test]`.
macro_rules! contract_check {
    ($case:expr, $check:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $check() {
            let case = $case;
            if let Some(reason) = $crate::sandbox::conformance_tests::Contract::skip_reason(&case) {
                eprintln!("SKIP {}: {reason}", stringify!($check));
                return;
            }
            $crate::sandbox::conformance_tests::$check(&case).await;
        }
    };
}

/// Declares the whole shared driver contract for one [`Contract`] adapter.
macro_rules! driver_contract {
    ($case:expr) => {
        $crate::sandbox::conformance_tests::contract_check!(
            $case,
            ordinary_and_nonzero_exits_with_separate_streams
        );
        $crate::sandbox::conformance_tests::contract_check!(
            $case,
            supplied_environment_exactly_replaces_host_environment
        );
        $crate::sandbox::conformance_tests::contract_check!(
            $case,
            pre_cancellation_does_not_start_a_child
        );
        $crate::sandbox::conformance_tests::contract_check!(
            $case,
            deadline_cancellation_removes_the_process_group
        );
        $crate::sandbox::conformance_tests::contract_check!(
            $case,
            an_ignored_term_escalates_to_kill_and_removes_the_group
        );
        $crate::sandbox::conformance_tests::contract_check!(
            $case,
            normal_leader_exit_removes_a_background_process
        );
        $crate::sandbox::conformance_tests::contract_check!(
            $case,
            driver_close_drains_active_work_and_is_idempotent
        );
        $crate::sandbox::conformance_tests::contract_check!(
            $case,
            executor_clones_requests_before_delegation
        );
        $crate::sandbox::conformance_tests::contract_check!(
            $case,
            canonical_paths_with_spaces_unicode_and_quotes
        );
        $crate::sandbox::conformance_tests::contract_check!($case, validation_errors_are_bounded);
        $crate::sandbox::conformance_tests::contract_check!($case, concurrent_calls_complete);
        $crate::sandbox::conformance_tests::contract_check!($case, advertised_read_confinement);
        $crate::sandbox::conformance_tests::contract_check!($case, advertised_write_confinement);
        $crate::sandbox::conformance_tests::contract_check!($case, advertised_local_tcp_allow);
        $crate::sandbox::conformance_tests::contract_check!($case, advertised_local_tcp_deny);
        $crate::sandbox::conformance_tests::contract_check!($case, advertised_unix_socket_deny);
    };
}

pub(crate) use {contract_check, driver_contract};

#[cfg(test)]
mod tests;
