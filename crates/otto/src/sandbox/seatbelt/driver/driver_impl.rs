//! The `Driver` trait implementation: execution, grace handling and close.

use super::*;

#[async_trait]
impl Driver for SeatbeltDriver {
    fn id(&self) -> DriverId {
        DriverId::new(super::super::ID)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read_confinement: true,
            write_confinement: true,
            unix_socket_deny: true,
            network_allow: self.network() == Some(NetworkMode::Allow),
            network_deny: self.network() == Some(NetworkMode::Deny),
        }
    }

    async fn execute(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>) {
        self.execute_with_grace(request, streams, cancel, DEFAULT_CANCELLATION_GRACE)
            .await
    }

    async fn execute_with_grace(
        &self,
        request: Request,
        streams: Streams<'_>,
        cancel: &CancellationToken,
        cancellation_grace: Duration,
    ) -> (ExitStatus, Result<(), Error>) {
        if !self.valid_request(&request) {
            return (ExitStatus::default(), Err(Error::InvalidRequest));
        }
        if let Err(error) = check_cancelled(cancel) {
            return (ExitStatus::default(), Err(error));
        }
        {
            let inner = self.inner.lock().expect("seatbelt driver state");
            if inner.closed {
                return (ExitStatus::default(), Err(Error::Closed));
            }
            if inner.poisoned {
                return (
                    ExitStatus::default(),
                    Err(Error::unavailable(UnavailableReason::RuntimeFailure)),
                );
            }
        }
        if let Err(error) = check_cancelled(cancel) {
            return (ExitStatus::default(), Err(error));
        }

        let report_denials = request.report_denials;
        let started = Instant::now();
        let spec = Spec {
            stdin: request.stdin,
            path: SANDBOX_EXEC_PATH.to_string(),
            args: self.sandbox_exec_args(request.argv.into_iter()),
            directory: request.dir,
            environment: request.env,
        };
        let latch = || self.poison();
        let mut stderr = StderrFilter::new(streams.stderr, Some(&latch));
        let (outcome, result) = self
            .processes
            .run_with_grace(
                spec,
                Streams {
                    stdout: streams.stdout,
                    stderr: &mut stderr,
                },
                cancel,
                cancellation_grace,
            )
            .await;
        let finish = stderr.finish();
        let filter_failure = stderr.infrastructure_failure();
        drop(stderr);

        let infrastructure = filter_failure
            || matches!(&result, Err(error) if infrastructure_error(error))
            || finish.is_err();
        if infrastructure {
            self.poison();
        }
        let mut status = ExitStatus {
            code: outcome.code,
            signaled: outcome.signaled,
            signal: outcome.signal,
            ..Default::default()
        };
        if check_cancelled(cancel).is_err() {
            return (status, Err(Error::Cancelled));
        }
        if infrastructure {
            return (
                ExitStatus::default(),
                Err(Error::unavailable(UnavailableReason::RuntimeFailure)),
            );
        }
        if report_denials
            && result.is_ok()
            && (status.code != 0 || status.signaled)
            && let Some(monitor) = self.monitor.as_ref().filter(|monitor| monitor.running())
        {
            // Log delivery lags the denied syscall.
            tokio::time::sleep(DENIAL_DELIVERY_WAIT).await;
            (status.denials, status.denials_omitted) =
                violations::summarize(monitor.since(started));
        }
        (status, bounded_execution_error(result))
    }

    fn close(&self) -> Result<(), Error> {
        let mut inner = self.inner.lock().expect("seatbelt driver state");
        if inner.closed {
            while !inner.close_done {
                inner = self.progress.wait(inner).expect("seatbelt driver state");
            }
            return inner.close_result.clone().unwrap_or(Ok(()));
        }
        inner.closed = true;
        drop(inner);

        self.close_monitor();
        let manager = self.processes.close();
        let cleanup = self.state.close();
        let result = match (manager, cleanup) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(_)) => Err(Error::unavailable(UnavailableReason::RuntimeFailure)),
            (Ok(()), Ok(())) => Ok(()),
        };

        let mut inner = self.inner.lock().expect("seatbelt driver state");
        inner.close_result = Some(result.clone());
        inner.close_done = true;
        drop(inner);
        self.progress.notify_all();
        result
    }
}
