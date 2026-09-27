//! Spawns and manages the `otto serve` child process: the exact `Command`
//! the app builds (reused by `desktop/src-tauri/tests/integration_serve.rs`
//! so the test exercises the same flags and pipes the app uses), the
//! background line reader that feeds [`crate::serve_url::wait_for_serve_url`],
//! `otto trust`, and quit-time shutdown.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

/// Spawns `otto serve --cwd <cwd> --listen 127.0.0.1:0 --exit-on-stdin-close`
/// with `env` as its whole environment (not merged with this process's own),
/// stdin/stdout piped, and stderr appended to `log_path`.
pub fn spawn_serve(
    otto_binary: &Path,
    cwd: &Path,
    env: &[(String, String)],
    log_path: &Path,
) -> std::io::Result<Child> {
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    Command::new(otto_binary)
        .arg("serve")
        .arg("--cwd")
        .arg(cwd)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--exit-on-stdin-close")
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log_file)
        .spawn()
}

/// Spawns a background thread that forwards `stdout`'s lines to the
/// returned channel until EOF or a read error, then drops the sender so
/// [`crate::serve_url::wait_for_serve_url`] sees a disconnect.
pub fn spawn_line_reader(stdout: std::process::ChildStdout) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// `otto trust <dir>`'s outcome.
pub struct TrustResult {
    pub success: bool,
    pub stderr: String,
}

/// Runs `otto trust <dir>` to completion with `env` as its environment.
pub fn run_trust(
    otto_binary: &Path,
    dir: &Path,
    env: &[(String, String)],
) -> std::io::Result<TrustResult> {
    let output = Command::new(otto_binary)
        .arg("trust")
        .arg(dir)
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .output()?;
    Ok(TrustResult {
        success: output.status.success(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Sends `SIGTERM`, waits up to `grace` for the child to exit, and sends
/// `SIGKILL` if it has not. Reaps the process either way. `libc::kill` on a
/// process that already exited returns `ESRCH`, which is not an error here:
/// the goal (the child is gone) already holds.
pub fn shutdown_child(child: &mut Child, grace: Duration) {
    let pid = child.id() as libc::pid_t;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(_) => return,
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let _ = child.wait();
}

/// The user's login shell from the password database (`getpwuid_r`, not
/// `$SHELL`, which a Finder/Dock launch does not set reliably either).
pub fn login_shell() -> std::io::Result<String> {
    let uid = unsafe { libc::getuid() };
    let mut buf = vec![0u8; 1024];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    loop {
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut result,
            )
        };
        if rc == 0 {
            break;
        }
        if rc == libc::ERANGE {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        return Err(std::io::Error::from_raw_os_error(rc));
    }
    if result.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no password database entry for the current user",
        ));
    }
    let shell = unsafe { std::ffi::CStr::from_ptr(pwd.pw_shell) };
    Ok(shell.to_string_lossy().into_owned())
}

/// What [`capture_login_shell_env`] returned.
pub enum EnvCapture {
    Captured(Vec<(String, String)>),
    /// The app's own environment, with the reason a login-shell capture was
    /// not used.
    Fallback {
        environment: Vec<(String, String)>,
        reason: String,
    },
}

/// Runs `<shell> -l -i -c 'printf OTTO_ENV_BEGIN; env -0'` with a `timeout`
/// deadline and parses its output. Falls back to this process's own
/// environment on any failure: an unresolvable shell, a timeout, a spawn
/// error, or output the parser rejects.
pub fn capture_login_shell_env(timeout: Duration) -> EnvCapture {
    match login_shell().and_then(|shell| run_env_probe(&shell, timeout)) {
        Ok(bytes) => match crate::env_capture::parse_env_output(&bytes) {
            Ok(pairs) => EnvCapture::Captured(pairs),
            Err(_) => fallback("the login shell's output had no OTTO_ENV_BEGIN marker"),
        },
        Err(error) => fallback(&error.to_string()),
    }
}

fn fallback(reason: &str) -> EnvCapture {
    EnvCapture::Fallback {
        environment: std::env::vars().collect(),
        reason: reason.to_string(),
    }
}

fn run_env_probe(shell: &str, timeout: Duration) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", "printf OTTO_ENV_BEGIN; env -0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        let _ = tx.send(buffer);
    });
    match rx.recv_timeout(timeout) {
        Ok(bytes) => {
            let _ = child.wait();
            Ok(bytes)
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "capturing the login shell's environment timed out",
            ))
        }
    }
}

/// Exit status plus the last `lines` lines of `log_path`, for the window
/// shown when the child exits before announcing its URL.
pub fn describe_early_exit(status: Option<ExitStatus>, log_path: &Path, lines: usize) -> String {
    let status_text = match status {
        Some(status) => format!("otto serve exited: {status}"),
        None => "otto serve exited".to_string(),
    };
    let tail = std::fs::read_to_string(log_path)
        .map(|contents| tail_lines(&contents, lines))
        .unwrap_or_default();
    if tail.is_empty() {
        status_text
    } else {
        format!("{status_text}\n\n{tail}")
    }
}

fn tail_lines(text: &str, count: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(count);
    all[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_lines_keeps_only_the_last_n() {
        let text = "1\n2\n3\n4\n5\n";
        assert_eq!(tail_lines(text, 2), "4\n5");
        assert_eq!(tail_lines(text, 10), "1\n2\n3\n4\n5");
    }
}
