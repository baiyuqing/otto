//! Fake MCP server run when `OTTO_MCP_FAKE_SERVER` is set.

use super::*;

// ---------------------------------------------------------------------
// Fake server
// ---------------------------------------------------------------------

pub(super) fn run_fake_server() {
    let mode = std::env::var("OTTO_MCP_FAKE_SERVER").expect("mode already checked present");
    if mode == "exit-with-descendant" {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; while :; do sleep 30; done"])
            .spawn()
            .expect("spawn stubborn descendant");
        let marker = std::env::var("OTTO_MCP_DESCENDANT_MARKER").expect("descendant marker");
        std::fs::write(marker, child.id().to_string()).expect("write descendant pid");
        std::mem::forget(child);
    }
    if mode == "stubborn" {
        ignore_sigterm_for_stubborn_test();
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    let cancel_marker = std::env::var("OTTO_MCP_CANCEL_MARKER").ok();
    let stdout_lock = Arc::new(Mutex::new(()));

    if mode == "garbage" {
        eprintln!("fake-server-stderr-marker");
    }

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        handle_message(&mode, message, &stdout_lock, cancel_marker.clone());
    }
    std::process::exit(0);
}

#[allow(unsafe_code)]
fn ignore_sigterm_for_stubborn_test() {
    // SAFETY: this helper runs only in the fake-server child process for this
    // integration test. It installs SIG_IGN before any threads are spawned so
    // `StdioTransport::close` must escalate from SIGTERM to SIGKILL.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
}

fn handle_message(
    mode: &str,
    message: Value,
    stdout_lock: &Arc<Mutex<()>>,
    cancel_marker: Option<String>,
) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = message.get("id").cloned();

    match method {
        "server/discover" => {
            if mode == "garbage" {
                send_garbage(stdout_lock);
            }
            match mode {
                "legacy" => send_error(stdout_lock, id, -32601, "method not found", None),
                "unsupported" => send_error(
                    stdout_lock,
                    id,
                    -32020,
                    "unsupported protocol version",
                    Some(json!({"supported": ["2025-11-25"]})),
                ),
                _ => send_result(stdout_lock, id, json!({"protocolVersion": MODERN_VERSION})),
            }
        }
        "initialize" => {
            send_result(stdout_lock, id, json!({"protocolVersion": LEGACY_VERSION}));
        }
        "notifications/initialized" => {}
        "tools/list" => {
            if mode == "garbage" {
                send_garbage(stdout_lock);
            }
            let cursor = message
                .get("params")
                .and_then(|p| p.get("cursor"))
                .and_then(Value::as_str);
            if mode == "modern" || mode == "garbage" {
                match cursor {
                    None if mode == "modern" => {
                        send_result(
                            stdout_lock,
                            id,
                            json!({"tools": [tool_desc("echo")], "nextCursor": "page2"}),
                        );
                    }
                    Some("page2") if mode == "modern" => {
                        send_result(
                            stdout_lock,
                            id,
                            json!({"tools": [
                                tool_desc("image"),
                                tool_desc("structured"),
                                tool_desc("iserror"),
                                tool_desc("slow"),
                                tool_desc("env"),
                            ]}),
                        );
                    }
                    _ => send_result(stdout_lock, id, json!({"tools": [tool_desc("echo")]})),
                }
            } else {
                let names: &[&str] = match mode {
                    "exit" | "exit-with-descendant" => &["die"],
                    _ => &[],
                };
                let tools: Vec<Value> = names.iter().map(|name| tool_desc(name)).collect();
                send_result(stdout_lock, id, json!({"tools": tools}));
            }
        }
        "tools/call" => {
            if mode == "garbage" {
                send_garbage(stdout_lock);
            }
            let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match name.as_str() {
                "echo" => {
                    let text = arguments
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    send_result(
                        stdout_lock,
                        id,
                        json!({"content": [{"type": "text", "text": text}], "isError": false, "resultType": "complete"}),
                    );
                }
                "image" => send_result(
                    stdout_lock,
                    id,
                    json!({"content": [{"type": "image", "mimeType": "image/png", "data": "YWJj"}], "resultType": "complete"}),
                ),
                "structured" => send_result(
                    stdout_lock,
                    id,
                    json!({"content": [], "structuredContent": {"ok": true, "n": 42}, "resultType": "complete"}),
                ),
                "iserror" => send_result(
                    stdout_lock,
                    id,
                    json!({"content": [{"type": "text", "text": "boom"}], "isError": true, "resultType": "complete"}),
                ),
                "slow" => {
                    let stdout_lock = stdout_lock.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(5));
                        send_result(
                            &stdout_lock,
                            id,
                            json!({"content": [{"type": "text", "text": "slow-done"}], "resultType": "complete"}),
                        );
                    });
                }
                "env" => {
                    // macOS injects __CF_USER_TEXT_ENCODING into every spawned
                    // process regardless of env_clear(); excluded here since it
                    // is not something the transport's caller controls.
                    let mut vars: Vec<String> = std::env::vars()
                        .filter(|(key, _)| key != "__CF_USER_TEXT_ENCODING")
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect();
                    vars.sort();
                    send_result(
                        stdout_lock,
                        id,
                        json!({"content": [{"type": "text", "text": vars.join("\n")}], "resultType": "complete"}),
                    );
                }
                "die" => {
                    eprintln!("fake-server-die-marker: shutting down");
                    std::process::exit(7);
                }
                _ => send_error(stdout_lock, id, -32601, "unknown tool", None),
            }
        }
        "notifications/cancelled" => {
            if let Some(path) = cancel_marker
                && let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
            {
                let request_id = message
                    .get("params")
                    .and_then(|p| p.get("requestId"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let _ = writeln!(file, "cancelled:{request_id}");
            }
        }
        _ => {}
    }
}

fn tool_desc(name: &str) -> Value {
    json!({"name": name})
}

fn send(stdout_lock: &Mutex<()>, value: &Value) {
    let _guard = stdout_lock.lock().expect("stdout lock");
    let text = serde_json::to_string(value).expect("serialize fake server response");
    let mut out = io::stdout();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

fn send_garbage(stdout_lock: &Mutex<()>) {
    let _guard = stdout_lock.lock().expect("stdout lock");
    let mut out = io::stdout();
    let _ = writeln!(out, "not-json-at-all-{}", std::process::id());
    let _ = out.flush();
}

fn send_result(stdout_lock: &Mutex<()>, id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    send(
        stdout_lock,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
}

fn send_error(
    stdout_lock: &Mutex<()>,
    id: Option<Value>,
    code: i64,
    message: &str,
    data: Option<Value>,
) {
    let Some(id) = id else { return };
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    send(
        stdout_lock,
        &json!({"jsonrpc": "2.0", "id": id, "error": error}),
    );
}
