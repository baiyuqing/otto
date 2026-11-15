//! The `otto eval` command as seen by `process.rs`.
//!
//! With the `eval` feature, `Command` and `Roots` are the types of the `otto-eval` crate and the
//! functions forward to it. Without the feature, `otto-eval` is not linked: `parse` answers
//! `otto eval ...` with a one-line message and exit code 2, and `Command` and `Roots` are
//! uninhabited types, so the code in `process.rs` that handles a parsed command is never reached.

#[cfg(not(feature = "eval"))]
pub(super) use disabled::*;
#[cfg(feature = "eval")]
pub(super) use enabled::*;

#[cfg(feature = "eval")]
mod enabled {
    use std::io::Write;
    use std::path::PathBuf;

    use otto::runtime::builder::Builder;
    use otto_core::config::resolve::Runtime;
    use otto_eval::eval_command::{self, EvalCommand};
    use tokio_util::sync::CancellationToken;

    pub(in crate::run) type Command = EvalCommand;
    pub(in crate::run) type Roots = eval_command::Roots;

    /// Parses `args` when the first one is `eval`; `Err` carries the exit code.
    pub(in crate::run) fn parse(
        args: &[String],
        stderr: &mut (dyn Write + Send),
    ) -> Result<Option<Command>, i32> {
        if !args.first().is_some_and(|first| first == "eval") {
            return Ok(None);
        }
        match eval_command::parse_command(&args[1..]) {
            Ok(command) => Ok(Some(command)),
            Err(message) => {
                let _ = writeln!(stderr, "{message}");
                Err(2)
            }
        }
    }

    pub(in crate::run) fn startup_flags(command: &Command) -> Vec<String> {
        command.startup_flags()
    }

    pub(in crate::run) fn is_dry_run(command: &Command) -> bool {
        command.is_dry_run()
    }

    pub(in crate::run) fn roots(session_root: PathBuf, workspace: String, home: String) -> Roots {
        Roots {
            session_root,
            workspace,
            home,
        }
    }

    pub(in crate::run) fn run_dry(
        command: &Command,
        stdout: &mut (dyn Write + Send),
        stderr: &mut (dyn Write + Send),
    ) -> i32 {
        eval_command::run_dry_command(command, stdout, stderr)
    }

    pub(in crate::run) async fn run(
        command: &Command,
        builder: &Builder,
        runtime: &Runtime,
        roots: &Roots,
        stdout: &mut (dyn Write + Send),
        stderr: &mut (dyn Write + Send),
        cancel: &CancellationToken,
    ) -> i32 {
        eval_command::run_command(command, builder, runtime, roots, stdout, stderr, cancel).await
    }
}

#[cfg(not(feature = "eval"))]
mod disabled {
    use std::io::Write;
    use std::path::PathBuf;

    use otto::runtime::builder::Builder;
    use otto_core::config::resolve::Runtime;
    use tokio_util::sync::CancellationToken;

    /// Uninhabited: no `otto eval` command exists without the `eval` feature.
    pub(in crate::run) enum Command {}
    /// Uninhabited, like `Command`.
    pub(in crate::run) enum Roots {}

    pub(in crate::run) const NOT_COMPILED: &str = "otto: the eval command is not compiled into this binary; \
         build with: cargo build -p otto-cli --features eval";

    /// Answers `otto eval ...` with `NOT_COMPILED` and `Err(2)`.
    pub(in crate::run) fn parse(
        args: &[String],
        stderr: &mut (dyn Write + Send),
    ) -> Result<Option<Command>, i32> {
        if args.first().is_some_and(|first| first == "eval") {
            let _ = writeln!(stderr, "{NOT_COMPILED}");
            return Err(2);
        }
        Ok(None)
    }

    pub(in crate::run) fn startup_flags(command: &Command) -> Vec<String> {
        match *command {}
    }

    pub(in crate::run) fn is_dry_run(command: &Command) -> bool {
        match *command {}
    }

    pub(in crate::run) fn roots(_: PathBuf, _: String, _: String) -> Roots {
        unreachable!("no eval command exists without the `eval` feature")
    }

    pub(in crate::run) fn run_dry(
        command: &Command,
        _: &mut (dyn Write + Send),
        _: &mut (dyn Write + Send),
    ) -> i32 {
        match *command {}
    }

    pub(in crate::run) async fn run(
        command: &Command,
        _: &Builder,
        _: &Runtime,
        _: &Roots,
        _: &mut (dyn Write + Send),
        _: &mut (dyn Write + Send),
        _: &CancellationToken,
    ) -> i32 {
        match *command {}
    }
}
