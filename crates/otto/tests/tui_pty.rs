//! PTY smoke test for the terminal frontend (`--ui tui`). Port of the intent
//! of `cmd/otto/tui_pty_test.go` (Go opens a real PTY and drives
//! `internal/tui` end to end); the two are structurally different because
//! Go's tests call `tui.Run` in-process against a fake backend, while this
//! test spawns the built `otto` binary against the same scripted loopback
//! server the other binary tests use (see `crates/otto/tests/common`), since
//! there is no way to hand an external integration test a fake
//! `otto::app::Controller`.
//!
//! ## Why the screen scraper here is much smaller than Go's
//!
//! `cmd/otto/pty_terminal_screen_test.go` (664 lines) interprets Bubble
//! Tea's inline renderer, which relies on scroll regions (`CSI r`),
//! insert/delete line/character (`CSI L`/`M`/`@`/`P`), character repeat
//! (`CSI b`), reverse index (`ESC M`), and OSC sequences. Otto emits none of
//! those: `tui::inline::LiveRegion` drives a `Viewport::Fixed` through
//! ratatui's crossterm backend, which writes only cursor moves, erases, SGR,
//! line feeds, and raw text. This was confirmed by reading the exact pinned
//! dependency sources (`ratatui-crossterm-0.1.2`, `ratatui-core-0.1.2`,
//! `crossterm-0.29.0`) rather than assumed, and no `vt100`/`vte`/
//! terminal-emulator crate is in `Cargo.lock`. [`Screen`] below implements
//! exactly the vocabulary that output can contain: `CSI r;cH`/`f` (cursor
//! position), `CSI J`/`1J`/`2J` (erase in display), `CSI K`/`1K`/`2K` (erase
//! in line), `CSI 6n` (the single cursor-position query `LiveRegion::new`
//! makes before the key reader starts; the reply travels the other way),
//! `CSI ?25h`/`l` (cursor visibility, no grid effect), `CSI ...m` (SGR,
//! content-inert), CR, LF (scrolls at the bottom row), and raw UTF-8 text.
//! Any other control sequence is a bug (either in this scraper's assumptions
//! or in the TUI emitting something unexpected) and fails the test loudly
//! rather than silently mis-rendering. `CSI ?1049h` (alternate screen) and
//! `CSI ?1000h` (mouse capture) are deliberately absent from that list: the
//! transcript lives in the terminal's own scrollback so that the wheel and
//! drag-selection keep working natively.

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
/// character split across two reads, mirroring Go's `Write`/`consume`
/// pattern in `ptyTerminalScreen`.
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
                    self.index();
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
            // Erase in display. `LiveRegion` clears from the cursor down
            // (`CSI J`) before every commit and whenever the live region is
            // resized; ratatui's `Terminal::resize` clears the whole screen
            // (`CSI 2J`) when the terminal gets narrower.
            (false, b'J') => {
                let column = self.x.min(self.width);
                match numbers.first().copied().unwrap_or(0) {
                    0 => {
                        self.cells[self.y][column..].fill(' ');
                        for row in &mut self.cells[self.y + 1..] {
                            row.fill(' ');
                        }
                    }
                    1 => {
                        self.cells[self.y][..=column.min(self.width - 1)].fill(' ');
                        for row in &mut self.cells[..self.y] {
                            row.fill(' ');
                        }
                    }
                    2 => {
                        for row in &mut self.cells {
                            row.fill(' ');
                        }
                    }
                    mode => return Err(format!("unsupported erase in display: {mode}")),
                }
                Ok(())
            }
            (false, b'K') => {
                let column = self.x.min(self.width);
                match numbers.first().copied().unwrap_or(0) {
                    0 => self.cells[self.y][column..].fill(' '),
                    1 => self.cells[self.y][..=column.min(self.width - 1)].fill(' '),
                    2 => self.cells[self.y].fill(' '),
                    mode => return Err(format!("unsupported erase in line: {mode}")),
                }
                Ok(())
            }
            // Cursor-position report request. `LiveRegion::new` makes exactly
            // one, before the key-reader thread exists; the reply travels
            // from the terminal to Otto and never reaches this stream.
            (false, b'n') => Ok(()),
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

    /// Line feed. On the bottom row the terminal scrolls instead of moving
    /// the cursor, which is how `LiveRegion` pushes committed rows into
    /// scrollback (`Backend::append_lines` writes plain `\n`).
    ///
    /// ponytail: rows that scroll off the top are dropped, not kept in a
    /// scrollback buffer. Upgrade path: keep them in a `Vec<Vec<char>>` if a
    /// test needs to assert on text the terminal has scrolled away.
    fn index(&mut self) {
        if self.y + 1 < self.height {
            self.y += 1;
        } else {
            self.cells.remove(0);
            self.cells.push(vec![' '; self.width]);
        }
    }

    fn write_text(&mut self, text: &str) {
        for ch in text.chars() {
            if self.x >= self.width {
                self.x = 0;
                self.index();
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

/// Waits for `needle` to stop appearing on screen. Used twice: to detect the
/// composer leaving its "Working (Esc to cancel)" busy title, because while a
/// turn runs Ctrl+C cancels the turn instead of arming the exit prompt (see
/// `App::is_interrupt_key`); and to detect the composer border going away
/// when `LiveRegion::finish` erases the live region at exit.
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

#[test]
fn screen_handles_carriage_return_line_feed_and_erase() {
    let mut screen = Screen::new(4, 2);

    // Raw mode: a line feed moves down without returning to column 0.
    screen.feed(b"abc\rX\nY").expect("valid terminal text");
    assert_eq!(screen.dump(), "Xbc \n Y  ");

    // On the bottom row the same line feed scrolls instead.
    screen.feed(b"\nZ").expect("scroll at the bottom row");
    assert_eq!(screen.dump(), " Y  \n  Z ");

    // Erase from the cursor down is how the live region is cleared.
    screen.feed(b"\x1b[1;1H\x1b[J").expect("erase in display");
    assert_eq!(screen.dump(), "    \n    ");
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
    let mut reply_master = master.try_clone().expect("clone master for DSR replies");
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
                    // Answer the cursor-position query `LiveRegion::new`
                    // makes, the way a real terminal does. Without a reply
                    // crossterm times out after two seconds and the region
                    // falls back to the bottom of the screen, which would
                    // leave that path untested.
                    if chunk.windows(4).any(|window| window == b"\x1b[6n") {
                        let report = format!("\x1b[{};{}R", screen.y + 1, screen.x + 1);
                        let _ = reply_master.write_all(report.as_bytes());
                    }
                }
            }
        }
    });

    eprintln!("[tui_pty] waiting for startup marker {STARTUP_MARKER:?}");
    wait_for_screen_text(&shared, STARTUP_MARKER);
    assert_no_alternate_screen_or_mouse_capture(&shared);
    eprintln!("[tui_pty] saw startup marker; typing prompt");

    master
        .write_all(format!("{PROMPT}\r").as_bytes())
        .expect("type the prompt");
    eprintln!("[tui_pty] waiting for reply {REPLY:?}");
    wait_for_screen_text(&shared, REPLY);
    eprintln!("[tui_pty] saw reply; waiting for the turn to finish (busy title to clear)");
    // The reply text lands on screen mid-turn (the streaming sink redraws on
    // every event), while the composer still shows "Working (Esc to
    // cancel)". Ctrl+C during that window cancels the turn rather than
    // arming the exit prompt.
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

    // Ctrl+C twice, not `/exit`: typing `/` opens the slash-command
    // suggestion list, which is 27 rows tall on this 30-row screen and
    // scrolls the committed transcript out of the visible grid before the
    // assertions below can look at it.
    master.write_all(b"\x03\x03").expect("press Ctrl+C twice");
    eprintln!("[tui_pty] pressed Ctrl+C twice; waiting for child to exit");
    let status = child.wait().expect("wait for the child to exit");
    eprintln!("[tui_pty] child exited: {status:?}");
    assert!(status.success(), "otto --ui tui exited with {status:?}");

    // There is no alternate screen to restore. `LiveRegion::finish` erases
    // the live region, which takes the composer border off screen, and
    // leaves the committed transcript where the shell prompt follows it.
    wait_for_screen_text_gone(&shared, STARTUP_MARKER);
    let screen = shared.screen.lock().unwrap().dump();
    assert!(
        screen.contains(&format!("> {PROMPT}")) && screen.contains(REPLY),
        "the transcript must stay in the terminal after exit:\n{screen}"
    );
    assert_no_alternate_screen_or_mouse_capture(&shared);
    eprintln!("[tui_pty] live region cleared, transcript retained");
}

/// The two sequences the inline-viewport design exists to avoid: the
/// alternate screen (no scrollback for the wheel to scroll) and mouse
/// capture (terminals stop doing native drag-selection while it is on).
fn assert_no_alternate_screen_or_mouse_capture(shared: &Shared) {
    assert!(
        !raw_contains(shared, b"\x1b[?1049h"),
        "the transcript must live in the terminal's own scrollback, not on the alternate screen"
    );
    assert!(
        !raw_contains(shared, b"\x1b[?1000h"),
        "the TUI must leave mouse events to the terminal for native selection and wheel scrolling"
    );
}
