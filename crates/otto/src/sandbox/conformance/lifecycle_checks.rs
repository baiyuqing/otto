//! Checks for exits, environment, cancellation, close and concurrency.

use super::*;

// ---------------------------------------------------------------- the checks

pub(crate) async fn ordinary_and_nonzero_exits_with_separate_streams(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;

    let (out, _lines) = Recorder::new();
    let (err, _errlines) = Recorder::new();
    let mut stdout = out.clone();
    let mut stderr = err.clone();
    let cancel = CancellationToken::new();
    let (status, result) = executor
        .execute(
            shell_request(
                case,
                &fixture,
                "printf stdout-contract; printf stderr-contract >&2",
            ),
            Streams {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            &cancel,
        )
        .await;
    result.expect("Execute");
    assert_eq!(
        (status.code, status.signaled, status.signal.as_str()),
        (0, false, ""),
        "zero-exit status"
    );
    assert_eq!(out.text(), "stdout-contract");
    assert_eq!(err.text(), "stderr-contract");

    let (status, result, _) = run(&executor, shell_request(case, &fixture, "exit 37")).await;
    result.expect("nonzero Execute");
    assert_eq!(
        (status.code, status.signaled, status.signal.as_str()),
        (37, false, ""),
        "nonzero status"
    );
    executor.close().expect("close");
}

pub(crate) async fn supplied_environment_exactly_replaces_host_environment(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let request = case.request(&fixture, vec!["/usr/bin/env".to_string()]);
    let expected = request.env.clone();
    assert!(
        !expected.iter().any(|entry| entry.contains(['\r', '\n'])),
        "environment fixture contains a newline"
    );
    let (status, result, dump) = run(&executor, request).await;
    result.expect("Execute");
    assert_eq!(status.code, 0, "env exited nonzero");
    assert!(
        environment_dump_matches(&dump, &expected),
        "complete child environment differs from Request.env: {dump:?}"
    );
    executor.close().expect("close");
}

fn environment_dump_matches(dump: &str, expected: &[String]) -> bool {
    if expected.is_empty() {
        return dump.is_empty();
    }
    if expected.iter().any(|entry| entry.contains(['\r', '\n'])) {
        return false;
    }
    let Some(body) = dump.strip_suffix('\n') else {
        return false;
    };
    let mut actual: Vec<&str> = body.split('\n').collect();
    let mut wanted: Vec<&str> = expected.iter().map(String::as_str).collect();
    actual.sort_unstable();
    wanted.sort_unstable();
    actual == wanted
}

pub(crate) async fn pre_cancellation_does_not_start_a_child(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let marker = fixture.workspace.join("pre-cancel marker");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_MARKER", &marker);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut stdout = std::io::sink();
    let mut stderr = std::io::sink();
    let (_, result) = executor
        .execute(
            shell_request(
                case,
                &local,
                r#"printf started > "$SANDBOX_CONFORMANCE_MARKER""#,
            ),
            Streams {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            &cancel,
        )
        .await;
    assert_eq!(result, Err(Error::Cancelled));
    assert!(
        !marker.exists(),
        "pre-cancelled child created the marker file"
    );
    executor.close().expect("close");
}

pub(crate) async fn deadline_cancellation_removes_the_process_group(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let blocked = make_fifo(&fixture.workspace, "deadline-blocked");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_BLOCK_FIFO", &blocked);
    let (recorder, mut lines) = Recorder::new();
    let cancel = CancellationToken::new();
    let handle = spawn_execute(
        executor.clone(),
        shell_request(
            case,
            &local,
            r#"/bin/sh -c 'echo "$$"; exec /bin/cat "$SANDBOX_CONFORMANCE_BLOCK_FIFO"' descendant & wait"#,
        ),
        recorder,
        cancel.clone(),
    );
    let pid = parse_pid(&await_line(&mut lines, "deadline descendant PID").await);
    cancel.cancel();
    let (status, result) = await_execution(handle, "deadline cancellation").await;
    assert_eq!(result, Err(Error::Cancelled));
    assert_signaled(&status);
    assert_gone(pid).await;
    executor.close().expect("close");
}

pub(crate) async fn an_ignored_term_escalates_to_kill_and_removes_the_group(case: &dyn Contract) {
    const GRACE: Duration = Duration::from_millis(100);
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let (recorder, mut lines) = Recorder::new();
    let cancel = CancellationToken::new();
    let handle = {
        let executor = executor.clone();
        let cancel = cancel.clone();
        let request = shell_request(
            case,
            &fixture,
            r#"trap '' TERM; echo "$$"; while :; do sleep 1; done"#,
        );
        tokio::spawn(async move {
            let mut stdout = recorder;
            let mut stderr = std::io::sink();
            executor
                .execute_with_grace(
                    request,
                    Streams {
                        stdout: &mut stdout,
                        stderr: &mut stderr,
                    },
                    &cancel,
                    GRACE,
                )
                .await
        })
    };
    // The PID is printed after the trap is installed.
    let pid = parse_pid(&await_line(&mut lines, "TERM-ignoring leader PID").await);
    let cancelled_at = Instant::now();
    cancel.cancel();
    let (status, result) = await_execution(handle, "cancellation of a TERM-ignoring child").await;
    assert!(
        cancelled_at.elapsed() >= GRACE,
        "SIGKILL preceded the {GRACE:?} grace"
    );
    assert_eq!(result, Err(Error::Cancelled));
    assert_signaled(&status);
    assert_gone(pid).await;
    executor.close().expect("close");
}

pub(crate) async fn normal_leader_exit_removes_a_background_process(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let blocked = make_fifo(&fixture.workspace, "normal-exit-blocked");
    let release = make_fifo(&fixture.workspace, "normal-exit-release");
    let local = fixture
        .with_environment("SANDBOX_CONFORMANCE_BLOCK_FIFO", &blocked)
        .with_environment("SANDBOX_CONFORMANCE_RELEASE_FIFO", &release);
    let (recorder, mut lines) = Recorder::new();
    let handle = spawn_execute(
        executor.clone(),
        shell_request(
            case,
            &local,
            r#"/bin/sh -c 'echo "$$"; exec /bin/cat "$SANDBOX_CONFORMANCE_BLOCK_FIFO"' descendant & IFS= read -r release < "$SANDBOX_CONFORMANCE_RELEASE_FIFO"; exit 0"#,
        ),
        recorder,
        CancellationToken::new(),
    );
    let descendant = parse_pid(&await_line(&mut lines, "background descendant PID").await);
    write_fifo(&release, "continue\n").await;
    let (status, result) = await_execution(handle, "normal leader exit").await;
    result.expect("Execute");
    assert_eq!((status.code, status.signaled), (0, false), "leader status");
    assert_gone(descendant).await;
    executor.close().expect("close");
}

pub(crate) async fn driver_close_drains_active_work_and_is_idempotent(case: &dyn Contract) {
    const CLOSERS: usize = 12;
    let (fixture, _) = setup(case).await;
    let base = case.new_driver(&fixture).await;
    let driver_work_release = Arc::new(Gate::default());
    let observer = Arc::new(CloseDrainObserver {
        inner: base.clone(),
        entered: Counter::default(),
        returned: Counter::default(),
        observations: Mutex::new(Vec::new()),
        driver_work_released: driver_work_release.clone(),
    });
    let executor = Arc::new(
        Executor::new(observer.clone(), fixture.policy, &fixture.workspace).expect("executor"),
    );

    let blocked = make_fifo(&fixture.workspace, "close-blocked");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_BLOCK_FIFO", &blocked);
    let (recorder, mut lines) = Recorder::new();
    let entered_stream_work = Arc::new(Gate::default());
    let barrier = BarrierRecorder {
        recorder,
        entered: entered_stream_work.clone(),
        release: driver_work_release.clone(),
    };
    let (publish, publication) = tokio::sync::oneshot::channel::<()>();
    let awaiting = Arc::new(Gate::default());
    let handle = {
        let executor = executor.clone();
        let request = shell_request(
            case,
            &local,
            r#"echo "$$"; exec /bin/cat "$SANDBOX_CONFORMANCE_BLOCK_FIFO""#,
        );
        let awaiting = awaiting.clone();
        let mut stdout = barrier;
        tokio::spawn(async move {
            let mut stderr = std::io::sink();
            let cancel = CancellationToken::new();
            let completed = executor
                .execute(
                    request,
                    Streams {
                        stdout: &mut stdout,
                        stderr: &mut stderr,
                    },
                    &cancel,
                )
                .await;
            awaiting.release();
            let _ = publication.await;
            completed
        })
    };

    let pid = parse_pid(&await_line(&mut lines, "close leader PID").await);
    await_gate(&entered_stream_work, "blocked driver-owned stream work").await;

    let mut closers = Vec::with_capacity(CLOSERS);
    for _ in 0..CLOSERS {
        let driver: Arc<dyn Driver> = observer.clone();
        closers.push(std::thread::spawn(move || driver.close()));
    }
    observer
        .entered
        .wait_for(CLOSERS, "Driver::close observation delegate entry");

    driver_work_release.release();
    observer.returned.wait_for(CLOSERS, "Driver::close returns");
    for closer in closers {
        closer
            .join()
            .expect("close thread")
            .expect("concurrent Driver::close");
    }
    for observation in observer.observations.lock().expect("observations").iter() {
        assert!(
            !observation.returned_while_blocked && observation.error.is_none(),
            "Driver::close observation = {observation:?}"
        );
    }
    assert_gone(pid).await;
    await_gate(&awaiting, "external Execute result-publication barrier").await;

    let _ = publish.send(());
    let (status, result) = await_execution(handle, "active Execute result publication").await;
    result.expect("active Execute");
    assert_signaled(&status);
    observer.close().expect("idempotent Driver::close");

    let (_, result, _) = run(&executor, shell_request(case, &local, "exit 0")).await;
    assert_eq!(result, Err(Error::Closed));
    base.close().expect("base close");
}

pub(crate) async fn executor_clones_requests_before_delegation(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let release = make_fifo(&fixture.workspace, "clone-release");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_RELEASE_FIFO", &release);
    let request = shell_request(
        case,
        &local,
        r#"printf '%s\n' "$$"; IFS= read -r release < "$SANDBOX_CONFORMANCE_RELEASE_FIFO"; printf 'clone:%s' "$SANDBOX_CONFORMANCE_VISIBLE""#,
    );
    // The caller keeps a copy and mutates it while the execution runs; the
    // child must still observe the values it was started with.
    let mut caller_copy = request.clone();
    let (recorder, mut lines) = Recorder::new();
    let handle = spawn_execute(
        executor.clone(),
        request,
        recorder.clone(),
        CancellationToken::new(),
    );
    let _ = parse_pid(&await_line(&mut lines, "request clone leader PID").await);
    for argument in &mut caller_copy.argv {
        *argument = "mutated-argument".to_string();
    }
    for entry in &mut caller_copy.env {
        *entry = "MUTATED_ENVIRONMENT=value".to_string();
    }
    write_fifo(&release, "continue\n").await;
    let (status, result) = await_execution(handle, "request clone execution").await;
    result.expect("Execute");
    assert_eq!(status.code, 0, "clone execution status");
    assert!(
        recorder.text().contains("clone:visible-value"),
        "cloned request output = {:?}",
        recorder.text()
    );
    executor.close().expect("close");
}
