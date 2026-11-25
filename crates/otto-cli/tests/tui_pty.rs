//! PTY smoke test for the terminal frontend (`--ui tui`). The test opens a
//! real PTY and spawns the built `otto` binary against the same scripted
//! loopback server the other binary tests use (see
//! `crates/otto-cli/tests/binary_harness`), because there is no way to hand an external
//! integration test a fake `otto::app::Controller`.
//!
//! ## Why the screen scraper is small
//!
//! ratatui's crossterm backend, on an alternate screen, emits no scroll
//! regions (`CSI r`), insert/delete line/character (`CSI L`/`M`/`@`/`P`),
//! character repeat (`CSI b`), reverse index (`ESC M`), or OSC sequences: it
//! redraws the whole frame every tick with only cursor moves, SGR, and raw
//! text. This was confirmed by reading the exact pinned dependency sources
//! (`ratatui-crossterm-0.1.2`, `ratatui-core-0.1.2`, `crossterm-0.29.0`)
//! rather than assumed, and no `vt100`/`vte`/terminal-emulator crate is in
//! `Cargo.lock`. [`Screen`] below implements exactly the vocabulary that
//! output can contain: `CSI r;cH`/`f` (cursor position), `CSI ?25h`/`l`
//! (cursor visibility, no grid effect), `CSI ?1000`/`1002`/`1003`/`1015`/`1006h`/`l`
//! (mouse capture, no grid effect), `CSI ?2004h`/`l` (bracketed paste, no grid
//! effect), `CSI ?1049h`/`l` (alt screen; enter resets the grid), `CSI ...m`
//! (SGR, content-inert), CR/LF, and raw UTF-8 text. Any other control sequence
//! is a bug (either in this scraper's assumptions or in the TUI emitting
//! something unexpected) and fails the test loudly rather than silently
//! mis-rendering.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

mod binary_harness;
use binary_harness::pty::{
    Screen, raw_contains, wait_for_raw_bytes, wait_for_screen_text, wait_for_screen_text_gone,
    wait_until,
};
use otto::fixtures::scripted_provider;

/// The text both busy composer titles end with (`draw_composer` in
/// `src/tui/render.rs`): "Working — type, then Enter to queue next input ·
/// Esc cancels turn" and "Queued next input · Ctrl+U withdraw · Esc cancels
/// turn".
const BUSY_TITLE: &str = "Esc cancels turn";

/// Returns the usual complete reply first, then holds the second provider
/// stream after its first text delta until the test releases it.
fn serve_with_held_second_turn(
    first_reply: &str,
    held_text: &str,
    released_text: &str,
) -> (String, Arc<AtomicUsize>, mpsc::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider");
    let base_url = format!("http://{}", listener.local_addr().expect("address"));
    let served = Arc::new(AtomicUsize::new(0));
    let received = Arc::clone(&served);
    let first_reply = scripted_provider::text_reply(first_reply);
    let prefix = format!(
        "data: {}\n\n",
        serde_json::json!({"choices": [{"delta": {"content": held_text}}]})
    );
    let tail = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices": [{"delta": {"content": released_text}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
    );
    let (release, gate) = mpsc::channel();

    thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(mut stream) = incoming else { continue };
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let header_end = loop {
                if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break index + 4;
                }
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break 0,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            };
            if header_end == 0 {
                continue;
            }
            let head = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = request[header_end..].to_vec();
            while body.len() < length {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => body.extend_from_slice(&buffer[..read]),
                }
            }
            match received.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{first_reply}",
                        first_reply.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                1 => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                    );
                    let _ = stream.write_all(prefix.as_bytes());
                    let _ = stream.flush();
                    let _ = gate.recv();
                    let _ = stream.write_all(tail.as_bytes());
                }
                _ => {
                    let fallback =
                        scripted_provider::text_reply("unexpected extra provider request");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{fallback}",
                        fallback.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
            }
        }
    });
    (base_url, served, release)
}

#[test]
fn screen_handles_carriage_return_and_line_feed() {
    let mut screen = Screen::new(4, 2);

    screen.feed(b"abc\rX\nY").expect("valid terminal text");
    assert_eq!(screen.dump(), "Xbc \n Y  ");

    screen.feed(b"\x1b[?1049hnew").expect("alternate screen");
    assert_eq!(screen.dump(), "new \n    ");
}

#[test]
fn the_tui_renders_a_prompt_reply_and_restores_the_terminal_on_exit() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::Builder::new()
        .prefix("otto-pty-workspace-")
        .tempdir()
        .expect("workspace");
    // The composer border is the first stable visible marker that a full TUI
    // frame reached the PTY. The footer is allowed to omit or truncate fields,
    // so don't synchronize on workspace text here.
    const STARTUP_MARKER: &str = "└";

    const PROMPT: &str = "send the scripted prompt";
    const REPLY: &str = "reply visible over the pty smoke test";
    const SECOND_PROMPT: &str = "keep the live turn running";
    const HELD_TEXT: &str = "stream held";
    const RELEASED_SUFFIX: &str = " then released";
    const RELEASED_TEXT: &str = "stream held then released";
    let (base_url, server_count, release_provider) =
        serve_with_held_second_turn(REPLY, HELD_TEXT, RELEASED_SUFFIX);

    // Seatbelt's private state directory must pre-exist even though this
    // smoke test issues no tool calls (see binary_e2e.rs for the same
    // requirement on the sandboxed-turn test).
    std::fs::create_dir_all(home.path().join("Library/Caches")).expect("cache base");
    let config_dir = home.path().join(".config/otto");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        binary_harness::model_config(&base_url, "OTTO_API_KEY"),
    )
    .expect("write config");

    let mut command = Command::new(env!("CARGO_BIN_EXE_otto"));
    command
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("SHELL", "/bin/sh")
        .env("OTTO_API_KEY", "sk-e2e-not-a-real-key")
        .arg("--cwd")
        .arg(workspace.path())
        .arg("--ui")
        .arg("tui");
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    binary_harness::rex_title::configure(&mut command, home.path());
    let (mut master, mut child, shared) = binary_harness::job_control::spawn(command);

    eprintln!("[tui_pty] waiting for startup marker {STARTUP_MARKER:?}");
    wait_for_screen_text(&shared, STARTUP_MARKER);
    // Mouse reporting is on from the first frame: the alternate screen has no
    // scrollback, so a wheel notch the terminal keeps to itself does nothing.
    // `?1002` comes with it, because taking the terminal's drag away means
    // Otto has to run the text selection itself.
    wait_for_raw_bytes(&shared, b"\x1b[?1000h");
    wait_for_raw_bytes(&shared, b"\x1b[?1002h");
    wait_for_raw_bytes(&shared, b"\x1b[?2004h");
    wait_for_raw_bytes(&shared, b"\x1b]7501;state=idle:app=otto\x1b\\");
    assert!(
        !raw_contains(&shared, b"\x1b[?1003h"),
        "any-motion reporting would wake the event loop on every pointer move"
    );
    binary_harness::job_control::suspend_resume(&mut master, &child, &shared, "local-draft");
    binary_harness::rex_title::assert_transitions(&shared, &["Otto", "previous title", "Otto"]);
    eprintln!("[tui_pty] saw startup marker; typing prompt");

    master
        .write_all(format!("{PROMPT}\r").as_bytes())
        .expect("type the prompt");
    eprintln!("[tui_pty] waiting for reply {REPLY:?}");
    wait_for_screen_text(&shared, REPLY);
    wait_for_raw_bytes(&shared, b"\x1b]7501;state=working:app=otto\x1b\\");
    wait_for_raw_bytes(&shared, b"\x1b]7501;state=done:app=otto\x1b\\");
    eprintln!("[tui_pty] saw reply; waiting for the turn to finish (busy title to clear)");
    // The reply text lands on screen mid-turn (the streaming sink redraws on
    // every event), while the composer still shows a busy title. Enter
    // during a turn queues the line (`App::handle_busy_composer_key`) and
    // runs it after the turn, so `/exit` typed now would take the queued
    // path instead of the idle one this test covers. The raw-bytes wait
    // fails if the busy title was never drawn, e.g. after its text changes,
    // instead of letting the screen wait below return at once.
    wait_for_raw_bytes(&shared, BUSY_TITLE.as_bytes());
    wait_for_screen_text_gone(&shared, BUSY_TITLE);
    eprintln!("[tui_pty] turn finished");
    // The composer was cleared on Enter, so the only thing that can still
    // put the prompt on screen is the transcript entry the submission
    // echoed into it.
    let screen = shared.screen.lock().unwrap().dump();
    assert!(
        screen.contains(&format!("❯ {PROMPT}")),
        "the submitted prompt is missing from the transcript:\n{screen}"
    );
    assert!(
        server_count.load(Ordering::SeqCst) >= 1,
        "the loopback server was never called"
    );

    assert_local_input_identity(home.path(), PROMPT);
    master
        .write_all(b"/inputs\r")
        .expect("list local input recovery");
    wait_for_screen_text(&shared, "Inputs for session");
    wait_for_screen_text(&shared, "Tui / Direct / delivered");

    master.write_all(b"/rename\r").expect("open rename form");
    wait_for_screen_text(&shared, "Rename session");
    master
        .write_all("中文会话".as_bytes())
        .expect("type the original Unicode name");
    master
        .write_all(b"\x1b[D\x7f")
        .expect("move left and delete one Unicode character");
    master
        .write_all("对\r".as_bytes())
        .expect("finish the Unicode name and submit");
    wait_for_screen_text(&shared, "Session name: 中文对话");

    master
        .write_all(b"/session\r")
        .expect("open session details");
    wait_for_screen_text(&shared, "Workspace:");
    wait_for_screen_text(&shared, "中文对话");
    master.write_all(b"\x1b").expect("return to the composer");
    wait_for_screen_text_gone(&shared, "Workspace:");

    master
        .write_all(format!("{SECOND_PROMPT}\r").as_bytes())
        .expect("start the held turn");
    wait_for_screen_text(&shared, HELD_TEXT);
    binary_harness::job_control::suspend_resume(&mut master, &child, &shared, "live-draft");
    wait_for_raw_bytes(&shared, BUSY_TITLE.as_bytes());
    master
        .write_all(b"/context\r")
        .expect("open context while busy");
    wait_for_screen_text(&shared, "Context");
    master
        .write_all(b"\x1b")
        .expect("return from context without canceling the turn");
    wait_for_screen_text_gone(&shared, "Context");
    wait_until(&shared, "the busy composer to remain visible", |shared| {
        shared.screen.lock().unwrap().contains(BUSY_TITLE)
    });
    assert_eq!(
        server_count.load(Ordering::SeqCst),
        2,
        "closing context must not cancel or restart the live turn"
    );

    // A centered feature panel never covers the composer. Closing it therefore
    // need not redraw an unchanged busy title; assert the current screen state
    // above instead of requiring a duplicate title in the raw terminal bytes.
    release_provider
        .send(())
        .expect("release the provider after returning to the composer");
    wait_for_screen_text(&shared, RELEASED_TEXT);
    wait_for_screen_text_gone(&shared, BUSY_TITLE);
    assert_eq!(
        server_count.load(Ordering::SeqCst),
        2,
        "the live turn should reach the provider exactly once"
    );

    master.write_all(b"/exit\r").expect("type /exit");
    eprintln!("[tui_pty] typed /exit; waiting for child to exit");
    let status = child.wait().expect("wait for the child to exit");
    eprintln!("[tui_pty] child exited: {status:?}");
    assert!(status.success(), "otto --ui tui exited with {status:?}");
    binary_harness::rex_title::assert_transitions(
        &shared,
        &[
            "Otto",
            "previous title",
            "Otto",
            "shell title",
            "Otto",
            "shell title",
        ],
    );

    // A clean terminal restore leaves the alternate screen and disables the
    // mouse reporting enabled at startup.
    wait_for_raw_bytes(&shared, b"\x1b[?1049l");
    wait_for_raw_bytes(&shared, b"\x1b[?1002l");
    {
        let raw = shared.raw.lock().unwrap();
        let prefix = b"\x1b]7501;";
        let start = raw
            .windows(prefix.len())
            .rposition(|bytes| bytes == prefix)
            .expect("program status record");
        assert!(
            raw[start..].starts_with(b"\x1b]7501;state=done:app=otto\x1b\\"),
            "completion must survive terminal restoration"
        );
    }
    wait_for_raw_bytes(&shared, b"\x1b[?1000l");
    wait_for_raw_bytes(&shared, b"\x1b[?2004l");
    eprintln!("[tui_pty] saw alt-screen exit sequence");
}

/// A child spawned on the harness PTY must not inherit the master descriptor.
/// If it did, the master would stay open after the test process exits, the
/// session would never get SIGHUP, and the shell and Otto would keep running.
#[test]
fn a_pty_child_does_not_inherit_the_master_descriptor() {
    use std::os::fd::AsRawFd as _;
    let mut probe = Command::new("/bin/sh");
    probe.args([
        "-c",
        "read master_fd; if { true >&\"$master_fd\"; } 2>/dev/null; then echo MASTER-INHERITED; else echo MASTER-CLOSED; fi",
    ]);
    let (mut master, mut child, shared) = binary_harness::pty::spawn(probe);
    // The child sees inherited descriptors under the parent's numbers.
    master
        .write_all(format!("{}\n", master.as_raw_fd()).as_bytes())
        .expect("send the master descriptor number");
    wait_until(&shared, "descriptor probe result", |shared| {
        raw_contains(shared, b"MASTER-CLOSED") || raw_contains(shared, b"MASTER-INHERITED")
    });
    let _ = child.wait();
    assert!(
        raw_contains(&shared, b"MASTER-CLOSED"),
        "the child inherited the PTY master descriptor"
    );
}

/// Inspect only the test-owned home, never a developer's real session tree.
fn assert_local_input_identity(home: &std::path::Path, prompt: &str) {
    fn collect(directory: &std::path::Path, records: &mut Vec<serde_json::Value>) {
        for entry in std::fs::read_dir(directory).expect("test session directory") {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, records);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                for line in std::fs::read_to_string(path).unwrap().lines() {
                    if let Ok(record) = serde_json::from_str(line) {
                        records.push(record);
                    }
                }
            }
        }
    }
    let mut records = Vec::new();
    collect(home, &mut records);
    let intent = records
        .iter()
        .find(|record| {
            record.get("customType").and_then(|value| value.as_str())
                == Some("otto.input_submissions")
                && record["data"]["text"] == prompt
        })
        .expect("PTY Enter persisted recoverable intent");
    let input_id = intent["data"]["id"].as_str().expect("stable input ID");
    assert_eq!(intent["data"]["provenance"]["source"], "tui");
    assert_eq!(intent["data"]["provenance"]["delivery"], "direct");
    assert!(
        records
            .iter()
            .any(|record| record["message"]["details"]["otto"]["inputId"] == input_id),
        "PTY Enter must deliver the same stable input ID"
    );
}
