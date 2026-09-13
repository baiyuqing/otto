//! Otto command line entry point.
//!
//! Everything the process does lives in `otto::cli::run`, which takes its
//! stdio, environment and terminal state as arguments so the whole startup
//! path is reachable from tests. This file only binds those arguments to real
//! process state and turns SIGINT into one cancellation.

use std::io::IsTerminal;
use std::os::unix::ffi::OsStrExt;

use tokio_util::sync::CancellationToken;

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
    // Go interrupts the running turn on the first SIGINT and cancels the
    // process only when no turn is active; the REPL port has no per-turn
    // interrupt yet, so every SIGINT cancels the process token.
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    otto::cli::run::run(
        &args,
        Box::new(std::io::BufReader::new(std::io::stdin())),
        &mut stdout,
        &mut stderr,
        environment,
        terminal,
        &cancel,
    )
    .await
}
