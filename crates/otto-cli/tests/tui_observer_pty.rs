//! Read-only observer navigation through the installed binary's terminal entry.

use std::io::Write as _;
use std::process::{Child, Command};

mod binary_harness;
use binary_harness::pty::{spawn, wait_for_screen_text, wait_for_screen_text_gone, wait_until};

struct TerminalChild(Child);

impl Drop for TerminalChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn observer_navigation_preserves_the_draft_and_never_accepts_input() {
    observer_round_trip("", b"\x1b[17~", b"\x1b");
}

#[test]
fn observer_uses_configured_bindings_from_the_selected_config() {
    observer_round_trip(
        "\n[ui.observer]\ntoggle = \"F8\"\nopen = \"Ctrl+Left\"\nback = \"Ctrl+Right\"\n",
        b"\x1b[1;5D",
        b"\x1b[1;5C",
    );
}

fn observer_round_trip(bindings: &str, open_key: &[u8], back_key: &[u8]) {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(home.path().join("Library/Caches")).expect("cache base");
    let config_dir = home.path().join(".config/otto");
    std::fs::create_dir_all(&config_dir).expect("configuration directory");
    // No request is submitted. A loopback-only unreachable endpoint makes an
    // accidental observer submission fail rather than reaching a live provider.
    let config_path = home.path().join("selected-observer.toml");
    std::fs::write(
        &config_path,
        format!(
            "{}{bindings}",
            binary_harness::model_config("http://127.0.0.1:1", "OTTO_OBSERVER_TEST_KEY")
        ),
    )
    .expect("write test configuration");
    let mut command = Command::new(env!("CARGO_BIN_EXE_otto"));
    command
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("OTTO_OBSERVER_TEST_KEY", "observer-test-placeholder")
        .arg("--config")
        .arg(&config_path)
        .arg("--cwd")
        .arg(workspace.path())
        .args(["--ui", "tui"]);
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    let (mut master, child, shared) = spawn(command);
    let mut child = TerminalChild(child);
    wait_for_screen_text(&shared, "└");

    const DRAFT: &str = "preserve-observer-draft";
    master.write_all(DRAFT.as_bytes()).expect("type draft");
    wait_for_screen_text(&shared, DRAFT);
    master.write_all(open_key).expect("open observer");
    wait_for_screen_text(&shared, "Sub-agents");
    wait_for_screen_text(&shared, "No sub-agents");
    wait_for_screen_text_gone(&shared, DRAFT);
    master
        .write_all(b"observer-must-ignore-this\r\x1b[200~pasted-readonly\x1b[201~")
        .expect("attempt input on read-only page");
    master.write_all(back_key).expect("return to main");
    wait_for_screen_text(&shared, DRAFT);
    wait_for_screen_text_gone(&shared, "No sub-agents");
    {
        let screen = shared.screen.lock().expect("screen").dump();
        assert!(!screen.contains("observer-must-ignore-this"), "{screen}");
        assert!(!screen.contains("pasted-readonly"), "{screen}");
    }

    let toggle_key: &[u8] = if bindings.is_empty() {
        b"\x1b[17~"
    } else {
        b"\x1b[19~"
    };
    master.write_all(toggle_key).expect("toggle opens observer");
    wait_for_screen_text(&shared, "Sub-agents");
    master
        .write_all(toggle_key)
        .expect("toggle returns to main");
    wait_for_screen_text(&shared, DRAFT);
    master.write_all(b"\x15/exit\r").expect("exit normally");
    wait_until(&shared, "normal observer test process exit", |_| {
        child.0.try_wait().expect("child status").is_some()
    });
    assert!(child.0.wait().expect("exit status").success());
}
