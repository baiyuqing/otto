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
use std::time::{Duration, Instant};

mod common;
use common::{Script, serve, text_reply};

const WIDTH: usize = 120;
const HEIGHT: usize = 30;
const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// A minimal terminal screen, built only for the ANSI vocabulary ratatui's
/// crossterm backend can emit (see the module doc). Fragmentation-tolerant:
/// `feed` buffers an incomplete escape sequence or a multi-byte UTF-8
/// character split across two reads.
struct Screen {
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    cells: Vec<Vec<char>>,
    cursor_visible: bool,
    pending: Vec<u8>,
}

impl Screen {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            x: 0,
            y: 0,
            cells: vec![vec![' '; width]; height],
            cursor_visible: true,
            pending: Vec::new(),
        }
    }

    /// Consumes newly-read bytes. Fails closed: any control sequence outside
    /// the vocabulary in the module doc is an `Err`, not a silent skip.
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.pending.extend_from_slice(bytes);
        loop {
            let Some(&first) = self.pending.first() else {
                return Ok(());
            };
            if first == 0x1b {
                if self.pending.len() < 2 {
                    return Ok(()); // wait for the rest of the escape sequence
                }
                if self.pending[1] != b'[' {
                    return Err(format!("unsupported escape 0x{:02x}", self.pending[1]));
                }
                let mut end = 2;
                while end < self.pending.len()
                    && matches!(self.pending[end], b'0'..=b'9' | b';' | b'?')
                {
                    end += 1;
                }
                let Some(&final_byte) = self.pending.get(end) else {
                    return Ok(()); // wait for the final byte
                };
                let params = self.pending[2..end].to_vec();
                self.apply_csi(&params, final_byte)?;
                self.pending.drain(..=end);
                continue;
            }
            if first == b'\r' || first == b'\n' {
                if first == b'\r' {
                    self.x = 0;
                } else {
                    self.y = self.y.saturating_add(1).min(self.height.saturating_sub(1));
                }
                self.pending.remove(0);
                continue;
            }
            if first < 0x20 || first == 0x7f {
                return Err(format!("unsupported control byte 0x{first:02x}"));
            }
            let mut end = 0;
            while end < self.pending.len()
                && self.pending[end] != 0x1b
                && self.pending[end] >= 0x20
                && self.pending[end] != 0x7f
            {
                end += 1;
            }
            match std::str::from_utf8(&self.pending[..end]) {
                Ok(text) => {
                    let text = text.to_string();
                    self.write_text(&text);
                    self.pending.drain(..end);
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        let text = std::str::from_utf8(&self.pending[..valid])
                            .expect("checked")
                            .to_string();
                        self.write_text(&text);
                        self.pending.drain(..valid);
                    } else if error.error_len().is_none() {
                        return Ok(()); // incomplete multi-byte char; wait for more
                    } else {
                        return Err("invalid utf-8 in pty output".to_string());
                    }
                }
            }
        }
    }

    fn apply_csi(&mut self, params: &[u8], final_byte: u8) -> Result<(), String> {
        let params_str =
            std::str::from_utf8(params).map_err(|_| "non-ascii CSI params".to_string())?;
        let (private, digits) = match params_str.strip_prefix('?') {
            Some(rest) => (true, rest),
            None => (false, params_str),
        };
        if digits.contains('?') {
            return Err(format!("unsupported CSI params: {params_str}"));
        }
        let numbers: Vec<i64> = if digits.is_empty() {
            Vec::new()
        } else {
            digits
                .split(';')
                .map(|part| part.parse().unwrap_or(0))
                .collect()
        };
        match (private, final_byte) {
            (false, b'H') | (false, b'f') => {
                let row = numbers.first().copied().unwrap_or(1).max(1) as usize;
                let col = numbers.get(1).copied().unwrap_or(1).max(1) as usize;
                self.y = (row - 1).min(self.height.saturating_sub(1));
                self.x = (col - 1).min(self.width.saturating_sub(1));
                Ok(())
            }
            (true, b'h') if numbers == [25] => {
                self.cursor_visible = true;
                Ok(())
            }
            (true, b'l') if numbers == [25] => {
                self.cursor_visible = false;
                Ok(())
            }
            // Input modes are content-inert; raw output assertions verify
            // that the TUI enables and restores them.
            (true, b'h') | (true, b'l')
                if matches!(
                    numbers.as_slice(),
                    [1000] | [1002] | [1003] | [1006] | [1015] | [2004]
                ) =>
            {
                Ok(())
            }
            (true, b'h') if numbers == [1049] => {
                for row in &mut self.cells {
                    row.fill(' ');
                }
                self.x = 0;
                self.y = 0;
                Ok(())
            }
            // The raw byte log, not the grid, asserts the restored main screen.
            (true, b'l') if numbers == [1049] => Ok(()),
            // SGR: content-inert (never changes which character occupies a cell).
            (false, b'm') => Ok(()),
            _ => Err(format!(
                "unsupported CSI: {}{}{}",
                if private { "?" } else { "" },
                params_str,
                final_byte as char
            )),
        }
    }

    fn write_text(&mut self, text: &str) {
        for ch in text.chars() {
            if self.x >= self.width {
                self.x = 0;
                self.y += 1;
            }
            if self.y >= self.height {
                // ponytail: no scrollback model; overflow past the bottom
                // row is dropped instead of shifting rows up. Upgrade path:
                // implement scroll-up if a test needs content that would
                // scroll off a fixed-size screen.
                break;
            }
            self.cells[self.y][self.x] = ch;
            self.x += 1;
        }
    }

    fn contains(&self, needle: &str) -> bool {
        self.cells
            .iter()
            .any(|row| row.iter().collect::<String>().contains(needle))
    }

    fn dump(&self) -> String {
        self.cells
            .iter()
            .map(|row| row.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// State shared between the test body and the background reader thread.
struct Shared {
    screen: Mutex<Screen>,
    raw: Mutex<Vec<u8>>,
    parse_error: Mutex<Option<String>>,
}

fn wait_until(shared: &Shared, what: &str, mut predicate: impl FnMut(&Shared) -> bool) {
    let start = Instant::now();
    loop {
        if let Some(message) = shared.parse_error.lock().unwrap().clone() {
            panic!("pty screen parse error while waiting for {what}: {message}");
        }
        if predicate(shared) {
            return;
        }
        if start.elapsed() > WAIT_TIMEOUT {
            panic!(
                "timed out after {WAIT_TIMEOUT:?} waiting for {what}\nscreen:\n{}",
                shared.screen.lock().unwrap().dump()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_screen_text(shared: &Shared, needle: &str) {
    wait_until(shared, &format!("screen text {needle:?}"), |shared| {
        shared.screen.lock().unwrap().contains(needle)
    });
}

/// Waits for `needle` to stop appearing on screen. Used to detect the
/// composer leaving its "Working (Esc to cancel)" busy title: while a turn
/// runs, `App::handle_key` drops every key except Ctrl+C/Ctrl+O (see
/// `app.rs`), so typing ahead (e.g. `/exit`) during that window is silently
/// swallowed rather than queued.
fn wait_for_screen_text_gone(shared: &Shared, needle: &str) {
    wait_until(
        shared,
        &format!("screen text {needle:?} to clear"),
        |shared| !shared.screen.lock().unwrap().contains(needle),
    );
}

fn raw_contains(shared: &Shared, needle: &[u8]) -> bool {
    let raw = shared.raw.lock().unwrap();
    raw.windows(needle.len()).any(|window| window == needle)
}

fn wait_for_raw_bytes(shared: &Shared, needle: &[u8]) {
    wait_until(
        shared,
        &format!("raw bytes {:?}", String::from_utf8_lossy(needle)),
        |shared| raw_contains(shared, needle),
    );
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

    let served = Arc::new(AtomicUsize::new(0));
    const PROMPT: &str = "send the scripted prompt";
    const REPLY: &str = "reply visible over the pty smoke test";
    let base_url = serve(Script {
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
    // every event), while the composer still shows "Working (Esc to
    // cancel)" and drops keys other than Ctrl+C/Ctrl+O. Typing `/exit`
    // before that title clears would be silently swallowed.
    wait_for_screen_text_gone(&shared, "Working (Esc to cancel)");
    eprintln!("[tui_pty] turn finished");
    // The composer was cleared on Enter, so the only thing that can still
    // put the prompt on screen is the transcript entry the submission
    // echoed into it.
    let screen = shared.screen.lock().unwrap().dump();
    assert!(
        screen.contains(&format!("> {PROMPT}")),
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
