//! End-to-end coverage of the built `otto` binary with `--sandbox off`.
//!
//! The counterpart to `binary_e2e`, which covers the Seatbelt path on macOS.
//! This one runs on every target, because `off` is the one execution mode
//! every target has — and on a target with no confined driver it is the only
//! way to run `bash` at all, so this is the run that proves the binary works
//! there end to end.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::{Script, serve, text_reply, tool_call_reply};

fn configure(home: &std::path::Path, base_url: &str) {
    std::fs::create_dir_all(home.join("Library/Caches")).expect("cache base");
    let config_dir = home.join(".config/otto");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "default_profile = \"test\"\n\n[profiles.test]\nprovider = \"openai-compatible\"\nbase_url = \"{base_url}\"\nmodel = \"gpt-test\"\napi_key_env = \"OTTO_API_KEY\"\n"
        ),
    )
    .expect("write config");
}

/// One `otto --prompt` run in `home`/`workspace` with `sandbox`, answered by a
/// scripted provider. Returns (stdout, stderr, success).
fn run_prompt(
    home: &std::path::Path,
    workspace: &std::path::Path,
    sandbox: &str,
    replies: Vec<String>,
    served: Arc<AtomicUsize>,
) -> (String, String, bool) {
    let base_url = serve(Script { replies, served });
    configure(home, &base_url);

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_otto"));
    command
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("OTTO_API_KEY", "sk-e2e-not-a-real-key")
        .arg("--cwd")
        .arg(workspace)
        .arg("--sandbox")
        .arg(sandbox)
        .arg("--prompt")
        .arg("run the tool calls");
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    let output = command.output().expect("run the otto binary");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

#[test]
fn the_binary_runs_a_turn_with_bash_and_write_when_the_sandbox_is_off() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let served = Arc::new(AtomicUsize::new(0));

    let (stdout, stderr, success) = run_prompt(
        home.path(),
        workspace.path(),
        "off",
        vec![
            tool_call_reply("call-1", "bash", r#"{"command":"echo hello-unconfined"}"#),
            tool_call_reply(
                "call-2",
                "write",
                r#"{"path":"notes.txt","content":"written by the agent\n"}"#,
            ),
            text_reply("all done"),
        ],
        Arc::clone(&served),
    );

    assert!(success, "stdout:\n{stdout}\nstderr:\n{stderr}");
    // The mode is unsafe, so it says so on every run.
    assert!(
        stderr.contains("warning: sandbox is off; bash runs unsandboxed"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("bash is unavailable"),
        "bash must exist with the sandbox off:\n{stderr}"
    );
    assert!(
        stdout.contains("[tool] bash (call-1)"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("[tool result] wrote notes.txt (21 bytes)"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("all done"), "stdout:\n{stdout}");

    let written = std::fs::read_to_string(workspace.path().join("notes.txt"))
        .expect("the write tool created notes.txt");
    assert_eq!(written, "written by the agent\n");
    assert_eq!(served.load(Ordering::SeqCst), 3, "stdout:\n{stdout}");
}

/// The fail-closed rule on a target with no confined driver: `auto` leaves no
/// `bash` tool at all, says why, and the run still completes with the file
/// tools. Only the explicit `off` above opens command execution.
#[test]
#[cfg(not(target_os = "macos"))]
fn without_a_confined_driver_bash_is_unavailable_but_file_tools_still_run() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let served = Arc::new(AtomicUsize::new(0));

    let (stdout, stderr, success) = run_prompt(
        home.path(),
        workspace.path(),
        "auto",
        vec![
            tool_call_reply(
                "call-1",
                "write",
                r#"{"path":"notes.txt","content":"written by the agent\n"}"#,
            ),
            text_reply("all done"),
        ],
        Arc::clone(&served),
    );

    assert!(success, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stderr.contains("bash is unavailable because the configured sandbox could not be established (reason: unsupported-platform)"),
        "stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("[tool result] wrote notes.txt (21 bytes)"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.txt")).expect("notes.txt"),
        "written by the agent\n"
    );
    assert_eq!(served.load(Ordering::SeqCst), 2, "stdout:\n{stdout}");
}
