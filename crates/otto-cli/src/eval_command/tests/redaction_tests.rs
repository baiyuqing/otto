use otto_core::agent::redactor::{REDACTION_MARKER, Redactor};

use super::run_tests::{DEFAULT_MODES, pipeline_provider};
use super::scripted_provider::{ScriptedProvider, environment, invocation, roots};
use super::session_fixture::{message_text, write_linear_session};
use crate::eval_command::execute;

const FIXTURE_SECRET: &str = "sk-test-eval-secret-0001";

#[tokio::test]
async fn files_and_provider_requests_hold_no_secret() {
    let directory = tempfile::tempdir().unwrap();
    let session = write_linear_session(directory.path(), 24, |index| {
        if index == 8 {
            format!("{} token {FIXTURE_SECRET} end", message_text(index))
        } else {
            message_text(index)
        }
    });
    let out = directory.path().join("out");
    let provider: ScriptedProvider = pipeline_provider(DEFAULT_MODES);
    let env = environment(
        provider.clone(),
        Redactor::new(&[FIXTURE_SECRET.to_string()]),
    );
    let arguments = [
        "view-tree",
        "all",
        "--session",
        session.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--view-low",
        "1",
        "--view-high",
        "1",
        "--count",
        "6",
    ];
    let mut stdout = Vec::new();
    execute(
        &env,
        &invocation(&arguments),
        &roots(directory.path()),
        &mut stdout,
    )
    .await
    .unwrap();

    let mut checked = 0;
    for entry in std::fs::read_dir(&out).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains(FIXTURE_SECRET),
            "{} holds the secret",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 8,
        "log, tree, view, build, cost, probes, answers and report files"
    );
    let log = std::fs::read_to_string(out.join("log.jsonl")).unwrap();
    assert!(
        log.contains(REDACTION_MARKER),
        "the secret in message 8 is replaced by the marker"
    );
    for request in provider.requests() {
        let serialized: String = request
            .messages
            .iter()
            .map(|message| message.text())
            .collect();
        assert!(
            !serialized.contains(FIXTURE_SECRET) && !request.system_prompt.contains(FIXTURE_SECRET)
        );
    }
}

#[tokio::test]
async fn a_closed_redaction_boundary_sends_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let session = write_linear_session(directory.path(), 4, message_text);
    let provider = ScriptedProvider::never_called();
    let env = environment(provider, Redactor::with_completeness(&[], false));
    let out = directory.path().join("out");
    let arguments = [
        "view-tree",
        "build",
        "--session",
        session.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ];
    let mut stdout = Vec::new();
    let error = execute(
        &env,
        &invocation(&arguments),
        &roots(directory.path()),
        &mut stdout,
    )
    .await
    .unwrap_err();
    assert!(error.contains("redaction boundary is closed"));
}
