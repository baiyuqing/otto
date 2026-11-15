//! Cleanup that runs on panic, installed by `main`.
//!
//! Release builds use `panic = "abort"`, so `Drop` does not run on panic.
//! This hook kills the registered child process groups (sandboxed bash and
//! MCP stdio servers, which rely on `kill_on_drop`) and resets the terminal
//! input modes (mouse reporting and bracketed paste). ratatui's own hook,
//! chained later by `ratatui::try_init` and therefore run first, restores raw
//! mode and the alternate screen. Locks, file descriptors and the
//! `otto serve` socket file need no hook: the kernel releases them, and
//! `server/listen.rs` removes a stale socket at the next start.

use otto::child_fence::Children;

/// Installs the panic hook when the build aborts on panic.
///
/// Under unwind the hook is skipped: `Drop` guards run, and a panicking tokio
/// task ends only that task, so killing every child would affect a process
/// that keeps running.
pub fn install() {
    if !cfg!(panic = "abort") {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        cleanup(Children::global());
        previous(info);
    }));
}

fn cleanup(children: &Children) {
    children.kill_all_without_blocking();
    otto::tui::reset_terminal_input_modes_after_panic();
}
