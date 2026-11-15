//! Process composition: the entry point `main` calls, from flag parsing to the
//! exit code.
//!
//! The tests pin the order of operations, the exact stderr text and the exit
//! codes.
//!
//! The composition root opens one process sandbox and hands it to a
//! [`SandboxSwitch`], so `/sandbox reload`, `POST /v1/sandbox/reload` and the
//! TUI all re-point bash at a new runtime without restarting the process.
//!
//! `sandbox`, `memory`, `login`, `logout`, `setup` and `trust` dispatch before flag
//! parsing, because their argument grammars are their own; nothing is left
//! unported.
//!
//! The TUI (`--ui tui`, or `--ui auto` on a terminal) dispatches to
//! [`otto::tui::run`].
//!
//! Safety: every diagnostic that could carry a host path, an environment name
//! or a provider URL goes through the redaction boundary before it is written.
//! A boundary that cannot prove it collected every secret renders the empty
//! string.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use otto_core::config::UiMode;
use otto_core::config::resolve::{Overrides, Runtime};
use otto_core::config::{resolve_server, resolve_ui_mode};
use otto_core::session::{CURRENT_VERSION, Header, RuntimeMetadata};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use otto::session;

use super::flags::{CliOptions, ParseFailure, Parsed, parse_flags};
use super::serve;
use otto::app::{Controller, PROMPT_ACTIVE};

use otto::cli::repl::{self, Repl};
use otto::runtime::SESSION_OPERATION_UNAVAILABLE;
use otto::runtime::boundary;
use otto::runtime::builder::{Builder, SharedSession};
use otto::runtime::startup::{
    self, EnvironmentLookup, capture_environment, environment_lookup, redact_with, resolve_home,
};
// Helpers other `cli` modules and `plugin` call through this path.
pub(crate) use otto::runtime::startup::{
    config_environment_for, load_config_for, resolve_home_for, sandbox_provider_environment_names,
};

mod attach;
mod delete;
mod process;
mod session_setup;
mod subcommands;
pub use process::*;
use session_setup::*;

const MAX_PROMPT_BYTES: usize = 1 << 20;

struct StartupTrace {
    enabled: bool,
    started: Instant,
    last: Instant,
    entries: Vec<(&'static str, std::time::Duration)>,
}

impl StartupTrace {
    fn from_lookup(lookup: &EnvironmentLookup, started: Instant) -> Self {
        let enabled = matches!(
            lookup.get("OTTO_STARTUP_TRACE").map(String::as_str),
            Some("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
        );
        Self {
            enabled,
            started,
            last: started,
            entries: Vec::new(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn mark(&mut self, label: &'static str) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        self.entries.push((label, now.duration_since(self.last)));
        self.last = now;
    }

    fn extend(&mut self, entries: Vec<(&'static str, std::time::Duration)>) {
        if !self.enabled {
            return;
        }
        self.entries.extend(entries);
    }

    fn finish_ready(&mut self, stderr: &mut (dyn Write + Send)) {
        if !self.enabled {
            return;
        }
        for (label, elapsed) in &self.entries {
            let _ = writeln!(stderr, "startup {label}: {}ms", elapsed.as_millis());
        }
        let _ = writeln!(
            stderr,
            "startup ready: {}ms",
            Instant::now().duration_since(self.started).as_millis()
        );
        self.enabled = false;
    }
}

/// Which frontend the resolved UI mode selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    Repl,
    Once,
    Tui,
}

/// Writes `otto: {message}\n` and returns exit code 1.
pub(crate) fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}

/// Runs one `otto` invocation and returns its exit code.
///
/// `environment_entries` is the raw process environment (`KEY=VALUE` byte
/// strings); `terminal` says whether stdin and stdout are both a terminal.
/// Both are parameters rather than reads of process state so the whole
/// startup path is reachable from tests.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    args: &[String],
    stdin: Box<dyn BufRead + Send + 'static>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    environment_entries: Vec<Vec<u8>>,
    terminal: bool,
    cancel: &CancellationToken,
    terminate: &super::terminate::Terminate,
) -> i32 {
    run_with_extensions(
        args,
        stdin,
        stdout,
        stderr,
        environment_entries,
        terminal,
        cancel,
        terminate,
        None,
    )
    .await
}

#[cfg(test)]
mod session_setup_tests;
#[cfg(test)]
mod tests;
