
use super::*;

fn spec(path: &str, args: &[&str], directory: &std::path::Path, env: &[&str]) -> Spec {
    Spec {
        stdin: None,
        path: path.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        directory: directory.to_path_buf(),
        environment: env.iter().map(|entry| (*entry).to_string()).collect(),
    }
}

async fn run(spec: Spec) -> (Outcome, Result<(), Error>, String) {
    let manager = Manager::default();
    let mut stdout = Vec::new();
    let mut stderr = std::io::sink();
    let cancel = CancellationToken::new();
    let (outcome, result) = manager
        .run(
            spec,
            Streams {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            &cancel,
        )
        .await;
    manager.close().expect("close");
    (
        outcome,
        result,
        String::from_utf8_lossy(&stdout).into_owned(),
    )
}

#[test]
fn a_slashed_path_is_used_verbatim_and_a_bare_name_needs_a_request_path() {
    let directory = std::path::Path::new("/");
    assert_eq!(
        resolve_executable(&spec("/bin/echo", &[], directory, &[])),
        Ok(PathBuf::from("/bin/echo"))
    );
    assert_eq!(
        resolve_executable(&spec("./echo", &[], directory, &[])),
        Ok(PathBuf::from("./echo"))
    );
    // Without `PATH` in the request there is nowhere to look: the host's
    // own `PATH` is never consulted.
    assert_eq!(
        resolve_executable(&spec("echo", &[], directory, &["LC_ALL=C"])),
        Err(Error::ChildLaunch)
    );
}

#[test]
fn a_bare_name_resolves_only_through_the_requests_path() {
    let directory = tempfile::tempdir().expect("temp dir");
    let executable = directory.path().join("tool");
    std::fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("write tool");
    std::fs::set_permissions(
        &executable,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("chmod tool");
    std::fs::create_dir(directory.path().join("tool directory")).expect("directory candidate");

    let listed = format!("PATH={}", directory.path().display());
    assert_eq!(
        resolve_executable(&spec("tool", &[], directory.path(), &[&listed])),
        Ok(executable.clone())
    );
    // A relative entry joins against the request's own directory, and an
    // empty entry means that directory too.
    assert_eq!(
        resolve_executable(&spec("tool", &[], directory.path(), &["PATH="])),
        Ok(executable.clone())
    );
    assert_eq!(
        resolve_executable(&spec("tool", &[], directory.path(), &["PATH=."])),
        Ok(executable)
    );
    // A directory named like the executable is skipped, not launched.
    assert_eq!(
        resolve_executable(&spec("tool directory", &[], directory.path(), &["PATH=."])),
        Err(Error::ChildLaunch)
    );
    // A readable but non-executable candidate is skipped.
    let plain = directory.path().join("plain");
    std::fs::write(&plain, "data").expect("write plain");
    std::fs::set_permissions(&plain, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("chmod plain");
    assert_eq!(
        resolve_executable(&spec("plain", &[], directory.path(), &["PATH=."])),
        Err(Error::ChildLaunch)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signalled_child_reports_the_darwin_signal_description() {
    let (outcome, result, _) = run(spec(
        "/bin/sh",
        &["-c", "kill -TERM $$"],
        std::path::Path::new("/"),
        &["PATH=/usr/bin:/bin"],
    ))
    .await;
    result.expect("run");
    assert_eq!(
        outcome,
        Outcome {
            code: -1,
            signaled: true,
            signal: "terminated".to_string(),
        }
    );

    let (outcome, result, _) = run(spec(
        "/bin/sh",
        &["-c", "kill -KILL $$"],
        std::path::Path::new("/"),
        &["PATH=/usr/bin:/bin"],
    ))
    .await;
    result.expect("run");
    assert_eq!(outcome.signal, "killed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_child_sees_only_the_request_environment_argv0_and_a_closed_stdin() {
    let (outcome, result, stdout) = run(spec(
        "/bin/sh",
        &[
            "-c",
            r#"printf '%s|%s|%s|' "$0" "$ONLY" "${HOME-unset}"; cat; printf 'eof'"#,
        ],
        std::path::Path::new("/"),
        &["ONLY=value"],
    ))
    .await;
    result.expect("run");
    assert_eq!(outcome.code, 0);
    assert_eq!(stdout, "/bin/sh|value|unset|eof");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backgrounded_descendant_is_killed_with_the_group() {
    let directory = tempfile::tempdir().expect("temp dir");
    let marker = directory.path().join("marker");
    let script = format!(
        "( sleep 5; printf escaped > '{}' ) >/dev/null 2>&1 & exit 0",
        marker.display()
    );
    let (outcome, result, _) = run(spec(
        "/bin/sh",
        &["-c", &script],
        directory.path(),
        &["PATH=/usr/bin:/bin"],
    ))
    .await;
    result.expect("run");
    assert_eq!(outcome.code, 0);
    // `close` returned, so the group was signalled and observed gone. The
    // descendant never reached its write.
    assert!(!marker.exists(), "backgrounded descendant survived");
}

/// A shell script that runs `trap`, then creates `ready`, then loops; the
/// TERM disposition is in place once `ready` exists. Signalling earlier
/// races the shell's startup and kills it with the default TERM action.
fn trapping_loop(trap: &str, ready: &std::path::Path) -> String {
    format!(
        "trap {trap} TERM; : > '{}'; while :; do sleep 1; done",
        ready.display()
    )
}

async fn wait_ready(ready: &std::path::Path) {
    for _ in 0..5_000 {
        if ready.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("child did not install its TERM handler");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_allows_a_cooperative_term_exit() {
    let manager = Manager::default();
    let mut stdout = std::io::sink();
    let mut stderr = std::io::sink();
    let directory = tempfile::tempdir().expect("temp dir");
    let ready = directory.path().join("ready");
    let command = trapping_loop("'exit 23'", &ready);
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        wait_ready(&ready).await;
        token.cancel();
    });
    let (outcome, result) = manager
        .run(
            spec(
                "/bin/sh",
                &["-c", &command],
                std::path::Path::new("/"),
                &["PATH=/usr/bin:/bin"],
            ),
            Streams {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            &cancel,
        )
        .await;
    assert_eq!(result, Err(Error::Cancelled));
    assert_eq!(outcome.code, 23);
    assert!(!outcome.signaled);
    manager.close().expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_escalates_an_ignored_term_to_kill() {
    let manager = Manager::default();
    let mut stdout = std::io::sink();
    let mut stderr = std::io::sink();
    let directory = tempfile::tempdir().expect("temp dir");
    let ready = directory.path().join("ready");
    let command = trapping_loop("''", &ready);
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        wait_ready(&ready).await;
        token.cancel();
    });
    let (outcome, result) = manager
        .run(
            spec(
                "/bin/sh",
                &["-c", &command],
                std::path::Path::new("/"),
                &["PATH=/usr/bin:/bin"],
            ),
            Streams {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            &cancel,
        )
        .await;
    assert_eq!(result, Err(Error::Cancelled));
    assert!(outcome.signaled);
    assert_eq!(outcome.signal, "killed");
    manager.close().expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_cancellation_grace_escalates_before_the_default() {
    let directory = tempfile::tempdir().expect("temp dir");
    let ready = directory.path().join("ready");
    let command = trapping_loop("''", &ready);
    let manager = Arc::new(Manager::default());
    let runner = manager.clone();
    let cancel = CancellationToken::new();
    let child_cancel = cancel.clone();
    let run = tokio::spawn(async move {
        let mut stdout = std::io::sink();
        let mut stderr = std::io::sink();
        runner
            .run_with_grace(
                spec(
                    "/bin/sh",
                    &["-c", &command],
                    directory.path(),
                    &["PATH=/usr/bin:/bin"],
                ),
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                &child_cancel,
                Duration::from_millis(50),
            )
            .await
    });

    wait_ready(&ready).await;

    let cancelled_at = std::time::Instant::now();
    cancel.cancel();
    let (outcome, result) = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("custom grace should kill before the default five seconds")
        .expect("run task");
    assert!(
        cancelled_at.elapsed() >= Duration::from_millis(40),
        "SIGKILL preceded the configured grace"
    );
    assert_eq!(result, Err(Error::Cancelled));
    assert!(outcome.signaled);
    assert_eq!(outcome.signal, "killed");
    manager.close().expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_close_cooperatively_cleans_up_once() {
    let directory = tempfile::tempdir().expect("temp dir");
    let ready = directory.path().join("ready");
    let command = trapping_loop("'exit 24'", &ready);
    let manager = Arc::new(Manager::default());
    let runner = manager.clone();
    let run = tokio::spawn(async move {
        let mut stdout = std::io::sink();
        let mut stderr = std::io::sink();
        let cancel = CancellationToken::new();
        runner
            .run(
                spec(
                    "/bin/sh",
                    &["-c", &command],
                    std::path::Path::new("/"),
                    &["PATH=/usr/bin:/bin"],
                ),
                Streams {
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                },
                &cancel,
            )
            .await
    });

    wait_ready(&ready).await;
    let first = {
        let manager = manager.clone();
        std::thread::spawn(move || manager.close())
    };
    let second = {
        let manager = manager.clone();
        std::thread::spawn(move || manager.close())
    };
    assert_eq!(first.join().expect("first close"), Ok(()));
    assert_eq!(second.join().expect("second close"), Ok(()));

    let (outcome, result) = run.await.expect("run task");
    result.expect("run result");
    assert_eq!(outcome.code, 24);
    assert!(!outcome.signaled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_manager_refuses_later_runs() {
    let manager = Manager::default();
    manager.close().expect("close");
    manager.close().expect("second close");
    let mut stdout = std::io::sink();
    let mut stderr = std::io::sink();
    let cancel = CancellationToken::new();
    let (outcome, result) = manager
        .run(
            spec(
                "/bin/sh",
                &["-c", "exit 0"],
                std::path::Path::new("/"),
                &["PATH=/usr/bin:/bin"],
            ),
            Streams {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
            &cancel,
        )
        .await;
    assert_eq!((outcome, result), (Outcome::default(), Err(Error::Closed)));
}
