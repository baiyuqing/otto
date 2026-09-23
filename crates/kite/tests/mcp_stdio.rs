//! Integration test and fake server for the stdio MCP transport and client.
//!
//! `harness = false`: this binary is both the test runner and, when
//! `KITE_MCP_FAKE_SERVER` is set, the fake server itself. Tests spawn
//! `std::env::current_exe()` (this same binary) as the child process with
//! that variable set to a mode name, so no external script or interpreter
//! is needed.
//!
//! Modes: `modern` (paged tool list, all call-shape tools), `legacy`
//! (rejects `server/discover` with `-32601`), `unsupported` (rejects it
//! with `-32020` and no modern version in `supported`), `garbage` (mixes
//! invalid lines and stderr output into a working `modern`-shaped session),
//! `exit` (exits when its one tool, `die`, is called), `stubborn` (ignores
//! EOF and SIGTERM until SIGKILL).

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kite::mcp::client::Client;
use kite::mcp::jsonrpc::modern_meta;
use kite::mcp::stdio::StdioTransport;
use kite::mcp::{
    CallError, ContentBlock, Era, LEGACY_VERSION, MODERN_VERSION, Outbound, ToolServer, Transport,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn main() {
    if std::env::var("KITE_MCP_FAKE_SERVER").is_ok() {
        run_fake_server();
        return;
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
    let mut failed = false;
    macro_rules! run {
        ($name:ident) => {
            match rt.block_on($name()) {
                Ok(()) => println!("ok {}", stringify!($name)),
                Err(reason) => {
                    println!("FAILED {}: {}", stringify!($name), reason);
                    failed = true;
                }
            }
        };
    }

    run!(test_modern_negotiation_and_paged_tools);
    run!(test_legacy_negotiation_method_not_found);
    run!(test_legacy_negotiation_unsupported_protocol_version);
    run!(test_garbage_line_and_stderr_tolerated);
    run!(test_echo_round_trip);
    run!(test_image_and_structured_decode);
    run!(test_is_error_propagates);
    run!(test_cancelled_call_returns_promptly);
    run!(test_call_timeout);
    run!(test_server_exit_fails_pending_and_later_calls);
    run!(test_close_terminates_child_and_is_idempotent);
    run!(test_close_escalates_stubborn_child_promptly);
    run!(test_env_restriction);

    if failed {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------

fn check(condition: bool, message: impl Into<String>) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

async fn connect_client(
    mode: &str,
    extra_env: &[(String, String)],
    connect_timeout: Duration,
    call_timeout: Duration,
) -> Result<Client, CallError> {
    let (command, env) = fake_server_command(mode, extra_env);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let transport = StdioTransport::spawn(&command, &[], &env, &cwd).await?;
    let cancel = CancellationToken::new();
    Client::connect(
        "test-server".to_string(),
        Box::new(transport),
        connect_timeout,
        call_timeout,
        &cancel,
    )
    .await
}

async fn connect_default(mode: &str) -> Result<Client, CallError> {
    connect_client(mode, &[], Duration::from_secs(5), Duration::from_secs(5)).await
}

fn fake_server_command(
    mode: &str,
    extra_env: &[(String, String)],
) -> (String, Vec<(String, String)>) {
    let exe = std::env::current_exe().expect("current exe path");
    let mut env = vec![("KITE_MCP_FAKE_SERVER".to_string(), mode.to_string())];
    env.extend_from_slice(extra_env);
    (exe.to_str().expect("utf8 exe path").to_string(), env)
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

async fn test_modern_negotiation_and_paged_tools() -> Result<(), String> {
    let client = connect_default("modern").await.map_err(|e| e.to_string())?;
    check(
        *client.era() == Era::Modern,
        format!("expected Modern era, got {:?}", client.era()),
    )?;
    let names: Vec<&str> = client.tools().iter().map(|t| t.name.as_str()).collect();
    for expected in ["echo", "image", "structured", "iserror", "slow", "env"] {
        check(
            names.contains(&expected),
            format!("missing tool {expected} in {names:?}"),
        )?;
    }
    check(
        names.len() == 6,
        format!("expected 6 tools from two pages, got {names:?}"),
    )?;
    client.close().await;
    Ok(())
}

async fn test_legacy_negotiation_method_not_found() -> Result<(), String> {
    let client = connect_default("legacy").await.map_err(|e| e.to_string())?;
    check(
        matches!(client.era(), Era::Legacy(version) if version == LEGACY_VERSION),
        format!("expected Legacy({LEGACY_VERSION}), got {:?}", client.era()),
    )?;
    client.close().await;
    Ok(())
}

async fn test_legacy_negotiation_unsupported_protocol_version() -> Result<(), String> {
    let client = connect_default("unsupported")
        .await
        .map_err(|e| e.to_string())?;
    check(
        matches!(client.era(), Era::Legacy(version) if version == LEGACY_VERSION),
        format!("expected Legacy({LEGACY_VERSION}), got {:?}", client.era()),
    )?;
    client.close().await;
    Ok(())
}

async fn test_garbage_line_and_stderr_tolerated() -> Result<(), String> {
    let (command, env) = fake_server_command("garbage", &[]);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let transport = StdioTransport::spawn(&command, &[], &env, &cwd)
        .await
        .map_err(|e| e.to_string())?;
    let cancel = CancellationToken::new();

    let discover = Outbound {
        method: "server/discover".to_string(),
        params: modern_meta(json!({})),
        era: None,
    };
    let result = transport
        .request(discover, &cancel)
        .await
        .map_err(|e| e.to_string())?;
    check(
        result.is_ok(),
        format!("discover failed despite garbage lines: {result:?}"),
    )?;

    let call = Outbound {
        method: "tools/call".to_string(),
        params: modern_meta(json!({"name": "echo", "arguments": {"message": "hi"}})),
        era: Some(Era::Modern),
    };
    let result = transport
        .request(call, &cancel)
        .await
        .map_err(|e| e.to_string())?;
    check(
        result.is_ok(),
        format!("call failed despite garbage lines: {result:?}"),
    )?;

    let mut tail = transport.stderr_tail();
    for _ in 0..50 {
        if tail.contains("fake-server-stderr-marker") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        tail = transport.stderr_tail();
    }
    check(
        tail.contains("fake-server-stderr-marker"),
        format!("stderr tail missing marker, got {tail:?}"),
    )?;

    transport.close().await;
    Ok(())
}

async fn test_echo_round_trip() -> Result<(), String> {
    let client = connect_default("modern").await.map_err(|e| e.to_string())?;
    let cancel = CancellationToken::new();
    let outcome = client
        .call("echo", json!({"message": "round-trip"}), &cancel)
        .await
        .map_err(|e| e.to_string())?;
    check(!outcome.is_error, "echo should not be an error")?;
    check(
        outcome.content
            == vec![ContentBlock::Text {
                text: "round-trip".to_string(),
            }],
        format!("unexpected echo content: {:?}", outcome.content),
    )?;
    client.close().await;
    Ok(())
}

async fn test_image_and_structured_decode() -> Result<(), String> {
    let client = connect_default("modern").await.map_err(|e| e.to_string())?;
    let cancel = CancellationToken::new();

    let image = client
        .call("image", json!({}), &cancel)
        .await
        .map_err(|e| e.to_string())?;
    match image.content.as_slice() {
        [
            ContentBlock::Image {
                mime_type,
                data_len,
            },
        ] => {
            check(
                mime_type == "image/png",
                format!("unexpected mime type {mime_type}"),
            )?;
            check(*data_len == 4, format!("unexpected data_len {data_len}"))?;
        }
        other => return Err(format!("unexpected image content: {other:?}")),
    }

    let structured = client
        .call("structured", json!({}), &cancel)
        .await
        .map_err(|e| e.to_string())?;
    check(
        structured.structured_content == Some(json!({"ok": true, "n": 42})),
        format!(
            "unexpected structured content: {:?}",
            structured.structured_content
        ),
    )?;

    client.close().await;
    Ok(())
}

async fn test_is_error_propagates() -> Result<(), String> {
    let client = connect_default("modern").await.map_err(|e| e.to_string())?;
    let cancel = CancellationToken::new();
    let outcome = client
        .call("iserror", json!({}), &cancel)
        .await
        .map_err(|e| e.to_string())?;
    check(outcome.is_error, "expected isError: true to propagate")?;
    client.close().await;
    Ok(())
}

async fn test_cancelled_call_returns_promptly() -> Result<(), String> {
    let marker = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    let marker_path = marker
        .path()
        .to_str()
        .expect("utf8 marker path")
        .to_string();
    let client = connect_client(
        "modern",
        &[("KITE_MCP_CANCEL_MARKER".to_string(), marker_path.clone())],
        Duration::from_secs(5),
        Duration::from_secs(30),
    )
    .await
    .map_err(|e| e.to_string())?;

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_clone.cancel();
    });

    let start = Instant::now();
    let result = client.call("slow", json!({}), &cancel).await;
    let elapsed = start.elapsed();

    check(
        matches!(result, Err(CallError::Cancelled)),
        format!("expected Cancelled, got {result:?}"),
    )?;
    check(
        elapsed < Duration::from_secs(1),
        format!("cancel took too long: {elapsed:?}"),
    )?;

    let mut found = false;
    for _ in 0..100 {
        if let Ok(contents) = std::fs::read_to_string(&marker_path)
            && contents.contains("cancelled")
        {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    check(found, "server did not record notifications/cancelled")?;

    client.close().await;
    Ok(())
}

async fn test_call_timeout() -> Result<(), String> {
    let client = connect_client(
        "modern",
        &[],
        Duration::from_secs(5),
        Duration::from_millis(150),
    )
    .await
    .map_err(|e| e.to_string())?;
    let cancel = CancellationToken::new();
    let result = client.call("slow", json!({}), &cancel).await;
    check(
        matches!(result, Err(CallError::Timeout)),
        format!("expected Timeout, got {result:?}"),
    )?;
    client.close().await;
    Ok(())
}

async fn test_server_exit_fails_pending_and_later_calls() -> Result<(), String> {
    let client = connect_default("exit").await.map_err(|e| e.to_string())?;
    let cancel = CancellationToken::new();

    let first = client.call("die", json!({}), &cancel).await;
    let message = match first {
        Err(CallError::Transport(message)) => message,
        other => return Err(format!("expected Transport error on exit, got {other:?}")),
    };
    check(
        message.contains("exited"),
        format!("error should mention exit: {message}"),
    )?;
    check(
        message.contains('7'),
        format!("error should mention exit status 7: {message}"),
    )?;
    check(
        message.contains("fake-server-die-marker"),
        format!("error should include the child's stderr tail: {message}"),
    )?;

    let second = client.call("die", json!({}), &cancel).await;
    check(
        matches!(second, Err(CallError::Transport(_))),
        format!("expected later call to also fail, got {second:?}"),
    )?;

    client.close().await;
    Ok(())
}

async fn test_close_terminates_child_and_is_idempotent() -> Result<(), String> {
    let (command, env) = fake_server_command("modern", &[]);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let transport = StdioTransport::spawn(&command, &[], &env, &cwd)
        .await
        .map_err(|e| e.to_string())?;
    let pid = transport.pid();

    transport.close().await;
    let gone = matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    );
    check(gone, format!("pid {pid} still alive after close"))?;

    transport.close().await; // idempotent: must not panic or hang
    Ok(())
}

async fn test_close_escalates_stubborn_child_promptly() -> Result<(), String> {
    let (command, env) = fake_server_command("stubborn", &[]);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let transport = StdioTransport::spawn(&command, &[], &env, &cwd)
        .await
        .map_err(|e| e.to_string())?;
    let pid = transport.pid();

    let started = Instant::now();
    transport.close().await;
    let elapsed = started.elapsed();

    check(
        elapsed < Duration::from_secs(1),
        format!("close took too long for stubborn child: {elapsed:?}"),
    )?;
    let mut gone = false;
    for _ in 0..50 {
        if matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
            Err(nix::errno::Errno::ESRCH)
        ) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check(gone, format!("pid {pid} still alive after close"))?;
    Ok(())
}

async fn test_env_restriction() -> Result<(), String> {
    let client = connect_client(
        "modern",
        &[(
            "KITE_MCP_ENV_MARKER".to_string(),
            "surprise-value".to_string(),
        )],
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .map_err(|e| e.to_string())?;
    let cancel = CancellationToken::new();
    let outcome = client
        .call("env", json!({}), &cancel)
        .await
        .map_err(|e| e.to_string())?;
    let text = match outcome.content.as_slice() {
        [ContentBlock::Text { text }] => text.clone(),
        other => return Err(format!("unexpected env content: {other:?}")),
    };
    let expected = "KITE_MCP_ENV_MARKER=surprise-value\nKITE_MCP_FAKE_SERVER=modern";
    check(
        text == expected,
        format!("child saw unexpected env, got {text:?}"),
    )?;
    client.close().await;
    Ok(())
}

// ---------------------------------------------------------------------
// Fake server
// ---------------------------------------------------------------------

fn run_fake_server() {
    let mode = std::env::var("KITE_MCP_FAKE_SERVER").expect("mode already checked present");
    if mode == "stubborn" {
        ignore_sigterm_for_stubborn_test();
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
    let cancel_marker = std::env::var("KITE_MCP_CANCEL_MARKER").ok();
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
                    "exit" => &["die"],
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
