//! Tests for cancellation, timeouts, errors, constructor limits and real execution.

use super::*;

#[tokio::test]
async fn invalid_arguments_and_pre_cancellation_never_execute() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::default());
    let tool = bash(&workspace, executor.clone(), &[], 1024, &[]);
    let cases: &[(&str, &str)] = &[
        (
            r#"{"command":"true","extra":true}"#,
            r#"json: unknown field "extra""#,
        ),
        ("{}", "missing required argument: command"),
        (
            r#"{"command":" \t "}"#,
            "missing required argument: command",
        ),
    ];
    for (arguments, want) in cases {
        let result = tool
            .execute(&raw(arguments), &CancellationToken::new())
            .await;
        assert!(result.is_error, "{arguments}: {result:?}");
        assert!(result.content.contains(want), "{arguments}: {result:?}");
    }
    assert_eq!(executor.calls(), 0);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = tool
        .execute(&raw(r#"{"command":"must not run"}"#), &cancel)
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.content, "stdout:\n\nstderr:\n\nstatus: cancelled");
    assert_eq!(
        outcome(&result).effect_certainty,
        otto_core::model::EffectCertainty::NotStarted
    );
    assert_eq!(
        outcome(&result).disposition,
        otto_core::model::OperationDisposition::Cancelled
    );
    assert_eq!(executor.calls(), 0);
}

#[tokio::test]
async fn caller_cancellation_reports_partial_output_and_the_signal() {
    let (_dir, workspace) = temp_workspace();
    let started = Arc::new(tokio::sync::Notify::new());
    let observed = started.clone();
    let executor = Arc::new(FakeExecutor::behaving(Box::new(
        move |_, streams, cancel| {
            let started = started.clone();
            Box::pin(async move {
                started.notify_one();
                cancel.cancelled().await;
                streams.stdout.write_all(b"partial output").unwrap();
                (
                    ExitStatus {
                        code: -1,
                        signaled: true,
                        signal: "killed".to_owned(),
                        ..Default::default()
                    },
                    Err(Error::Cancelled),
                )
            })
        },
    )));
    let tool = bash(
        &workspace,
        executor,
        &[],
        1024,
        &["longer-than-partial-output-secret"],
    );
    let cancel = CancellationToken::new();
    let call = {
        let cancel = cancel.clone();
        async move { tool.execute(&raw(r#"{"command":"wait"}"#), &cancel).await }
    };
    let waiter = async {
        observed.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(call, waiter);
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        result.content,
        "stdout:\npartial output\nstderr:\n\nstatus: cancelled; signal: killed"
    );
    assert_eq!(
        outcome(&result).effect_certainty,
        otto_core::model::EffectCertainty::Unknown
    );
}

#[tokio::test(start_paused = true)]
async fn the_timeout_is_reported_with_the_go_duration_text() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::behaving(Box::new(|_, _, cancel| {
        Box::pin(async move {
            cancel.cancelled().await;
            (
                ExitStatus {
                    code: -1,
                    signaled: true,
                    signal: "killed".to_owned(),
                    ..Default::default()
                },
                Err(Error::Cancelled),
            )
        })
    })));
    let tool = bash(&workspace, executor, &[], 1024, &[]);
    let result = run(&tool, "wait").await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        result.content,
        "stdout:\n\nstderr:\n\nstatus: timed out after 3s; signal: killed"
    );
    assert_eq!(
        outcome(&result).disposition,
        otto_core::model::OperationDisposition::DeadlineExceeded
    );
    assert_eq!(
        outcome(&result).effect_certainty,
        otto_core::model::EffectCertainty::Unknown
    );
}

#[tokio::test(start_paused = true)]
async fn a_fired_timeout_survives_a_clean_executor_return() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::behaving(Box::new(|_, _, cancel| {
        Box::pin(async move {
            cancel.cancelled().await;
            (
                ExitStatus {
                    code: -1,
                    signaled: true,
                    signal: "killed".to_owned(),
                    ..Default::default()
                },
                Ok(()),
            )
        })
    })));
    let tool = bash(&workspace, executor, &[], 1024, &[]);
    let result = run(&tool, "wait").await;
    assert!(
        result
            .content
            .contains("status: timed out after 3s; signal: killed"),
        "{result:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn parent_cancellation_wins_the_timeout_race() {
    let (_dir, workspace) = temp_workspace();
    let observed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let seen = observed.clone();
    let gate = release.clone();
    let executor = Arc::new(FakeExecutor::behaving(Box::new(move |_, _, cancel| {
        let observed = observed.clone();
        let release = release.clone();
        Box::pin(async move {
            cancel.cancelled().await;
            observed.notify_one();
            release.notified().await;
            (
                ExitStatus {
                    code: -1,
                    signaled: true,
                    signal: "killed".to_owned(),
                    ..Default::default()
                },
                Err(Error::Cancelled),
            )
        })
    })));
    let tool = bash(&workspace, executor, &[], 1024, &[]);
    let cancel = CancellationToken::new();
    let call = {
        let cancel = cancel.clone();
        async move { tool.execute(&raw(r#"{"command":"wait"}"#), &cancel).await }
    };
    let driver = async {
        seen.notified().await;
        cancel.cancel();
        gate.notify_one();
    };
    let (result, ()) = tokio::join!(call, driver);
    assert!(
        result.content.contains("status: cancelled; signal: killed"),
        "{result:?}"
    );
    assert!(!result.content.contains("timed out"), "{result:?}");
}

#[tokio::test(start_paused = true)]
async fn a_successful_return_does_not_report_the_timeout() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::default());
    let tool = bash(&workspace, executor, &[], 1024, &[]);
    let result = run(&tool, "true").await;
    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains("exit_code: 0"), "{result:?}");
    assert!(!result.content.contains("timed out"), "{result:?}");
}

#[tokio::test]
async fn infrastructure_errors_are_fixed_and_discard_diagnostics() {
    let (_dir, workspace) = temp_workspace();
    const SECRET: &str = "infrastructure-secret-value";
    let errors = [
        Error::ChildLaunch,
        Error::ChildWait,
        Error::ChildTerminate,
        Error::Closed,
        Error::Unavailable(UnavailableReason::RuntimeFailure),
        Error::InvalidRequest,
        Error::EnvironmentUnsafe,
        Error::UnsupportedPolicy,
    ];
    for error in errors {
        let executor = Arc::new(FakeExecutor {
            stdout_chunks: vec![b"diagnostic-".to_vec(), SECRET.as_bytes().to_vec()],
            stderr_chunks: vec![format!("raw stderr {SECRET}").into_bytes()],
            error: Some(error.clone()),
            ..FakeExecutor::default()
        });
        let tool = bash(&workspace, executor, &[], 8, &[SECRET]);
        let result = run(&tool, "ignored").await;
        assert!(result.is_error, "{error:?}: {result:?}");
        assert_eq!(result.content, SANDBOX_EXECUTION_UNAVAILABLE, "{error:?}");
        let expected = match error {
            Error::ChildWait | Error::ChildTerminate => otto_core::model::EffectCertainty::Unknown,
            _ => otto_core::model::EffectCertainty::NotStarted,
        };
        assert_eq!(outcome(&result).effect_certainty, expected);
    }
}

#[tokio::test]
async fn an_infrastructure_error_wins_an_ended_context() {
    let (_dir, workspace) = temp_workspace();
    const SECRET: &str = "infrastructure-secret-value";
    let started = Arc::new(tokio::sync::Notify::new());
    let observed = started.clone();
    let executor = Arc::new(FakeExecutor::behaving(Box::new(
        move |_, streams, cancel| {
            let started = started.clone();
            Box::pin(async move {
                started.notify_one();
                cancel.cancelled().await;
                streams.stdout.write_all(SECRET.as_bytes()).unwrap();
                (
                    ExitStatus {
                        code: -1,
                        signaled: true,
                        signal: "killed".to_owned(),
                        ..Default::default()
                    },
                    Err(Error::ChildTerminate),
                )
            })
        },
    )));
    let tool = bash(&workspace, executor, &[], 1024, &[SECRET]);
    let cancel = CancellationToken::new();
    let call = {
        let cancel = cancel.clone();
        async move { tool.execute(&raw(r#"{"command":"wait"}"#), &cancel).await }
    };
    let driver = async {
        observed.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(call, driver);
    assert!(result.is_error, "{result:?}");
    assert_eq!(result.content, SANDBOX_EXECUTION_UNAVAILABLE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_calls_keep_output_and_redaction_independent() {
    let (_dir, workspace) = temp_workspace();
    let arrived = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let signal = arrived.clone();
    let gate = release.clone();
    let executor = Arc::new(FakeExecutor::behaving(Box::new(
        move |request, streams, _| {
            let arrived = arrived.clone();
            let release = release.clone();
            let name = request.argv[2].clone();
            Box::pin(async move {
                streams
                    .stdout
                    .write_all(format!("{name}:shared-").as_bytes())
                    .unwrap();
                // Register before publishing readiness: notify_waiters does not
                // retain a wakeup for a future that has not registered yet.
                let notified = release.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                arrived.add_permits(1);
                notified.await;
                streams.stdout.write_all(b"secret").unwrap();
                streams
                    .stderr
                    .write_all(format!("stderr-{name}").as_bytes())
                    .unwrap();
                (ExitStatus::default(), Ok(()))
            })
        },
    )));
    let tool = Arc::new(bash(&workspace, executor, &[], 1024, &["shared-secret"]));

    let mut handles = Vec::new();
    for name in ["first", "second"] {
        let tool = tool.clone();
        handles.push(tokio::spawn(async move { (name, run(&tool, name).await) }));
    }
    let _permits = signal.acquire_many(2).await.expect("both calls arrive");
    gate.notify_waiters();

    for handle in handles {
        let (name, result) = handle.await.expect("the call completes");
        let other = if name == "first" { "second" } else { "first" };
        assert!(!result.is_error, "{name}: {result:?}");
        assert!(
            result
                .content
                .contains(&format!("{name}:{}", tool.redaction_marker)),
            "{name}: {result:?}"
        );
        assert!(
            result.content.contains(&format!("stderr-{name}")),
            "{name}: {result:?}"
        );
        assert!(
            !result.content.contains(&format!("{other}:")),
            "{name}: {result:?}"
        );
        assert!(
            !result.content.contains("shared-secret"),
            "{name}: {result:?}"
        );
    }
}

/// Which shell invocation each platform gets. macOS runs a login shell so
/// the command sees the `PATH` `path_helper` builds; nowhere else, where
/// `/etc/profile.d/*` would write into the tool result the model reads
/// and could restore variables the environment filter removed.
#[test]
fn only_macos_starts_the_command_shell_as_a_login_shell() {
    if cfg!(target_os = "macos") {
        assert_eq!(SHELL_FLAGS, "-lc");
    } else {
        assert_eq!(SHELL_FLAGS, "-c");
    }
}

#[tokio::test]
async fn it_runs_a_real_command_through_the_direct_driver() {
    let (_dir, workspace) = temp_workspace();
    let executor = Executor::new(
        Arc::new(crate::sandbox::direct::DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        workspace.root(),
    )
    .expect("the executor opens");
    let tool = BashTool::new(
        &workspace,
        Arc::new(executor),
        "/bin/sh",
        strings(&[
            "PATH=/usr/bin:/bin",
            "HOME=",
            "ENV=",
            "LC_ALL=C",
            "OTTO_DIRECT_VALUE=deterministic",
        ]),
        Duration::from_secs(3),
        4096,
        &[],
        Arc::new(crate::tool::bash_output::Logs::for_session("test-session")),
    )
    .expect("the configuration is valid");
    let result = run(
            &tool,
            r#"printf 'cwd='; /bin/pwd; printf 'env=%s\n' "$OTTO_DIRECT_VALUE"; printf 'problem\n' >&2; exit 7"#,
        )
        .await;
    assert!(!result.is_error, "{result:?}");
    for expected in [
        format!("stdout:\ncwd={}", workspace.root().display()),
        "env=deterministic".to_owned(),
        "stderr:\nproblem".to_owned(),
        "exit_code: 7".to_owned(),
    ] {
        assert!(
            result.content.contains(&expected),
            "missing {expected:?}: {result:?}"
        );
    }
}

#[tokio::test]
async fn a_driver_launch_failure_is_not_started_and_runs_nothing() {
    use crate::faults::sandbox::{FaultyDriver, SandboxOp};
    use otto_faults::{Fault, FaultError, FaultPlan, Rule, Trigger};

    let (_dir, workspace) = temp_workspace();
    let plan = FaultPlan::new(vec![Rule::new(
        SandboxOp::Execute,
        Trigger::Nth(1),
        Fault::Before(FaultError::Os(libc::EAGAIN)),
    )]);
    let executor = Executor::new(
        Arc::new(FaultyDriver::new(
            Arc::new(crate::sandbox::direct::DirectDriver::new()),
            plan.clone(),
        )),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        workspace.root(),
    )
    .expect("the executor opens");
    let tool = BashTool::new(
        &workspace,
        Arc::new(executor),
        "/bin/sh",
        strings(&["PATH=/usr/bin:/bin"]),
        Duration::from_secs(3),
        4096,
        &[],
        Arc::new(crate::tool::bash_output::Logs::for_session("test-session")),
    )
    .expect("the configuration is valid");

    let result = run(&tool, "touch marker").await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(result.content, SANDBOX_EXECUTION_UNAVAILABLE);
    assert_eq!(
        outcome(&result).effect_certainty,
        otto_core::model::EffectCertainty::NotStarted
    );
    assert!(!workspace.root().join("marker").exists());
    plan.assert_all_fired();
}

#[test]
fn the_constructor_rejects_every_invalid_boundary_with_one_safe_error() {
    let (dir, workspace) = temp_workspace();
    let removed = tempfile::tempdir().expect("a temporary directory is creatable");
    let removed_root = removed.path().join("removed");
    std::fs::create_dir(&removed_root).expect("the directory is creatable");
    let removed_workspace = Workspace::new(&removed_root).expect("the workspace opens");
    std::fs::remove_dir(&removed_root).expect("the directory is removable");

    let cases: &[(&str, &Workspace, &str, Duration, usize)] = &[
        (
            "missing workspace",
            &removed_workspace,
            "/bin/sh",
            Duration::from_secs(1),
            1,
        ),
        ("empty shell", &workspace, "", Duration::from_secs(1), 1),
        (
            "whitespace shell",
            &workspace,
            " \t\n",
            Duration::from_secs(1),
            1,
        ),
        (
            "NUL shell",
            &workspace,
            "/bin/\u{0}sh",
            Duration::from_secs(1),
            1,
        ),
        ("zero timeout", &workspace, "/bin/sh", Duration::ZERO, 1),
        (
            "zero output cap",
            &workspace,
            "/bin/sh",
            Duration::from_secs(1),
            0,
        ),
    ];
    for (name, workspace, shell, timeout, max_output_bytes) in cases {
        let outcome = BashTool::new(
            workspace,
            Arc::new(FakeExecutor::default()),
            shell,
            Vec::new(),
            *timeout,
            *max_output_bytes,
            &strings(&["must-not-appear"]),
            Arc::new(crate::tool::bash_output::Logs::for_session("test-session")),
        );
        let Err(error) = outcome else {
            panic!("{name} should be rejected");
        };
        assert_eq!(
            error.to_string(),
            "invalid sandboxed bash configuration",
            "{name}"
        );
    }
    drop(dir);
}

#[test]
fn the_constructor_handles_former_byte_marker_exhaustion() {
    let (_dir, workspace) = temp_workspace();
    let all_bytes = String::from_utf8_lossy(&(0u8..=255).collect::<Vec<u8>>()).into_owned();
    BashTool::new(
        &workspace,
        Arc::new(FakeExecutor::default()),
        "/bin/sh",
        Vec::new(),
        Duration::from_secs(1),
        1024,
        &[all_bytes],
        Arc::new(crate::tool::bash_output::Logs::for_session("test-session")),
    )
    .expect("an exhausted byte set still constructs safely");
}

#[tokio::test]
async fn it_registers_with_an_explicit_empty_environment() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::default());
    let tool = bash(&workspace, executor.clone(), &[], 1024, &[]);
    let registry = crate::tool::registry::Registry::new(vec![Box::new(tool)])
        .expect("the registry accepts one tool");
    let definitions: Vec<String> = registry
        .tools()
        .iter()
        .map(|tool| tool.definition().name)
        .collect();
    assert_eq!(definitions, vec!["bash".to_owned()]);

    let operation_id = otto_core::model::OperationId::new("op_test").unwrap();
    let arguments = raw(r#"{"command":"true"}"#);
    let result = otto_core::tool::ToolExecutor::execute(
        &registry,
        otto_core::tool::ToolCall {
            tool_call_id: "test",
            operation_id: &operation_id,
            name: "bash",
            arguments: &arguments,
            attempt: 1,
        },
        &CancellationToken::new(),
    )
    .await;
    assert!(!result.result.is_error, "{result:?}");
    let requests = executor.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].env.is_empty());
}

#[test]
fn durations_render_the_way_go_prints_them() {
    assert_eq!(go_duration(Duration::from_secs(3)), "3s");
    assert_eq!(go_duration(Duration::from_millis(1500)), "1.5s");
    assert_eq!(go_duration(Duration::from_secs(90)), "1m30s");
    assert_eq!(go_duration(Duration::from_secs(3600)), "1h0m0s");
    assert_eq!(go_duration(Duration::from_millis(250)), "250ms");
    assert_eq!(go_duration(Duration::from_micros(1)), "1\u{b5}s");
    assert_eq!(go_duration(Duration::from_nanos(999)), "999ns");
}
