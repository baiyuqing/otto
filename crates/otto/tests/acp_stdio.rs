//! End-to-end coverage of `otto acp` through the built binary over pipes:
//! docs/specs/2026-10-02-acp-agent-server.md, "Tests".

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;
use common::{Script, serve, text_reply, tool_call_reply};

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

    fn new_session(&mut self, workspace: &Path) -> String {
        let (_, response) = self.call("session/new", json!({"cwd": workspace, "mcpServers": []}));
        response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new failed: {response}"))
            .to_string()
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

#[test]
fn initialize_advertises_the_implemented_capabilities() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    configure(home.path(), "http://127.0.0.1:1");
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    let (_, response) = client.call("initialize", json!({"protocolVersion": 99}));
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"]["list"],
        json!({})
    );
    assert_eq!(result["authMethods"], json!([]));
    assert_eq!(result["agentInfo"]["name"], "otto");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn invalid_requests_get_the_specified_error_codes() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    configure(home.path(), "http://127.0.0.1:1");
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();

    let code = |client: &mut Client, method: &str, params: Value| -> i64 {
        let (_, response) = client.call(method, params);
        response["error"]["code"]
            .as_i64()
            .unwrap_or_else(|| panic!("expected an error: {response}"))
    };
    let cwd = json!(workspace.path());
    assert_eq!(
        code(
            &mut client,
            "session/new",
            json!({"cwd": elsewhere.path(), "mcpServers": []})
        ),
        -32602,
        "cwd outside the workspace"
    );
    assert_eq!(
        code(
            &mut client,
            "session/new",
            json!({"cwd": cwd, "mcpServers": [{"name": "x", "command": "y", "args": [], "env": []}]})
        ),
        -32602,
        "non-empty mcpServers"
    );
    assert_eq!(
        code(
            &mut client,
            "session/load",
            json!({"cwd": cwd, "sessionId": "NOT-HEX", "mcpServers": []})
        ),
        -32602,
        "non-hex session id"
    );
    assert_eq!(
        code(
            &mut client,
            "session/load",
            json!({"cwd": cwd, "sessionId": "0".repeat(32), "mcpServers": []})
        ),
        -32002,
        "unknown session id"
    );
    assert_eq!(
        code(&mut client, "session/prompt", prompt(&"1".repeat(32), "hi")),
        -32002,
        "prompt for a session this connection never loaded"
    );
    assert_eq!(code(&mut client, "session/bogus", json!({})), -32601);

    client.send(json!([1, 2]));
    let frame = client.recv();
    assert_eq!(frame["error"]["code"], -32600);
    let stdin = client.stdin.as_mut().unwrap();
    writeln!(stdin, "{{not json").unwrap();
    let frame = client.recv();
    assert_eq!(frame["error"]["code"], -32700);
    assert_eq!(frame["id"], Value::Null);
    assert_eq!(client.close(), Some(0));
}

#[test]
fn prompt_streams_updates_then_list_and_load_see_the_session() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("hello.txt"), "file-body\n").unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let (base_url, _requests) = serve(Script {
        replies: vec![
            tool_call_reply("call-1", "read", r#"{"path":"hello.txt"}"#),
            text_reply("all done"),
        ],
        served: Arc::clone(&served),
    });
    configure(home.path(), &base_url);

    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());
    let (frames, response) = client.call("session/prompt", prompt(&session_id, "read hello"));
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let live = kinds(&frames);
    assert_eq!(live.first(), Some(&"tool_call"), "{frames:?}");
    assert!(live.contains(&"tool_call_update"));
    assert_eq!(live.last(), Some(&"agent_message_chunk"));
    let call = updates(&frames)[0];
    assert_eq!(call["toolCallId"], "call-1");
    assert_eq!(call["kind"], "read");
    assert_eq!(call["title"], "hello.txt");
    assert_eq!(call["status"], "in_progress");
    let done = updates(&frames)
        .into_iter()
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .unwrap();
    assert_eq!(done["status"], "completed");
    assert!(
        done["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("file-body")
    );
    assert_eq!(
        updates(&frames).last().unwrap()["content"]["text"],
        "all done"
    );

    let (_, listed) = client.call("session/list", json!({"cwd": workspace.path()}));
    let sessions = listed["result"]["sessions"].as_array().expect("sessions");
    let row = sessions
        .iter()
        .find(|row| row["sessionId"] == json!(session_id))
        .unwrap_or_else(|| panic!("session missing from {listed}"));
    assert_eq!(row["title"], "read hello");
    assert!(row["updatedAt"].is_string());
    assert_eq!(client.close(), Some(0));

    // A new process replays the stored history before the load response.
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let (frames, response) = client.call(
        "session/load",
        json!({"cwd": workspace.path(), "sessionId": session_id, "mcpServers": []}),
    );
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
    let replayed = updates(&frames);
    assert_eq!(replayed[0]["content"]["text"], "read hello");
    assert_eq!(replayed[1]["toolCallId"], "call-1");
    assert_eq!(replayed[2]["toolCallId"], "call-1");
    assert_eq!(replayed[3]["content"]["text"], "all done");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn cancel_ends_a_running_prompt_with_cancelled() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());

    let id = client.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);
    // A second prompt on the busy session is refused.
    let (_, busy) = client.call("session/prompt", prompt(&session_id, "again"));
    assert_eq!(busy["error"]["code"], -32603);
    assert_eq!(busy["error"]["message"], "a prompt is already running");

    client.send(
        json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session_id}}),
    );
    let (_, response) = client.finish(id, &mut |frame| panic!("unexpected request: {frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn stdin_eof_during_a_prompt_answers_cancelled_and_exits_zero() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());

    let id = client.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);
    client.stdin = None;
    let (_, response) = client.finish(id, &mut |frame| panic!("unexpected request: {frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(client.close(), Some(0));
}

/// The first `.jsonl` at `root/<workspace key>/`; child transcripts live one
/// level deeper and are not matched.
fn find_session_file(root: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|workspace| std::fs::read_dir(workspace.path()).ok())
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path())
        .find(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        })
}

fn sigterm(client: &Client) {
    let pid = nix::unistd::Pid::from_raw(client.child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).expect("send SIGTERM");
}

#[test]
fn sigterm_without_a_lease_cancels_the_prompt_and_exits_zero() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure(home.path(), &base_url);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());

    let id = client.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);
    sigterm(&client);
    let (_, response) = client.finish(id, &mut |frame| panic!("unexpected request: {frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(client.close(), Some(0));
}

#[test]
fn sigterm_with_a_lease_migrates_the_session() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (base_url, accepted) = serve_stalled();
    configure_with(home.path(), &base_url, true);
    let mut client = Client::spawn(home.path(), workspace.path(), "off");
    client.initialize();
    let session_id = client.new_session(workspace.path());

    let id = client.start("session/prompt", prompt(&session_id, "wait"));
    wait_for_count(&accepted, 1);
    let session_path = find_session_file(&home.path().join(".otto/sessions"))
        .expect("the first prompt wrote the session file");
    sigterm(&client);
    let (_, response) = client.finish(id, &mut |frame| panic!("unexpected request: {frame}"));
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(client.close(), Some(0));

    let inbox = std::fs::read_to_string(session_path.with_extension("inbox.json")).expect("inbox");
    assert!(inbox.contains("This session was moved"), "{inbox}");
    let heartbeat = std::fs::read_to_string(session_path.with_extension("lease").join("heartbeat"))
        .expect("heartbeat");
    assert!(heartbeat.contains("\"released\":true"), "{heartbeat}");
}

/// The elevated bash approval round trip needs Seatbelt, so it runs on macOS.
#[cfg(target_os = "macos")]
mod approval {
    use super::*;

    struct Run {
        frames: Vec<Value>,
        response: Value,
        permission_requests: Vec<Value>,
        served: usize,
    }

    fn run(option: &str, replies: Vec<String>) -> Run {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("elevated.txt"), "outside-seatbelt\n").unwrap();
        let served = Arc::new(AtomicUsize::new(0));
        let (base_url, _requests) = serve(Script {
            replies,
            served: Arc::clone(&served),
        });
        configure(home.path(), &base_url);
        let mut client = Client::spawn(home.path(), workspace.path(), "seatbelt");
        client.initialize();
        let session_id = client.new_session(workspace.path());
        let id = client.start("session/prompt", prompt(&session_id, "read the fixture"));
        let mut permission_requests = Vec::new();
        let (frames, response) = client.finish(id, &mut |frame| {
            assert_eq!(frame["method"], "session/request_permission");
            permission_requests.push(frame.clone());
            json!({"outcome": {"outcome": "selected", "optionId": option}})
        });
        let served = served.load(Ordering::SeqCst);
        assert_eq!(client.close(), Some(0));
        Run {
            frames,
            response,
            permission_requests,
            served,
        }
    }

    fn escalated(call_id: &str) -> String {
        let arguments = json!({
            "command": "cat \"$HOME/elevated.txt\"",
            "sandbox_permissions": "require_escalated",
            "justification": "read the reviewed home fixture",
        })
        .to_string();
        tool_call_reply(call_id, "bash", &arguments)
    }

    #[test]
    fn allow_once_runs_the_retry_in_the_same_prompt() {
        let result = run(
            "allow_once",
            vec![
                escalated("call-1"),
                text_reply("approval needed"),
                escalated("call-2"),
                text_reply("done"),
            ],
        );
        assert_eq!(
            result.response["result"]["stopReason"], "end_turn",
            "{}",
            result.response
        );
        assert_eq!(result.permission_requests.len(), 1);
        let params = &result.permission_requests[0]["params"];
        assert_eq!(params["toolCall"]["toolCallId"], "call-1");
        assert_eq!(params["toolCall"]["title"], "cat \"$HOME/elevated.txt\"");
        assert_eq!(params["toolCall"]["kind"], "execute");
        let options: Vec<(&str, &str)> = params["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| {
                (
                    option["optionId"].as_str().unwrap(),
                    option["kind"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            options,
            [("allow_once", "allow_once"), ("reject_once", "reject_once")]
        );
        assert_eq!(result.served, 4);
        let text = serde_json::to_string(&result.frames).unwrap();
        assert!(text.contains("outside-seatbelt"), "{text}");
        assert_eq!(
            updates(&result.frames).last().unwrap()["content"]["text"],
            "done"
        );
    }

    #[test]
    fn reject_once_ends_the_turn_without_a_retry() {
        let result = run(
            "reject_once",
            vec![
                escalated("call-1"),
                text_reply("approval needed"),
                escalated("call-2"),
                text_reply("done"),
            ],
        );
        assert_eq!(
            result.response["result"]["stopReason"], "end_turn",
            "{}",
            result.response
        );
        assert_eq!(result.permission_requests.len(), 1);
        assert_eq!(
            result.served, 2,
            "the retry prompt must not reach the provider"
        );
        let text = serde_json::to_string(&result.frames).unwrap();
        assert!(!text.contains("outside-seatbelt"), "{text}");
    }
}
