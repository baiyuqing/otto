//! Development evaluations run by `otto eval`.
//!
//! The only target is `memory-retraction` (`memory_eval_command`); `eval_command` holds the
//! command parser, the dispatch and the infrastructure the target shares. The crate depends on
//! `otto` and `otto-core` only. `otto-cli` links it into the `otto` binary only when built with
//! its `eval` feature (`cargo build -p otto-cli --features eval`).

use std::io::Write;

pub mod eval_command;
pub mod memory_eval_command;

/// Writes `otto: {message}\n` to `stderr` and returns exit code 1. It has the same output
/// format and return code as `otto-cli`'s `run::fail`.
pub(crate) fn fail(stderr: &mut (dyn Write + Send), message: &str) -> i32 {
    let _ = writeln!(stderr, "otto: {message}");
    1
}
