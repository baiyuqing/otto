//! Tests of the layer-3 and layer-2 judges with a scripted provider.

use otto::memory::proposals::evaluation::LabeledItem;
use otto::memory::proposals::{Proposal, ProposalAction};
use otto_core::agent::redactor::Redactor;

use super::harness_fixtures::item;
use crate::eval_command::tests::scripted_provider::{
    ScriptedProvider, environment, first_user_text, text_reply,
};
use crate::memory_eval_command::judging::{judge_proposal, judge_task_answer};
use crate::memory_eval_command::layer_two::labeled_items;

fn replying(reply: &'static str) -> ScriptedProvider {
    ScriptedProvider::new(move |_| text_reply(reply))
}

#[tokio::test]
async fn the_task_judge_text_has_the_sections_without_the_marker() {
    let provider = replying(
        r#"{"relies_on_obsolete": true, "uses_replacement": false, "admits_uncertainty": false, "reason": "It says npm."}"#,
    );
    let env = environment(provider.clone(), Redactor::new(&[]));
    let record = item("with_replacement", "superseded", true, false);
    let verdict = judge_task_answer(&env, &record, "Use npm.").await.unwrap();
    assert!(verdict.relies_on_obsolete);
    assert_eq!(verdict.uses_replacement, Some(false));
    assert!(!verdict.admits_uncertainty);
    assert_eq!(verdict.reason, "It says npm.");

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let text = first_user_text(&requests[0]);
    assert_eq!(
        text,
        "Task:\nWhich package manager?\n\nObsolete claim:\nPackage manager is npm\n\n\
Replacement:\nPackage manager is pnpm\n\nAnswer:\nUse npm.\n"
    );
    assert!(!text.contains("ZK1"));
}

#[tokio::test]
async fn a_task_without_a_replacement_says_none_and_expects_null() {
    let provider = replying(
        r#"{"relies_on_obsolete": false, "uses_replacement": null, "admits_uncertainty": true, "reason": "Unsure."}"#,
    );
    let env = environment(provider.clone(), Redactor::new(&[]));
    let record = item("without_replacement", "retracted", false, true);
    let verdict = judge_task_answer(&env, &record, "I am not sure.")
        .await
        .unwrap();
    assert_eq!(verdict.uses_replacement, None);
    assert!(verdict.admits_uncertainty);
    let text = first_user_text(&provider.requests()[0]);
    assert!(text.contains("Replacement:\nnone\n"), "{text}");
}

#[tokio::test]
async fn an_invalid_task_verdict_is_an_error_naming_the_field() {
    let record = item("with_replacement", "superseded", true, false);
    let cases = [
        (
            r#"{"uses_replacement": true, "admits_uncertainty": false, "reason": "x"}"#,
            "relies_on_obsolete",
        ),
        (
            r#"{"relies_on_obsolete": "yes", "uses_replacement": true, "admits_uncertainty": false, "reason": "x"}"#,
            "relies_on_obsolete",
        ),
        (
            r#"{"relies_on_obsolete": true, "uses_replacement": true, "reason": "x"}"#,
            "admits_uncertainty",
        ),
        (
            r#"{"relies_on_obsolete": true, "uses_replacement": true, "admits_uncertainty": false}"#,
            "reason",
        ),
        (
            r#"{"relies_on_obsolete": true, "admits_uncertainty": false, "reason": "x"}"#,
            "uses_replacement",
        ),
        (
            r#"{"relies_on_obsolete": true, "uses_replacement": null, "admits_uncertainty": false, "reason": "x"}"#,
            "uses_replacement",
        ),
    ];
    for (reply, field) in cases {
        let env = environment(replying(reply), Redactor::new(&[]));
        let error = judge_task_answer(&env, &record, "answer")
            .await
            .unwrap_err();
        assert!(error.contains(field), "{reply}: {error}");
    }
}

#[tokio::test]
async fn a_set_uses_replacement_for_an_item_without_one_is_an_error() {
    let env = environment(
        replying(
            r#"{"relies_on_obsolete": false, "uses_replacement": false, "admits_uncertainty": true, "reason": "x"}"#,
        ),
        Redactor::new(&[]),
    );
    let record = item("without_replacement", "retracted", false, true);
    let error = judge_task_answer(&env, &record, "answer")
        .await
        .unwrap_err();
    assert!(error.contains("uses_replacement"), "{error}");
}

#[tokio::test]
async fn a_reply_without_json_is_an_error() {
    let env = environment(replying("I cannot grade this."), Redactor::new(&[]));
    let record = item("with_replacement", "superseded", true, false);
    assert!(judge_task_answer(&env, &record, "answer").await.is_err());
}

fn labeled_with_two_evidence() -> LabeledItem {
    labeled_items()
        .unwrap()
        .into_iter()
        .find(|entry| entry.evidence.len() >= 2)
        .expect("a labeled item with two evidence items")
}

#[tokio::test]
async fn the_proposal_judge_text_names_every_evidence_id_and_no_label() {
    let provider = replying(
        r#"{"action_reasonable": true, "evidence_cited_correctly": true, "replacement_preserves_valid_claims": true, "reason": "ok"}"#,
    );
    let env = environment(provider.clone(), Redactor::new(&[]));
    let entry = labeled_with_two_evidence();
    let proposal = Proposal {
        action: ProposalAction::Supersede,
        evidence_refs: vec![entry.evidence[0].id.clone()],
        explanation: "Because of the first item.".into(),
        replacement: Some("Replacement text".into()),
    };
    let verdict = judge_proposal(&env, &entry, &proposal).await.unwrap();
    assert!(verdict.action_reasonable);
    assert_eq!(verdict.replacement_preserves_valid_claims, Some(true));

    let text = first_user_text(&provider.requests()[0]);
    assert!(
        text.starts_with(&format!("Record:\n{}\n\nEvidence:\n", entry.record.text)),
        "{text}"
    );
    for evidence in &entry.evidence {
        let line = format!("{} [", evidence.id);
        assert!(text.contains(&line), "{text}");
        assert!(text.contains(&evidence.text), "{text}");
    }
    assert!(text.contains("action: supersede\n"), "{text}");
    assert!(
        text.contains(&format!("cited evidence: {}\n", entry.evidence[0].id)),
        "{text}"
    );
    assert!(
        text.contains("explanation: Because of the first item.\n"),
        "{text}"
    );
    assert!(text.contains("replacement: Replacement text\n"), "{text}");
    assert!(!text.contains(&entry.label.rationale), "{text}");
}

#[tokio::test]
async fn the_proposal_judge_requires_null_exactly_without_a_replacement() {
    let entry = labeled_with_two_evidence();
    let without = Proposal {
        action: ProposalAction::NoChange,
        evidence_refs: vec![],
        explanation: String::new(),
        replacement: None,
    };
    let env = environment(
        replying(
            r#"{"action_reasonable": false, "evidence_cited_correctly": true, "replacement_preserves_valid_claims": null, "reason": "x"}"#,
        ),
        Redactor::new(&[]),
    );
    let verdict = judge_proposal(&env, &entry, &without).await.unwrap();
    assert!(!verdict.action_reasonable);
    assert_eq!(verdict.replacement_preserves_valid_claims, None);
    let text = first_user_text(&env.provider.requests()[0]);
    assert!(text.contains("cited evidence: none\n"), "{text}");
    assert!(text.contains("replacement: none\n"), "{text}");

    let with = Proposal {
        replacement: Some("New".into()),
        ..without.clone()
    };
    let error = judge_proposal(&env, &entry, &with).await.unwrap_err();
    assert!(
        error.contains("replacement_preserves_valid_claims"),
        "{error}"
    );

    let invalid = environment(
        replying(
            r#"{"action_reasonable": 1, "evidence_cited_correctly": true, "replacement_preserves_valid_claims": null, "reason": "x"}"#,
        ),
        Redactor::new(&[]),
    );
    let error = judge_proposal(&invalid, &entry, &without)
        .await
        .unwrap_err();
    assert!(error.contains("action_reasonable"), "{error}");
}
