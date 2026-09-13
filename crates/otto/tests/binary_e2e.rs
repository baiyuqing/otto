//! End-to-end coverage of the built `otto` binary.
//!
//! One `--approve` run against a loopback OpenAI-compatible server, with
//! Seatbelt enabled, that makes the model issue a `bash` call and a `write`
//! call. The assertions are on what the binary printed and on what it left in
//! the workspace, so every layer between the flag parser and the sandbox is
//! exercised in one process.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::{Script, serve, text_reply, tool_call_reply};

/// Reads the single session file the run left under `$HOME/.otto/sessions`.
fn only_session_transcript(home: &std::path::Path) -> String {
    let workspaces = std::fs::read_dir(home.join(".otto/sessions")).expect("session root");
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

#[test]
fn the_binary_runs_one_approved_turn_with_bash_and_write_under_seatbelt() {
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

    // Seatbelt puts its private state under `$HOME/Library/Caches`, and both
    // Go's `createState` and the Rust port require that directory to exist
    // already. A real macOS home always has it; a temporary one does not.
    std::fs::create_dir_all(home.path().join("Library/Caches")).expect("cache base");

    let config_dir = home.path().join(".config/otto");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"{base_url}\"\nmodel = \"gpt-test\"\napi_key_env = \"OTTO_API_KEY\"\n"
        ),
    )
    .expect("write config");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_otto"));
    command
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("OTTO_API_KEY", "sk-e2e-not-a-real-key")
        .arg("--cwd")
        .arg(workspace.path())
        .arg("--sandbox")
        .arg("seatbelt")
        .arg("--approve")
        .arg("run the two tool calls");
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    let output = command.output().expect("run the otto binary");

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
