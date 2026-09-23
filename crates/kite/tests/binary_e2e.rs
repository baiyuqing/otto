//! End-to-end coverage of the built `kite` binary.
//!
//! One `--prompt` run against a loopback OpenAI-compatible server, with
//! Seatbelt enabled, that makes the model issue a `bash` call and a `write`
//! call. The assertions are on what the binary printed and on what it left in
//! the workspace, so every layer between the flag parser and the sandbox is
//! exercised in one process.
//!
//! macOS only: both runs are about the Seatbelt path — one turn under the
//! sandbox, and one command elevated out of it. Kite has no confined driver
//! on other targets, where `bash` is disabled unless `--sandbox off` is asked
//! for explicitly, so there is nothing here for them to run. An end-to-end
//! run of that `off` path would be a separate test.
#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{io::Write, process::Stdio};

mod common;
use common::{Script, serve, text_reply, tool_call_reply};

/// Reads the single session file the run left under `$HOME/.kite/sessions`.
fn only_session_transcript(home: &std::path::Path) -> String {
    let workspaces = std::fs::read_dir(home.join(".kite/sessions")).expect("session root");
    let mut transcripts = Vec::new();
    for workspace in workspaces {
        let workspace = workspace.expect("workspace directory");
        for session in std::fs::read_dir(workspace.path()).expect("workspace sessions") {
            let session = session.expect("session file");
            transcripts.push(std::fs::read_to_string(session.path()).expect("read session"));
        }
    }
    assert_eq!(transcripts.len(), 1, "one session per run");
    transcripts.remove(0)
}

fn configure(home: &std::path::Path, base_url: &str) {
    std::fs::create_dir_all(home.join("Library/Caches")).expect("cache base");
    let config_dir = home.join(".config/kite");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"{base_url}\"\nmodel = \"gpt-test\"\napi_key_env = \"KITE_API_KEY\"\n"
        ),
    )
    .expect("write config");
}

#[test]
fn the_binary_runs_one_prompt_turn_with_bash_and_write_under_seatbelt() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("README.md"), "# fixture\n").expect("seed README");

    let served = Arc::new(AtomicUsize::new(0));
    let base_url = serve(Script {
        replies: vec![
            tool_call_reply("call-1", "bash", r#"{"command":"echo hello-from-bash"}"#),
            tool_call_reply(
                "call-2",
                "write",
                r#"{"path":"notes.txt","content":"written by the agent\n"}"#,
            ),
            text_reply("all done"),
        ],
        served: Arc::clone(&served),
    });

    configure(home.path(), &base_url);

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kite"));
    command
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("KITE_API_KEY", "sk-e2e-not-a-real-key")
        .arg("--cwd")
        .arg(workspace.path())
        .arg("--sandbox")
        .arg("seatbelt")
        .arg("--prompt")
        .arg("run the two tool calls");
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    let output = command.output().expect("run the kite binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit = {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        !stderr.contains("bash is unavailable"),
        "Seatbelt did not come up:\n{stderr}"
    );
    assert!(
        stdout.contains("[tool] bash (call-1)"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("[tool] write (call-2)"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("[tool result] wrote notes.txt (21 bytes)"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("all done"), "stdout:\n{stdout}");

    // The REPL renders only a tool result's first line, and bash's begins with
    // the "stdout:" header, so the sandboxed command's own output is asserted
    // from the session transcript instead.
    let transcript = only_session_transcript(home.path());
    assert!(
        transcript.contains("hello-from-bash"),
        "transcript:\n{transcript}"
    );

    let written = std::fs::read_to_string(workspace.path().join("notes.txt"))
        .expect("the write tool created notes.txt");
    assert_eq!(written, "written by the agent\n");
    assert_eq!(served.load(Ordering::SeqCst), 3, "stdout:\n{stdout}");
}

#[test]
fn an_interactive_approval_runs_one_exact_command_outside_seatbelt() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(home.path().join("elevated.txt"), "outside-seatbelt\n").expect("home fixture");

    let arguments = serde_json::json!({
        "command": "cat \"$HOME/elevated.txt\"",
        "sandbox_permissions": "require_escalated",
        "justification": "read the reviewed home fixture",
    })
    .to_string();
    let served = Arc::new(AtomicUsize::new(0));
    let base_url = serve(Script {
        replies: vec![
            tool_call_reply("call-1", "bash", &arguments),
            text_reply("approval needed"),
            tool_call_reply("call-2", "bash", &arguments),
            text_reply("done"),
        ],
        served: Arc::clone(&served),
    });
    configure(home.path(), &base_url);

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kite"));
    child
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("KITE_API_KEY", "sk-e2e-not-a-real-key")
        .arg("--cwd")
        .arg(workspace.path())
        .arg("--sandbox")
        .arg("seatbelt")
        .arg("--ui")
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        child.env("TMPDIR", tmpdir);
    }
    let mut child = child.spawn().expect("run kite");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"read the fixture\n/approve approval-1\n/exit\n")
        .expect("write prompts");
    let output = child.wait_with_output().expect("wait for kite");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit = {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("Only the user can approve it, by typing /approve approval-1 in Kite"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("done"), "stdout:\n{stdout}");
    assert!(
        only_session_transcript(home.path()).contains("outside-seatbelt"),
        "stdout:\n{stdout}"
    );
    assert_eq!(served.load(Ordering::SeqCst), 4, "stdout:\n{stdout}");
}
