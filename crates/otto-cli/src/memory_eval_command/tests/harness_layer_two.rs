//! Tests of the layer-2 backend and run with scripted providers.

use std::time::Duration;

use otto::decision::{Backend, Failure};
use otto::memory::proposals::round_one_questions;
use otto_core::agent::redactor::Redactor;
use otto_core::operation::OperationControl;
use otto_core::provider::{Provider, ProviderSettlement, Request, Response, StreamSink};

use crate::eval_command::output_files::OutputDir;
use crate::eval_command::tests::scripted_provider::{
    ScriptedProvider, environment, first_user_text, text_reply,
};
use crate::memory_eval_command::layer_two::{
    EvalDecisionBackend, PROPOSALS_FILE, ProposalRecord, labeled_items, run_layer_two,
};

/// A provider whose calls never finish.
#[derive(Clone)]
struct SilentProvider;

#[async_trait::async_trait]
impl Provider for SilentProvider {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        _control: &dyn OperationControl,
    ) -> ProviderSettlement {
        std::future::pending::<()>().await;
        ProviderSettlement::succeeded(Response::default(), 1)
    }
}

/// Answers a decision round in the session-backend format. A record about the vim editor gets a
/// `supersede` with a replacement; every other record gets `no_change`. The judge always agrees.
fn scripted_layer_two_provider() -> ScriptedProvider {
    ScriptedProvider::new(|request| {
        let text = first_user_text(request);
        if text.starts_with("Record:\n") {
            let preserves = if text.contains("replacement: none\n") {
                "null"
            } else {
                "true"
            };
            return text_reply(&format!(
                r#"{{"action_reasonable": true, "evidence_cited_correctly": true, "replacement_preserves_valid_claims": {preserves}, "reason": "ok"}}"#
            ));
        }
        if text.contains("\"explanation\":") {
            return text_reply(
                r#"{"explanation": "The first item shows the change.", "replacement": "Prefers the helix editor for every project"}"#,
            );
        }
        let evidence_count = text.matches("\"evidence_").count();
        let supersede = text.contains("Prefers the vim editor for every project");
        let mut reply = format!(
            r#"{{"action": "{}""#,
            if supersede { "supersede" } else { "no_change" }
        );
        for index in 1..=evidence_count {
            reply.push_str(&format!(
                r#", "evidence_{index}": {}"#,
                supersede && index == 1
            ));
        }
        reply.push('}');
        text_reply(&reply)
    })
}

fn output_dir(directory: &std::path::Path) -> OutputDir {
    OutputDir::create(&directory.join("out"), Redactor::new(&[])).unwrap()
}

#[tokio::test]
async fn rows_carry_the_score_and_the_judge_verdict() {
    let directory = tempfile::tempdir().unwrap();
    let out = output_dir(directory.path());
    let provider = scripted_layer_two_provider();
    let env = environment(provider.clone(), Redactor::new(&[]));
    let mut stdout = Vec::new();
    let rows = run_layer_two(&env, &out, true, &mut stdout).await.unwrap();

    assert_eq!(rows.len(), 26);
    let vim = rows
        .iter()
        .find(|row| row.name == "distinction_preference_changed_supersede")
        .expect("the supersede item");
    assert_eq!(vim.generated.action.as_deref(), Some("supersede"));
    assert_eq!(
        vim.generated.replacement.as_deref(),
        Some("Prefers the helix editor for every project")
    );
    assert_eq!(
        vim.generated.explanation,
        "The first item shows the change."
    );
    assert!(vim.score.action_correct);
    let verdict = vim.judge.as_ref().expect("judged");
    assert_eq!(verdict.replacement_preserves_valid_claims, Some(true));
    assert!(vim.judge_error.is_empty());

    let unchanged = rows
        .iter()
        .find(|row| row.generated.action.as_deref() == Some("no_change"))
        .expect("a no_change row");
    assert_eq!(
        unchanged
            .judge
            .as_ref()
            .unwrap()
            .replacement_preserves_valid_claims,
        None
    );
    assert_eq!(unchanged.score.name, unchanged.name);

    let stored: Vec<ProposalRecord> = out.read_jsonl(PROPOSALS_FILE).unwrap();
    assert_eq!(stored, rows);
    let printed = String::from_utf8(stdout).unwrap();
    assert!(
        printed.contains("layer 2 distinction_preference_changed_supersede: supersede\n"),
        "{printed}"
    );
    assert_eq!(printed.lines().count(), 26);
}

#[tokio::test]
async fn a_rerun_over_the_same_directory_makes_no_provider_call() {
    let directory = tempfile::tempdir().unwrap();
    let out = output_dir(directory.path());
    let provider = scripted_layer_two_provider();
    let env = environment(provider.clone(), Redactor::new(&[]));
    let first = run_layer_two(&env, &out, true, &mut Vec::new())
        .await
        .unwrap();
    let calls = provider.requests().len();
    assert!(
        calls > 52,
        "two rounds and a judge call per supersede, got {calls}"
    );

    let mut stdout = Vec::new();
    let second = run_layer_two(&env, &out, true, &mut stdout).await.unwrap();
    assert_eq!(provider.requests().len(), calls);
    assert_eq!(second, first);
    assert!(stdout.is_empty());
}

#[tokio::test]
async fn no_judge_leaves_the_verdict_empty() {
    let directory = tempfile::tempdir().unwrap();
    let out = output_dir(directory.path());
    let provider = scripted_layer_two_provider();
    let env = environment(provider.clone(), Redactor::new(&[]));
    let rows = run_layer_two(&env, &out, false, &mut Vec::new())
        .await
        .unwrap();
    assert!(
        rows.iter()
            .all(|row| row.judge.is_none() && row.judge_error.is_empty())
    );
    assert!(
        provider
            .requests()
            .iter()
            .all(|request| !first_user_text(request).starts_with("Record:\n"))
    );
}

#[tokio::test]
async fn a_judge_failure_is_stored_in_judge_error() {
    let directory = tempfile::tempdir().unwrap();
    let out = output_dir(directory.path());
    let base = scripted_layer_two_provider();
    let provider = ScriptedProvider::new(move |request| {
        if first_user_text(request).starts_with("Record:\n") {
            text_reply("no verdict")
        } else {
            base.reply(request)
        }
    });
    let env = environment(provider, Redactor::new(&[]));
    let rows = run_layer_two(&env, &out, true, &mut Vec::new())
        .await
        .unwrap();
    assert!(
        rows.iter()
            .all(|row| row.judge.is_none() && !row.judge_error.is_empty())
    );
}

#[tokio::test(start_paused = true)]
async fn a_provider_that_never_answers_gives_unavailable_rows() {
    let directory = tempfile::tempdir().unwrap();
    let out = output_dir(directory.path());
    let env = environment_with(SilentProvider);
    let rows = run_layer_two(&env, &out, true, &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(rows.len(), 26);
    for row in &rows {
        assert!(row.generated.unavailable.is_some(), "{}", row.name);
        assert_eq!(row.generated.action, None);
        assert!(row.score.unavailable.is_some());
        assert!(row.judge.is_none());
    }
}

#[tokio::test]
async fn a_canceled_run_stops_without_rows() {
    let directory = tempfile::tempdir().unwrap();
    let out = output_dir(directory.path());
    let env = environment(ScriptedProvider::never_called(), Redactor::new(&[]));
    env.cancel.cancel();
    let error = run_layer_two(&env, &out, true, &mut Vec::new())
        .await
        .unwrap_err();
    assert_eq!(error, "canceled");
    assert!(
        out.read_jsonl::<ProposalRecord>(PROPOSALS_FILE)
            .unwrap()
            .is_empty()
    );
}

fn environment_with<P>(
    provider: P,
) -> std::sync::Arc<crate::eval_command::model_calls::EvalEnvironment<P>> {
    let template = environment(ScriptedProvider::never_called(), Redactor::new(&[]));
    std::sync::Arc::new(crate::eval_command::model_calls::EvalEnvironment {
        provider,
        redactor: Redactor::new(&[]),
        provider_name: template.provider_name.clone(),
        profile_model: template.profile_model.clone(),
        thinking: String::new(),
        compaction: template.compaction,
        cancel: tokio_util::sync::CancellationToken::new(),
    })
}

#[tokio::test(start_paused = true)]
async fn the_backend_maps_a_timeout_to_failure_timeout() {
    let backend = EvalDecisionBackend {
        env: environment_with(SilentProvider),
    };
    let entry = labeled_items().unwrap().remove(0);
    let questions = round_one_questions(&entry.input("memory-eval"));
    let exchange = backend
        .ask("state", &questions, Duration::from_secs(5))
        .await;
    assert_eq!(exchange.result.unwrap_err(), Failure::Timeout);
    assert_eq!(exchange.response, None);
    assert!(exchange.request.starts_with("state\n"));
}

#[tokio::test]
async fn the_backend_maps_text_that_is_not_json_to_invalid_and_keeps_the_reply() {
    let env = environment(
        ScriptedProvider::new(|_| text_reply("not json")),
        Redactor::new(&[]),
    );
    let backend = EvalDecisionBackend { env };
    let entry = labeled_items().unwrap().remove(0);
    let questions = round_one_questions(&entry.input("memory-eval"));
    let exchange = backend
        .ask("state", &questions, Duration::from_secs(5))
        .await;
    assert!(matches!(exchange.result, Err(Failure::Invalid(_))));
    assert_eq!(exchange.response.as_deref(), Some("not json"));
}

#[tokio::test]
async fn the_backend_sends_the_session_prompt_and_reports_the_profile_model() {
    let provider = ScriptedProvider::new(|_| text_reply("{}"));
    let env = environment(provider.clone(), Redactor::new(&[]));
    let backend = EvalDecisionBackend { env };
    assert_eq!(backend.model(), "profile-model");
    assert!(!backend.auxiliary());
    let entry = labeled_items().unwrap().remove(0);
    let questions = round_one_questions(&entry.input("memory-eval"));
    let exchange = backend
        .ask("state", &questions, Duration::from_secs(5))
        .await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(first_user_text(&requests[0]), exchange.request);
    assert_eq!(
        exchange.request,
        otto::decision::session::user_text("state", &questions)
    );
}
