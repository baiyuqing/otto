//! `otto eval`: the development evaluation targets and the infrastructure they share.
//!
//! The only target is `memory-retraction` (see `crate::memory_eval_command`). The submodules
//! hold what the target needs: the provider environment with usage metering, the redacting output
//! directory and the tool-less executor. Every text an evaluation writes passes through the
//! runtime's redactor. The commands change no session and register no product tool.

pub(crate) mod model_calls;
pub(crate) mod output_files;
pub(crate) mod tool_executors;

#[cfg(test)]
pub(crate) mod tests;

use std::io::Write;
use std::path::PathBuf;

use otto_core::config::resolve::Runtime;
use tokio_util::sync::CancellationToken;

use otto::runtime::builder::Builder;

/// One of the evaluation targets of `otto eval`.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalCommand {
    MemoryRetraction(crate::memory_eval_command::Invocation),
}

impl EvalCommand {
    /// Whether the command only prints its size estimate.
    pub fn is_dry_run(&self) -> bool {
        match self {
            Self::MemoryRetraction(invocation) => invocation.is_dry_run(),
        }
    }

    /// The flags the shared option parser needs to locate the workspace and configuration.
    pub fn startup_flags(&self) -> Vec<String> {
        match self {
            Self::MemoryRetraction(invocation) => invocation.startup_flags(),
        }
    }
}

/// Parses the arguments after `eval`: the target, then the target's own grammar.
pub fn parse_command(args: &[String]) -> Result<EvalCommand, String> {
    match args.first().map(String::as_str) {
        Some("memory-retraction") => {
            crate::memory_eval_command::parse(&args[1..]).map(EvalCommand::MemoryRetraction)
        }
        Some(other) => Err(format!(
            "otto: unknown eval target {other:?}; the target is memory-retraction"
        )),
        None => Err("otto: eval requires a target: memory-retraction".into()),
    }
}

/// Runs the `--dry-run` of the target.
pub fn run_dry_command(
    command: &EvalCommand,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
) -> i32 {
    match command {
        EvalCommand::MemoryRetraction(invocation) => {
            crate::memory_eval_command::run_dry(invocation, stdout, stderr)
        }
    }
}

/// Runs the target against the runtime's provider.
pub async fn run_command(
    command: &EvalCommand,
    builder: &Builder,
    runtime: &Runtime,
    roots: &Roots,
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    cancel: &CancellationToken,
) -> i32 {
    match command {
        EvalCommand::MemoryRetraction(invocation) => {
            crate::memory_eval_command::run(
                invocation, builder, runtime, roots, stdout, stderr, cancel,
            )
            .await
        }
    }
}

/// Where the session files and the evaluation output live.
pub struct Roots {
    pub session_root: PathBuf,
    pub workspace: String,
    pub home: String,
}
