use super::*;
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy, direct::DirectDriver};
fn runtime(hooks: otto_core::config::plugins::Hooks) -> (tempfile::TempDir, Arc<Runtime>) {
    let temp_dir = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(temp_dir.path()).unwrap();
    let executor = Executor::new(
        Arc::new(DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        &workspace,
    )
    .unwrap();
    let runtime = Runtime::new(
        &super::super::Snapshot::default(),
        temp_dir.path(),
        &hooks,
        Execution {
            executor: Some(Arc::new(executor)),
            environment: vec!["PATH=/usr/bin:/bin".into()],
            workspace,
            guard: None,
            secrets: vec!["test-private-value".into()],
            logger: Arc::new(crate::log::Logger::discard()),
        },
    )
    .unwrap();
    (temp_dir, runtime)
}
fn hook(script: &str) -> Hook {
    Hook {
        command: vec!["/bin/sh".into(), "-c".into(), script.into()],
        timeout: "1s".into(),
        matcher: Vec::new(),
    }
}
#[tokio::test]
async fn ordered_hooks_exchange_json_and_match_remote_mcp_names() {
    let mut first = hook(
        "read input; printf '%s' '{\"arguments\":{\"tool\":\"mcp__demo__read\",\"arguments\":{\"value\":2}}}'",
    );
    first.matcher = vec!["mcp__demo__*".into()];
    let second = hook(
        "read input; case \"$input\" in *'\"value\":2'*) printf '%s' '{\"decision\":\"deny\",\"reason\":\"blocked\"}';; *) exit 1;; esac",
    );
    let (_temp, runtime) = runtime(otto_core::config::plugins::Hooks {
        before_tool: vec![first, second],
        ..Default::default()
    });
    let response = runtime
        .bind("s".into(), None, String::new(), "new", None)
        .run(
            "before_tool",
            json!({"tool":"mcp_call_tool","arguments":{"tool":"mcp__demo__read","arguments":{}}}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.decision, Some(Decision::Deny));
    assert_eq!(response.reason.as_deref(), Some("blocked"));
}
#[tokio::test]
async fn invalid_timeout_failure_and_unsupported_response_fail_closed() {
    for script in [
        "read input; printf 'not-json'",
        "exit 1",
        "sleep 20",
        "read input; printf '%s' '{\"content\":\"wrong event\"}'",
    ] {
        let mut entry = hook(script);
        entry.timeout = "50ms".into();
        let (_temp, runtime) = runtime(otto_core::config::plugins::Hooks {
            before_tool: vec![entry],
            ..Default::default()
        });
        let response = runtime
            .bind("s".into(), None, String::new(), "new", None)
            .run("before_tool", json!({}), &CancellationToken::new())
            .await;
        assert!(response.is_err(), "{script}");
    }
}
#[tokio::test]
async fn input_and_output_are_redacted_and_after_failure_does_not_skip_later_hooks() {
    let first = hook("exit 1");
    let second = hook(
        "read input; case \"$input\" in *test-private-value*) exit 3;; esac; printf '%s' '{\"content\":\"test-private-value\"}'",
    );
    let (_temp, runtime) = runtime(otto_core::config::plugins::Hooks {
        after_tool: vec![first, second.clone()],
        ..Default::default()
    });
    assert!(
        runtime
            .bind("s".into(), None, String::new(), "new", None)
            .run(
                "after_tool",
                json!({"content":"test-private-value"}),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    let (_temp, runtime) = self::runtime(otto_core::config::plugins::Hooks {
        after_tool: vec![second],
        ..Default::default()
    });
    let response = runtime
        .bind("s".into(), None, String::new(), "new", None)
        .run(
            "after_tool",
            json!({"content":"test-private-value"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.content.as_deref(), Some("[REDACTED]"));
}
