//! Otto command line entry point.
//!
//! Everything the process does lives in `otto::cli::run`, which takes its
//! stdio, environment and terminal state as arguments so the whole startup
//! path is reachable from tests. This file only binds those arguments to
//! real process state, turns SIGINT into one cancellation, and decides what
//! SIGTERM does through `otto::cli::terminate::Terminate` (cancel and
//! migrate, cancel and exit, or die by the signal; see that module).

use std::io::IsTerminal;
use std::os::unix::ffi::OsStrExt;

use tokio_util::sync::CancellationToken;

use otto::cli::terminate::{Action, Terminate, die_by_sigterm};

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("otto: start runtime: {error}");
            return std::process::ExitCode::from(1);
        }
    };
    let code = runtime.block_on(async_main());
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}

async fn async_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let environment = std::env::vars_os()
        .map(|(name, value)| {
            let mut entry = name.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            entry
        })
        .collect();

    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let cancel = CancellationToken::new();
    // The REPL has no per-turn interrupt yet, so every SIGINT cancels the
    // process token.
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });

    let terminate = std::sync::Arc::new(Terminate::new());
    // Registration fails only when the signal cannot be installed at all
    // (already registered elsewhere in-process, or the platform refuses
    // it); skip SIGTERM handling in that case rather than fail startup,
    // matching how the old serve-only handler treated the same error.
    if let Ok(mut signals) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        let signal_cancel = cancel.clone();
        let signal_terminate = std::sync::Arc::clone(&terminate);
        tokio::spawn(async move {
            while signals.recv().await.is_some() {
                match signal_terminate.on_signal(otto::failover::lease::holds_lease()) {
                    Action::Migrate | Action::Cancel => signal_cancel.cancel(),
                    Action::Die => die_by_sigterm(),
                    Action::Ignore => {}
                }
            }
        });
    }

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let code = otto::cli::run::run(
        &args,
        Box::new(std::io::BufReader::new(std::io::stdin())),
        &mut stdout,
        &mut stderr,
        environment,
        terminal,
        &cancel,
        &terminate,
    )
    .await;
    // A migration cancels the running turn through the same path SIGINT
    // uses, which `run` reports as exit code 130; once the migration itself
    // has completed (recorded, notified, MCP closed, lease released), that
    // cancellation was the successful outcome, not a failure, so report 0.
    // Any other code (for example 1, a close error) is kept.
    if terminate.migrating() && code == 130 {
        0
    } else {
        code
    }
}
