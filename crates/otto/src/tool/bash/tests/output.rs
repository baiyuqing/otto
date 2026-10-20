//! Tests for output caps, previews, exit status and sandbox denials.

use super::*;

#[tokio::test]
async fn the_private_home_note_is_advertised_only_when_requested() {
    let note = "HOME and ~ are a private sandbox directory, not the user's home directory; use absolute paths for the user's files.";
    for (plain, private) in [
        (bash_definition(false), bash_definition(true)),
        (
            bash_definition_with_approvals(false),
            bash_definition_with_approvals(true),
        ),
    ] {
        assert!(!plain.description.contains("HOME"));
        assert!(
            private.description.ends_with(note),
            "{}",
            private.description
        );
        assert!(
            private
                .description
                .starts_with(&plain.description.trim_end_matches('.').to_owned())
        );
    }
    let (_dir, workspace) = temp_workspace();
    let tool = bash(&workspace, Arc::new(FakeExecutor::default()), &[], 1, &[]);
    assert!(!tool.definition().description.contains(note));
    assert!(
        tool.with_private_home()
            .definition()
            .description
            .ends_with(note)
    );
}

#[tokio::test]
async fn the_definition_is_the_shared_schema() {
    let (_dir, workspace) = temp_workspace();
    let tool = bash(&workspace, Arc::new(FakeExecutor::default()), &[], 1, &[]);
    assert_eq!(
        serde_json::to_value(tool.definition()).unwrap(),
        serde_json::to_value(bash_definition(false)).unwrap()
    );
}

#[tokio::test]
async fn it_delegates_the_exact_request_and_keeps_redaction_state_per_call() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(
        FakeExecutor::chunks(&["split-*", "secret"], &["problem"]).with_status(ExitStatus {
            code: 7,
            ..ExitStatus::default()
        }),
    );
    let tool = bash(
        &workspace,
        executor.clone(),
        &["FIRST=original", "SECOND=preserved"],
        1024,
        &["split-*secret"],
    );

    let first = run(&tool, "first command").await;
    assert!(!first.is_error, "{first:?}");
    let marker = stdout_body(&first.content);
    assert_eq!(marker.chars().count(), 1, "{first:?}");
    assert_ne!(marker, "*");
    assert!(!first.content.contains("split-*secret"));

    let second = run(&tool, "second command").await;
    assert!(!second.is_error, "{second:?}");
    assert_eq!(stdout_body(&second.content), marker);

    let expected: Vec<Request> = ["first command", "second command"]
        .iter()
        .map(|command| Request {
            argv: strings(&["/bin/sh", SHELL_FLAGS, command]),
            dir: workspace.root().to_path_buf(),
            env: strings(&["FIRST=original", "SECOND=preserved"]),
            report_denials: true,
            stdin: None,
        })
        .collect();
    assert_eq!(executor.requests(), expected);
}

#[tokio::test]
async fn it_caps_each_stream_independently_and_reports_the_exit_code() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(
        FakeExecutor::chunks(&["1234567890abcdef"], &["abcdefghijklmnop"]).with_status(
            ExitStatus {
                code: 7,
                signaled: false,
                signal: "must-be-ignored".to_owned(),
                ..Default::default()
            },
        ),
    );
    let tool = bash(&workspace, executor, &[], 12, &[]);
    let result = run(&tool, "ignored").await;
    assert!(!result.is_error);
    assert!(
        result
            .content
            .starts_with("stdout:\n123456\n[truncated: 4 bytes omitted]\nabcdef")
    );
    assert!(
        result
            .content
            .contains("stderr:\nabcdef\n[truncated: 4 bytes omitted]\nklmnop")
    );
    assert!(result.content.ends_with("exit_code: 7"));
    let paths: Vec<_> = result
        .content
        .lines()
        .filter_map(|line| line.strip_prefix("full_output: "))
        .collect();
    assert_eq!(paths.len(), 2);
    assert_eq!(
        std::fs::read_to_string(workspace.root().join(paths[0])).unwrap(),
        "1234567890abcdef"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.root().join(paths[1])).unwrap(),
        "abcdefghijklmnop"
    );
}

#[tokio::test]
async fn default_preview_caps_large_output_and_read_can_retrieve_an_omitted_range() {
    let (_dir, workspace) = temp_workspace();
    let text = format!(
        "{}failure detail in the middle\n{}final summary\n",
        "ok\n".repeat(2000),
        "ok\n".repeat(2000)
    );
    let executor = Arc::new(FakeExecutor::chunks(&[&text], &[]).with_status(ExitStatus {
        code: 1,
        ..Default::default()
    }));
    let tool = bash(&workspace, executor, &[], 51200, &[]);
    let result = run(&tool, "test").await;
    assert!(result.content.len() < 4600);
    assert!(!result.content.contains("failure detail in the middle"));
    assert!(result.content.contains("final summary"));
    assert!(result.content.ends_with("exit_code: 1"));
    let path = result
        .content
        .lines()
        .find_map(|line| line.strip_prefix("full_output: "))
        .unwrap();
    let read = crate::tool::read::ReadTool::new(&workspace, 51200);
    let args =
        serde_json::value::to_raw_value(&json!({"path":path,"offset":2001,"limit":1})).unwrap();
    let excerpt = read.execute(&args, &CancellationToken::new()).await;
    assert!(!excerpt.is_error);
    assert_eq!(excerpt.content, "failure detail in the middle\n");
}

#[tokio::test(start_paused = true)]
async fn timeout_keeps_partial_overflow_log_and_execution_status() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::behaving(Box::new(|_, streams, cancel| {
        Box::pin(async move {
            streams
                .stdout
                .write_all(b"begin lots of partial output end")
                .unwrap();
            cancel.cancelled().await;
            (
                ExitStatus {
                    code: -1,
                    signaled: true,
                    signal: "killed".into(),
                    ..Default::default()
                },
                Err(Error::Cancelled),
            )
        })
    })));
    let tool = bash(&workspace, executor, &[], 8, &[]);
    let result = run(&tool, "test").await;
    assert!(
        result
            .content
            .contains("status: timed out after 3s; signal: killed")
    );
    assert_eq!(
        outcome(&result).disposition,
        otto_core::model::OperationDisposition::DeadlineExceeded
    );
    let path = result
        .content
        .lines()
        .find_map(|line| line.strip_prefix("full_output: "))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.root().join(path)).unwrap(),
        "begin lots of partial output end"
    );
}

#[tokio::test]
async fn a_signaled_exit_appends_the_signal() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::default().with_status(ExitStatus {
        code: -1,
        signaled: true,
        signal: "killed".to_owned(),
        ..Default::default()
    }));
    let tool = bash(&workspace, executor, &[], 1024, &[]);
    let result = run(&tool, "ignored").await;
    assert!(!result.is_error);
    assert_eq!(
        result.content,
        "stdout:\n\nstderr:\n\nexit_code: -1; signal: killed"
    );
}

fn denied_status(count: usize, omitted: usize, target: &str) -> ExitStatus {
    ExitStatus {
        code: 1,
        denials: (0..count)
            .map(|i| crate::sandbox::Denial {
                operation: "file-read-data".to_owned(),
                target: format!("{target}{i}"),
            })
            .collect(),
        denials_omitted: omitted,
        ..Default::default()
    }
}

#[tokio::test]
async fn denials_render_between_stderr_and_the_summary() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::default().with_status(denied_status(2, 0, "/x/")));
    let tool = bash(&workspace, executor, &[], 1024, &[]);
    let result = run(&tool, "ignored").await;
    assert_eq!(
        result.content,
        "stdout:\n\nstderr:\n\nsandbox_denied:\nfile-read-data /x/0\nfile-read-data /x/1\nexit_code: 1"
    );
}

#[tokio::test]
async fn omitted_denials_are_counted() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::default().with_status(denied_status(1, 3, "/x/")));
    let tool = bash(&workspace, executor, &[], 1024, &[]);
    let result = run(&tool, "ignored").await;
    assert!(
        result
            .content
            .contains("file-read-data /x/0\n[3 more omitted]\nexit_code: 1"),
        "{result:?}"
    );
}

#[tokio::test]
async fn no_denials_render_no_section() {
    let (_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::default().with_status(denied_status(0, 0, "")));
    let tool = bash(&workspace, executor, &[], 1024, &[]);
    let result = run(&tool, "ignored").await;
    assert_eq!(result.content, "stdout:\n\nstderr:\n\nexit_code: 1");
}

#[tokio::test]
async fn denial_targets_are_redacted() {
    let (_dir, workspace) = temp_workspace();
    const SECRET: &str = "denied-secret-value";
    let executor = Arc::new(FakeExecutor::default().with_status(denied_status(1, 0, SECRET)));
    let tool = bash(&workspace, executor, &[], 1024, &[SECRET]);
    let result = run(&tool, "ignored").await;
    assert!(!result.content.contains(SECRET), "{result:?}");
    assert!(result.content.contains("sandbox_denied:"), "{result:?}");
}

#[tokio::test]
async fn the_signal_text_is_redacted_too() {
    let (_dir, workspace) = temp_workspace();
    const SECRET: &str = "signal-secret-value";
    let executor = Arc::new(FakeExecutor::default().with_status(ExitStatus {
        code: -1,
        signaled: true,
        signal: format!("killed-{SECRET}"),
        ..Default::default()
    }));
    let tool = bash(&workspace, executor, &[], 1024, &[SECRET]);
    let result = run(&tool, "ignored").await;
    assert!(!result.is_error);
    assert!(!result.content.contains(SECRET), "{result:?}");
    assert!(
        result
            .content
            .contains(&format!("signal: killed-{}", tool.redaction_marker)),
        "{result:?}"
    );
}
