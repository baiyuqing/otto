//! End-to-end coverage of `otto acp --attach` through the built binary: a
//! real `otto serve` on a Unix socket with a scripted provider, and one or
//! more relays driven over pipes. docs/specs/2026-10-02-shared-session.md,
//! "`otto acp --attach` forwards one ACP connection to `otto serve`".

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod binary_harness;
use binary_harness::assert_terminal_tool_update;
use otto::fixtures::scripted_provider::{
    Script, serve, summary_reply, text_reply, tool_call_reply,
};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

fn configure(home: &Path, base_url: &str, socket_in_config: Option<&Path>) {
    std::fs::create_dir_all(home.join("Library/Caches")).expect("cache base");
    let config_dir = home.join(".config/otto");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let mut text = format!(
        "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"{base_url}\"\nmodel = \"gpt-test\"\napi_key_env = \"OTTO_API_KEY\"\n"
    );
    if let Some(socket) = socket_in_config {
        text.push_str(&format!("\n[server]\nsocket = \"{}\"\n", socket.display()));
    }
    std::fs::write(config_dir.join("config.toml"), text).expect("write config");
}

fn otto(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otto"));
    command
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("OTTO_API_KEY", "sk-e2e-not-a-real-key");
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    command
}

/// A provider that accepts connections and never answers them.
fn serve_stalled() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let base_url = format!("http://{}", listener.local_addr().expect("local address"));
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    std::thread::spawn(move || {
        let held = Mutex::new(Vec::new());
        for stream in listener.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            held.lock().unwrap().push(stream);
        }
    });
    (base_url, accepted)
}

/// A provider that answers its first request only after `release` fires and
/// every later request at once, with `replies` in order. The count is the
/// number of requests received.
fn serve_gated(replies: Vec<String>) -> (String, Arc<AtomicUsize>, Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let base_url = format!("http://{}", listener.local_addr().expect("local address"));
    let received = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&received);
    let (release, gate) = channel::<()>();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut raw = Vec::new();
            let mut buffer = [0u8; 4096];
            let head_end = loop {
                if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break 0,
                    Ok(read) => raw.extend_from_slice(&buffer[..read]),
                }
            };
            if head_end == 0 {
                continue;
            }
            let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = raw.len() - head_end;
            while body < length {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => body += read,
                }
            }
            let index = counter.fetch_add(1, Ordering::SeqCst);
            if index == 0 {
                let _ = gate.recv_timeout(TIMEOUT);
            }
            let reply = replies
                .get(index)
                .cloned()
                .unwrap_or_else(|| text_reply("out of script"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (base_url, received, release)
}

fn wait_for_count(count: &AtomicUsize, at_least: usize) {
    let start = Instant::now();
    while count.load(Ordering::SeqCst) < at_least {
        assert!(start.elapsed() < TIMEOUT, "provider was never called");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One HTTP/1.1 request over the socket; returns the status and the body.
fn http(socket: &Path, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let mut stream = UnixStream::connect(socket).expect("connect to serve");
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let body = body.unwrap_or("");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: otto\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read the reply");
    let status = reply
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("status line in {reply:?}"));
    let body = reply.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    (status, body.to_string())
}

/// Reads the response to `GET path` until `needle` appears in the body.
#[cfg(target_os = "macos")]
fn read_until(socket: &Path, path: &str, needle: &str) -> String {
    let mut stream = UnixStream::connect(socket).expect("connect to serve");
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: otto\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let start = Instant::now();
    let mut text = String::new();
    let mut buffer = [0u8; 4096];
    while !text.contains(needle) {
        assert!(start.elapsed() < TIMEOUT, "no {needle:?} in {text:?}");
        match stream.read(&mut buffer) {
            Ok(0) => panic!("stream ended without {needle:?}: {text:?}"),
            Ok(read) => text.push_str(&String::from_utf8_lossy(&buffer[..read])),
            Err(error) => panic!("read: {error}; so far {text:?}"),
        }
    }
    text
}

/// A temporary directory only the owner can enter, as serve requires of the
/// directory holding its socket.
fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("socket dir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("chmod socket dir");
    dir
}

/// An `otto serve` child on a Unix socket in a temporary directory.
struct Serve {
    child: Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Serve {
    /// With `config_socket` set, serve takes its socket from `[server].socket`
    /// in the config instead of `--socket`.
    fn start_with(
        home: &Path,
        workspace: &Path,
        sandbox: &str,
        config_socket: Option<&Path>,
    ) -> Self {
        let dir = private_dir();
        let socket = config_socket.map_or_else(|| dir.path().join("o.sock"), Path::to_path_buf);
        let mut command = otto(home);
        command
            .arg("serve")
            .arg("--cwd")
            .arg(workspace)
            .arg("--sandbox")
            .arg(sandbox);
        if config_socket.is_none() {
            command.arg("--socket").arg(&socket);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn otto serve");
        let serve = Self {
            child,
            socket,
            _dir: dir,
        };
        let start = Instant::now();
        loop {
            if UnixStream::connect(&serve.socket).is_ok()
                && http(&serve.socket, "GET", "/healthz", None).0 == 200
            {
                return serve;
            }
            assert!(start.elapsed() < TIMEOUT, "otto serve did not start");
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

impl Serve {
    fn start(home: &Path, workspace: &Path, sandbox: &str) -> Self {
        Self::start_with(home, workspace, sandbox, None)
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An `otto acp` child (local or `--attach`) driven over its pipes.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Client {
    fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn otto acp");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
            next_id: 1,
        }
    }

    fn local(home: &Path, workspace: &Path) -> Self {
        let mut command = otto(home);
        command
            .arg("acp")
            .arg("--cwd")
            .arg(workspace)
            .arg("--sandbox")
            .arg("off");
        Self::spawn(command)
    }

    /// A relay to `socket`; with `None` the socket comes from the config.
    fn attach(home: &Path, workspace: &Path, socket: Option<&Path>) -> Self {
        let mut command = otto(home);
        command
            .arg("acp")
            .arg("--attach")
            .arg("--cwd")
            .arg(workspace);
        if let Some(socket) = socket {
            command.arg("--socket").arg(socket);
        }
        Self::spawn(command)
    }

    fn send(&mut self, frame: Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{frame}").expect("write a frame");
        stdin.flush().expect("flush");
    }

    /// The next stdout line, which must be one JSON-RPC 2.0 frame.
    fn recv(&mut self) -> Value {
        let line = match self.lines.recv_timeout(TIMEOUT) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => panic!("no frame within {TIMEOUT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("otto closed stdout"),
        };
        let frame: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {line:?}"));
        assert_eq!(frame["jsonrpc"], "2.0", "line: {line}");
        frame
    }

    fn start(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Reads frames until the response to `id`; returns the frames before it
    /// and the response. The agent's own requests are answered by
    /// `on_request`, which returns the `result`.
    fn finish(
        &mut self,
        id: u64,
        on_request: &mut dyn FnMut(&Value) -> Value,
    ) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            let frame = self.recv();
            if frame.get("method").is_some() && frame.get("id").is_some() {
                let result = on_request(&frame);
                self.send(json!({"jsonrpc": "2.0", "id": frame["id"], "result": result}));
                before.push(frame);
            } else if frame.get("method").is_none() && frame["id"] == json!(id) {
                return (before, frame);
            } else {
                before.push(frame);
            }
        }
    }

    fn call(&mut self, method: &str, params: Value) -> (Vec<Value>, Value) {
        let id = self.start(method, params);
        self.finish(id, &mut |frame| panic!("unexpected request: {frame}"))
    }

    fn initialize(&mut self) -> Value {
        self.call("initialize", json!({"protocolVersion": 1})).1
    }

    /// Opens a session and returns its id and the `available_commands_update`
    /// frame that follows the response.
    fn new_session_with_commands(&mut self, workspace: &Path) -> (String, Value) {
        let (_, response) = self.call("session/new", json!({"cwd": workspace, "mcpServers": []}));
        let id = response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new failed: {response}"))
            .to_string();
        (id, self.recv())
    }

    fn new_session(&mut self, workspace: &Path) -> String {
        self.new_session_with_commands(workspace).0
    }

    fn load_session(&mut self, workspace: &Path, session_id: &str) -> (Vec<Value>, Value) {
        self.call(
            "session/load",
            json!({"cwd": workspace, "sessionId": session_id, "mcpServers": []}),
        )
    }

    fn cancel(&mut self, session_id: &str) {
        self.send(
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session_id}}),
        );
    }

    /// Waits for the process to exit; returns its exit code.
    fn wait(&mut self) -> Option<i32> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return status.code();
            }
            assert!(start.elapsed() < TIMEOUT, "otto acp did not exit");
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// Closes stdin and waits for exit.
    fn close(&mut self) -> Option<i32> {
        self.stdin = None;
        self.wait()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn updates(frames: &[Value]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|frame| frame["method"] == "session/update")
        .map(|frame| &frame["params"]["update"])
        .collect()
}

fn kinds(frames: &[Value]) -> Vec<&str> {
    updates(frames)
        .iter()
        .map(|update| update["sessionUpdate"].as_str().expect("sessionUpdate"))
        .collect()
}

/// The text of every `agent_message_chunk`, concatenated.
fn agent_text(frames: &[Value]) -> String {
    updates(frames)
        .iter()
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .map(|update| update["content"]["text"].as_str().unwrap_or_default())
        .collect()
}

fn prompt(session_id: &str, text: &str) -> Value {
    json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]})
}

fn read_script() -> Vec<String> {
    vec![
        tool_call_reply("call-1", "read", r#"{"path":"hello.txt"}"#),
        text_reply("all done"),
    ]
}

/// A home, a workspace and a provider serving `replies`.
fn fixture(replies: Vec<String>) -> (tempfile::TempDir, tempfile::TempDir, String) {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("hello.txt"), "file-body\n").unwrap();
    let (base_url, _requests) = serve(Script {
        replies,
        served: Arc::new(AtomicUsize::new(0)),
    });
    (home, workspace, base_url)
}

/// Provider request bodies, one entry per request received.
fn provider_with(replies: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    serve(Script {
        replies,
        served: Arc::new(AtomicUsize::new(0)),
    })
}

#[cfg(target_os = "macos")]
#[path = "acp_attach/approval.rs"]
mod approval;
#[path = "acp_attach/commands.rs"]
mod commands;
#[path = "acp_attach/relay.rs"]
mod relay;
