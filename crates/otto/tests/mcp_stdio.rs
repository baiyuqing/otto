//! Integration test and fake server for the stdio MCP transport and client.
//!
//! `harness = false`: this binary is both the test runner and, when
//! `OTTO_MCP_FAKE_SERVER` is set, the fake server itself. Tests spawn
//! `std::env::current_exe()` (this same binary) as the child process with
//! that variable set to a mode name, so no external script or interpreter
//! is needed.
//!
//! Modes: `modern` (paged tool list, all call-shape tools), `legacy`
//! (rejects `server/discover` with `-32601`), `unsupported` (rejects it
//! with `-32020` and no modern version in `supported`), `garbage` (mixes
//! invalid lines and stderr output into a working `modern`-shaped session),
//! `exit` (exits when its one tool, `die`, is called),
//! `exit-with-descendant` (the leader exits while a stubborn process-group
//! descendant retains its pipes), `stubborn` (ignores EOF and SIGTERM until SIGKILL).

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use otto::faults::mcp::{FaultyTransport, McpOp};
use otto::faults::{Fault, FaultPlan, Filter, Rule, Trigger};
use otto::mcp::client::Client;
use otto::mcp::jsonrpc::modern_meta;
use otto::mcp::stdio::StdioTransport;
use otto::mcp::{
    CallError, ContentBlock, Era, LEGACY_VERSION, MODERN_VERSION, Outbound, ToolServer, Transport,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn main() {
    if std::env::var("OTTO_MCP_FAKE_SERVER").is_ok() {
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
    run!(test_natural_leader_exit_kills_stubborn_process_group);
    run!(test_close_terminates_child_and_is_idempotent);
    run!(test_close_escalates_stubborn_child_promptly);
    run!(test_env_restriction);
    run!(test_hung_stdio_request_times_out_and_stops_the_server);

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

async fn connect_client_with_grace(
    mode: &str,
    extra_env: &[(String, String)],
    connect_timeout: Duration,
    call_timeout: Duration,
    cancellation_grace: Duration,
) -> Result<Client, CallError> {
    let (command, env) = fake_server_command(mode, extra_env);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let transport =
        StdioTransport::spawn_with_grace(&command, &[], &env, &cwd, cancellation_grace).await?;
    let cancel = CancellationToken::new();
    Client::connect_with_grace(
        "test-server".to_string(),
        Box::new(transport),
        connect_timeout,
        call_timeout,
        cancellation_grace,
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
    let mut env = vec![("OTTO_MCP_FAKE_SERVER".to_string(), mode.to_string())];
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

/// Waits for the fake server to record a `notifications/cancelled` it
/// received. Polls, because the notification is written after the call the
/// test is watching has already returned.
async fn wait_for_cancel_marker(path: &str) -> bool {
    for _ in 0..100 {
        if let Ok(contents) = std::fs::read_to_string(path)
            && contents.contains("cancelled")
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn test_cancelled_call_returns_promptly() -> Result<(), String> {
    let marker = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    let marker_path = marker
        .path()
        .to_str()
        .expect("utf8 marker path")
        .to_string();
    let grace = Duration::from_millis(100);
    let client = connect_client_with_grace(
        "modern",
        &[("OTTO_MCP_CANCEL_MARKER".to_string(), marker_path.clone())],
        Duration::from_secs(5),
        Duration::from_secs(30),
        grace,
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
        elapsed >= grace,
        format!("stdio cancel returned before grace elapsed: {elapsed:?}"),
    )?;
    check(
        elapsed < Duration::from_secs(1),
        format!("cancel took too long: {elapsed:?}"),
    )?;
    check(
        matches!(
            client
                .call(
                    "echo",
                    json!({"message": "later"}),
                    &CancellationToken::new()
                )
                .await,
            Err(CallError::Stopping)
        ),
        "stdio client accepted a call after cancellation cleanup",
    )?;

    check(
        wait_for_cancel_marker(&marker_path).await,
        "server did not record notifications/cancelled",
    )?;

    client.close().await;
    Ok(())
}

/// A call the server never answers times out, and the server is told the
/// request is cancelled rather than left working on an answer no one reads.
async fn test_call_timeout() -> Result<(), String> {
    let marker = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    let marker_path = marker
        .path()
        .to_str()
        .expect("utf8 marker path")
        .to_string();
    let client = connect_client(
        "modern",
        &[("OTTO_MCP_CANCEL_MARKER".to_string(), marker_path.clone())],
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
    check(
        !cancel.is_cancelled(),
        "the call timeout must not cancel the caller's own token",
    )?;
    check(
        wait_for_cancel_marker(&marker_path).await,
        "server did not record notifications/cancelled for the timed-out call",
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

async fn test_natural_leader_exit_kills_stubborn_process_group() -> Result<(), String> {
    let marker = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    let marker_path = marker
        .path()
        .to_str()
        .expect("utf8 marker path")
        .to_string();
    let grace = Duration::from_millis(100);
    let client = connect_client_with_grace(
        "exit-with-descendant",
        &[(
            "OTTO_MCP_DESCENDANT_MARKER".to_string(),
            marker_path.clone(),
        )],
        Duration::from_secs(5),
        Duration::from_secs(5),
        grace,
    )
    .await
    .map_err(|e| e.to_string())?;

    let result = client
        .call("die", json!({}), &CancellationToken::new())
        .await;
    check(
        matches!(result, Err(CallError::Transport(_))),
        format!("natural leader exit should be Transport, got {result:?}"),
    )?;
    let descendant_pid: i32 = std::fs::read_to_string(&marker_path)
        .map_err(|e| e.to_string())?
        .trim()
        .parse()
        .map_err(|e| format!("invalid descendant pid: {e}"))?;
    let descendant_deadline = Instant::now() + Duration::from_secs(1);
    while !matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(descendant_pid), None),
        Err(nix::errno::Errno::ESRCH)
    ) && Instant::now() < descendant_deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check(
        matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(descendant_pid), None),
            Err(nix::errno::Errno::ESRCH)
        ),
        format!("descendant pid {descendant_pid} survived natural leader cleanup"),
    )?;
    tokio::time::timeout(Duration::from_secs(1), client.close())
        .await
        .map_err(|_| "close hung after natural-exit cleanup".to_string())?;
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
    let grace = Duration::from_millis(100);
    let transport = StdioTransport::spawn_with_grace(&command, &[], &env, &cwd, grace)
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

/// A transport that never answers `tools/call` and ignores cancellation: the
/// call times out after the grace, and stdio stops the shared server.
async fn test_hung_stdio_request_times_out_and_stops_the_server() -> Result<(), String> {
    let grace = Duration::from_millis(100);
    let (command, env) = fake_server_command("modern", &[]);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let transport = StdioTransport::spawn_with_grace(&command, &[], &env, &cwd, grace)
        .await
        .map_err(|e| e.to_string())?;
    let pid = transport.pid();
    let plan = FaultPlan::new(vec![
        Rule::new(McpOp::Request, Trigger::Nth(1), Fault::Hang)
            .filter(Filter::NameSuffix("tools/call")),
    ]);
    let client = Client::connect_with_grace(
        "test-server".to_string(),
        Box::new(FaultyTransport::new(Box::new(transport), plan.clone())),
        Duration::from_secs(5),
        Duration::from_millis(150),
        grace,
        &CancellationToken::new(),
    )
    .await
    .map_err(|e| e.to_string())?;

    let started = Instant::now();
    let first = client
        .call("echo", json!({}), &CancellationToken::new())
        .await;
    let elapsed = started.elapsed();
    plan.assert_all_fired();
    check(
        matches!(first, Err(CallError::Timeout)),
        format!("expected Timeout, got {first:?}"),
    )?;
    check(
        elapsed < Duration::from_secs(2),
        format!("the hung call took {elapsed:?}"),
    )?;
    println!("hung call returned after {elapsed:?}");
    let second = client
        .call("echo", json!({}), &CancellationToken::new())
        .await;
    check(
        matches!(second, Err(CallError::Stopping)),
        format!("expected Stopping, got {second:?}"),
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
    check(gone, format!("pid {pid} still alive after the timeout"))?;
    Ok(())
}

async fn test_env_restriction() -> Result<(), String> {
    let client = connect_client(
        "modern",
        &[(
            "OTTO_MCP_ENV_MARKER".to_string(),
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
    let expected = "OTTO_MCP_ENV_MARKER=surprise-value\nOTTO_MCP_FAKE_SERVER=modern";
    check(
        text == expected,
        format!("child saw unexpected env, got {text:?}"),
    )?;
    client.close().await;
    Ok(())
}

#[path = "mcp_stdio/fake_server.rs"]
mod fake_server;
use fake_server::run_fake_server;
