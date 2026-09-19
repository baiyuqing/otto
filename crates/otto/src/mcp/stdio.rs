//! The stdio transport: a child process speaking newline-delimited
//! JSON-RPC over its stdin/stdout.
//!
//! Owned by the stdio/client step; see `docs/specs/2026-09-19-mcp-design.md`
//! ("Transports > stdio"). The child's environment is exactly what the
//! caller supplies (`env_clear()` first); the caller is responsible for
//! adding `PATH`/`HOME`/`TMPDIR`/`LANG`/`TERM` when the config contract
//! requires it.
//!
//! Ownership: [`StdioTransport::spawn`] starts two background tasks — one
//! reads stdout and routes responses to pending calls, one drains stderr
//! into a bounded tail — that run for the transport's lifetime and are torn
//! down by [`StdioTransport::close`]. Concurrency: [`request`] and
//! [`notify`] serialize writes through a shared stdin lock; concurrent calls
//! are safe. Cancellation: [`request`] races the response against the
//! cancellation token and, on cancel, sends `notifications/cancelled`
//! itself, since it is the only place that knows the request id. Errors: a
//! malformed line is skipped, not fatal; a line over 16 MiB or a closed
//! child fails every pending call and every later call with the same
//! message, without respawning (restart policy belongs to the composition
//! layer, not this transport).
//!
//! [`request`]: Transport::request
//! [`notify`]: Transport::notify

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use super::jsonrpc::{self, RpcError};
use super::{CallError, Outbound, Transport};

/// A line over this size is treated as a transport failure rather than
/// buffered indefinitely.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
/// How much of the child's stderr is kept for failure messages.
const STDERR_TAIL_BYTES: usize = 4 * 1024;
/// How long `close` waits after dropping stdin, and again after `SIGTERM`,
/// before escalating.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

type PendingResult = Result<Result<Value, RpcError>, CallError>;
type Pending = HashMap<i64, oneshot::Sender<PendingResult>>;

/// A child process reached over stdin/stdout. See the module documentation
/// for lifecycle and concurrency.
pub struct StdioTransport {
    pid: u32,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pending: Arc<Mutex<Pending>>,
    /// Set once the child has exited or stdout has closed; holds the
    /// message every pending and later call fails with.
    dead: Arc<Mutex<Option<String>>>,
    next_id: AtomicI64,
    stderr_tail: Arc<std::sync::Mutex<String>>,
    close_done: Mutex<bool>,
}

impl StdioTransport {
    /// Spawns `command` with exactly `env` as its environment (after
    /// `env_clear()`), piping stdin/stdout/stderr.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<Self, CallError> {
        let mut builder = Command::new(command);
        builder.args(args);
        builder.env_clear();
        for (key, value) in env {
            builder.env(key, value);
        }
        builder.current_dir(cwd);
        builder.stdin(Stdio::piped());
        builder.stdout(Stdio::piped());
        builder.stderr(Stdio::piped());
        builder.kill_on_drop(true);

        let mut child = builder.spawn().map_err(|error| {
            CallError::Transport(format!("failed to start mcp server: {error}"))
        })?;
        let pid = child.id().ok_or_else(|| {
            CallError::Transport("mcp server exited before it could be tracked".to_string())
        })?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let stdin = Arc::new(Mutex::new(Some(stdin)));
        let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(HashMap::new()));
        let dead: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));

        tokio::spawn(drain_stderr(stderr, stderr_tail.clone()));
        tokio::spawn(reader_loop(
            child,
            stdout,
            pending.clone(),
            dead.clone(),
            stdin.clone(),
        ));

        Ok(Self {
            pid,
            stdin,
            pending,
            dead,
            next_id: AtomicI64::new(1),
            stderr_tail,
            close_done: Mutex::new(false),
        })
    }

    /// The child's process id. Exposed for tests that verify shutdown.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The last few KiB the child wrote to stderr, for failure messages.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().expect("stderr tail lock").clone()
    }
}

async fn write_line(stdin: &Mutex<Option<ChildStdin>>, line: &str) -> Result<(), CallError> {
    let mut guard = stdin.lock().await;
    let Some(handle) = guard.as_mut() else {
        return Err(CallError::Transport(
            "mcp server stdin is closed".to_string(),
        ));
    };
    let result: std::io::Result<()> = async {
        handle.write_all(line.as_bytes()).await?;
        handle.write_all(b"\n").await?;
        handle.flush().await
    }
    .await;
    result.map_err(|error| CallError::Transport(format!("write to mcp server failed: {error}")))
}

#[async_trait::async_trait]
impl Transport for StdioTransport {
    async fn request(
        &self,
        outbound: Outbound,
        cancel: &CancellationToken,
    ) -> Result<Result<Value, RpcError>, CallError> {
        if let Some(reason) = self.dead.lock().await.clone() {
            return Err(CallError::Transport(reason));
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = jsonrpc::Request::new(id, &outbound.method, outbound.params);
        let line = serde_json::to_string(&request)
            .map_err(|error| CallError::Transport(format!("encode mcp request: {error}")))?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(error) = write_line(&self.stdin, &line).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }

        tokio::select! {
            biased;
            result = rx => match result {
                Ok(outcome) => outcome,
                Err(_) => Err(CallError::Transport(
                    self.dead
                        .lock()
                        .await
                        .clone()
                        .unwrap_or_else(|| "mcp server connection closed".to_string()),
                )),
            },
            () = cancel.cancelled() => {
                self.pending.lock().await.remove(&id);
                let _ = self
                    .notify(Outbound {
                        method: "notifications/cancelled".to_string(),
                        params: json!({"requestId": id, "reason": "cancelled"}),
                        era: outbound.era,
                    })
                    .await;
                Err(CallError::Cancelled)
            }
        }
    }

    async fn notify(&self, outbound: Outbound) -> Result<(), CallError> {
        if let Some(reason) = self.dead.lock().await.clone() {
            return Err(CallError::Transport(reason));
        }
        let notification = jsonrpc::Notification::new(&outbound.method, outbound.params);
        let line = serde_json::to_string(&notification)
            .map_err(|error| CallError::Transport(format!("encode mcp notification: {error}")))?;
        write_line(&self.stdin, &line).await
    }

    async fn close(&self) {
        let mut done = self.close_done.lock().await;
        if *done {
            return;
        }
        *done = true;

        // Drop stdin: most well-behaved servers exit on EOF.
        self.stdin.lock().await.take();
        if wait_for_exit(self.pid, SHUTDOWN_WAIT).await {
            return;
        }
        let _ = signal::kill(Pid::from_raw(self.pid as i32), Signal::SIGTERM);
        if wait_for_exit(self.pid, SHUTDOWN_WAIT).await {
            return;
        }
        let _ = signal::kill(Pid::from_raw(self.pid as i32), Signal::SIGKILL);
    }
}

async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if process_gone(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn process_gone(pid: u32) -> bool {
    matches!(
        signal::kill(Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

/// Reads stdout line by line, routing responses to pending calls, until the
/// child exits or stdout closes, then fails every pending (and future) call.
async fn reader_loop(
    mut child: Child,
    stdout: ChildStdout,
    pending: Arc<Mutex<Pending>>,
    dead: Arc<Mutex<Option<String>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
) {
    let mut reader = tokio::io::BufReader::new(stdout);
    loop {
        match read_line_capped(&mut reader, MAX_LINE_BYTES).await {
            Ok(Some(line)) => {
                if let Ok(text) = String::from_utf8(line) {
                    handle_line(&text, &pending, &stdin).await;
                }
                // Non-UTF-8 output is not valid JSON-RPC; skipped.
            }
            Ok(None) => break,
            Err(_) => {
                // A line over the cap means the transport can no longer be
                // trusted to frame messages correctly; stop trusting it.
                let _ = child.start_kill();
                break;
            }
        }
    }

    let status = child.wait().await.ok();
    let reason = match status {
        Some(status) => format!("server exited ({status})"),
        None => "server exited".to_string(),
    };
    *dead.lock().await = Some(reason.clone());
    let orphaned: Vec<_> = std::mem::take(&mut *pending.lock().await)
        .into_values()
        .collect();
    for tx in orphaned {
        let _ = tx.send(Err(CallError::Transport(reason.clone())));
    }
}

async fn handle_line(text: &str, pending: &Mutex<Pending>, stdin: &Mutex<Option<ChildStdin>>) {
    let Ok(incoming) = jsonrpc::parse_incoming(text) else {
        return;
    };
    match incoming {
        jsonrpc::Incoming::Response { id, result } => {
            if let Some(id) = id.as_i64()
                && let Some(tx) = pending.lock().await.remove(&id)
            {
                let _ = tx.send(Ok(Ok(result)));
            }
        }
        jsonrpc::Incoming::Error { id, error } => {
            if let Some(id) = id.as_i64()
                && let Some(tx) = pending.lock().await.remove(&id)
            {
                let _ = tx.send(Ok(Err(error)));
            }
        }
        jsonrpc::Incoming::Notification { .. } => {}
        jsonrpc::Incoming::Request { id, method, .. } => {
            if method == "ping" {
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": {}});
                if let Ok(line) = serde_json::to_string(&reply) {
                    let _ = write_line(stdin, &line).await;
                }
            }
            // Any other server-initiated request is out of scope; ignored.
        }
    }
}

/// Reads one line, buffering across partial reads and failing once `cap`
/// bytes have been seen without a newline. Returns `Ok(None)` at EOF with no
/// trailing partial line.
async fn read_line_capped(
    reader: &mut (impl AsyncBufRead + Unpin),
    cap: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() { None } else { Some(buf) });
        }
        if let Some(pos) = available.iter().position(|&byte| byte == b'\n') {
            buf.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Some(buf));
        }
        let consumed = available.len();
        buf.extend_from_slice(available);
        reader.consume(consumed);
        if buf.len() > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("mcp server line exceeds {cap} bytes"),
            ));
        }
    }
}

async fn drain_stderr(stderr: ChildStderr, tail: Arc<std::sync::Mutex<String>>) {
    let mut reader = stderr;
    let mut chunk = vec![0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let text = String::from_utf8_lossy(&chunk[..count]);
                let mut guard = tail.lock().expect("stderr tail lock");
                guard.push_str(&text);
                if guard.len() > STDERR_TAIL_BYTES {
                    let cut = guard.len() - STDERR_TAIL_BYTES;
                    let boundary = (cut..=guard.len())
                        .find(|&index| guard.is_char_boundary(index))
                        .unwrap_or(guard.len());
                    guard.drain(..boundary);
                }
            }
        }
    }
}
