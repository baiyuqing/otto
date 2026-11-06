//! The #591 §9 boundary table: each action reaches the store as the matching
//! candidate, with scope, kind, key, target id and base revision taken from
//! the target record.

use super::*;

const REPLACEMENT: &str = "Prefers the helix editor for every project";

#[tokio::test]
async fn retract_with_one_local_evidence_queues_a_retract_candidate_without_content() {
    let fixture = fixture();
    let (generator, asked) = generator(vec![
        round_one("retract", &[true]),
        round_two("Evidence e1 withdraws the editor preference.", None),
    ]);
    let candidate = proposed(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await,
    );
    assert_eq!(candidate.action, CandidateAction::Retract);
    assert_eq!(candidate.proposed.text, "");
    assert_eq!(
        candidate.reason,
        "Evidence e1 withdraws the editor preference."
    );
    assert_eq!(candidate.target_id, fixture.target.id);
    assert_eq!(candidate.base_revision, fixture.target.revision);
    assert_eq!(candidate.state, CandidateState::Pending);
    assert_eq!(pending_candidates(&fixture.service).len(), 1);
    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 2);
    let round_two_ids: Vec<&str> = asked[1]
        .questions
        .iter()
        .map(|question| question.id)
        .collect();
    assert_eq!(round_two_ids, [EXPLANATION_QUESTION]);
}

#[tokio::test]
async fn supersede_with_a_replacement_keeps_kind_key_and_scope_of_the_target() {
    let fixture = fixture();
    let (generator, asked) = generator(vec![
        round_one("supersede", &[true]),
        round_two("Evidence e1 says the editor changed.", Some(REPLACEMENT)),
    ]);
    let candidate = proposed(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await,
    );
    assert_eq!(candidate.action, CandidateAction::Supersede);
    assert_eq!(candidate.proposed.text, REPLACEMENT);
    assert_eq!(candidate.proposed.kind, fixture.target.kind);
    assert_eq!(candidate.proposed.key, fixture.target.key);
    assert_eq!(candidate.proposed.scope, fixture.target.scope);
    assert_eq!(candidate.target_id, fixture.target.id);
    assert_eq!(candidate.base_revision, fixture.target.revision);
    let asked = asked.lock().unwrap();
    let round_two_ids: Vec<&str> = asked[1]
        .questions
        .iter()
        .map(|question| question.id)
        .collect();
    assert_eq!(round_two_ids, [EXPLANATION_QUESTION, REPLACEMENT_QUESTION]);
}

#[tokio::test]
async fn correct_with_a_replacement_queues_a_correct_candidate() {
    let fixture = fixture();
    let (generator, _) = generator(vec![
        round_one("correct", &[true]),
        round_two(
            "Evidence e1 shows the old editor was never used.",
            Some(REPLACEMENT),
        ),
    ]);
    let candidate = proposed(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await,
    );
    assert_eq!(candidate.action, CandidateAction::Correct);
    assert_eq!(candidate.proposed.text, REPLACEMENT);
    assert_eq!(candidate.proposed.scope, fixture.target.scope);
    assert_eq!(candidate.target_id, fixture.target.id);
}

#[tokio::test]
async fn no_change_asks_one_round_and_queues_nothing() {
    let fixture = fixture();
    let (generator, asked) = generator(vec![round_one("no_change", &[false])]);
    let outcome = run(
        &generator,
        &fixture,
        input(&fixture.target, local_evidence(), &all_actions()),
    )
    .await;
    match outcome {
        Outcome::NoChange { explanation } => assert_eq!(explanation, ""),
        other => panic!("expected Outcome::NoChange, got {other:?}"),
    }
    assert_eq!(asked.lock().unwrap().len(), 1);
    assert!(pending_candidates(&fixture.service).is_empty());
}

#[tokio::test]
async fn forget_with_local_evidence_queues_a_forget_candidate_without_content() {
    let fixture = fixture();
    let (generator, _) = generator(vec![
        round_one("forget", &[true]),
        round_two(
            "Evidence e1 is an explicit request to stop retaining this.",
            None,
        ),
    ]);
    let candidate = proposed(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await,
    );
    assert_eq!(candidate.action, CandidateAction::Forget);
    assert_eq!(candidate.proposed.text, "");
    assert_eq!(candidate.target_id, fixture.target.id);
    assert_eq!(candidate.base_revision, fixture.target.revision);
    assert!(!candidate.reason.contains("vim"));
}

#[tokio::test]
async fn needs_review_queues_a_needs_review_candidate_without_cited_evidence() {
    let fixture = fixture();
    let (generator, _) = generator(vec![
        round_one("needs_review", &[false]),
        round_two(
            "Evidence e1 conflicts with only one line of the record.",
            None,
        ),
    ]);
    let candidate = proposed(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await,
    );
    assert_eq!(candidate.action, CandidateAction::NeedsReview);
    assert_eq!(candidate.proposed.text, "");
    assert_eq!(
        candidate.reason,
        "Evidence e1 conflicts with only one line of the record."
    );
}

#[tokio::test]
async fn candidate_source_is_the_model_origin_and_the_input_session() {
    let fixture = fixture();
    let (generator, _) = generator(vec![
        round_one("retract", &[true]),
        round_two("Evidence e1 withdraws it.", None),
    ]);
    let candidate = proposed(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await,
    );
    assert_eq!(candidate.proposed.source.origin, Some(Origin::Model));
    assert_eq!(candidate.proposed.source.session_id, "session-1");
}

#[test]
fn propose_request_takes_identity_from_the_target_and_content_only_for_replacements() {
    let fixture = fixture();
    let input = input(&fixture.target, local_evidence(), &all_actions());
    let proposal = |action, replacement: Option<&str>| Proposal {
        action,
        evidence_refs: vec!["e1".into()],
        explanation: "because".into(),
        replacement: replacement.map(str::to_owned),
    };
    assert!(propose_request(&input, &proposal(ProposalAction::NoChange, None)).is_none());
    for action in [
        ProposalAction::Retract,
        ProposalAction::Forget,
        ProposalAction::NeedsReview,
    ] {
        // A replacement the model supplied for an action without content is dropped.
        let request = propose_request(&input, &proposal(action, Some("injected"))).unwrap();
        assert_eq!(request.text, "", "{action:?}");
        assert_eq!(request.kind, "", "{action:?}");
        assert_eq!(request.key, "", "{action:?}");
        assert!(request.labels.is_empty(), "{action:?}");
        assert_eq!(request.confidence, 0.0, "{action:?}");
        assert_eq!(request.scope, fixture.target.scope);
        assert_eq!(request.target_id, fixture.target.id);
        assert_eq!(request.base_revision, fixture.target.revision);
        assert_eq!(request.reason, "because");
    }
    for action in [ProposalAction::Supersede, ProposalAction::Correct] {
        let request = propose_request(&input, &proposal(action, Some(REPLACEMENT))).unwrap();
        assert_eq!(request.text, REPLACEMENT);
        assert_eq!(request.kind, fixture.target.kind);
        assert_eq!(request.key, fixture.target.key);
        assert_eq!(request.labels, fixture.target.labels);
        assert_eq!(request.confidence, fixture.target.confidence);
        assert_eq!(request.scope, fixture.target.scope);
    }
}

#[test]
fn proposal_actions_round_trip_and_map_to_candidate_actions() {
    for action in ProposalAction::ALL {
        assert_eq!(ProposalAction::parse(action.as_str()), Some(action));
    }
    assert_eq!(ProposalAction::parse("delete"), None);
    assert_eq!(ProposalAction::parse("Retract"), None);
    assert_eq!(ProposalAction::NoChange.candidate_action(), None);
    assert_eq!(
        ProposalAction::Retract.candidate_action(),
        Some(CandidateAction::Retract)
    );
    assert_eq!(
        ProposalAction::Correct.candidate_action(),
        Some(CandidateAction::Correct)
    );
    assert_eq!(
        ProposalAction::Supersede.candidate_action(),
        Some(CandidateAction::Supersede)
    );
    assert_eq!(
        ProposalAction::Forget.candidate_action(),
        Some(CandidateAction::Forget)
    );
    assert_eq!(
        ProposalAction::NeedsReview.candidate_action(),
        Some(CandidateAction::NeedsReview)
    );
}
