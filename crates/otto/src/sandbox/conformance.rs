//! The shared driver conformance suite.
//!
//! Port of the Go package `internal/sandbox/sandboxtest`. Every [`Driver`]
//! implementation runs the same checks through a [`Contract`] adapter, so a
//! new driver cannot advertise a capability it does not enforce.
//!
//! Ownership: [`Fixture`] owns a temporary tree that lives until the last
//! clone is dropped, which also removes the workspace on a panicking check.
//!
//! Concurrency: the checks drive an [`Executor`] from several tasks and
//! threads at once and require a multi-threaded Tokio runtime, because
//! [`Driver::close`] blocks until active executions finish.
//!
//! Divergences from the Go suite, each unrepresentable rather than skipped:
//!
//! * Go's `t.Setenv` half of the environment check is dropped. Rust's
//!   `std::env::set_var` is `unsafe` and this workspace denies `unsafe_code`.
//!   The exact-match assertion already proves the host environment is absent.
//! * `std::process::Command` keeps the child environment in a `BTreeMap`, so
//!   the dump arrives name-sorted. The check compares sorted vectors and still
//!   requires the exact set.
//! * Go mutates the caller's `Request` after `Execute` starts. A `Request` is
//!   moved into [`CommandExecutor::execute`] here, so the caller holds no
//!   alias; the clone check keeps only the observable half.
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

/// How long any wait may take before the check fails, matching Go's 10s.
const AWAIT: Duration = Duration::from_secs(10);

/// The temporary tree every check runs against.
///
/// Cloning shares the tree; `environment` is the only field a check varies.
#[derive(Debug, Clone)]
pub(crate) struct Fixture {
    pub(crate) base: PathBuf,
    pub(crate) workspace: PathBuf,
    pub(crate) outside_file: PathBuf,
    pub(crate) allowed_read: PathBuf,
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

    /// Opens a driver for `fixture`. Panics rather than returning an error,
    /// matching Go's `testing.TB.Fatal`.
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

fn new_fixture() -> Fixture {
    // Go pins the tree to `/tmp` for the same reason: the Unix socket below
    // must stay inside the 104-byte `sun_path`, which a `$TMPDIR` under
    // `/var/folders` exhausts on its own.
    let root = tempfile::Builder::new()
        .prefix("otto-sandbox-contract-")
        .tempdir_in("/tmp")
        .expect("conformance fixture");
    let base = std::fs::canonicalize(root.path()).expect("canonical fixture base");
    let workspace = base.join("workspace space ü 'quote;()[]");
    std::fs::create_dir(&workspace).expect("workspace");
    set_private(&workspace);
    let outside_file = base.join("outside file");
    let allowed_read = base.join("allowed read file");
    write_private(&outside_file, "outside-data");
    write_private(&allowed_read, "allowed-data");
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
        outside_file,
        allowed_read,
        _root: Arc::new(root),
    }
}

fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("chmod 0700");
}

fn write_private(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, contents).expect("fixture file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod 0600");
}

/// Builds the fixture and reads the driver's capabilities from a probe driver
/// that is closed again, matching Go's `RunDriverContract` preamble.
async fn setup(case: &dyn Contract) -> (Fixture, Capabilities) {
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

fn policy_for_network(capabilities: Capabilities, network: NetworkMode) -> Option<Policy> {
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

async fn open(case: &dyn Contract, fixture: &Fixture) -> (Arc<dyn Driver>, Arc<Executor>) {
    open_with_policy(case, fixture, fixture.policy).await
}

async fn open_with_policy(
    case: &dyn Contract,
    fixture: &Fixture,
    policy: Policy,
) -> (Arc<dyn Driver>, Arc<Executor>) {
    let driver = case.new_driver(fixture).await;
    let executor =
        Executor::new(driver.clone(), policy, &fixture.workspace).expect("sandbox executor");
    (driver, Arc::new(executor))
}

fn shell_request(case: &dyn Contract, fixture: &Fixture, script: &str) -> Request {
    case.request(fixture, case.shell_command(script))
}

async fn run(executor: &Executor, request: Request) -> (ExitStatus, Result<(), Error>, String) {
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

/// A `Write` sink that keeps everything written and publishes complete lines.
///
/// Clones share one buffer, so a clone may be moved into an executing task
/// while the check keeps another to read from.
#[derive(Clone)]
pub(crate) struct Recorder {
    state: Arc<Mutex<Vec<u8>>>,
    reported: Arc<Mutex<usize>>,
    lines: UnboundedSender<String>,
}

impl Recorder {
    fn new() -> (Self, UnboundedReceiver<String>) {
        let (lines, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                state: Arc::new(Mutex::new(Vec::new())),
                reported: Arc::new(Mutex::new(0)),
                lines,
            },
            receiver,
        )
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.state.lock().expect("recorder")).into_owned()
    }
}

impl std::io::Write for Recorder {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut buffer = self.state.lock().expect("recorder");
        let mut reported = self.reported.lock().expect("recorder");
        buffer.extend_from_slice(data);
        while let Some(offset) = buffer[*reported..].iter().position(|byte| *byte == b'\n') {
            let line = String::from_utf8_lossy(&buffer[*reported..*reported + offset]).into_owned();
            *reported += offset + 1;
            let _ = self.lines.send(line);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A one-way latch two threads use to hand off, replacing Go's closed channel.
#[derive(Debug, Default)]
struct Gate {
    opened: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn release(&self) {
        *self.opened.lock().expect("gate") = true;
        self.changed.notify_all();
    }

    fn released(&self) -> bool {
        *self.opened.lock().expect("gate")
    }

    /// Blocks the calling thread until released or [`AWAIT`] elapses.
    fn wait(&self) {
        let mut opened = self.opened.lock().expect("gate");
        let deadline = Instant::now() + AWAIT;
        while !*opened {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            let (guard, timeout) = self
                .changed
                .wait_timeout(opened, remaining)
                .expect("gate wait");
            opened = guard;
            if timeout.timed_out() {
                return;
            }
        }
    }
}

/// A [`Recorder`] whose first write blocks until `release` opens, holding the
/// driver inside its own stream work. Port of Go's `driverWorkBarrierWriter`.
#[derive(Clone)]
struct BarrierRecorder {
    recorder: Recorder,
    entered: Arc<Gate>,
    release: Arc<Gate>,
}

impl std::io::Write for BarrierRecorder {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let written = self.recorder.write(data)?;
        self.entered.release();
        self.release.wait();
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Counts signals from several threads so a check can wait for all of them.
#[derive(Debug, Default)]
struct Counter {
    count: Mutex<usize>,
    changed: Condvar,
}

impl Counter {
    fn add(&self) {
        *self.count.lock().expect("counter") += 1;
        self.changed.notify_all();
    }

    fn wait_for(&self, wanted: usize, description: &str) {
        let mut count = self.count.lock().expect("counter");
        let deadline = Instant::now() + AWAIT;
        while *count < wanted {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for {description}"));
            let (guard, timeout) = self
                .changed
                .wait_timeout(count, remaining)
                .expect("counter wait");
            count = guard;
            if timeout.timed_out() && *count < wanted {
                panic!("timed out waiting for {description}");
            }
        }
    }
}

/// What one `Close` caller observed. Port of Go's `closeDrainObservation`.
#[derive(Debug)]
struct Observation {
    error: Option<Error>,
    returned_while_blocked: bool,
}

/// Wraps a driver to prove `Close` crossed the boundary and only returned
/// after the driver's own stream work was released.
struct CloseDrainObserver {
    inner: Arc<dyn Driver>,
    entered: Counter,
    returned: Counter,
    observations: Mutex<Vec<Observation>>,
    driver_work_released: Arc<Gate>,
}

#[async_trait]
impl Driver for CloseDrainObserver {
    fn id(&self) -> DriverId {
        self.inner.id()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>) {
        self.inner.execute(request, streams, cancel).await
    }

    fn close(&self) -> Result<(), Error> {
        self.entered.add();
        let result = self.inner.close();
        let observation = Observation {
            error: result.clone().err(),
            returned_while_blocked: !self.driver_work_released.released(),
        };
        self.observations
            .lock()
            .expect("observations")
            .push(observation);
        self.returned.add();
        result
    }
}

fn make_fifo(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).expect("mkfifo");
    path
}

/// Opens the FIFO for writing, which unblocks the reader inside the sandbox.
async fn write_fifo(path: &Path, value: &str) {
    let path = path.to_path_buf();
    let value = value.to_string();
    let write = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
        file.write_all(value.as_bytes())
    });
    tokio::time::timeout(AWAIT, write)
        .await
        .expect("timed out opening the release FIFO")
        .expect("FIFO writer task")
        .expect("write FIFO");
}

async fn await_line(lines: &mut UnboundedReceiver<String>, description: &str) -> String {
    tokio::time::timeout(AWAIT, lines.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
        .unwrap_or_else(|| panic!("stream closed before {description}"))
}

async fn await_gate(gate: &Gate, description: &str) {
    let deadline = Instant::now() + AWAIT;
    while !gate.released() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn await_execution<T: Send + 'static>(
    handle: tokio::task::JoinHandle<T>,
    description: &str,
) -> T {
    tokio::time::timeout(AWAIT, handle)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
        .unwrap_or_else(|_| panic!("{description} task panicked"))
}

fn parse_pid(value: &str) -> i32 {
    let pid: i32 = value
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("invalid child PID {value:?}"));
    assert!(pid > 0, "invalid child PID {value:?}");
    pid
}

fn assert_signaled(status: &ExitStatus) {
    assert!(
        status.code == -1 && status.signaled && !status.signal.is_empty(),
        "status = {status:?}, want signal termination"
    );
}

fn assert_gone_once(pid: i32) {
    let result = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None);
    if result.is_ok() {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::SIGKILL);
    }
    assert_eq!(
        result,
        Err(nix::errno::Errno::ESRCH),
        "signal 0 for process {pid} did not report ESRCH"
    );
}

/// Runs `request` on its own task so the check can drive the child meanwhile.
fn spawn_execute<W>(
    executor: Arc<Executor>,
    request: Request,
    mut stdout: W,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<(ExitStatus, Result<(), Error>)>
where
    W: std::io::Write + Send + 'static,
{
    tokio::spawn(async move {
        let mut stderr = std::io::sink();
        executor
            .execute(
                request,
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                &cancel,
            )
            .await
    })
}

// ---------------------------------------------------------------- the checks

pub(crate) async fn ordinary_and_nonzero_exits_with_separate_streams(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;

    let (out, _lines) = Recorder::new();
    let (err, _errlines) = Recorder::new();
    let mut stdout = out.clone();
    let mut stderr = err.clone();
    let cancel = CancellationToken::new();
    let (status, result) = executor
        .execute(
            shell_request(
                case,
                &fixture,
                "printf stdout-contract; printf stderr-contract >&2",
            ),
            Streams {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            &cancel,
        )
        .await;
    result.expect("Execute");
    assert_eq!(
        (status.code, status.signaled, status.signal.as_str()),
        (0, false, ""),
        "zero-exit status"
    );
    assert_eq!(out.text(), "stdout-contract");
    assert_eq!(err.text(), "stderr-contract");

    let (status, result, _) = run(&executor, shell_request(case, &fixture, "exit 37")).await;
    result.expect("nonzero Execute");
    assert_eq!(
        (status.code, status.signaled, status.signal.as_str()),
        (37, false, ""),
        "nonzero status"
    );
    executor.close().expect("close");
}

pub(crate) async fn supplied_environment_exactly_replaces_host_environment(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let request = case.request(&fixture, vec!["/usr/bin/env".to_string()]);
    let expected = request.env.clone();
    assert!(
        !expected.iter().any(|entry| entry.contains(['\r', '\n'])),
        "environment fixture contains a newline"
    );
    let (status, result, dump) = run(&executor, request).await;
    result.expect("Execute");
    assert_eq!(status.code, 0, "env exited nonzero");
    assert!(
        environment_dump_matches(&dump, &expected),
        "complete child environment differs from Request.env: {dump:?}"
    );
    executor.close().expect("close");
}

fn environment_dump_matches(dump: &str, expected: &[String]) -> bool {
    if expected.is_empty() {
        return dump.is_empty();
    }
    if expected.iter().any(|entry| entry.contains(['\r', '\n'])) {
        return false;
    }
    let Some(body) = dump.strip_suffix('\n') else {
        return false;
    };
    let mut actual: Vec<&str> = body.split('\n').collect();
    let mut wanted: Vec<&str> = expected.iter().map(String::as_str).collect();
    actual.sort_unstable();
    wanted.sort_unstable();
    actual == wanted
}

pub(crate) async fn pre_cancellation_does_not_start_a_child(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let marker = fixture.workspace.join("pre-cancel marker");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_MARKER", &marker);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut stdout = std::io::sink();
    let mut stderr = std::io::sink();
    let (_, result) = executor
        .execute(
            shell_request(
                case,
                &local,
                r#"printf started > "$SANDBOX_CONFORMANCE_MARKER""#,
            ),
            Streams {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            &cancel,
        )
        .await;
    assert_eq!(result, Err(Error::Cancelled));
    assert!(
        !marker.exists(),
        "pre-cancelled child created the marker file"
    );
    executor.close().expect("close");
}

pub(crate) async fn deadline_cancellation_removes_the_process_group(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let blocked = make_fifo(&fixture.workspace, "deadline-blocked");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_BLOCK_FIFO", &blocked);
    let (recorder, mut lines) = Recorder::new();
    let cancel = CancellationToken::new();
    let handle = spawn_execute(
        executor.clone(),
        shell_request(
            case,
            &local,
            r#"/bin/sh -c 'echo "$$"; exec /bin/cat "$SANDBOX_CONFORMANCE_BLOCK_FIFO"' descendant & wait"#,
        ),
        recorder,
        cancel.clone(),
    );
    let pid = parse_pid(&await_line(&mut lines, "deadline descendant PID").await);
    cancel.cancel();
    let (status, result) = await_execution(handle, "deadline cancellation").await;
    assert_eq!(result, Err(Error::Cancelled));
    assert_signaled(&status);
    assert_gone_once(pid);
    executor.close().expect("close");
}

pub(crate) async fn normal_leader_exit_removes_a_background_process(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let blocked = make_fifo(&fixture.workspace, "normal-exit-blocked");
    let release = make_fifo(&fixture.workspace, "normal-exit-release");
    let local = fixture
        .with_environment("SANDBOX_CONFORMANCE_BLOCK_FIFO", &blocked)
        .with_environment("SANDBOX_CONFORMANCE_RELEASE_FIFO", &release);
    let (recorder, mut lines) = Recorder::new();
    let handle = spawn_execute(
        executor.clone(),
        shell_request(
            case,
            &local,
            r#"/bin/sh -c 'echo "$$"; exec /bin/cat "$SANDBOX_CONFORMANCE_BLOCK_FIFO"' descendant & IFS= read -r release < "$SANDBOX_CONFORMANCE_RELEASE_FIFO"; exit 0"#,
        ),
        recorder,
        CancellationToken::new(),
    );
    let descendant = parse_pid(&await_line(&mut lines, "background descendant PID").await);
    write_fifo(&release, "continue\n").await;
    let (status, result) = await_execution(handle, "normal leader exit").await;
    result.expect("Execute");
    assert_eq!((status.code, status.signaled), (0, false), "leader status");
    assert_gone_once(descendant);
    executor.close().expect("close");
}

pub(crate) async fn driver_close_drains_active_work_and_is_idempotent(case: &dyn Contract) {
    const CLOSERS: usize = 12;
    let (fixture, _) = setup(case).await;
    let base = case.new_driver(&fixture).await;
    let driver_work_release = Arc::new(Gate::default());
    let observer = Arc::new(CloseDrainObserver {
        inner: base.clone(),
        entered: Counter::default(),
        returned: Counter::default(),
        observations: Mutex::new(Vec::new()),
        driver_work_released: driver_work_release.clone(),
    });
    let executor = Arc::new(
        Executor::new(observer.clone(), fixture.policy, &fixture.workspace).expect("executor"),
    );

    let blocked = make_fifo(&fixture.workspace, "close-blocked");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_BLOCK_FIFO", &blocked);
    let (recorder, mut lines) = Recorder::new();
    let entered_stream_work = Arc::new(Gate::default());
    let barrier = BarrierRecorder {
        recorder,
        entered: entered_stream_work.clone(),
        release: driver_work_release.clone(),
    };
    let (publish, publication) = tokio::sync::oneshot::channel::<()>();
    let awaiting = Arc::new(Gate::default());
    let handle = {
        let executor = executor.clone();
        let request = shell_request(
            case,
            &local,
            r#"echo "$$"; exec /bin/cat "$SANDBOX_CONFORMANCE_BLOCK_FIFO""#,
        );
        let awaiting = awaiting.clone();
        let mut stdout = barrier;
        tokio::spawn(async move {
            let mut stderr = std::io::sink();
            let cancel = CancellationToken::new();
            let completed = executor
                .execute(
                    request,
                    Streams {
                        stdout: &mut stdout,
                        stderr: &mut stderr,
                    },
                    &cancel,
                )
                .await;
            awaiting.release();
            let _ = publication.await;
            completed
        })
    };

    let pid = parse_pid(&await_line(&mut lines, "close leader PID").await);
    await_gate(&entered_stream_work, "blocked driver-owned stream work").await;

    let mut closers = Vec::with_capacity(CLOSERS);
    for _ in 0..CLOSERS {
        let driver: Arc<dyn Driver> = observer.clone();
        closers.push(std::thread::spawn(move || driver.close()));
    }
    observer
        .entered
        .wait_for(CLOSERS, "Driver::close observation delegate entry");

    driver_work_release.release();
    observer.returned.wait_for(CLOSERS, "Driver::close returns");
    for closer in closers {
        closer
            .join()
            .expect("close thread")
            .expect("concurrent Driver::close");
    }
    for observation in observer.observations.lock().expect("observations").iter() {
        assert!(
            !observation.returned_while_blocked && observation.error.is_none(),
            "Driver::close observation = {observation:?}"
        );
    }
    assert_gone_once(pid);
    await_gate(&awaiting, "external Execute result-publication barrier").await;

    let _ = publish.send(());
    let (status, result) = await_execution(handle, "active Execute result publication").await;
    result.expect("active Execute");
    assert_signaled(&status);
    observer.close().expect("idempotent Driver::close");

    let (_, result, _) = run(&executor, shell_request(case, &local, "exit 0")).await;
    assert_eq!(result, Err(Error::Closed));
    base.close().expect("base close");
}

pub(crate) async fn executor_clones_requests_before_delegation(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let release = make_fifo(&fixture.workspace, "clone-release");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_RELEASE_FIFO", &release);
    let request = shell_request(
        case,
        &local,
        r#"printf '%s\n' "$$"; IFS= read -r release < "$SANDBOX_CONFORMANCE_RELEASE_FIFO"; printf 'clone:%s' "$SANDBOX_CONFORMANCE_VISIBLE""#,
    );
    // The caller keeps a copy and mutates it while the execution runs; the
    // child must still observe the values it was started with.
    let mut caller_copy = request.clone();
    let (recorder, mut lines) = Recorder::new();
    let handle = spawn_execute(
        executor.clone(),
        request,
        recorder.clone(),
        CancellationToken::new(),
    );
    let _ = parse_pid(&await_line(&mut lines, "request clone leader PID").await);
    for argument in &mut caller_copy.argv {
        *argument = "mutated-argument".to_string();
    }
    for entry in &mut caller_copy.env {
        *entry = "MUTATED_ENVIRONMENT=value".to_string();
    }
    write_fifo(&release, "continue\n").await;
    let (status, result) = await_execution(handle, "request clone execution").await;
    result.expect("Execute");
    assert_eq!(status.code, 0, "clone execution status");
    assert!(
        recorder.text().contains("clone:visible-value"),
        "cloned request output = {:?}",
        recorder.text()
    );
    executor.close().expect("close");
}

pub(crate) async fn canonical_paths_with_spaces_unicode_and_quotes(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let (status, result, stdout) = run(
        &executor,
        shell_request(
            case,
            &fixture,
            r#"printf special-data > "$SANDBOX_CONFORMANCE_SPECIAL_PATH"; /bin/pwd"#,
        ),
    )
    .await;
    result.expect("Execute");
    assert_eq!(status.code, 0, "special-path status");
    let special = fixture.environment_value("SANDBOX_CONFORMANCE_SPECIAL_PATH");
    assert_eq!(
        std::fs::read_to_string(&special).expect("special-path file"),
        "special-data"
    );
    assert_eq!(
        stdout.trim_end(),
        fixture.workspace.to_str().expect("workspace text"),
        "pwd is not the workspace"
    );
    executor.close().expect("close");
}

pub(crate) async fn validation_errors_are_bounded(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let sensitive = fixture.workspace.join("missing secret path");
    let mut request = shell_request(case, &fixture, "exit 0");
    request.dir = sensitive.clone();
    request.argv.push("secret-argument-value".to_string());
    request
        .env
        .push("SECRET_VALUE=secret-environment-value".to_string());
    let (_, result, _) = run(&executor, request).await;
    assert_eq!(result, Err(Error::InvalidRequest));
    let text = Error::InvalidRequest.to_string();
    for secret in [
        sensitive.to_str().expect("path text"),
        "secret-argument-value",
        "secret-environment-value",
    ] {
        assert!(
            !text.contains(secret),
            "validation error exposed request data: {text}"
        );
    }
    executor.close().expect("close");
}

pub(crate) async fn concurrent_calls_complete(case: &dyn Contract) {
    if case.skip_concurrent_calls() {
        eprintln!("SKIP concurrent_calls_complete: disabled for this driver");
        return;
    }
    const CALLERS: usize = 16;
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let mut handles = Vec::with_capacity(CALLERS);
    for _ in 0..CALLERS {
        handles.push(spawn_execute(
            executor.clone(),
            shell_request(case, &fixture, "exit 0"),
            std::io::sink(),
            CancellationToken::new(),
        ));
    }
    for handle in handles {
        let (status, result) = await_execution(handle, "concurrent Execute").await;
        result.expect("concurrent Execute");
        assert_eq!((status.code, status.signaled), (0, false));
    }
    executor.close().expect("close");
}

pub(crate) async fn advertised_read_confinement(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    if !capabilities.read_confinement {
        return;
    }
    let (_driver, executor) = open(case, &fixture).await;
    const SCRIPT: &str = r#"/bin/cat "$SANDBOX_CONFORMANCE_READ_PATH""#;

    let local = fixture.with_environment("SANDBOX_CONFORMANCE_READ_PATH", &fixture.allowed_read);
    let (status, result, _) = run(&executor, shell_request(case, &local, SCRIPT)).await;
    result.expect("allowed read Execute");
    assert_eq!(status.code, 0, "allowed read status");

    let local = fixture.with_environment("SANDBOX_CONFORMANCE_READ_PATH", &fixture.outside_file);
    let (status, result, _) = run(&executor, shell_request(case, &local, SCRIPT)).await;
    result.expect("outside read Execute");
    assert_ne!(status.code, 0, "outside read was permitted");

    let link = fixture.workspace.join("outside read symlink");
    std::os::unix::fs::symlink(&fixture.outside_file, &link).expect("symlink");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_READ_PATH", &link);
    let (status, result, _) = run(&executor, shell_request(case, &local, SCRIPT)).await;
    result.expect("symlink read Execute");
    assert_ne!(status.code, 0, "symlink read was permitted");
    executor.close().expect("close");
}

pub(crate) async fn advertised_write_confinement(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    if !capabilities.write_confinement {
        return;
    }
    let (_driver, executor) = open(case, &fixture).await;

    let outside_write = fixture.base.join("outside write target");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_WRITE_PATH", &outside_write);
    let (status, result, _) = run(
        &executor,
        shell_request(
            case,
            &local,
            r#"printf denied > "$SANDBOX_CONFORMANCE_WRITE_PATH""#,
        ),
    )
    .await;
    result.expect("outside write Execute");
    assert_ne!(status.code, 0, "outside write was permitted");
    assert!(!outside_write.exists(), "outside write target exists");

    let target = fixture.base.join("outside symlink target");
    write_private(&target, "unchanged");
    let link = fixture.workspace.join("outside write symlink");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_WRITE_PATH", &link);
    let (status, result, _) = run(
        &executor,
        shell_request(
            case,
            &local,
            r#"printf changed > "$SANDBOX_CONFORMANCE_WRITE_PATH""#,
        ),
    )
    .await;
    result.expect("symlink write Execute");
    assert_ne!(status.code, 0, "symlink write was permitted");
    assert_eq!(
        std::fs::read_to_string(&target).expect("symlink target"),
        "unchanged",
        "outside symlink target changed"
    );
    executor.close().expect("close");
}

pub(crate) async fn advertised_local_tcp_allow(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    let Some(policy) = policy_for_network(capabilities, NetworkMode::Allow) else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("tcp listener");
    let address = listener.local_addr().expect("listener address");
    let accepted = tokio::task::spawn_blocking(move || listener.accept().map(|_| ()));
    let (_driver, executor) = open_with_policy(case, &fixture, policy).await;
    let argv = case.tcp_client(&address.ip().to_string(), &address.port().to_string());
    let (status, result, _) = run(&executor, case.request(&fixture, argv)).await;
    result.expect("TCP client Execute");
    assert_eq!(status.code, 0, "TCP client status");
    tokio::time::timeout(AWAIT, accepted)
        .await
        .expect("timed out waiting for the allowed TCP accept")
        .expect("accept task")
        .expect("TCP accept");
    executor.close().expect("close");
}

pub(crate) async fn advertised_local_tcp_deny(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    let Some(policy) = policy_for_network(capabilities, NetworkMode::Deny) else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("tcp listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("listener address");
    let (_driver, executor) = open_with_policy(case, &fixture, policy).await;
    let argv = case.tcp_client(&address.ip().to_string(), &address.port().to_string());
    let (status, result, _) = run(&executor, case.request(&fixture, argv)).await;
    result.expect("TCP deny Execute");
    assert_ne!(status.code, 0, "TCP deny status");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "TCP listener accepted a connection despite NetworkDeny"
    );
    executor.close().expect("close");
}

pub(crate) async fn advertised_unix_socket_deny(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    if !capabilities.unix_socket_deny {
        return;
    }
    let _ = std::fs::remove_file(&fixture.unix_socket);
    let listener = std::os::unix::net::UnixListener::bind(&fixture.unix_socket).expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let (_driver, executor) = open(case, &fixture).await;
    let argv = case.unix_client(&fixture.unix_socket);
    let (status, result, _) = run(&executor, case.request(&fixture, argv)).await;
    result.expect("Unix deny Execute");
    assert_ne!(status.code, 0, "Unix deny status");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "Unix listener accepted a connection despite UnixSocketDeny"
    );
    executor.close().expect("close");
}

/// Declares one conformance check as a `#[tokio::test]`.
macro_rules! contract_check {
    ($case:expr, $check:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $check() {
            let case = $case;
            if let Some(reason) = $crate::sandbox::conformance::Contract::skip_reason(&case) {
                eprintln!("SKIP {}: {reason}", stringify!($check));
                return;
            }
            $crate::sandbox::conformance::$check(&case).await;
        }
    };
}

/// Declares the whole shared driver contract for one [`Contract`] adapter.
macro_rules! driver_contract {
    ($case:expr) => {
        $crate::sandbox::conformance::contract_check!(
            $case,
            ordinary_and_nonzero_exits_with_separate_streams
        );
        $crate::sandbox::conformance::contract_check!(
            $case,
            supplied_environment_exactly_replaces_host_environment
        );
        $crate::sandbox::conformance::contract_check!(
            $case,
            pre_cancellation_does_not_start_a_child
        );
        $crate::sandbox::conformance::contract_check!(
            $case,
            deadline_cancellation_removes_the_process_group
        );
        $crate::sandbox::conformance::contract_check!(
            $case,
            normal_leader_exit_removes_a_background_process
        );
        $crate::sandbox::conformance::contract_check!(
            $case,
            driver_close_drains_active_work_and_is_idempotent
        );
        $crate::sandbox::conformance::contract_check!(
            $case,
            executor_clones_requests_before_delegation
        );
        $crate::sandbox::conformance::contract_check!(
            $case,
            canonical_paths_with_spaces_unicode_and_quotes
        );
        $crate::sandbox::conformance::contract_check!($case, validation_errors_are_bounded);
        $crate::sandbox::conformance::contract_check!($case, concurrent_calls_complete);
        $crate::sandbox::conformance::contract_check!($case, advertised_read_confinement);
        $crate::sandbox::conformance::contract_check!($case, advertised_write_confinement);
        $crate::sandbox::conformance::contract_check!($case, advertised_local_tcp_allow);
        $crate::sandbox::conformance::contract_check!($case, advertised_local_tcp_deny);
        $crate::sandbox::conformance::contract_check!($case, advertised_unix_socket_deny);
    };
}

pub(crate) use {contract_check, driver_contract};
