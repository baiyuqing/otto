//! Subcommands whose argument grammar is their own and that run before flag parsing.

use super::*;

/// Runs the subcommand named by `args[0]` and returns its exit code, or `None` when `args[0]`
/// names no such subcommand and the normal flag-parsing path continues.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_subcommand(
    args: &[String],
    stdin: &mut Box<dyn BufRead + Send + 'static>,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    environment_entries: &[Vec<u8>],
    cancel: &CancellationToken,
    memory_extension: bool,
) -> Option<i32> {
    if memory_extension && args.first().is_some_and(|argument| argument == "memory") {
        return Some(fail(
            stderr,
            "memory operation is unsupported by the native record management interface; use the Rust memory service API",
        ));
    }
    if args
        .first()
        .is_some_and(|a| matches!(a.as_str(), "config" | "plugin"))
    {
        let entries = match capture_environment(environment_entries.to_vec()) {
            Ok(v) => v,
            Err(e) => return Some(fail(stderr, &e)),
        };
        let lookup = match environment_lookup(&entries) {
            Ok(v) => v,
            Err(e) => return Some(fail(stderr, &e)),
        };
        if args[0] == "plugin" {
            return Some(crate::plugin::run(&args[1..], stdout, stderr, &lookup));
        }
        return Some(crate::config_command::run(
            &args[1..],
            stdout,
            stderr,
            &lookup,
        ));
    }
    if args.first().is_some_and(|command| command == "workspace") {
        return Some(crate::workspace_command::run(&args[1..], stdout, stderr));
    }
    if args.first().is_some_and(|command| command == "storage") {
        return Some(crate::storage::run(&args[1..], stdout, stderr));
    }
    // These dispatch before flag parsing, because their argument grammars are
    // their own.
    if let Some(first) = args.first()
        && matches!(first.as_str(), "sandbox" | "memory")
    {
        let host_entries = match capture_environment(environment_entries.to_vec()) {
            Ok(entries) => entries,
            Err(message) => return Some(fail(stderr, &message)),
        };
        let lookup = match environment_lookup(&host_entries) {
            Ok(lookup) => lookup,
            Err(message) => return Some(fail(stderr, &message)),
        };
        if first == "memory" {
            return Some(crate::memory_command::run(
                &args[1..],
                stdout,
                stderr,
                &lookup,
            ));
        }
        return Some(
            crate::sandbox_setup::run(
                &args[1..],
                stdin,
                stdout,
                stderr,
                &host_entries,
                &lookup,
                cancel,
            )
            .await,
        );
    }
    if let Some(first) = args.first()
        && first == "mcp"
    {
        let host_entries = match capture_environment(environment_entries.to_vec()) {
            Ok(entries) => entries,
            Err(message) => return Some(fail(stderr, &message)),
        };
        let lookup = match environment_lookup(&host_entries) {
            Ok(lookup) => lookup,
            Err(message) => return Some(fail(stderr, &message)),
        };
        return Some(crate::mcp::run(&args[1..], stdout, stderr, &lookup, cancel).await);
    }
    if let Some(first) = args.first()
        && matches!(first.as_str(), "login" | "logout")
    {
        let host_entries = match capture_environment(environment_entries.to_vec()) {
            Ok(entries) => entries,
            Err(message) => return Some(fail(stderr, &message)),
        };
        let lookup = match environment_lookup(&host_entries) {
            Ok(lookup) => lookup,
            Err(message) => return Some(fail(stderr, &message)),
        };
        let home = match resolve_home(&lookup) {
            Ok(home) => home,
            Err(message) => return Some(fail(stderr, &message)),
        };
        return Some(
            otto_terminal::cli::login::run_auth_command(args, stdout, stderr, &home, cancel).await,
        );
    }
    if let Some(first) = args.first()
        && first == "setup"
    {
        let host_entries = match capture_environment(environment_entries.to_vec()) {
            Ok(entries) => entries,
            Err(message) => return Some(fail(stderr, &message)),
        };
        let lookup = match environment_lookup(&host_entries) {
            Ok(lookup) => lookup,
            Err(message) => return Some(fail(stderr, &message)),
        };
        return Some(crate::setup::run(
            &args[1..],
            stdin,
            stdout,
            stderr,
            &lookup,
        ));
    }
    if let Some(first) = args.first()
        && first == "trust"
    {
        let host_entries = match capture_environment(environment_entries.to_vec()) {
            Ok(entries) => entries,
            Err(message) => return Some(fail(stderr, &message)),
        };
        let lookup = match environment_lookup(&host_entries) {
            Ok(lookup) => lookup,
            Err(message) => return Some(fail(stderr, &message)),
        };
        return Some(crate::trust::run(&args[1..], stdout, stderr, &lookup));
    }
    None
}
