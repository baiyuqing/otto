//! Read-only `git` invocations through a sandbox command executor.

use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{CommandExecutor, Request, Streams};

/// Runs `git` with `args` in `dir` through `executor` and returns its standard
/// output, at most 12 KiB. Returns `None` when the deadline passes, the process
/// fails, or the output exceeds the cap. `core.fsmonitor` is disabled so a
/// repository setting cannot start a daemon.
pub(crate) async fn run_git(
    executor: &Arc<dyn CommandExecutor>,
    dir: &str,
    environment: &[String],
    args: &[&str],
    deadline: tokio::time::Instant,
    cancel: &CancellationToken,
) -> Option<String> {
    let mut argv = vec![
        "git".to_string(),
        "-c".to_string(),
        "core.fsmonitor=false".to_string(),
    ];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let request = Request {
        argv,
        dir: Path::new(dir).to_path_buf(),
        env: environment.to_vec(),
        ..Default::default()
    };
    struct BoundedOutput {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl std::io::Write for BoundedOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let available = (12 * 1024usize).saturating_sub(self.bytes.len());
            self.bytes
                .extend_from_slice(&bytes[..available.min(bytes.len())]);
            self.exceeded |= bytes.len() > available;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = BoundedOutput {
        bytes: Vec::new(),
        exceeded: false,
    };
    let mut discard = std::io::sink();
    let streams = Streams {
        stdout: &mut out,
        stderr: &mut discard,
    };
    let mut execution = Box::pin(executor.execute(request, streams, cancel));
    let (status, result) = match tokio::time::timeout_at(deadline, &mut execution).await {
        Ok(outcome) => outcome,
        Err(_) => {
            cancel.cancel();
            let _ =
                tokio::time::timeout(crate::sandbox::DEFAULT_CANCELLATION_GRACE, execution).await;
            return None;
        }
    };
    drop(execution);
    result.ok()?;
    if status.code != 0 || status.signaled || out.exceeded {
        return None;
    }
    Some(String::from_utf8_lossy(&out.bytes).into_owned())
}
