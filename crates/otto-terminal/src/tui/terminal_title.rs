//! Best-effort transient title for the real Rex terminal, not its tab name.

use std::io::{self, IsTerminal, Write};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const QUERY_LIMIT: usize = 4096;
const QUERY_TIMEOUT: Duration = Duration::from_millis(750);

struct Title {
    previous: String,
    applied: bool,
}

// Job control and panic cleanup do not have access to the owning TUI future.
// Unlike UI state, this refers only to the one process-wide real terminal.
static TITLE: Mutex<Option<Title>> = Mutex::new(None);

pub(super) struct TerminalTitle(bool);

impl TerminalTitle {
    pub(super) async fn enter() -> Self {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Self(false);
        }
        let Some(previous) = query_previous().await else {
            return Self(false);
        };
        let Ok(mut state) = TITLE.lock() else {
            return Self(false);
        };
        if state.is_some() {
            return Self(false);
        }
        let mut title = Title {
            previous,
            applied: false,
        };
        title.apply();
        *state = Some(title);
        Self(true)
    }
}

impl Drop for TerminalTitle {
    fn drop(&mut self) {
        if self.0
            && let Ok(mut state) = TITLE.lock()
            && let Some(mut title) = state.take()
        {
            title.restore();
        }
    }
}

pub(super) fn restore() {
    // Also called from the panic hook, where it must not wait on itself.
    if let Ok(mut state) = TITLE.try_lock()
        && let Some(title) = state.as_mut()
    {
        title.restore();
    }
}

#[cfg(unix)]
pub(super) fn resume() {
    if let Ok(mut state) = TITLE.lock()
        && let Some(title) = state.as_mut()
    {
        title.apply();
    }
}

impl Title {
    fn apply(&mut self) {
        if !self.applied {
            // Restore even if the write succeeded but flushing failed.
            self.applied = true;
            let _ = set_title("Otto");
        }
    }

    fn restore(&mut self) {
        if self.applied && set_title(&self.previous).is_ok() {
            self.applied = false;
        }
    }
}

fn set_title(title: &str) -> io::Result<()> {
    let mut output = io::stdout().lock();
    write!(output, "\x1b]2;{title}\x1b\\")?;
    output.flush()
}

async fn query_previous() -> Option<String> {
    let session = std::env::var("REX_SESSION").ok()?;
    let block = std::env::var("REX_BLOCK").ok()?;
    if !session.starts_with("session:") || !block.starts_with("block:") {
        return None;
    }
    // Never start a server, take input from the terminal, or surface CLI logs.
    let mut child = Command::new("rex")
        .args([
            "--autostart=false",
            "--timeout=500ms",
            "block",
            "call",
            "-s",
            &session,
            "-b",
            &block,
            "title",
            "{}",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut output = child.stdout.take()?.take(QUERY_LIMIT as u64 + 1);
    let mut bytes = Vec::new();
    tokio::time::timeout(QUERY_TIMEOUT, async {
        output.read_to_end(&mut bytes).await.ok()?;
        if bytes.len() > QUERY_LIMIT || !child.wait().await.ok()?.success() {
            return None;
        }
        parse_title(&bytes)
    })
    .await
    .ok()?
}

fn parse_title(bytes: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Reply {
        title: String,
    }
    let reply = serde_json::from_slice::<Reply>(bytes).ok()?;
    // The result comes from another process; never let it terminate the OSC
    // or otherwise inject terminal controls. An empty title is valid.
    (!reply.title.chars().any(char::is_control)).then_some(reply.title)
}

#[cfg(test)]
#[path = "terminal_title_tests.rs"]
mod tests;
