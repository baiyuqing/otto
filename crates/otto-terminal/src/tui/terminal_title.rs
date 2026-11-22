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

enum TitleState {
    Inactive,
    Abandoned,
    Released,
    ForegroundWithoutOverride,
    ForegroundWithOverride { previous: String },
}

// Job control and panic cleanup operate on the one process-wide real terminal.
static TITLE: Mutex<TitleState> = Mutex::new(TitleState::Inactive);

pub(super) struct TerminalTitle(());

impl TerminalTitle {
    pub(super) async fn enter() -> Option<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return None;
        }
        {
            let mut state = TITLE.lock().ok()?;
            if !matches!(*state, TitleState::Inactive) {
                return None;
            }
            *state = TitleState::Released;
        }
        // Construct the guard before awaiting so cancellation releases its slot.
        let guard = Self(());
        acquire_foreground().await;
        Some(guard)
    }
}

impl Drop for TerminalTitle {
    fn drop(&mut self) {
        if let Ok(mut state) = TITLE.lock() {
            restore_override(std::mem::replace(&mut *state, TitleState::Inactive));
        }
    }
}

#[cfg(unix)]
pub(super) fn release_foreground() {
    if let Ok(mut state) = TITLE.lock()
        && matches!(
            *state,
            TitleState::ForegroundWithoutOverride | TitleState::ForegroundWithOverride { .. }
        )
    {
        restore_override(std::mem::replace(&mut *state, TitleState::Released));
    }
}

pub(super) fn restore_after_panic() {
    // Never wait on a lock interrupted by this panic. Invalidate any snapshot
    // we consume so the guard does not replay it during unwinding.
    if let Ok(mut state) = TITLE.try_lock()
        && !matches!(*state, TitleState::Inactive)
    {
        restore_override(std::mem::replace(&mut *state, TitleState::Abandoned));
    }
}

pub(super) async fn acquire_foreground() {
    if !TITLE
        .lock()
        .is_ok_and(|state| matches!(*state, TitleState::Released))
    {
        return;
    }
    // The shell can set a different title while it owns the terminal. Never
    // carry a restore point across that handoff, even if the new query fails.
    let previous = query_previous().await;
    if let Ok(mut state) = TITLE.lock()
        && matches!(*state, TitleState::Released)
    {
        *state = match previous {
            Some(previous) => {
                // Keep the restore point even if writing or flushing fails.
                let state = TitleState::ForegroundWithOverride { previous };
                let _ = set_title("Otto");
                state
            }
            None => TitleState::ForegroundWithoutOverride,
        };
    }
}

fn restore_override(state: TitleState) {
    if let TitleState::ForegroundWithOverride { previous } = state {
        let _ = set_title(&previous);
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
