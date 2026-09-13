//! The unconfined driver.
//!
//! Port of `internal/sandbox/direct`. It runs the child with no confinement at
//! all, so it advertises only [`Capabilities::network_allow`] and the executor
//! accepts it exclusively for the unconfined/allow policy. Process-group
//! containment still applies: see [`super::process`].

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::process::{Manager, Spec};
use super::{Capabilities, Driver, DriverId, Error, ExitStatus, Request, Streams};

/// The identifier this driver reports, matching the Go `direct.ID`.
pub const ID: &str = "direct";

/// Runs commands without any sandbox.
#[derive(Debug, Default)]
pub struct DirectDriver {
    processes: Manager,
}

impl DirectDriver {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Driver for DirectDriver {
    fn id(&self) -> DriverId {
        DriverId::new(ID)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            network_allow: true,
            ..Capabilities::default()
        }
    }

    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>) {
        let Some((path, args)) = request.argv.split_first() else {
            return (ExitStatus::default(), Err(Error::InvalidRequest));
        };
        let spec = Spec {
            path: path.clone(),
            args: args.to_vec(),
            directory: request.dir,
            environment: request.env,
        };
        let (outcome, result) = self.processes.run(spec, streams, cancel).await;
        let status = ExitStatus {
            code: outcome.code,
            signaled: outcome.signaled,
            signal: outcome.signal,
        };
        (status, result)
    }

    fn close(&self) -> Result<(), Error> {
        self.processes.close()
    }
}

#[cfg(test)]
mod tests {
    // Go's `TestDirectDoesNotRetainRequestOrWriters` mutates the caller's argv
    // and env after the call to prove the driver kept no alias. Half of that is
    // unrepresentable here: `Request` is moved into `execute`, so the caller has
    // no copy left to mutate. The writer half survives as an output assertion.
    use super::*;
    use std::io::Write as _;
    use std::sync::Arc;

    use crate::sandbox::{CommandExecutor, Executor, FilesystemMode, NetworkMode, Policy};

    /// A temporary directory resolved through symlinks, matching Go's
    /// `canonicalTempDir`. macOS puts `t.TempDir()` under `/var`, which is a
    /// symlink, and the executor only accepts canonical directories.
    fn canonical_temp_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().expect("temp dir");
        let canonical = std::fs::canonicalize(directory.path()).expect("canonicalize");
        (directory, canonical)
    }

    fn unconfined_allow() -> Policy {
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        }
    }

    fn discard() -> (std::io::Sink, std::io::Sink) {
        (std::io::sink(), std::io::sink())
    }

    #[test]
    fn identity_capabilities_and_policy_authority() {
        let driver = DirectDriver::new();
        assert_eq!(driver.id().as_str(), "direct");
        assert_eq!(driver.id().as_str(), ID);
        assert_eq!(
            driver.capabilities(),
            Capabilities {
                network_allow: true,
                ..Capabilities::default()
            }
        );
        driver.close().expect("close");

        let (_guard, workspace) = canonical_temp_dir();
        let accepted = Arc::new(DirectDriver::new());
        let executor = Executor::new(accepted, unconfined_allow(), &workspace)
            .expect("explicit unconfined allow");
        executor.close().expect("executor close");

        for policy in [
            Policy {
                filesystem: FilesystemMode::WorkspaceWrite,
                network: NetworkMode::Allow,
            },
            Policy {
                filesystem: FilesystemMode::WorkspaceWrite,
                network: NetworkMode::Deny,
            },
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Deny,
            },
        ] {
            let driver = Arc::new(DirectDriver::new());
            let Err(error) = Executor::new(driver.clone(), policy, &workspace) else {
                panic!("{policy:?} should be rejected");
            };
            assert_eq!(error, Error::UnsupportedPolicy);
            driver.close().expect("rejected driver close");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn maps_signals_and_sanitizes_launch_errors() {
        let (_guard, workspace) = canonical_temp_dir();
        let driver = Arc::new(DirectDriver::new());
        let executor = Executor::new(driver, unconfined_allow(), &workspace).expect("new executor");
        let cancel = CancellationToken::new();

        let (mut out, mut err) = discard();
        let (status, result) = executor
            .execute(
                Request {
                    argv: vec!["/bin/sh".into(), "-c".into(), "kill -TERM $$".into()],
                    dir: workspace.clone(),
                    env: vec!["PATH=/usr/bin:/bin".into(), "LC_ALL=C".into()],
                },
                Streams {
                    stdout: &mut out,
                    stderr: &mut err,
                },
                &cancel,
            )
            .await;
        result.expect("signal execute");
        assert_eq!(status.code, -1);
        assert!(status.signaled);
        assert_eq!(status.signal, "terminated");

        let sensitive_path = workspace.join("missing executable with secret");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let (_status, result) = executor
            .execute(
                Request {
                    argv: vec![
                        sensitive_path.to_string_lossy().into_owned(),
                        "secret-argument-value".into(),
                    ],
                    dir: workspace.clone(),
                    env: vec!["SECRET_VALUE=secret-environment-value".into()],
                },
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                &cancel,
            )
            .await;
        let Err(error) = result else {
            panic!("missing executable should fail");
        };
        assert_eq!(error, Error::ChildLaunch);
        let text = error.to_string();
        assert_eq!(text, Error::ChildLaunch.to_string());
        for secret in [
            sensitive_path.to_string_lossy().into_owned(),
            "secret-argument-value".to_string(),
            "secret-environment-value".to_string(),
        ] {
            assert!(!text.contains(&secret), "launch error exposed {secret}");
        }
        executor.close().expect("executor close");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_execute_and_close() {
        const EXECUTE_CALLERS: usize = 20;
        const CLOSE_CALLERS: usize = 12;

        let (_guard, workspace) = canonical_temp_dir();
        let driver = Arc::new(DirectDriver::new());
        let cancel = CancellationToken::new();

        let mut executions = Vec::with_capacity(EXECUTE_CALLERS);
        for _ in 0..EXECUTE_CALLERS {
            let driver = driver.clone();
            let workspace = workspace.clone();
            let cancel = cancel.clone();
            executions.push(tokio::spawn(async move {
                let (mut out, mut err) = discard();
                let (_status, result) = driver
                    .execute(
                        Request {
                            argv: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
                            dir: workspace,
                            env: vec!["PATH=/usr/bin:/bin".into()],
                        },
                        Streams {
                            stdout: &mut out,
                            stderr: &mut err,
                        },
                        &cancel,
                    )
                    .await;
                result
            }));
        }
        let mut closes = Vec::with_capacity(CLOSE_CALLERS);
        for _ in 0..CLOSE_CALLERS {
            let driver = driver.clone();
            closes.push(tokio::task::spawn_blocking(move || driver.close()));
        }

        for execution in executions {
            match execution.await.expect("execute task") {
                Ok(()) | Err(Error::Closed) => {}
                Err(error) => panic!("execute error = {error}"),
            }
        }
        for close in closes {
            close.await.expect("close task").expect("close");
        }

        let (mut out, mut err) = discard();
        let (_status, result) = driver
            .execute(
                Request {
                    argv: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
                    dir: workspace,
                    env: vec!["PATH=/usr/bin:/bin".into()],
                },
                Streams {
                    stdout: &mut out,
                    stderr: &mut err,
                },
                &cancel,
            )
            .await;
        assert_eq!(result, Err(Error::Closed));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn does_not_retain_writers_after_completion() {
        let (_guard, workspace) = canonical_temp_dir();
        let driver = DirectDriver::new();
        let cancel = CancellationToken::new();

        let mut stdout = Vec::new();
        let mut stderr = std::io::sink();
        let (status, result) = driver
            .execute(
                Request {
                    argv: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        r#"printf '%s' "$DIRECT_VALUE""#.into(),
                    ],
                    dir: workspace,
                    env: vec!["DIRECT_VALUE=original".into()],
                },
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                &cancel,
            )
            .await;
        result.expect("execute");
        assert_eq!(status.code, 0);
        assert_eq!(String::from_utf8_lossy(&stdout), "original");

        // A retained writer would still be reachable from the driver; writing
        // through the caller's borrow proves the borrow was returned and shows
        // nothing else appends afterwards.
        stdout.write_all(b"-after").expect("write after execute");
        driver.close().expect("close");
        assert_eq!(String::from_utf8_lossy(&stdout), "original-after");
    }
}

/// The shared driver contract, run against the unconfined driver.
///
/// Port of Go's `TestDirectDriverContract`. The Go case re-execs the test
/// binary for the network clients; `/usr/bin/nc` does the same job without a
/// helper-process gate.
#[cfg(test)]
mod contract {
    use std::path::Path;
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::DirectDriver;
    use crate::sandbox::conformance::{Contract, Fixture};
    use crate::sandbox::{Driver, Request};

    #[derive(Debug, Clone, Copy)]
    struct DirectContract;

    #[async_trait]
    impl Contract for DirectContract {
        async fn new_driver(&self, _fixture: &Fixture) -> Arc<dyn Driver> {
            Arc::new(DirectDriver::new())
        }

        fn request(&self, fixture: &Fixture, argv: Vec<String>) -> Request {
            Request {
                argv,
                dir: fixture.workspace.clone(),
                env: fixture.environment.clone(),
            }
        }

        fn shell_command(&self, script: &str) -> Vec<String> {
            vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
        }

        fn tcp_client(&self, host: &str, port: &str) -> Vec<String> {
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf contract | /usr/bin/nc -w 5 "$1" "$2""#.to_string(),
                "conformance".to_string(),
                host.to_string(),
                port.to_string(),
            ]
        }

        fn unix_client(&self, path: &Path) -> Vec<String> {
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                r#"printf contract | /usr/bin/nc -U "$1""#.to_string(),
                "conformance".to_string(),
                path.display().to_string(),
            ]
        }
    }

    crate::sandbox::conformance::driver_contract!(DirectContract);
}
