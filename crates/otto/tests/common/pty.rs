//! The PTY screen scraper shared by `tui_pty.rs` and `tui_attach_pty.rs`.
//! See the module doc of `tui_pty.rs` for why it is this small.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// A minimal terminal screen, built only for the ANSI vocabulary ratatui's
/// crossterm backend can emit (see the module doc). Fragmentation-tolerant:
/// `feed` buffers an incomplete escape sequence or a multi-byte UTF-8
/// character split across two reads.
pub struct Screen {
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    cells: Vec<Vec<char>>,
    cursor_visible: bool,
    pending: Vec<u8>,
}

impl Screen {
    pub fn new(width: usize, height: usize) -> Self {
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
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
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

    pub fn contains(&self, needle: &str) -> bool {
        self.cells
            .iter()
            .any(|row| row.iter().collect::<String>().contains(needle))
    }

    pub fn dump(&self) -> String {
        self.cells
            .iter()
            .map(|row| row.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// State shared between the test body and the background reader thread.
pub struct Shared {
    pub screen: Mutex<Screen>,
    pub raw: Mutex<Vec<u8>>,
    pub parse_error: Mutex<Option<String>>,
}

pub fn wait_until(shared: &Shared, what: &str, mut predicate: impl FnMut(&Shared) -> bool) {
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

pub fn wait_for_screen_text(shared: &Shared, needle: &str) {
    wait_until(shared, &format!("screen text {needle:?}"), |shared| {
        shared.screen.lock().unwrap().contains(needle)
    });
}

/// Waits for `needle` to stop appearing on screen. Returns at once if
/// `needle` was never drawn, so a caller that needs it to have appeared
/// first checks that separately (see [`wait_for_raw_bytes`]).
pub fn wait_for_screen_text_gone(shared: &Shared, needle: &str) {
    wait_until(
        shared,
        &format!("screen text {needle:?} to clear"),
        |shared| !shared.screen.lock().unwrap().contains(needle),
    );
}

pub fn raw_contains(shared: &Shared, needle: &[u8]) -> bool {
    let raw = shared.raw.lock().unwrap();
    raw.windows(needle.len()).any(|window| window == needle)
}

pub fn wait_for_raw_bytes(shared: &Shared, needle: &[u8]) {
    wait_until(
        shared,
        &format!("raw bytes {:?}", String::from_utf8_lossy(needle)),
        |shared| raw_contains(shared, needle),
    );
}

/// Spawns `command` on a 120x30 PTY and starts the reader thread that feeds
/// the screen. Returns the master (type into it), the child and the screen.
pub fn spawn(
    mut command: std::process::Command,
) -> (std::fs::File, std::process::Child, std::sync::Arc<Shared>) {
    use std::io::Read as _;
    use std::process::Stdio;
    let winsize = nix::pty::Winsize {
        ws_row: 30,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&winsize), None).expect("open a pty");
    let master = std::fs::File::from(pty.master);
    let slave = std::fs::File::from(pty.slave);
    command
        .stdin(Stdio::from(slave.try_clone().expect("clone slave")))
        .stdout(Stdio::from(slave.try_clone().expect("clone slave")))
        .stderr(Stdio::from(slave));
    let child = command.spawn().expect("spawn the otto binary");
    let shared = std::sync::Arc::new(Shared {
        screen: Mutex::new(Screen::new(120, 30)),
        raw: Mutex::new(Vec::new()),
        parse_error: Mutex::new(None),
    });
    let reader = std::sync::Arc::clone(&shared);
    let mut reader_master = master.try_clone().expect("clone master");
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader_master.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    reader
                        .raw
                        .lock()
                        .unwrap()
                        .extend_from_slice(&buffer[..read]);
                    if let Err(message) = reader.screen.lock().unwrap().feed(&buffer[..read]) {
                        *reader.parse_error.lock().unwrap() = Some(message);
                        return;
                    }
                }
            }
        }
    });
    (master, child, shared)
}
