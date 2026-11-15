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
            package_environment: Default::default(),
            workspace,
            guard: None,
            secrets: vec!["test-private-value".into()],
            logger: Arc::new(otto_platform::log::Logger::discard()),
        },
    )
    .unwrap();
    (temp_dir, runtime)
}
/// Binding with a fixed identity, `headless: false` and an empty history path.
pub(crate) fn binding() -> SessionBinding {
    SessionBinding {
        session_id: "s".into(),
        parent_session_id: None,
        task_id: String::new(),
        mode: "new".into(),
        headless: false,
        history_path: Arc::new(String::new),
    }
}
fn hook(script: &str) -> Hook {
    Hook {
        command: vec!["/bin/sh".into(), "-c".into(), script.into()],
        timeout: "1s".into(),
        matcher: Vec::new(),
        lua: None,
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
        .bind(binding(), None)
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
            .bind(binding(), None)
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
            .bind(binding(), None)
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
        .bind(binding(), None)
        .run(
            "after_tool",
            json!({"content":"test-private-value"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.content.as_deref(), Some("[REDACTED]"));
}
#[tokio::test]
async fn oversized_turn_end_input_drops_the_turn_and_still_runs_the_hook() {
    let mut turn_end = hook(
        "input=$(cat); case \"$input\" in *'\"turn\":'*) exit 4;; *'\"turn_omitted\":\"size\"'*) ;; *) exit 5;; esac",
    );
    turn_end.timeout = "10s".into();
    let (_temp, runtime) = runtime(otto_core::config::plugins::Hooks {
        turn_end: vec![turn_end],
        ..Default::default()
    });
    let bound = runtime.bind(binding(), None);
    let oversized_text = "a".repeat(MAX_INPUT + 1);
    bound
        .run(
            "turn_end",
            json!({"status":"success","turn":{"messages":[{"role":"user","blocks":[{"type":"text","text":oversized_text}]}]}}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    // Input still over the limit without a `turn` is an error.
    assert!(
        bound
            .run(
                "turn_end",
                json!({"status":oversized_text}),
                &CancellationToken::new(),
            )
            .await
            .is_err()
    );
}
#[tokio::test]
async fn payload_carries_headless_and_the_history_path_read_at_each_call() {
    let history = Arc::new(std::sync::Mutex::new(String::new()));
    let reader = history.clone();
    let turn_end = hook(
        "read input; printf '%s' \"$input\" | grep -q '\"headless\":true' || exit 3; \
         printf '{\"notice\":\"%s\"}' \"$(printf '%s' \"$input\" | sed 's/.*\"history_path\":\"\\([^\"]*\\)\".*/\\1/')\"",
    );
    let (_temp, runtime) = runtime(otto_core::config::plugins::Hooks {
        turn_end: vec![turn_end],
        ..Default::default()
    });
    let bound = runtime.bind(
        SessionBinding {
            headless: true,
            history_path: Arc::new(move || reader.lock().unwrap().clone()),
            ..binding()
        },
        None,
    );
    let run = || async {
        bound
            .run(
                "turn_end",
                json!({"status":"success"}),
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .notice
    };
    assert_eq!(run().await.as_deref(), Some(""));
    *history.lock().unwrap() = "/home/u/s1.jsonl".into();
    assert_eq!(run().await.as_deref(), Some("/home/u/s1.jsonl"));
}
#[tokio::test]
async fn notices_from_several_hooks_are_joined_and_accepted_on_every_event() {
    let first = hook("read input; printf '%s' '{\"notice\":\"one\"}'");
    let second = hook("read input; printf '%s' '{\"notice\":\"two\",\"content\":\"x\"}'");
    let turn_end = hook("read input; printf '%s' '{\"notice\":\"n\"}'");
    let (_temp, runtime) = runtime(otto_core::config::plugins::Hooks {
        after_tool: vec![first, second],
        turn_end: vec![turn_end],
        ..Default::default()
    });
    let bound = runtime.bind(binding(), None);
    let response = bound
        .run(
            "after_tool",
            json!({"content":"c"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.notice.as_deref(), Some("one\ntwo"));
    assert_eq!(response.content.as_deref(), Some("x"));
    let response = bound
        .run(
            "turn_end",
            json!({"status":"success"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.notice.as_deref(), Some("n"));
}
#[tokio::test]
async fn a_denying_hook_keeps_the_notices_of_earlier_hooks() {
    let first = hook("read input; printf '%s' '{\"notice\":\"a\"}'");
    let second =
        hook("read input; printf '%s' '{\"decision\":\"deny\",\"reason\":\"r\",\"notice\":\"b\"}'");
    let (_temp, runtime) = runtime(otto_core::config::plugins::Hooks {
        before_tool: vec![first, second],
        ..Default::default()
    });
    let response = runtime
        .bind(binding(), None)
        .run(
            "before_tool",
            json!({"tool":"write"}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.decision, Some(Decision::Deny));
    assert_eq!(response.reason.as_deref(), Some("r"));
    assert_eq!(response.notice.as_deref(), Some("a\nb"));
}
