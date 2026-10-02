//! PTY test for `otto --attach`: the TUI as a client of a running
//! `otto serve`. docs/specs/2026-10-02-shared-session.md.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

mod common;
use common::pty::{raw_contains, spawn, wait_for_raw_bytes, wait_for_screen_text, wait_until};
use common::{Script, serve, text_reply};

fn otto(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otto"));
    command
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("OTTO_API_KEY", "sk-e2e-not-a-real-key");
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    command
}

const DISCONNECTED: &str = "disconnected from otto serve";

fn healthy(socket: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = "GET /healthz HTTP/1.1\r\nHost: otto\r\nConnection: close\r\n\r\n";
    let mut reply = String::new();
    stream.write_all(request.as_bytes()).is_ok()
        && stream.read_to_string(&mut reply).is_ok()
        && reply.starts_with("HTTP/1.1 200")
}

struct Serve(Child);

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_serve(home: &Path, workspace: &Path, socket: &Path) -> Serve {
    let server = Serve(
        otto(home)
            .args(["serve", "--cwd"])
            .arg(workspace)
            .args(["--sandbox", "off", "--socket"])
            .arg(socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn otto serve"),
    );
    let start = Instant::now();
    while !healthy(socket) {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "otto serve did not start"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    server
}

/// One HTTP/1.1 request over the socket; returns the body once serve closes
/// the connection.
fn http(socket: &Path, method: &str, path: &str, body: &str) -> String {
    let mut stream = UnixStream::connect(socket).expect("connect to serve");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: otto\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read the reply");
    reply
        .split_once("\r\n\r\n")
        .map_or("", |(_, body)| body)
        .to_string()
}

#[test]
fn attach_renders_a_served_turn_rejects_local_commands_and_restores_the_terminal() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let socket_dir = tempfile::tempdir().expect("socket dir");
    std::fs::set_permissions(socket_dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("chmod socket dir");
    let socket = socket_dir.path().join("o.sock");

    const PROMPT: &str = "send the attached prompt";
    const REPLY: &str = "reply visible over the attach pty";
    const OTHER_PROMPT: &str = "prompt from another client";
    const OTHER_REPLY: &str = "reply to the other client";
    let served = Arc::new(AtomicUsize::new(0));
    let (base_url, _requests) = serve(Script {
        replies: vec![text_reply(REPLY), text_reply(OTHER_REPLY)],
        served: Arc::clone(&served),
    });
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

    let mut server = start_serve(home.path(), workspace.path(), &socket);

    let mut attach = otto(home.path());
    attach
        .args(["--attach", "--socket"])
        .arg(&socket)
        .arg("--cwd")
        .arg(workspace.path());
    let (mut master, mut child, shared) = spawn(attach);

    wait_for_screen_text(&shared, "└");
    wait_for_raw_bytes(&shared, b"\x1b[?1049h");

    // No local echo: the `user_message` frame from serve puts the prompt on
    // screen, followed by the streamed reply.
    master
        .write_all(format!("{PROMPT}\r").as_bytes())
        .expect("type the prompt");
    wait_for_screen_text(&shared, &format!("❯ {PROMPT}"));
    wait_for_screen_text(&shared, REPLY);

    master.write_all(b"/model\r").expect("type /model");
    wait_for_screen_text(&shared, "/model: not available with --attach");

    // A turn another client starts over the socket appears with its prompt
    // and its reply.
    let listed = http(
        &socket,
        "GET",
        &format!(
            "/v1/sessions?workspace={}",
            workspace.path().canonicalize().unwrap().display()
        ),
        "",
    );
    let listed: serde_json::Value = serde_json::from_str(&listed).expect("session list");
    let session = listed["sessions"][0]["id"]
        .as_str()
        .expect("session id")
        .to_string();
    http(
        &socket,
        "POST",
        &format!("/v1/sessions/{session}/turns"),
        &format!("{{\"text\":\"{OTHER_PROMPT}\",\"queue\":true}}"),
    );
    wait_for_screen_text(&shared, &format!("❯ {OTHER_PROMPT}"));
    wait_for_screen_text(&shared, OTHER_REPLY);

    // With serve gone the prompt is refused and the loop keeps retrying; the
    // session is resumed when serve returns on the same socket.
    drop(server);
    wait_for_screen_text(&shared, DISCONNECTED);
    master
        .write_all(b"refused prompt\r")
        .expect("type a prompt");
    wait_until(&shared, "the prompt to be refused", |shared| {
        shared
            .screen
            .lock()
            .unwrap()
            .dump()
            .matches(DISCONNECTED)
            .count()
            >= 2
    });
    server = start_serve(home.path(), workspace.path(), &socket);
    wait_for_screen_text(&shared, "reconnected to otto serve");

    master.write_all(b"/exit\r").expect("type /exit");
    let status = child.wait().expect("wait for otto --attach");
    assert!(status.success(), "otto --attach exited with {status:?}");
    wait_for_raw_bytes(&shared, b"\x1b[?1049l");
    wait_for_raw_bytes(&shared, b"\x1b[?1002l");
    assert!(raw_contains(&shared, b"\x1b[?2004l"));
    drop(server);
}
