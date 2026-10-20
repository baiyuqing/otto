//! FIFO, line-wait and process-state helpers for the checks.

use super::*;

pub(super) fn make_fifo(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).expect("mkfifo");
    path
}

/// Opens the FIFO for writing, which unblocks the reader inside the sandbox.
pub(super) async fn write_fifo(path: &Path, value: &str) {
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

pub(super) async fn await_line(lines: &mut UnboundedReceiver<String>, description: &str) -> String {
    tokio::time::timeout(AWAIT, lines.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
        .unwrap_or_else(|| panic!("stream closed before {description}"))
}

pub(super) async fn await_gate(gate: &Gate, description: &str) {
    let deadline = Instant::now() + AWAIT;
    while !gate.released() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

pub(super) async fn await_execution<T: Send + 'static>(
    handle: tokio::task::JoinHandle<T>,
    description: &str,
) -> T {
    tokio::time::timeout(AWAIT, handle)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
        .unwrap_or_else(|_| panic!("{description} task panicked"))
}

pub(super) fn parse_pid(value: &str) -> i32 {
    let pid: i32 = value
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("invalid child PID {value:?}"));
    assert!(pid > 0, "invalid child PID {value:?}");
    pid
}

pub(super) fn assert_signaled(status: &ExitStatus) {
    assert!(
        status.code == -1 && status.signaled && !status.signal.is_empty(),
        "status = {status:?}, want signal termination"
    );
}

/// Asserts that `pid` stops executing, within [`SETTLE`].
///
/// The driver signals the group before it returns, but a signal is not a
/// synchronous death: the kernel closes the child's descriptors — which is
/// what ends the driver's output drain and lets `execute` return — before the
/// task finishes leaving the process table. Probing once therefore races
/// teardown, which is how this read as a survivor on a loaded CI runner while
/// passing everywhere else.
///
/// The window only absorbs that teardown. A descendant that was never
/// signalled keeps answering for the whole of it and still fails the check,
/// which is the leak this contract exists to catch.
///
/// `ESRCH` is the usual answer. A process that has been killed but not yet
/// reaped answers signal 0 too, so a zombie counts as gone: it runs no code,
/// holds no descriptors, and only its exit status is left. Whoever reaps an
/// orphan (PID 1 on Linux, which a container's init may do lazily) is not
/// part of the contract being checked here.
pub(super) async fn assert_gone(pid: i32) {
    let deadline = Instant::now() + SETTLE;
    loop {
        let result = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None);
        if result == Err(nix::errno::Errno::ESRCH) || is_zombie(pid) {
            return;
        }
        if Instant::now() >= deadline {
            // Never leak it out of the test, whatever the verdict.
            let _ =
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::SIGKILL);
            panic!("process {pid} is still executing {SETTLE:?} after its execution ended");
        }
        tokio::time::sleep(SETTLE_INTERVAL).await;
    }
}

/// Whether `pid` is a process that has exited and not yet been reaped.
///
/// `/proc/<pid>/stat` holds the state as the first field after the executable
/// name, which is itself parenthesized and may contain spaces, so the scan
/// starts after the last `)`.
#[cfg(target_os = "linux")]
pub(super) fn is_zombie(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, after_name)) = stat.rsplit_once(')') else {
        return false;
    };
    after_name.split_whitespace().next() == Some("Z")
}

/// macOS has no `/proc`, and launchd reaps an orphan promptly, so a process
/// that still answers signal 0 there is a real survivor.
#[cfg(not(target_os = "linux"))]
pub(super) fn is_zombie(_pid: i32) -> bool {
    false
}

/// Runs `request` on its own task so the check can drive the child meanwhile.
pub(super) fn spawn_execute<W>(
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
