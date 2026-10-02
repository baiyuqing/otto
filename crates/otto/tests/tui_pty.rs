//! PTY smoke test for the terminal frontend (`--ui tui`). The test opens a
//! real PTY and spawns the built `otto` binary against the same scripted
//! loopback server the other binary tests use (see
//! `crates/otto/tests/common`), because there is no way to hand an external
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

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod common;
use common::pty::{
    Screen, Shared, raw_contains, wait_for_raw_bytes, wait_for_screen_text,
    wait_for_screen_text_gone,
};
use common::{Script, serve, text_reply};

const WIDTH: usize = 120;
const HEIGHT: usize = 30;
/// The text both busy composer titles end with (`draw_composer` in
/// `src/tui/render.rs`): "Working — type, then Enter to queue next input ·
/// Esc cancels turn" and "Queued next input · Ctrl+U withdraw · Esc cancels
/// turn".
const BUSY_TITLE: &str = "Esc cancels turn";

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

    let served = Arc::new(AtomicUsize::new(0));
    const PROMPT: &str = "send the scripted prompt";
    const REPLY: &str = "reply visible over the pty smoke test";
    let (base_url, _requests) = serve(Script {
        replies: vec![text_reply(REPLY)],
        served: Arc::clone(&served),
    });

    // Seatbelt's private state directory must pre-exist even though this
    // smoke test issues no tool calls (see binary_e2e.rs for the same
    // requirement on the sandboxed-turn test).
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

    let winsize = nix::pty::Winsize {
        ws_row: HEIGHT as u16,
        ws_col: WIDTH as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&winsize), None).expect("open a pty");
    let mut master = File::from(pty.master);
    let slave = File::from(pty.slave);
    let stdin_fd = slave.try_clone().expect("clone slave for stdin");
    let stdout_fd = slave.try_clone().expect("clone slave for stdout");
    let stderr_fd = slave; // moved in; no clone needed for the last use

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
        .arg("tui")
        .stdin(Stdio::from(stdin_fd))
        .stdout(Stdio::from(stdout_fd))
        .stderr(Stdio::from(stderr_fd));
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    }
    let mut child = command.spawn().expect("spawn the otto binary");
    // The parent must hold no slave-side descriptor once the child owns its
    // copies, or the master will never see EOF after the child exits; the
    // `Stdio::from` handles above are consumed into `command` and closed by
    // `spawn()` on the parent side, so nothing further to drop here.

    let shared = Arc::new(Shared {
        screen: Mutex::new(Screen::new(WIDTH, HEIGHT)),
        raw: Mutex::new(Vec::new()),
        parse_error: Mutex::new(None),
    });
    let reader_shared = Arc::clone(&shared);
    let mut reader_master = master
        .try_clone()
        .expect("clone master for the reader thread");
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader_master.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    let chunk = &buffer[..read];
                    reader_shared.raw.lock().unwrap().extend_from_slice(chunk);
                    let mut screen = reader_shared.screen.lock().unwrap();
                    if let Err(message) = screen.feed(chunk) {
                        *reader_shared.parse_error.lock().unwrap() = Some(message);
                        return;
                    }
                }
            }
        }
    });

    eprintln!("[tui_pty] waiting for startup marker {STARTUP_MARKER:?}");
    wait_for_screen_text(&shared, STARTUP_MARKER);
    // Mouse reporting is on from the first frame: the alternate screen has no
    // scrollback, so a wheel notch the terminal keeps to itself does nothing.
    // `?1002` comes with it, because taking the terminal's drag away means
    // Otto has to run the text selection itself.
    wait_for_raw_bytes(&shared, b"\x1b[?1000h");
    wait_for_raw_bytes(&shared, b"\x1b[?1002h");
    wait_for_raw_bytes(&shared, b"\x1b[?2004h");
    assert!(
        !raw_contains(&shared, b"\x1b[?1003h"),
        "any-motion reporting would wake the event loop on every pointer move"
    );
    eprintln!("[tui_pty] saw startup marker; typing prompt");

    master
        .write_all(format!("{PROMPT}\r").as_bytes())
        .expect("type the prompt");
    eprintln!("[tui_pty] waiting for reply {REPLY:?}");
    wait_for_screen_text(&shared, REPLY);
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
        served.load(Ordering::SeqCst) >= 1,
        "the loopback server was never called"
    );

    master.write_all(b"/exit\r").expect("type /exit");
    eprintln!("[tui_pty] typed /exit; waiting for child to exit");
    let status = child.wait().expect("wait for the child to exit");
    eprintln!("[tui_pty] child exited: {status:?}");
    assert!(status.success(), "otto --ui tui exited with {status:?}");

    // A clean terminal restore leaves the alternate screen and disables the
    // mouse reporting enabled at startup.
    wait_for_raw_bytes(&shared, b"\x1b[?1049l");
    wait_for_raw_bytes(&shared, b"\x1b[?1002l");
    wait_for_raw_bytes(&shared, b"\x1b[?1000l");
    wait_for_raw_bytes(&shared, b"\x1b[?2004l");
    eprintln!("[tui_pty] saw alt-screen exit sequence");
}
