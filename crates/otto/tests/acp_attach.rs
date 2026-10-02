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

mod common;
use common::{Script, serve, text_reply, tool_call_reply};

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

    fn new_session(&mut self, workspace: &Path) -> String {
        let (_, response) = self.call("session/new", json!({"cwd": workspace, "mcpServers": []}));
        response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new failed: {response}"))
            .to_string()
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

#[test]
fn a_relayed_prompt_sends_the_same_updates_as_local_acp() {
    // Local `otto acp`.
    let (home, workspace, base_url) = fixture(read_script());
    configure(home.path(), &base_url, None);
    let mut local = Client::local(home.path(), workspace.path());
    local.initialize();
    let session_id = local.new_session(workspace.path());
    let (local_frames, response) = local.call("session/prompt", prompt(&session_id, "read hello"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(local.close(), Some(0));

    // The relay, with the socket taken from `[server].socket`.
    let (home, workspace, base_url) = fixture(read_script());
    let sockets = private_dir();
    let socket = sockets.path().join("o.sock");
    configure(home.path(), &base_url, Some(&socket));
    let _serve = Serve::start_with(home.path(), workspace.path(), "off", Some(&socket));
    let mut relay = Client::attach(home.path(), workspace.path(), None);
    let init = relay.initialize();
    assert_eq!(init["result"]["agentInfo"]["name"], "otto");
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);
    assert!(
        init["result"]["agentCapabilities"]["_meta"].is_null(),
        "the relay must not advertise memory review: {init}"
    );
    let session_id = relay.new_session(workspace.path());
    let (_, memory) = relay.call("_otto/memory/pending", json!({"sessionId": session_id}));
    assert_eq!(memory["error"]["code"], -32601, "{memory}");
    let (relay_frames, response) = relay.call("session/prompt", prompt(&session_id, "read hello"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

    assert_eq!(
        kinds(&relay_frames),
        ["tool_call", "tool_call_update", "agent_message_chunk"],
        "{relay_frames:?}"
    );
    assert_eq!(updates(&relay_frames), updates(&local_frames));
    assert_eq!(relay.close(), Some(0));
}

#[test]
fn session_load_replays_history_before_the_response_and_list_titles_follow_the_session() {
    let (home, workspace, base_url) = fixture(read_script());
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut first = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    first.initialize();
    let session_id = first.new_session(workspace.path());
    let (_, response) = first.call("session/prompt", prompt(&session_id, "read hello"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

    let title = |client: &mut Client| -> Value {
        let (_, listed) = client.call("session/list", json!({"cwd": workspace.path()}));
        let rows = listed["result"]["sessions"].as_array().expect("sessions");
        let row = rows
            .iter()
            .find(|row| row["sessionId"] == json!(session_id))
            .unwrap_or_else(|| panic!("session missing from {listed}"));
        assert!(row["updatedAt"].is_string(), "{row}");
        row["title"].clone()
    };
    assert_eq!(title(&mut first), "read hello");
    let (status, _) = http(
        &serve.socket,
        "PATCH",
        &format!("/v1/sessions/{session_id}"),
        Some(r#"{"name":"named session"}"#),
    );
    assert_eq!(status, 200);
    assert_eq!(title(&mut first), "named session");

    // A second relay replays the history, then answers.
    let mut second = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    second.initialize();
    let (frames, response) = second.load_session(workspace.path(), &session_id);
    assert_eq!(
        response["result"]["sessionId"],
        json!(session_id),
        "{response}"
    );
    assert_eq!(
        kinds(&frames),
        [
            "user_message_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk"
        ],
        "{frames:?}"
    );
    assert_eq!(updates(&frames)[0]["content"]["text"], "read hello");
    assert_eq!(updates(&frames)[3]["content"]["text"], "all done");

    // An id serve has no file for is the error local `otto acp` returns.
    let (_, unknown) = second.load_session(workspace.path(), &"0".repeat(32));
    assert_eq!(unknown["error"]["code"], -32002, "{unknown}");
    assert_eq!(unknown["error"]["message"], "unknown sessionId");
    let (_, invalid) = second.load_session(workspace.path(), "NOT-HEX");
    assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
    // `cwd` checks are the local ones.
    let (_, elsewhere) = second.call("session/new", json!({"cwd": home.path(), "mcpServers": []}));
    assert_eq!(elsewhere["error"]["code"], -32602, "{elsewhere}");
    assert_eq!(first.close(), Some(0));
    assert_eq!(second.close(), Some(0));
}

#[test]
fn prompts_of_two_relays_on_one_session_queue_and_each_gets_only_its_own_text() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, received, release) = serve_gated(vec![
        text_reply("reply to first"),
        text_reply("reply to second"),
    ]);
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut first = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    first.initialize();
    let session_id = first.new_session(workspace.path());
    let mut second = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    second.initialize();
    // Nothing is stored yet, so the load finds the session open on serve.
    let (_, loaded) = second.load_session(workspace.path(), &session_id);
    assert_eq!(loaded["result"]["sessionId"], json!(session_id), "{loaded}");

    let first_id = first.start("session/prompt", prompt(&session_id, "first"));
    wait_for_count(&received, 1);
    let second_id = second.start("session/prompt", prompt(&session_id, "second"));
    // The second turn is queued behind the first, which is held at the
    // provider; give the request time to reach serve.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(received.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();

    let (first_frames, first_response) = first.finish(first_id, &mut |frame| panic!("{frame}"));
    let (second_frames, second_response) = second.finish(second_id, &mut |frame| panic!("{frame}"));
    assert_eq!(first_response["result"]["stopReason"], "end_turn");
    assert_eq!(second_response["result"]["stopReason"], "end_turn");
    assert_eq!(agent_text(&first_frames), "reply to first");
    assert_eq!(agent_text(&second_frames), "reply to second");
    assert_eq!(first.close(), Some(0));
    assert_eq!(second.close(), Some(0));
}

#[test]
fn session_cancel_ends_a_running_prompt_and_a_queued_one() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure(home.path(), &base_url, None);
    let serve = Serve::start(home.path(), workspace.path(), "off");
    let mut first = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    first.initialize();
    let session_id = first.new_session(workspace.path());
    let mut second = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    second.initialize();
    second.load_session(workspace.path(), &session_id);

    let first_id = first.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);
    // A second prompt on the same relay session is refused as locally.
    let (_, busy) = first.call("session/prompt", prompt(&session_id, "again"));
    assert_eq!(busy["error"]["message"], "a prompt is already running");

    // The queued prompt is cancelled while the first still runs.
    let second_id = second.start("session/prompt", prompt(&session_id, "queued"));
    std::thread::sleep(Duration::from_millis(300));
    second.cancel(&session_id);
    let (_, response) = second.finish(second_id, &mut |frame| panic!("{frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");

    first.cancel(&session_id);
    let (_, response) = first.finish(first_id, &mut |frame| panic!("{frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(first.close(), Some(0));
    assert_eq!(second.close(), Some(0));
}

#[test]
fn losing_serve_during_a_prompt_answers_an_error_and_exits_with_status_1() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure(home.path(), &base_url, None);
    let mut serve = Serve::start(home.path(), workspace.path(), "off");
    let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
    relay.initialize();
    let session_id = relay.new_session(workspace.path());
    let id = relay.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);

    serve.child.kill().expect("stop serve");
    let (_, response) = relay.finish(id, &mut |frame| panic!("{frame}"));
    assert_eq!(response["error"]["code"], -32603, "{response}");
    assert_eq!(
        response["error"]["message"], "connection to otto serve lost",
        "{response}"
    );
    assert_eq!(relay.wait(), Some(1));
}

#[test]
fn start_without_serve_prints_the_reason_and_exits_with_status_1() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    configure(home.path(), "http://127.0.0.1:1", None);
    let sockets = tempfile::tempdir().unwrap();
    let socket = sockets.path().join("absent.sock");
    let output = otto(home.path())
        .args(["acp", "--attach", "--cwd"])
        .arg(workspace.path())
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::null())
        .output()
        .expect("run otto acp --attach");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with(&format!(
            "otto serve is not reachable at {}: ",
            socket.display()
        )),
        "{stderr}"
    );
}

/// The elevated bash approval needs Seatbelt on serve, so it runs on macOS.
#[cfg(target_os = "macos")]
mod approval {
    use super::*;

    fn escalated(call_id: &str) -> String {
        let arguments = json!({
            "command": "cat \"$HOME/elevated.txt\"",
            "sandbox_permissions": "require_escalated",
            "justification": "read the reviewed home fixture",
        })
        .to_string();
        tool_call_reply(call_id, "bash", &arguments)
    }

    fn script() -> (Vec<String>, Arc<AtomicUsize>) {
        (
            vec![
                escalated("call-1"),
                text_reply("approval needed"),
                escalated("call-2"),
                text_reply("done"),
            ],
            Arc::new(AtomicUsize::new(0)),
        )
    }

    fn start() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Serve,
        Arc<AtomicUsize>,
    ) {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("elevated.txt"), "outside-seatbelt\n").unwrap();
        let (replies, served) = script();
        let (base_url, _requests) = serve(Script {
            replies,
            served: Arc::clone(&served),
        });
        configure(home.path(), &base_url, None);
        let serve = Serve::start(home.path(), workspace.path(), "seatbelt");
        (home, workspace, serve, served)
    }

    fn answered_by_relay(option: &str) -> (Value, Vec<Value>, usize) {
        let (home, workspace, serve, served) = start();
        let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
        relay.initialize();
        let session_id = relay.new_session(workspace.path());
        let id = relay.start("session/prompt", prompt(&session_id, "read the fixture"));
        let mut requests = Vec::new();
        let (frames, response) = relay.finish(id, &mut |frame| {
            assert_eq!(frame["method"], "session/request_permission");
            requests.push(frame.clone());
            json!({"outcome": {"outcome": "selected", "optionId": option}})
        });
        assert_eq!(requests.len(), 1);
        let params = &requests[0]["params"];
        assert_eq!(params["toolCall"]["toolCallId"], "call-1");
        assert_eq!(params["toolCall"]["title"], "cat \"$HOME/elevated.txt\"");
        assert_eq!(
            params["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| option["optionId"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["allow_once", "reject_once"]
        );
        let served = served.load(Ordering::SeqCst);
        assert_eq!(relay.close(), Some(0));
        (response, frames, served)
    }

    #[test]
    fn allow_once_runs_the_retry_in_the_same_prompt() {
        let (response, frames, served) = answered_by_relay("allow_once");
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        assert_eq!(served, 4);
        assert!(
            serde_json::to_string(&frames)
                .unwrap()
                .contains("outside-seatbelt")
        );
        assert!(agent_text(&frames).ends_with("done"));
    }

    #[test]
    fn reject_once_ends_the_turn_without_a_retry() {
        let (response, frames, served) = answered_by_relay("reject_once");
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        assert_eq!(served, 2, "the retry prompt must not reach the provider");
        assert!(
            !serde_json::to_string(&frames)
                .unwrap()
                .contains("outside-seatbelt")
        );
    }

    #[test]
    fn a_decision_over_http_withdraws_the_open_permission_request() {
        let (home, workspace, serve, served) = start();
        let mut relay = Client::attach(home.path(), workspace.path(), Some(&serve.socket));
        relay.initialize();
        let session_id = relay.new_session(workspace.path());
        let id = relay.start("session/prompt", prompt(&session_id, "read the fixture"));
        let mut frames = Vec::new();
        let request = loop {
            let frame = relay.recv();
            if frame["method"] == "session/request_permission" {
                break frame;
            }
            frames.push(frame);
        };

        // Another client reads the approval id from the turn's events and
        // allows it.
        let (_, session) = http(
            &serve.socket,
            "GET",
            &format!("/v1/sessions/{session_id}"),
            None,
        );
        let session: Value = serde_json::from_str(&session).expect("session json");
        let turn_id = session["turn"]["id"]
            .as_str()
            .expect("running turn")
            .to_string();
        let events = read_until(
            &serve.socket,
            &format!("/v1/sessions/{session_id}/turns/{turn_id}/events"),
            "approval_requested",
        );
        let approval_id = events
            .split("\"approval_id\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("approval id")
            .to_string();
        let (status, _) = http(
            &serve.socket,
            "POST",
            &format!("/v1/sessions/{session_id}/approvals/{approval_id}"),
            Some(r#"{"decision":"allow"}"#),
        );
        assert_eq!(status, 200);

        // The relay withdraws the request; its answer, sent late as a
        // denial, changes nothing: the prompt ends with the retry's text.
        let cancel = loop {
            let frame = relay.recv();
            if frame["method"] == "$/cancel_request" {
                break frame;
            }
            frames.push(frame);
        };
        assert_eq!(cancel["params"]["requestId"], request["id"], "{cancel}");
        relay.send(json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {"outcome": {"outcome": "selected", "optionId": "reject_once"}},
        }));
        let (rest, response) = relay.finish(id, &mut |frame| panic!("{frame}"));
        frames.extend(rest);
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        assert!(agent_text(&frames).ends_with("done"), "{frames:?}");
        assert_eq!(served.load(Ordering::SeqCst), 4);
        assert_eq!(relay.close(), Some(0));
    }
}
