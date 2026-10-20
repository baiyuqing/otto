//! Tests for the Markdown run logs.

use super::*;

fn only_file(dir: &Path) -> PathBuf {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("log dir")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    files.remove(0)
}

#[tokio::test]
async fn a_run_log_shows_every_proposal_with_its_outcome() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(Vec::new()).await;
    let (user, tool) = lint_session(&fixture).await;
    fixture
        .tool_exchange(
            "bash",
            r#"{"command":"curl -s https://example.com"}"#,
            "external page text",
        )
        .await;
    let external = fixture.session.messages().last().expect("tool").id.clone();
    let quote = serde_json::json!([{"entry": user, "quote": "always run the lint gate"}]);
    let skill_quote = serde_json::json!([
        {"entry": user, "quote": "always run the lint gate"},
        {"entry": tool, "quote": "clippy found nothing"},
    ]);
    push(
        &fixture,
        Reply::Text(
            serde_json::json!({
                "memories": [
                    {"action": "create", "scope": "user", "kind": "preference",
                     "key": "lint-first", "text": "Run the lint gate before commits",
                     "confidence": 0.9, "reason": "stated by the user", "evidence": quote},
                    {"action": "create", "scope": "user", "kind": "bogus-kind",
                     "key": "other", "text": "Text of the invalid memory",
                     "confidence": 0.9, "reason": "invalid kind", "evidence": quote},
                ],
                "skills": [{
                    "action": "create", "name": "lint-gate",
                    "description": "Run the lint gate",
                    "body": "too short", "reason": "ran cleanly", "evidence": skill_quote,
                }],
            })
            .to_string(),
        ),
    );
    let dir = tempfile::tempdir().expect("log dir");
    let runs = dir.path().join("runs");
    let reflector = fixture
        .reflector(ReflectionRuntime {
            skill_source: SkillSource::Any,
            ..ReflectionRuntime::default()
        })
        .with_log_dir(runs.clone());
    let report = fixture
        .run(&reflector, &CancellationToken::new())
        .await
        .expect("run");

    assert_eq!(report.candidates.len(), 1, "{report:?}");
    let path = report.log.clone().expect("log").expect("written");
    assert_eq!(path, only_file(&runs));
    assert!(
        report
            .line()
            .ends_with(&format!("; log: {}", path.display()))
    );
    let skill_reason = match &report.items.iter().find(|i| i.kind == runlog::Kind::Skill) {
        Some(runlog::Item {
            outcome: runlog::Outcome::Dropped(reason),
            ..
        }) => *reason,
        other => panic!("skill was not dropped: {other:?}"),
    };
    let log = std::fs::read_to_string(&path).expect("read log");
    println!("{log}");
    assert!(log.contains(&report.run_id));
    assert!(log.contains("Run the lint gate before commits"));
    assert!(log.contains(&format!("queued as candidate {}", report.candidates[0])));
    assert!(log.contains("Text of the invalid memory"));
    assert!(log.contains("dropped: bad_kind"));
    assert!(log.contains("too short"));
    assert_eq!(skill_reason, "skill_body_too_short");
    assert!(log.contains("dropped: skill_body_too_short"));
    assert!(
        log.contains(&format!("- `{external}` (bash)")),
        "the external entry is listed: {log}"
    );
    assert!(!log.contains("external page text"));
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let mode = std::fs::metadata(&runs).expect("meta").permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[tokio::test]
async fn noop_and_invalid_answer_runs_write_a_log() {
    let dir = tempfile::tempdir().expect("log dir");

    let fixture = Fixture::new(Vec::new()).await;
    fixture.say(Role::Assistant, "nothing from the user").await;
    let runs = dir.path().join("noop");
    let report = fixture
        .run(
            &fixture.reflector(enabled()).with_log_dir(runs.clone()),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    assert_eq!(report.status, Status::Noop);
    assert!(report.line().contains("nothing new to reflect on; log: "));
    let log = std::fs::read_to_string(only_file(&runs)).expect("log");
    assert!(log.contains("status: noop"), "{log}");

    let fixture = Fixture::new(vec![Reply::Text("sure, here you go".into())]).await;
    fixture.say(Role::User, "a topic worth reflecting on").await;
    let runs = dir.path().join("invalid");
    let error = fixture
        .run(
            &fixture.reflector(enabled()).with_log_dir(runs.clone()),
            &CancellationToken::new(),
        )
        .await
        .expect_err("invalid");
    assert!(matches!(error, Error::InvalidAnswer(_)));
    let log = std::fs::read_to_string(only_file(&runs)).expect("log");
    assert!(log.contains("sure, here you go"), "{log}");
    assert!(log.contains("status: failed"), "{log}");
    assert!(log.contains("invalid_answer"), "{log}");
}
