//! End-to-end coverage of an uncatchable kill (`SIGKILL`) through the built
//! `otto` binary, beside `tests/sigterm.rs`: the process runs no handler, so
//! the session file holds an unanswered tool call that the next open repairs.

#![cfg(unix)]
#![allow(unsafe_code)]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

mod common;
use common::{Script, serve, text_reply, tool_call_reply};

use otto::session::Store;
use otto_core::model::{Block, BlockType};

fn configure(home: &Path, base_url: &str) {
    std::fs::create_dir_all(home.join("Library/Caches")).expect("cache base");
    let config_dir = home.join(".config/otto");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let text = format!(
        "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"{base_url}\"\nmodel = \"gpt-test\"\napi_key_env = \"OTTO_API_KEY\"\n"
    );
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

/// The one session file at `root/<workspace key>/<id>.jsonl`.
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

#[test]
fn sigkill_during_a_tool_call_leaves_a_session_that_reopens_with_the_call_marked_unknown_and_resumes()
 {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let (base_url, _requests) = serve(Script {
        replies: vec![tool_call_reply(
            "call-1",
            "bash",
            r#"{"command":"sleep 30"}"#,
        )],
        served: Arc::new(AtomicUsize::new(0)),
    });
    configure(home.path(), &base_url);

    let sessions_root = home.path().join(".otto/sessions");
    let mut child = spawn_otto(home.path(), workspace.path(), &["--prompt", "go"]);
    let session_path = wait_for(
        || find_session_file(&sessions_root),
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
    let killed = unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    assert_eq!(killed, 0, "send SIGKILL");
    let status = wait_exit(&mut child, Duration::from_secs(10));
    assert_eq!(status.signal(), Some(libc::SIGKILL), "status: {status:?}");

    let (reopened, warnings) = Store::open(&session_path).expect("reopen the killed session");
    let warnings: Vec<_> = warnings.iter().map(|w| w.message.as_str()).collect();
    assert_eq!(warnings, ["repaired dangling tool call call-1"]);
    let messages = reopened.messages();
    let last = messages.last().expect("messages");
    assert_eq!(
        last.blocks[0].block_type,
        BlockType::ToolResult,
        "the dangling tool call is answered when the session is reopened"
    );
    assert_eq!(last.blocks[0].tool_call_id, "call-1");
    // A bash call has a durable operation record, so the repair text names
    // that record rather than saying the call may have run.
    assert!(
        last.blocks[0]
            .text
            .starts_with("tool result unavailable after prior session interruption"),
        "{}",
        last.blocks[0].text
    );
    drop(reopened);

    let (resume_url, requests) = serve(Script {
        replies: vec![text_reply("resumed")],
        served: Arc::new(AtomicUsize::new(0)),
    });
    configure(home.path(), &resume_url);
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
    let status = wait_exit(&mut resumed, Duration::from_secs(10));
    assert!(status.success(), "resume exit status: {status:?}");
    let requests = requests.lock().unwrap();
    let first = requests.first().expect("one request");
    assert!(
        first.contains("call-1"),
        "the resumed request carries the repaired tool result: {first}"
    );
}
