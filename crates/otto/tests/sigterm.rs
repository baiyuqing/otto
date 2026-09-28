//! End-to-end coverage of `SIGTERM` migration through the built `otto`
//! binary: docs/specs/2026-09-28-session-failover.md, "A planned migration
//! is `SIGTERM` and cancels running work".
//!
//! `libc::kill` is the only way to send this process's own spawned child a
//! real `SIGTERM`; no safe wrapper is already a workspace dependency.

#![cfg(unix)]
#![allow(unsafe_code)]

use std::collections::BTreeSet;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

mod common;
use common::{Script, serve, text_reply, tool_call_reply, two_tool_call_reply};

use otto::session::Store;
use otto_core::model::{Block, BlockType};

fn configure(home: &Path, base_url: &str, failover: bool) {
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

fn spawn_otto(home: &Path, workspace: &Path, args: &[&str]) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otto"));
    command
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("OTTO_API_KEY", "sk-e2e-not-a-real-key")
        .arg("--cwd")
        .arg(workspace)
        .arg("--sandbox")
        .arg("off")
        .args(args)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    command.spawn().expect("spawn the otto binary")
}

/// Spawns `otto serve`. Unlike `spawn_otto`, `serve` must be `argv[0]`
/// (`cli::flags::parse_flags` only recognizes the subcommand there), so this
/// builds its own argument list instead of reusing `spawn_otto`'s fixed
/// `--cwd`/`--sandbox` prefix.
fn spawn_otto_serve(home: &Path, workspace: &Path, socket: &Path) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otto"));
    command
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("OTTO_API_KEY", "sk-e2e-not-a-real-key")
        .arg("serve")
        .arg("--socket")
        .arg(socket)
        .arg("--cwd")
        .arg(workspace)
        .arg("--sandbox")
        .arg("off")
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    command.spawn().expect("spawn the otto binary")
}

/// Polls `probe` until it returns `Some`, sleeping 30 ms between attempts.
/// Panics if `timeout` elapses first.
fn wait_for<T>(mut probe: impl FnMut() -> Option<T>, timeout: Duration, what: &str) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Finds the one session file at `root/<workspace key>/<id>.jsonl`. It looks
/// only at that depth: child transcripts live one level deeper, in
/// `<id>/<task_id>-<session id>.jsonl`, and directory order is unspecified.
fn find_session_file(root: &Path) -> Option<PathBuf> {
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

/// Finds the one child transcript for `task_id` inside `children_dir`. The
/// real wiring (`cli::wiring::child_sessions`) names it
/// `<task_id>-<random session id>.jsonl`, not a fixed `<task_id>-child.jsonl`
/// suffix (that fixed name is only a unit-test fixture convention elsewhere
/// in this crate).
fn find_child_transcript(children_dir: &Path, task_id: &str) -> Option<PathBuf> {
    let prefix = format!("{task_id}-");
    let entries = std::fs::read_dir(children_dir).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.starts_with(&prefix))
        })
}

fn has_tool_call(path: &Path, name: &str) -> bool {
    let Ok(messages) = Store::read_transcript(path) else {
        return false;
    };
    messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block: &Block| block.block_type == BlockType::ToolCall && block.tool_name == name)
    })
}

/// True once every `toolCall` block in the transcript has a matching
/// `toolResult` block with the same `tool_call_id`, and at least one exists.
fn all_tool_calls_answered(path: &Path) -> bool {
    let Ok(messages) = Store::read_transcript(path) else {
        return false;
    };
    let mut calls = BTreeSet::new();
    let mut results = BTreeSet::new();
    for message in &messages {
        for block in &message.blocks {
            match block.block_type {
                BlockType::ToolCall => {
                    calls.insert(block.tool_call_id.clone());
                }
                BlockType::ToolResult => {
                    results.insert(block.tool_call_id.clone());
                }
                _ => {}
            }
        }
    }
    !calls.is_empty() && calls == results
}

fn wait_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            return status;
        }
        assert!(
            start.elapsed() < timeout,
            "the process did not exit in time"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// S9(a): a lease-managed session receiving one `SIGTERM` while a bash tool
/// call and a sub-agent's own bash tool call are both running migrates
/// instead of dying, records both as interrupted, and a later `--resume`
/// picks the session straight up instead of waiting out the takeover delay.
#[test]
fn sigterm_migrates_a_lease_managed_session_and_a_later_resume_takes_over_immediately() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let served = Arc::new(AtomicUsize::new(0));
    let (base_url, _requests) = serve(Script {
        replies: vec![
            two_tool_call_reply(
                "call-agent",
                "agent",
                r#"{"prompt":"child go"}"#,
                "call-bash-parent",
                "bash",
                r#"{"command":"sleep 30"}"#,
            ),
            tool_call_reply("call-bash-child", "bash", r#"{"command":"sleep 30"}"#),
        ],
        served: Arc::clone(&served),
    });
    configure(home.path(), &base_url, true);

    let sessions_root = home.path().join(".otto/sessions");
    let mut child = spawn_otto(home.path(), workspace.path(), &["--prompt", "go"]);

    let session_path = wait_for(
        || find_session_file(&sessions_root).filter(|_| sessions_root.exists()),
        Duration::from_secs(10),
        "the session file to appear",
    );
    let children_dir = session_path.with_extension("");

    wait_for(
        || has_tool_call(&session_path, "bash").then_some(()),
        Duration::from_secs(10),
        "the parent's bash toolCall to be recorded",
    );
    let child_transcript = wait_for(
        || find_child_transcript(&children_dir, "t1").filter(|path| has_tool_call(path, "bash")),
        Duration::from_secs(10),
        "the child's bash toolCall to be recorded",
    );
    std::thread::sleep(Duration::from_millis(300));

    // SAFETY: `child.id()` is this test's own spawned `otto` subprocess, not
    // the test process itself.
    let killed = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(killed, 0, "send SIGTERM");

    let status = wait_exit(&mut child, Duration::from_secs(15));
    assert!(status.success(), "exit status: {status:?}");

    assert!(
        all_tool_calls_answered(&session_path),
        "every parent toolCall must have a toolResult"
    );
    assert!(
        all_tool_calls_answered(&child_transcript),
        "every child toolCall must have a toolResult"
    );

    let records = otto::subagent::interrupted::scan(&children_dir);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].task_id, "t1");
    assert_eq!(
        records[0].final_status.as_deref(),
        Some(otto::subagent::interrupted::INTERRUPTED_STATUS)
    );

    let inbox_path = session_path.with_extension("inbox.json");
    let inbox = std::fs::read_to_string(&inbox_path).expect("read inbox");
    assert!(inbox.contains("This session was moved"), "{inbox}");
    assert!(inbox.contains("t1"), "{inbox}");

    let heartbeat_path = session_path.with_extension("lease").join("heartbeat");
    let heartbeat = std::fs::read_to_string(&heartbeat_path).expect("read heartbeat");
    assert!(heartbeat.contains("\"released\":true"), "{heartbeat}");

    // A resumed run on the same, now-released lease takes the session
    // straight up: a takeover would instead wait 7/6 of `lease_seconds`
    // (14 s with the fixture's 12 s) without a heartbeat change before
    // taking the session, well past this test's 5 s bound.
    let served2 = Arc::new(AtomicUsize::new(0));
    let (base_url2, requests2) = serve(Script {
        replies: vec![text_reply("resumed")],
        served: served2,
    });
    configure(home.path(), &base_url2, true);
    let mut resumed = spawn_otto(
        home.path(),
        workspace.path(),
        &[
            "--resume",
            session_path.to_str().expect("utf8 path"),
            "--prompt",
            "next",
        ],
    );
    let status = wait_exit(&mut resumed, Duration::from_secs(5));
    assert!(status.success(), "resume exit status: {status:?}");

    let lease_dir = session_path.with_extension("lease");
    assert!(lease_dir.join("epoch-2").exists(), "epoch-2 must exist");
    assert!(
        !lease_dir.join("fenced-1.jsonl").exists(),
        "a plain resume of a released lease must not fence epoch 1"
    );

    let requests2 = requests2.lock().unwrap();
    let first_request = requests2.first().expect("one request");
    let moved_at = first_request
        .find("This session was moved")
        .expect("moved text in the resumed request");
    // The literal message content, not a bare "next": the system prompt's
    // own tool descriptions contain "next" as a plain word (e.g. "your next
    // step"), well before the moved notification.
    let next_at = first_request
        .find("\"content\":\"next\"")
        .expect("the new prompt in the resumed request");
    assert!(
        moved_at < next_at,
        "the moved notification must precede the new prompt"
    );
}

/// S9(c): with no `[failover]`, a `SIGTERM` has no lease to migrate and no
/// running `otto serve` to cancel cleanly, so `Terminate::on_signal` decides
/// `Die` and the process is killed by the signal.
#[test]
fn sigterm_without_a_lease_kills_the_process() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let served = Arc::new(AtomicUsize::new(0));
    let (base_url, _requests) = serve(Script {
        replies: vec![tool_call_reply(
            "call-1",
            "bash",
            r#"{"command":"sleep 30"}"#,
        )],
        served: Arc::clone(&served),
    });
    configure(home.path(), &base_url, false);

    let sessions_root = home.path().join(".otto/sessions");
    let mut child = spawn_otto(home.path(), workspace.path(), &["--prompt", "go"]);

    let session_path = wait_for(
        || find_session_file(&sessions_root).filter(|_| sessions_root.exists()),
        Duration::from_secs(10),
        "the session file to appear",
    );
    wait_for(
        || has_tool_call(&session_path, "bash").then_some(()),
        Duration::from_secs(10),
        "the bash toolCall to be recorded",
    );
    std::thread::sleep(Duration::from_millis(300));

    // SAFETY: `child.id()` is this test's own spawned `otto` subprocess.
    let killed = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(killed, 0, "send SIGTERM");

    let status = wait_exit(&mut child, Duration::from_secs(15));
    assert_eq!(status.signal(), Some(libc::SIGTERM), "status: {status:?}");
}

/// SIGTERM handling for a `serve` process moved from `serve.rs`'s
/// `spawn_terminate` to `main.rs` plus `Terminate::set_serve`/
/// `Action::Cancel`. With no lease held, `Terminate::on_signal` must still
/// resolve to `Action::Cancel`, not `Action::Die`: an `otto serve` process
/// exits 0 on `SIGTERM` instead of being killed by the signal.
#[test]
fn sigterm_without_a_lease_shuts_serve_down_cleanly() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    configure(home.path(), "http://127.0.0.1:1", false);
    // A not-yet-existing subdirectory: `ensure_socket_directory` creates and
    // chmod's it to 0700 itself. The tempdir root cannot be reused directly
    // here, since `tempfile::tempdir()` creates it at mode 0755 in this
    // environment, which `ensure_socket_directory` rejects as
    // group-/world-accessible.
    let socket = home.path().join("run").join("otto.sock");

    let mut child = spawn_otto_serve(home.path(), workspace.path(), &socket);

    wait_for(
        || socket.exists().then_some(()),
        Duration::from_secs(10),
        "the socket file to appear",
    );

    // SAFETY: `child.id()` is this test's own spawned `otto` subprocess.
    let killed = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(killed, 0, "send SIGTERM");

    let status = wait_exit(&mut child, Duration::from_secs(10));
    assert_eq!(
        status.code(),
        Some(0),
        "status: {status:?}, signal: {:?}",
        status.signal()
    );
}
