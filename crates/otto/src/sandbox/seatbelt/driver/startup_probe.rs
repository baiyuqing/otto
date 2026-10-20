//! The startup self-test and its probes.

use super::*;

impl SeatbeltDriver {
    /// Proves the profile allows what it promises and denies what it forbids.
    ///
    /// Five probes run under the real profile: the profile loads at all, a file
    /// under the private home reads back, two writable fixtures accept a write
    /// through a descriptor they re-identify by device, inode, type, owner,
    /// permissions and link count, and a file under the profile directory is
    /// refused for both reading and writing.
    pub(super) async fn run_startup_self_test(
        &self,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        const ALLOWED_READ: &str = "otto-seatbelt-allowed-read";
        const ALLOWED_WRITE: &str = "otto-seatbelt-allowed-write";
        const DENIED: &str = "otto-seatbelt-denied-fixture";

        let deadline = cancel.child_token();
        let timer = {
            let token = deadline.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SELF_TEST_TIMEOUT).await;
                token.cancel();
            })
        };
        let home = self.state.directories.home.to_string_lossy().into_owned();
        let temp = self.state.directories.temp.to_string_lossy().into_owned();
        let mut fixtures = selftest::Fixtures::prepare(
            &home,
            &self.workspace,
            &temp,
            &self.state.profiles,
            ALLOWED_READ,
            DENIED,
        )
        .map_err(|_| Error::unavailable(UnavailableReason::SelfTestFailed))?;

        let probes = self
            .self_test_probes(
                &fixtures,
                cancel,
                &deadline,
                ALLOWED_READ,
                ALLOWED_WRITE,
                DENIED,
            )
            .await;
        let cleanup = fixtures.cleanup();
        timer.abort();
        if probes.is_err() || cleanup.is_err() {
            return probes.and(Err(Error::unavailable(UnavailableReason::SelfTestFailed)));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn self_test_probes(
        &self,
        fixtures: &selftest::Fixtures,
        caller: &CancellationToken,
        deadline: &CancellationToken,
        allowed_read: &str,
        allowed_write: &str,
        denied: &str,
    ) -> Result<(), Error> {
        let unavailable = || Error::unavailable(UnavailableReason::SelfTestFailed);
        let read_fixture = fixtures
            .get(selftest::Kind::AllowedRead)
            .map_err(|_| unavailable())?;
        let denied_read = fixtures
            .get(selftest::Kind::DeniedRead)
            .map_err(|_| unavailable())?;
        let denied_write = fixtures
            .get(selftest::Kind::DeniedWrite)
            .map_err(|_| unavailable())?;

        let (outcome, stdout) = self
            .run_probe(caller, deadline, &["/bin/sh", "-c", "exit 0"], None)
            .await?;
        if !successful(&outcome) || !stdout.is_empty() {
            return Err(unavailable());
        }

        let (outcome, stdout) = self
            .run_probe(caller, deadline, &["/bin/cat", read_fixture.path()], None)
            .await?;
        if !successful(&outcome)
            || stdout != allowed_read.as_bytes()
            || read_fixture.validate_contents(allowed_read).is_err()
        {
            return Err(unavailable());
        }

        let arguments = fixtures
            .write_probe_arguments()
            .map_err(|_| unavailable())?;
        let mut write_probe: Vec<&str> = vec![
            "/usr/bin/perl",
            "-MFcntl=O_WRONLY,O_NOFOLLOW",
            "-e",
            WRITE_PROBE_SCRIPT,
            allowed_write,
        ];
        write_probe.extend(arguments.iter().map(String::as_str));
        let (outcome, stdout) = self
            .run_probe(caller, deadline, &write_probe, Some(fixtures))
            .await?;
        if !successful(&outcome) || !stdout.is_empty() {
            return Err(unavailable());
        }
        if fixtures.validate_written_contents(allowed_write).is_err() {
            return Err(unavailable());
        }

        let (outcome, stdout) = self
            .run_probe(caller, deadline, &["/bin/cat", denied_read.path()], None)
            .await?;
        if !denied_result(&outcome)
            || !stdout.is_empty()
            || denied_read.validate_contents(denied).is_err()
        {
            return Err(unavailable());
        }

        let (outcome, stdout) = self
            .run_probe(
                caller,
                deadline,
                &[
                    "/bin/sh",
                    "-c",
                    r#"printf changed > "$1""#,
                    "probe",
                    denied_write.path(),
                ],
                None,
            )
            .await?;
        if !denied_result(&outcome)
            || !stdout.is_empty()
            || denied_write.validate_contents(denied).is_err()
        {
            return Err(unavailable());
        }
        Ok(())
    }

    /// Runs one probe under the real profile with a fixed, minimal environment.
    ///
    /// `revalidate` is re-checked immediately before dispatch, closing the
    /// window between building the probe's arguments and the child starting.
    async fn run_probe(
        &self,
        caller: &CancellationToken,
        deadline: &CancellationToken,
        argv: &[&str],
        revalidate: Option<&selftest::Fixtures>,
    ) -> Result<(Outcome, Vec<u8>), Error> {
        let unavailable = || Error::unavailable(UnavailableReason::SelfTestFailed);
        check_cancelled(caller)?;
        check_cancelled(deadline)?;

        let mut stdout = BoundedCollector::new(SELF_TEST_OUTPUT_LIMIT);
        let mut ordinary = BoundedCollector::new(SELF_TEST_OUTPUT_LIMIT);
        let mut stderr = StderrFilter::new(&mut ordinary, None);

        let home = self.state.directories.home.to_string_lossy().into_owned();
        let temp = self.state.directories.temp.to_string_lossy().into_owned();
        let spec = Spec {
            stdin: None,
            path: SANDBOX_EXEC_PATH.to_string(),
            args: self.sandbox_exec_args(argv.iter().map(|value| (*value).to_string())),
            directory: std::path::PathBuf::from(&self.workspace),
            environment: vec![
                "PATH=/usr/bin:/bin".to_string(),
                format!("HOME={home}"),
                format!("TMPDIR={temp}"),
                "LC_ALL=C".to_string(),
            ],
        };
        if let Some(fixtures) = revalidate {
            fixtures
                .validate_before_write_dispatch()
                .map_err(|_| unavailable())?;
            check_cancelled(caller)?;
            check_cancelled(deadline)?;
        }

        let (outcome, result) = self
            .processes
            .run(
                spec,
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                deadline,
            )
            .await;
        let finish = stderr.finish();
        let infrastructure = stderr.infrastructure_failure();
        if result.is_err()
            || finish.is_err()
            || infrastructure
            || stdout.overflowed()
            || ordinary.overflowed()
        {
            return Err(unavailable());
        }
        Ok((outcome, stdout.into_bytes()))
    }
}
