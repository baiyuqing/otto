//! End-to-end coverage of `otto acp` through the built binary over pipes:
//! docs/specs/2026-10-02-acp-agent-server.md, "Tests".

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;
use common::{
    Script, assert_terminal_tool_update, serve, summary_reply, text_reply, tool_call_reply,
};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

fn configure(home: &Path, base_url: &str) {
    configure_with(home, base_url, false);
}

fn configure_with(home: &Path, base_url: &str, failover: bool) {
    std::fs::create_dir_all(home.join("Library/Caches")).expect("cache base");
    let config_dir = home.join(".config/otto");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let mut text = format!(
        "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"{base_url}\"\nmodel = \"gpt-test\"\napi_key_env = \"OTTO_API_KEY\"\n"
    );
    if failover {
        text.push_str("\n[failover]\nenabled = true\nlease_seconds = 12\n");
    }
    std::fs::write(config_dir.join("config.toml"), text).expect("write config");
}

/// A provider that accepts connections and never answers them, so a turn
/// stays running until the client cancels it. The count is the number of
/// connections accepted.
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

fn serve_gated(replies: Vec<String>) -> (String, Arc<AtomicUsize>, Sender<()>) {
    serve_gated_at(replies, 0)
}

/// Like `serve_gated`, but the request with index `gated` waits for the release.
fn serve_gated_at(replies: Vec<String>, gated: usize) -> (String, Arc<AtomicUsize>, Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let base_url = format!("http://{}", listener.local_addr().expect("local address"));
    let served = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&served);
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
            while raw.len() - head_end < length {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => raw.extend_from_slice(&buffer[..read]),
                }
            }
            let index = counter.fetch_add(1, Ordering::SeqCst);
            if index == gated {
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
    (base_url, served, release)
}

/// An `otto acp` child driven over its pipes.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Client {
    fn spawn(home: &Path, workspace: &Path, sandbox: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_otto"));
        command
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("SHELL", "/bin/sh")
            .env("OTTO_API_KEY", "sk-e2e-not-a-real-key")
            .arg("acp")
            .arg("--cwd")
            .arg(workspace)
            .arg("--sandbox")
            .arg(sandbox)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmpdir);
        }
        let mut child = command.spawn().expect("spawn otto acp");
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
        let kinds = ["method", "result", "error"]
            .iter()
            .filter(|key| frame.get(**key).is_some())
            .count();
        assert!(
            kinds >= 1,
            "line is not a request, response or notification: {line}"
        );
        frame
    }

    /// Sends a request; returns the id.
    fn start(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// Reads frames until the response to `id`. `on_request` answers the
    /// agent's own requests: it gets each one and returns the `result`.
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
        let (_, response) = self.call("initialize", json!({"protocolVersion": 1}));
        response
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

    /// Closes stdin and waits for exit; returns the exit code.
    fn close(&mut self) -> Option<i32> {
        self.stdin = None;
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return status.code();
            }
            assert!(start.elapsed() < TIMEOUT, "otto acp did not exit");
            std::thread::sleep(Duration::from_millis(30));
        }
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

fn wait_for_count(count: &AtomicUsize, at_least: usize) {
    let start = Instant::now();
    while count.load(Ordering::SeqCst) < at_least {
        assert!(start.elapsed() < TIMEOUT, "provider was never called");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn prompt(session_id: &str, text: &str) -> Value {
    json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]})
}

/// Provider request bodies, one entry per request received.
fn provider_with(replies: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    serve(Script {
        replies,
        served: Arc::new(AtomicUsize::new(0)),
    })
}

fn message_text(frames: &[Value]) -> String {
    updates(frames)
        .iter()
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .map(|update| update["content"]["text"].as_str().unwrap_or_default())
        .collect()
}

fn assert_task_queue_capability(body: &str) {
    let request: Value = serde_json::from_str(body).expect("captured model request");
    let queue = request["tools"]
        .as_array()
        .expect("tool schemas")
        .iter()
        .find(|tool| tool["function"]["name"] == "task_queue")
        .unwrap_or_else(|| panic!("task_queue schema missing from request: {request}"));
    assert_eq!(
        queue["function"]["parameters"]["required"],
        json!(["prompt"])
    );
    assert_eq!(
        queue["function"]["parameters"]["properties"]["prompt"]["maxLength"],
        32000
    );
    let system = request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "system")
        .and_then(|message| message["content"].as_str())
        .expect("system prompt");
    assert!(system.contains("<task_control_policy>"), "{system}");
    assert!(
        system.contains("at most 16 follow-ups run in FIFO order"),
        "{system}"
    );
}

/// Reads frames until a wake reply update arrives and returns its text.
fn next_wake_reply(client: &mut Client) -> String {
    loop {
        let frame = client.recv();
        let update = &frame["params"]["update"];
        if update["_meta"]["otto"]["wakeReply"] == true {
            assert_eq!(update["sessionUpdate"], "agent_message_chunk");
            assert_eq!(update["_meta"]["otto"]["replyId"], 1);
            return update["content"]["text"].as_str().unwrap().to_string();
        }
    }
}

#[cfg(target_os = "macos")]
#[path = "acp_stdio/approval.rs"]
mod approval;
#[path = "acp_stdio/commands.rs"]
mod commands;
#[path = "acp_stdio/memory.rs"]
mod memory;
#[path = "acp_stdio/protocol.rs"]
mod protocol;
#[path = "acp_stdio/tasks.rs"]
mod tasks;
