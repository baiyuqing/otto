//! Interactive subprocesses with the same group and failover ownership as commands.

use super::*;
use std::future::Future;
use tokio::process::{ChildStdin, ChildStdout};

impl Manager {
    /// Run a protocol against a managed process. The callback must return when
    /// cancelled. On its completion the entire process group is terminated and
    /// observed gone before this method returns.
    pub(crate) async fn run_interactive<T, F, Fut>(
        &self,
        spec: Spec,
        cancel: &CancellationToken,
        cancellation_grace: Duration,
        protocol: F,
    ) -> Result<T, Error>
    where
        F: FnOnce(ChildStdout, ChildStdin) -> Fut,
        Fut: Future<Output = Result<T, Error>>,
    {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let program = resolve_executable(&spec).map_err(|_| Error::ChildLaunch)?;
        let mut command = std::process::Command::new(&program);
        command
            .arg0(&spec.path)
            .args(&spec.args)
            .current_dir(&spec.directory)
            .env_clear();
        for entry in &spec.environment {
            let (name, value) = entry.split_once('=').unwrap_or((entry.as_str(), ""));
            command.env(name, value);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let (entry, mut child) = {
            let mut state = self.state.lock().expect("process state");
            if state.closing {
                return Err(Error::Closed);
            }
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let child = command.spawn().map_err(|_| Error::ChildLaunch)?;
            let pid = child.id().ok_or(Error::ChildLaunch)? as i32;
            let entry = Arc::new(Entry {
                pid,
                cancellation_grace,
                termination: Mutex::new(Termination::default()),
                cleanup_changed: tokio::sync::Notify::new(),
                _fence_registration: crate::failover::children::Children::register(
                    crate::failover::children::Children::global(),
                    pid,
                    true,
                ),
            });
            state.active.insert(pid, entry.clone());
            (entry, child)
        };
        let stdin = child.stdin.take().ok_or(Error::ChildLaunch)?;
        let stdout = child.stdout.take().ok_or(Error::ChildLaunch)?;
        let cleanup = CancellationToken::new();
        let mut discarded_stdout = std::io::sink();
        let mut discarded_stderr = std::io::sink();
        let streams = Streams {
            stdout: &mut discarded_stdout,
            stderr: &mut discarded_stderr,
        };
        let execute = protocol(stdout, stdin);
        let drain = drain(&mut child, streams, &cleanup, &entry);
        tokio::pin!(execute);
        tokio::pin!(drain);
        let (failure, result) = tokio::select! {
            biased;
            result = &mut execute => {
                cleanup.cancel();
                let (_, failure) = drain.await;
                (failure, result)
            }
            (_, failure) = &mut drain => {
                // Consume a response already buffered when the agent exited.
                match tokio::time::timeout(Duration::from_millis(100), &mut execute).await {
                    Ok(result) => (failure, result),
                    Err(_) => {
                        // Wake a protocol blocked on approval after its process exits.
                        cancel.cancel();
                        let _ = tokio::time::timeout(Duration::from_secs(6), &mut execute).await;
                        (failure, Err(Error::ChildWait))
                    }
                }
            }
        };
        {
            let mut state = self.state.lock().expect("process state");
            state.active.remove(&entry.pid);
        }
        self.progress.notify_all();
        if let Some(error) = failure.filter(|error| *error != Error::Cancelled) {
            return Err(error);
        }
        result
    }
}

#[cfg(test)]
mod tests;
