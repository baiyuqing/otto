//! Native child processes with process-group lifetime control.
//!
//! Every child is the leader of a fresh process group. Once cancellation,
//! manager close, or leader exit starts cleanup, the group receives `SIGTERM`,
//! gets a bounded grace period, and is then killed if it remains alive.
//!
//! Ownership: [`Manager`] owns the set of running children. A [`Manager`] is
//! shared behind `&self` and is safe to use from any task.
//!
//! Cancellation: cancelling the token passed to [`Manager::run`] starts
//! cooperative cleanup and makes the call return [`Error::Cancelled`] if it
//! wins the race with leader completion. The final child status is still
//! reported.
//!
//! Errors: only the fixed sandbox errors escape, never a message derived from
//! the request, so a launch failure cannot leak an argument or a path.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::Signal;
use nix::unistd::{AccessFlags, Pid};
use tokio::io::AsyncReadExt;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{Error, Streams};

/// Grace afforded to a process group after `SIGTERM` before `SIGKILL`.
const TERMINATION_GRACE: Duration = Duration::from_secs(5);

/// Bounds how long the post-exit drain waits for a descendant that inherited
/// the child's pipe. Signals are sent before it, so it only limits how long a
/// leaked descendant can hold the read side open.
const DRAIN_DEADLINE: Duration = Duration::from_secs(5);

/// How long the Darwin `EPERM` postcondition observation polls for the group
/// to disappear.
const GROUP_OBSERVATION_DEADLINE: Duration = Duration::from_millis(100);

/// How long each probe waits before the next.
const GROUP_OBSERVATION_INTERVAL: Duration = Duration::from_millis(1);

const PIPE_CHUNK: usize = 32 * 1024;

/// One child to launch. `path` is the program; `args` excludes `argv[0]`,
/// which is always `path` exactly as supplied.
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    pub(crate) path: String,
    pub(crate) args: Vec<String>,
    pub(crate) directory: PathBuf,
    pub(crate) environment: Vec<String>,
}

/// How the child finished. `code` is `-1` and `signal` is the signal
/// description whenever `signaled` is set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub(crate) code: i32,
    pub(crate) signaled: bool,
    pub(crate) signal: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupTrigger {
    Completion,
    Cancellation,
    Close,
}

/// The single owner of process-group cleanup transitions.
#[derive(Debug, Default)]
struct Termination {
    trigger: Option<CleanupTrigger>,
    term_deadline: Option<Instant>,
    kill_sent: bool,
    observation_deadline: Option<Instant>,
    leader_reaped: bool,
    group_gone: bool,
    settled: bool,
    failure: Option<Error>,
}

#[derive(Debug)]
struct Entry {
    pid: i32,
    termination: Mutex<Termination>,
    cleanup_changed: tokio::sync::Notify,
    /// Keeps this process group registered with `failover::children::Children`
    /// for the entry's lifetime, so a lease watchdog's fence action can kill
    /// it independently of this manager's own lifecycle.
    _fence_registration: crate::failover::children::Registration,
}

impl Entry {
    fn begin_cleanup(&self, trigger: CleanupTrigger) {
        let mut state = self.termination.lock().expect("termination state");
        if state.trigger.is_some() {
            return;
        }
        state.trigger = Some(trigger);
        state.term_deadline = Some(Instant::now() + TERMINATION_GRACE);
        signal_group(self.pid, Signal::SIGTERM, &mut state);
        settle_cleanup(&mut state);
        drop(state);
        self.cleanup_changed.notify_one();
    }

    fn leader_completed(&self) {
        let mut state = self.termination.lock().expect("termination state");
        state.leader_reaped = true;
        // Once wait(2) yields a status, completion is retained unless
        // cancellation already won the race. Close is only a cleanup trigger.
        if !matches!(state.trigger, Some(CleanupTrigger::Cancellation)) {
            state.trigger = Some(CleanupTrigger::Completion);
        }
        if state.term_deadline.is_none() {
            state.term_deadline = Some(Instant::now() + TERMINATION_GRACE);
            signal_group(self.pid, Signal::SIGTERM, &mut state);
        }
        if !state.group_gone {
            match nix::sys::signal::kill(Pid::from_raw(-self.pid), None) {
                Err(Errno::ESRCH) => state.group_gone = true,
                Ok(()) | Err(Errno::EPERM) => {}
                Err(_) => state.failure = Some(Error::ChildTerminate),
            }
        }
        settle_cleanup(&mut state);
        drop(state);
        self.cleanup_changed.notify_one();
    }

    fn advance_cleanup(&self) {
        let mut state = self.termination.lock().expect("termination state");
        if state.settled || state.trigger.is_none() {
            return;
        }
        let group = Pid::from_raw(-self.pid);
        match nix::sys::signal::kill(group, None) {
            Err(Errno::ESRCH) => state.group_gone = true,
            Ok(()) | Err(Errno::EPERM) => {}
            Err(_) => state.failure = Some(Error::ChildTerminate),
        }

        let now = Instant::now();
        if !state.group_gone
            && !state.kill_sent
            && state.term_deadline.is_some_and(|deadline| now >= deadline)
        {
            match nix::sys::signal::kill(Pid::from_raw(-self.pid), Signal::SIGKILL) {
                Ok(()) => {
                    state.kill_sent = true;
                    state.observation_deadline = Some(now + GROUP_OBSERVATION_DEADLINE);
                }
                Err(Errno::ESRCH) => state.group_gone = true,
                Err(_) => {
                    state.failure = Some(Error::ChildTerminate);
                    state.settled = true;
                }
            }
        }
        if state.kill_sent
            && !state.group_gone
            && state
                .observation_deadline
                .is_some_and(|deadline| now >= deadline)
        {
            state.failure = Some(Error::ChildTerminate);
            state.settled = true;
        }
        settle_cleanup(&mut state);
        drop(state);
        self.cleanup_changed.notify_one();
    }

    fn next_cleanup_wake(&self) -> Option<Instant> {
        let state = self.termination.lock().expect("termination state");
        if state.settled || state.trigger.is_none() {
            return None;
        }
        if state.kill_sent {
            Some(Instant::now() + GROUP_OBSERVATION_INTERVAL)
        } else {
            state.term_deadline
        }
    }

    fn settled(&self) -> bool {
        self.termination.lock().expect("termination state").settled
    }

    fn cancelled(&self) -> bool {
        matches!(
            self.termination.lock().expect("termination state").trigger,
            Some(CleanupTrigger::Cancellation)
        )
    }

    fn failure(&self) -> Option<Error> {
        self.termination
            .lock()
            .expect("termination state")
            .failure
            .clone()
    }
}

fn signal_group(pid: i32, signal: Signal, state: &mut Termination) {
    match nix::sys::signal::kill(Pid::from_raw(-pid), signal) {
        Ok(()) => {}
        Err(Errno::ESRCH) => state.group_gone = true,
        Err(_) => state.failure = Some(Error::ChildTerminate),
    }
}

fn settle_cleanup(state: &mut Termination) {
    if state.leader_reaped && state.group_gone {
        state.settled = true;
    }
}

#[derive(Debug, Default)]
struct State {
    closing: bool,
    closed: bool,
    close_error: Option<Error>,
    active: HashMap<i32, Arc<Entry>>,
}

/// Launches children and guarantees their process groups are gone afterwards.
#[derive(Debug, Default)]
pub(crate) struct Manager {
    state: Mutex<State>,
    /// Signalled when a child leaves the active set and when `close` finishes.
    progress: Condvar,
}

impl Manager {
    /// Runs `spec` to completion, forwarding its output into `streams`.
    ///
    /// Returns [`Error::Closed`] once [`Manager::close`] has begun, and
    /// [`Error::Cancelled`] when `cancel` fires. In the cancelled case the
    /// returned [`Outcome`] still describes how the child died.
    pub(crate) async fn run(
        &self,
        spec: Spec,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (Outcome, Result<(), Error>) {
        if cancel.is_cancelled() {
            return (Outcome::default(), Err(Error::Cancelled));
        }
        let Ok(program) = resolve_executable(&spec) else {
            return (Outcome::default(), Err(Error::ChildLaunch));
        };

        let mut command = std::process::Command::new(&program);
        command.arg0(&spec.path);
        command.args(&spec.args);
        command.current_dir(&spec.directory);
        command.env_clear();
        for entry in &spec.environment {
            let (name, value) = entry.split_once('=').unwrap_or((entry.as_str(), ""));
            command.env(name, value);
        }
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.process_group(0);

        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);

        let entry = {
            let mut state = self.state.lock().expect("process state");
            if state.closing {
                return (Outcome::default(), Err(Error::Closed));
            }
            if cancel.is_cancelled() {
                return (Outcome::default(), Err(Error::Cancelled));
            }
            let Ok(child) = command.spawn() else {
                return (Outcome::default(), Err(Error::ChildLaunch));
            };
            let Some(pid) = child.id() else {
                return (Outcome::default(), Err(Error::ChildLaunch));
            };
            let entry = Arc::new(Entry {
                pid: pid as i32,
                termination: Mutex::new(Termination::default()),
                cleanup_changed: tokio::sync::Notify::new(),
                _fence_registration: crate::failover::children::Children::register(
                    crate::failover::children::Children::global(),
                    pid as i32,
                    true,
                ),
            });
            state.active.insert(entry.pid, entry.clone());
            // The child is handed to the drain loop below while the lock is
            // held, so `close` can never miss a started child.
            (entry, child)
        };
        let (entry, mut child) = entry;

        let (outcome, failure) = drain(&mut child, streams, cancel, &entry).await;

        {
            let mut state = self.state.lock().expect("process state");
            state.active.remove(&entry.pid);
        }
        self.progress.notify_all();

        (outcome, failure.map_or(Ok(()), Err))
    }

    /// Cooperatively terminates every live group, waits for their executions
    /// to settle, and rejects later runs. Idempotent: concurrent and later
    /// callers wait for the first close and observe its result.
    ///
    /// Blocking: this waits for active executions, so it must not be called
    /// from the only worker thread of a current-thread runtime.
    pub(crate) fn close(&self) -> Result<(), Error> {
        let mut state = self.state.lock().expect("process state");
        if state.closing {
            while !state.closed {
                state = self.progress.wait(state).expect("process state");
            }
            return state.close_error.clone().map_or(Ok(()), Err);
        }
        state.closing = true;
        let active: Vec<Arc<Entry>> = state.active.values().cloned().collect();
        drop(state);

        for entry in &active {
            entry.begin_cleanup(CleanupTrigger::Close);
        }

        let mut state = self.state.lock().expect("process state");
        while !state.active.is_empty() {
            state = self.progress.wait(state).expect("process state");
        }
        let failure = active.iter().find_map(|entry| entry.failure());
        state.close_error = failure.clone();
        state.closed = true;
        drop(state);
        self.progress.notify_all();
        failure.map_or(Ok(()), Err)
    }
}

/// Copies both pipes while driving bounded process-group cleanup. The entry
/// remains active until the leader is reaped and the group is observed gone.
async fn drain(
    child: &mut tokio::process::Child,
    streams: Streams<'_>,
    cancel: &CancellationToken,
    entry: &Entry,
) -> (Outcome, Option<Error>) {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out_chunk = vec![0u8; PIPE_CHUNK];
    let mut err_chunk = vec![0u8; PIPE_CHUNK];
    let mut status = None;
    let mut failure = None;
    let mut cancellation_observed = false;
    let mut pipe_deadline = None;

    loop {
        entry.advance_cleanup();
        if status.is_some() && entry.settled() && stdout.is_none() && stderr.is_none() {
            break;
        }
        if pipe_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            stdout = None;
            stderr = None;
            continue;
        }

        let cleanup_wake = entry.next_cleanup_wake();
        let cleanup_changed = entry.cleanup_changed.notified();
        tokio::select! {
            biased;
            result = child.wait(), if status.is_none() => {
                match result {
                    Ok(exit) => status = Some(exit),
                    Err(_) => {
                        failure = failure.or(Some(Error::ChildWait));
                        status = Some(std::process::ExitStatus::default());
                    }
                }
                entry.leader_completed();
                pipe_deadline.get_or_insert_with(|| Instant::now() + DRAIN_DEADLINE);
            }
            () = cancel.cancelled(), if !cancellation_observed && status.is_none() => {
                cancellation_observed = true;
                entry.begin_cleanup(CleanupTrigger::Cancellation);
            }
            result = read_from(&mut stdout, &mut out_chunk), if stdout.is_some() => {
                match result {
                    Ok(0) => stdout = None,
                    Ok(count) => {
                        if streams.stdout.write_all(&out_chunk[..count]).is_err() {
                            failure = failure.or(Some(Error::ChildWait));
                            stdout = None;
                        }
                    }
                    Err(_) => {
                        failure = failure.or(Some(Error::ChildWait));
                        stdout = None;
                    }
                }
            }
            result = read_from(&mut stderr, &mut err_chunk), if stderr.is_some() => {
                match result {
                    Ok(0) => stderr = None,
                    Ok(count) => {
                        if streams.stderr.write_all(&err_chunk[..count]).is_err() {
                            failure = failure.or(Some(Error::ChildWait));
                            stderr = None;
                        }
                    }
                    Err(_) => {
                        failure = failure.or(Some(Error::ChildWait));
                        stderr = None;
                    }
                }
            }
            () = sleep_until(cleanup_wake), if cleanup_wake.is_some() => {}
            () = cleanup_changed => {}
            () = sleep_until(pipe_deadline), if pipe_deadline.is_some() => {}
        }
    }

    failure = failure.or_else(|| entry.failure());
    if entry.cancelled() {
        failure = Some(Error::Cancelled);
    }
    (status.map_or_else(Outcome::default, outcome_from), failure)
}

async fn read_from<R>(pipe: &mut Option<R>, buffer: &mut [u8]) -> std::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    match pipe {
        Some(pipe) => pipe.read(buffer).await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn outcome_from(status: std::process::ExitStatus) -> Outcome {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        return Outcome {
            code: -1,
            signaled: true,
            signal: signal_description(signal),
        };
    }
    Outcome {
        code: status.code().unwrap_or(-1),
        signaled: false,
        signal: String::new(),
    }
}

/// Resolves `spec.path`: a name containing a separator is used verbatim, and a
/// bare name is looked up only in the request's own `PATH`, never the host's.
fn resolve_executable(spec: &Spec) -> Result<PathBuf, Error> {
    if spec.path.contains('/') {
        return Ok(PathBuf::from(&spec.path));
    }
    let request_path = spec
        .environment
        .iter()
        .find_map(|entry| entry.strip_prefix("PATH="))
        .ok_or(Error::ChildLaunch)?;
    let base = std::path::absolute(&spec.directory).map_err(|_| Error::ChildLaunch)?;

    let directories: Vec<&str> = if request_path.is_empty() {
        vec![""]
    } else {
        request_path.split(':').collect()
    };
    for directory in directories {
        let directory = if directory.is_empty() { "." } else { directory };
        let directory = Path::new(directory);
        let directory = if directory.is_absolute() {
            directory.to_path_buf()
        } else {
            base.join(directory)
        };
        let candidate = directory.join(&spec.path);
        match std::fs::metadata(&candidate) {
            Ok(info) if !info.is_dir() => {}
            _ => continue,
        }
        if nix::unistd::access(&candidate, AccessFlags::X_OK).is_ok() {
            return Ok(candidate);
        }
    }
    Err(Error::ChildLaunch)
}

/// Go's `syscall.Signal.String()` on Darwin. Otto surfaces these strings in
/// tool output, so they are reproduced rather than the `SIGKILL` spelling.
fn signal_description(signal: i32) -> String {
    const NAMES: [&str; 32] = [
        "signal 0",
        "hangup",
        "interrupt",
        "quit",
        "illegal instruction",
        "trace/BPT trap",
        "abort trap",
        "EMT trap",
        "floating point exception",
        "killed",
        "bus error",
        "segmentation fault",
        "bad system call",
        "broken pipe",
        "alarm clock",
        "terminated",
        "urgent I/O condition",
        "suspended (signal)",
        "suspended",
        "continued",
        "child exited",
        "stopped (tty input)",
        "stopped (tty output)",
        "I/O possible",
        "cputime limit exceeded",
        "filesize limit exceeded",
        "virtual timer expired",
        "profiling timer expired",
        "window size changes",
        "information request",
        "user defined signal 1",
        "user defined signal 2",
    ];
    usize::try_from(signal)
        .ok()
        .and_then(|index| NAMES.get(index))
        .map_or_else(|| format!("signal {signal}"), |name| (*name).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(path: &str, args: &[&str], directory: &std::path::Path, env: &[&str]) -> Spec {
        Spec {
            path: path.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            directory: directory.to_path_buf(),
            environment: env.iter().map(|entry| (*entry).to_string()).collect(),
        }
    }

    async fn run(spec: Spec) -> (Outcome, Result<(), Error>, String) {
        let manager = Manager::default();
        let mut stdout = Vec::new();
        let mut stderr = std::io::sink();
        let cancel = CancellationToken::new();
        let (outcome, result) = manager
            .run(
                spec,
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                &cancel,
            )
            .await;
        manager.close().expect("close");
        (
            outcome,
            result,
            String::from_utf8_lossy(&stdout).into_owned(),
        )
    }

    #[test]
    fn a_slashed_path_is_used_verbatim_and_a_bare_name_needs_a_request_path() {
        let directory = std::path::Path::new("/");
        assert_eq!(
            resolve_executable(&spec("/bin/echo", &[], directory, &[])),
            Ok(PathBuf::from("/bin/echo"))
        );
        assert_eq!(
            resolve_executable(&spec("./echo", &[], directory, &[])),
            Ok(PathBuf::from("./echo"))
        );
        // Without `PATH` in the request there is nowhere to look: the host's
        // own `PATH` is never consulted.
        assert_eq!(
            resolve_executable(&spec("echo", &[], directory, &["LC_ALL=C"])),
            Err(Error::ChildLaunch)
        );
    }

    #[test]
    fn a_bare_name_resolves_only_through_the_requests_path() {
        let directory = tempfile::tempdir().expect("temp dir");
        let executable = directory.path().join("tool");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("write tool");
        std::fs::set_permissions(
            &executable,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("chmod tool");
        std::fs::create_dir(directory.path().join("tool directory")).expect("directory candidate");

        let listed = format!("PATH={}", directory.path().display());
        assert_eq!(
            resolve_executable(&spec("tool", &[], directory.path(), &[&listed])),
            Ok(executable.clone())
        );
        // A relative entry joins against the request's own directory, and an
        // empty entry means that directory too.
        assert_eq!(
            resolve_executable(&spec("tool", &[], directory.path(), &["PATH="])),
            Ok(executable.clone())
        );
        assert_eq!(
            resolve_executable(&spec("tool", &[], directory.path(), &["PATH=."])),
            Ok(executable)
        );
        // A directory named like the executable is skipped, not launched.
        assert_eq!(
            resolve_executable(&spec("tool directory", &[], directory.path(), &["PATH=."])),
            Err(Error::ChildLaunch)
        );
        // A readable but non-executable candidate is skipped.
        let plain = directory.path().join("plain");
        std::fs::write(&plain, "data").expect("write plain");
        std::fs::set_permissions(&plain, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("chmod plain");
        assert_eq!(
            resolve_executable(&spec("plain", &[], directory.path(), &["PATH=."])),
            Err(Error::ChildLaunch)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_signalled_child_reports_the_darwin_signal_description() {
        let (outcome, result, _) = run(spec(
            "/bin/sh",
            &["-c", "kill -TERM $$"],
            std::path::Path::new("/"),
            &["PATH=/usr/bin:/bin"],
        ))
        .await;
        result.expect("run");
        assert_eq!(
            outcome,
            Outcome {
                code: -1,
                signaled: true,
                signal: "terminated".to_string(),
            }
        );

        let (outcome, result, _) = run(spec(
            "/bin/sh",
            &["-c", "kill -KILL $$"],
            std::path::Path::new("/"),
            &["PATH=/usr/bin:/bin"],
        ))
        .await;
        result.expect("run");
        assert_eq!(outcome.signal, "killed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_child_sees_only_the_request_environment_argv0_and_a_closed_stdin() {
        let (outcome, result, stdout) = run(spec(
            "/bin/sh",
            &[
                "-c",
                r#"printf '%s|%s|%s|' "$0" "$ONLY" "${HOME-unset}"; cat; printf 'eof'"#,
            ],
            std::path::Path::new("/"),
            &["ONLY=value"],
        ))
        .await;
        result.expect("run");
        assert_eq!(outcome.code, 0);
        assert_eq!(stdout, "/bin/sh|value|unset|eof");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_backgrounded_descendant_is_killed_with_the_group() {
        let directory = tempfile::tempdir().expect("temp dir");
        let marker = directory.path().join("marker");
        let script = format!(
            "( sleep 5; printf escaped > '{}' ) >/dev/null 2>&1 & exit 0",
            marker.display()
        );
        let (outcome, result, _) = run(spec(
            "/bin/sh",
            &["-c", &script],
            directory.path(),
            &["PATH=/usr/bin:/bin"],
        ))
        .await;
        result.expect("run");
        assert_eq!(outcome.code, 0);
        // `close` returned, so the group was signalled and observed gone. The
        // descendant never reached its write.
        assert!(!marker.exists(), "backgrounded descendant survived");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_allows_a_cooperative_term_exit() {
        let manager = Manager::default();
        let mut stdout = std::io::sink();
        let mut stderr = std::io::sink();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
        });
        let (outcome, result) = manager
            .run(
                spec(
                    "/bin/sh",
                    &["-c", "trap 'exit 23' TERM; while :; do sleep 1; done"],
                    std::path::Path::new("/"),
                    &["PATH=/usr/bin:/bin"],
                ),
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                &cancel,
            )
            .await;
        assert_eq!(result, Err(Error::Cancelled));
        assert_eq!(outcome.code, 23);
        assert!(!outcome.signaled);
        manager.close().expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_escalates_an_ignored_term_to_kill() {
        let manager = Manager::default();
        let mut stdout = std::io::sink();
        let mut stderr = std::io::sink();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            token.cancel();
        });
        let (outcome, result) = manager
            .run(
                spec(
                    "/bin/sh",
                    &["-c", "trap '' TERM; while :; do sleep 1; done"],
                    std::path::Path::new("/"),
                    &["PATH=/usr/bin:/bin"],
                ),
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                &cancel,
            )
            .await;
        assert_eq!(result, Err(Error::Cancelled));
        assert!(outcome.signaled);
        assert_eq!(outcome.signal, "killed");
        manager.close().expect("close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_close_cooperatively_cleans_up_once() {
        let manager = Arc::new(Manager::default());
        let runner = manager.clone();
        let run = tokio::spawn(async move {
            let mut stdout = std::io::sink();
            let mut stderr = std::io::sink();
            let cancel = CancellationToken::new();
            runner
                .run(
                    spec(
                        "/bin/sh",
                        &["-c", "trap 'exit 24' TERM; while :; do sleep 1; done"],
                        std::path::Path::new("/"),
                        &["PATH=/usr/bin:/bin"],
                    ),
                    Streams {
                        stdout: &mut stdout,
                        stderr: &mut stderr,
                    },
                    &cancel,
                )
                .await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let first = {
            let manager = manager.clone();
            std::thread::spawn(move || manager.close())
        };
        let second = {
            let manager = manager.clone();
            std::thread::spawn(move || manager.close())
        };
        assert_eq!(first.join().expect("first close"), Ok(()));
        assert_eq!(second.join().expect("second close"), Ok(()));

        let (outcome, result) = run.await.expect("run task");
        result.expect("run result");
        assert_eq!(outcome.code, 24);
        assert!(!outcome.signaled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_manager_refuses_later_runs() {
        let manager = Manager::default();
        manager.close().expect("close");
        manager.close().expect("second close");
        let mut stdout = std::io::sink();
        let mut stderr = std::io::sink();
        let cancel = CancellationToken::new();
        let (outcome, result) = manager
            .run(
                spec(
                    "/bin/sh",
                    &["-c", "exit 0"],
                    std::path::Path::new("/"),
                    &["PATH=/usr/bin:/bin"],
                ),
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                &cancel,
            )
            .await;
        assert_eq!((outcome, result), (Outcome::default(), Err(Error::Closed)));
    }
}
