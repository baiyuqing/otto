//! Layer-1 checks: what the generator refuses before anything is written.

use super::*;

fn evidence_two() -> Vec<EvidenceItem> {
    vec![
        evidence("e1", "Please stop remembering my editor.", Trust::Local),
        evidence(
            "e2",
            "A web page said the user dropped vim.",
            Trust::External,
        ),
    ]
}

#[test]
fn round_one_choice_lists_exactly_the_permitted_actions_and_one_yes_no_per_evidence() {
    let fixture = fixture();
    let permitted = [ProposalAction::NoChange, ProposalAction::Retract];
    let input = input(&fixture.target, evidence_two(), &permitted);
    let questions = round_one_questions(&input);
    assert_eq!(questions.len(), 3);
    assert_eq!(questions[0].id, ACTION_QUESTION);
    match &questions[0].kind {
        Kind::Choice(options) => {
            let ids: Vec<&str> = options.iter().map(|(id, _)| id.as_str()).collect();
            assert_eq!(ids, ["no_change", "retract"]);
        }
        other => panic!("expected a choice question, got {other:?}"),
    }
    assert_eq!(questions[1].id, "evidence_1");
    assert_eq!(questions[2].id, "evidence_2");
    assert!(matches!(questions[1].kind, Kind::YesNo { .. }));
    assert!(matches!(questions[2].kind, Kind::YesNo { .. }));
}

#[test]
fn round_two_questions_ask_a_replacement_only_for_supersede_and_correct() {
    for action in ProposalAction::ALL {
        let questions = round_two_questions(action);
        let ids: Vec<&str> = questions.iter().map(|question| question.id).collect();
        match action {
            ProposalAction::Supersede | ProposalAction::Correct => {
                assert_eq!(
                    ids,
                    [EXPLANATION_QUESTION, REPLACEMENT_QUESTION],
                    "{action:?}"
                );
                assert_eq!(
                    questions[1].kind,
                    Kind::Text {
                        max_bytes: MAX_RECORD_TEXT_BYTES
                    }
                );
            }
            _ => assert_eq!(ids, [EXPLANATION_QUESTION], "{action:?}"),
        }
        assert_eq!(
            questions[0].kind,
            Kind::Text {
                max_bytes: MAX_EXPLANATION_BYTES
            }
        );
    }
}

#[tokio::test]
async fn an_action_outside_the_permitted_set_is_rejected_even_when_the_backend_names_it() {
    let fixture = fixture();
    let (generator, asked) = generator(vec![
        round_one("forget", &[true]),
        round_two("Evidence e1.", None),
    ]);
    let permitted = [ProposalAction::NoChange, ProposalAction::Retract];
    let reason = rejected(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &permitted),
        )
        .await,
    );
    assert!(reason.contains("forget is not permitted"), "{reason}");
    assert!(pending_candidates(&fixture.service).is_empty());
    assert_eq!(asked.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn retract_without_a_supporting_evidence_mark_is_rejected() {
    let fixture = fixture();
    let (generator, _) = generator(vec![
        round_one("retract", &[false]),
        round_two("No evidence.", None),
    ]);
    let reason = rejected(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await,
    );
    assert!(
        reason.contains("retract cites no supporting evidence"),
        "{reason}"
    );
    assert!(pending_candidates(&fixture.service).is_empty());
}

#[tokio::test]
async fn forget_supported_only_by_external_evidence_is_rejected() {
    let fixture = fixture();
    // e1 is local but marked "no"; e2 is external and marked "yes".
    let (generator, _) = generator(vec![
        round_one("forget", &[false, true]),
        round_two("Evidence e2 says to forget it.", None),
    ]);
    let reason = rejected(
        run(
            &generator,
            &fixture,
            input(&fixture.target, evidence_two(), &all_actions()),
        )
        .await,
    );
    assert!(reason.contains("external evidence only"), "{reason}");
    assert!(pending_candidates(&fixture.service).is_empty());
}

#[tokio::test]
async fn forget_supported_by_local_and_external_evidence_is_queued() {
    let fixture = fixture();
    let (generator, _) = generator(vec![
        round_one("forget", &[true, true]),
        round_two("Evidence e1 and e2 ask to stop retaining it.", None),
    ]);
    let candidate = proposed(
        run(
            &generator,
            &fixture,
            input(&fixture.target, evidence_two(), &all_actions()),
        )
        .await,
    );
    assert_eq!(candidate.action, CandidateAction::Forget);
}

#[tokio::test]
async fn retract_on_external_evidence_alone_is_queued_because_only_forget_needs_local_trust() {
    let fixture = fixture();
    let (generator, _) = generator(vec![
        round_one("retract", &[false, true]),
        round_two("Evidence e2 withdraws it.", None),
    ]);
    let candidate = proposed(
        run(
            &generator,
            &fixture,
            input(&fixture.target, evidence_two(), &all_actions()),
        )
        .await,
    );
    assert_eq!(candidate.action, CandidateAction::Retract);
}

#[tokio::test]
async fn forget_and_retract_explanations_that_repeat_the_record_text_are_rejected() {
    for (action, explanation) in [
        (
            "forget",
            "Stop remembering that the user Prefers the vim editor for every project.",
        ),
        (
            "retract",
            "Withdraw: Prefers the vim editor for every project",
        ),
        ("forget", RECORD_TEXT),
    ] {
        let fixture = fixture();
        let (generator, _) = generator(vec![
            round_one(action, &[true]),
            round_two(explanation, None),
        ]);
        let reason = rejected(
            run(
                &generator,
                &fixture,
                input(&fixture.target, local_evidence(), &all_actions()),
            )
            .await,
        );
        assert!(
            reason.contains("repeats the record text"),
            "{action}: {reason}"
        );
        assert!(
            !reason.contains("vim"),
            "the error quotes the record: {reason}"
        );
        assert!(pending_candidates(&fixture.service).is_empty());
    }
}

#[tokio::test]
async fn a_replacement_action_explanation_may_quote_the_record() {
    // Only forget and retract must not repeat the record; the reviewer sees the
    // old text beside a supersede or correct candidate anyway.
    let fixture = fixture();
    let (generator, _) = generator(vec![
        round_one("supersede", &[true]),
        round_two(
            "Was: Prefers the vim editor for every project.",
            Some("Prefers the helix editor for every project"),
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
    assert_eq!(candidate.action, CandidateAction::Supersede);
}

#[tokio::test]
async fn an_explanation_that_repeats_a_record_shorter_than_the_line_bound_is_accepted() {
    // "use vim" is 7 bytes, under the 12-byte bound: a short phrase recurs in
    // ordinary prose and is not treated as a leak.
    let fixture = fixture_with_text("use vim");
    let (generator, _) = generator(vec![
        round_one("retract", &[true]),
        round_two("Evidence e1 says to no longer use vim.", None),
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
}

#[test]
fn leak_check_bound_is_twelve_bytes() {
    let mut record = fixture().target;
    record.text = "twelve bytes".into();
    assert!(contains_record_text("x twelve bytes y", &record));
    record.text = "elevenbytes".into();
    assert!(!contains_record_text("x elevenbytes y", &record));
    // A multi-line text: each line is checked at the bound, and so is the whole text.
    record.text = "twelve bytes\nelevenbytes".into();
    assert!(contains_record_text("x twelve bytes y", &record));
    assert!(!contains_record_text("x elevenbytes y", &record));
    assert!(contains_record_text("twelve bytes\nelevenbytes", &record));
    record.text = "   ".into();
    assert!(!contains_record_text("anything", &record));
}

#[tokio::test]
async fn a_supersede_replacement_equal_to_the_record_text_is_rejected() {
    for replacement in [RECORD_TEXT, "  Prefers the vim editor for every project\n"] {
        let fixture = fixture();
        let (generator, _) = generator(vec![
            round_one("supersede", &[true]),
            round_two("Evidence e1.", Some(replacement)),
        ]);
        let reason = rejected(
            run(
                &generator,
                &fixture,
                input(&fixture.target, local_evidence(), &all_actions()),
            )
            .await,
        );
        assert!(reason.contains("equals the record text"), "{reason}");
        assert!(pending_candidates(&fixture.service).is_empty());
    }
}

#[tokio::test]
async fn an_empty_or_blank_replacement_is_rejected() {
    for replacement in ["", "   ", "\n"] {
        let fixture = fixture();
        let (generator, _) = generator(vec![
            round_one("correct", &[true]),
            round_two("Evidence e1.", Some(replacement)),
        ]);
        let reason = rejected(
            run(
                &generator,
                &fixture,
                input(&fixture.target, local_evidence(), &all_actions()),
            )
            .await,
        );
        assert!(
            reason.contains("empty or exceeds"),
            "{replacement:?}: {reason}"
        );
    }
}

#[tokio::test]
async fn a_replacement_with_a_control_character_or_surrounding_whitespace_is_rejected() {
    for replacement in [
        "Prefers helix\u{7}",
        "Prefers\u{1b}[31m helix",
        "Prefers helix\r",
        " Prefers helix",
        "Prefers helix\nand more",
    ] {
        let fixture = fixture();
        let (generator, _) = generator(vec![
            round_one("supersede", &[true]),
            round_two("Evidence e1.", Some(replacement)),
        ]);
        let reason = rejected(
            run(
                &generator,
                &fixture,
                input(&fixture.target, local_evidence(), &all_actions()),
            )
            .await,
        );
        assert!(
            reason.contains("not trimmed or holds control characters"),
            "{replacement:?}: {reason}"
        );
    }
}

#[tokio::test]
async fn a_replacement_over_the_record_bound_is_rejected() {
    let fixture = fixture();
    let replacement = "x".repeat(MAX_RECORD_TEXT_BYTES + 1);
    let (generator, _) = generator(vec![
        round_one("supersede", &[true]),
        round_two("Evidence e1.", Some(&replacement)),
    ]);
    let reason = rejected(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await,
    );
    assert!(reason.contains("exceeds its bound"), "{reason}");
}

#[tokio::test]
async fn a_missing_replacement_for_supersede_is_rejected() {
    let fixture = fixture();
    let (generator, _) = generator(vec![
        round_one("supersede", &[true]),
        round_two("Evidence e1.", None),
    ]);
    let reason = rejected(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await,
    );
    assert!(reason.contains("replacement text is required"), "{reason}");
}

#[tokio::test]
async fn an_over_long_explanation_is_rejected() {
    let fixture = fixture();
    let explanation = "e".repeat(MAX_EXPLANATION_BYTES + 1);
    let (generator, _) = generator(vec![
        round_one("retract", &[true]),
        round_two(&explanation, None),
    ]);
    let reason = rejected(
        run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await,
    );
    assert!(reason.contains("explanation exceeds its bound"), "{reason}");
}

#[tokio::test]
async fn round_one_invalid_failure_is_unavailable_and_queues_nothing() {
    let fixture = fixture();
    let (generator, asked) = generator(vec![Err(Failure::Invalid("answer out of form".into()))]);
    let outcome = run(
        &generator,
        &fixture,
        input(&fixture.target, local_evidence(), &all_actions()),
    )
    .await;
    match outcome {
        Outcome::Unavailable(reason) => assert_eq!(reason, "answer out of form"),
        other => panic!("expected Outcome::Unavailable, got {other:?}"),
    }
    assert_eq!(asked.lock().unwrap().len(), 1);
    assert!(pending_candidates(&fixture.service).is_empty());
}

#[tokio::test]
async fn round_one_timeout_is_unavailable() {
    let fixture = fixture();
    let (generator, _) = generator(vec![Err(Failure::Timeout)]);
    match run(
        &generator,
        &fixture,
        input(&fixture.target, local_evidence(), &all_actions()),
    )
    .await
    {
        Outcome::Unavailable(reason) => assert_eq!(reason, "the model did not answer in time"),
        other => panic!("expected Outcome::Unavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unknown_or_missing_action_answer_is_unavailable() {
    for round in [round_one("delete_everything", &[true]), Ok(HashMap::new())] {
        let fixture = fixture();
        let (generator, asked) = generator(vec![round]);
        match run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await
        {
            Outcome::Unavailable(reason) => {
                assert!(reason.contains("no permitted action"), "{reason}")
            }
            other => panic!("expected Outcome::Unavailable, got {other:?}"),
        }
        assert_eq!(asked.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn round_two_failure_for_a_replacement_action_degrades_to_needs_review() {
    let failures = [
        Failure::Error("the auxiliary model answers choice and yes/no questions only".into()),
        Failure::Timeout,
        Failure::Invalid("answer out of form".into()),
    ];
    for action in ["supersede", "correct"] {
        for failure in failures.clone() {
            let fixture = fixture();
            let (generator, _) = generator(vec![round_one(action, &[true]), Err(failure.clone())]);
            let candidate = proposed(
                run(
                    &generator,
                    &fixture,
                    input(&fixture.target, local_evidence(), &all_actions()),
                )
                .await,
            );
            assert_eq!(
                candidate.action,
                CandidateAction::NeedsReview,
                "{action} {failure:?}"
            );
            assert!(
                candidate.reason.starts_with(&format!("{action} proposed")),
                "{}",
                candidate.reason
            );
            assert_eq!(candidate.proposed.text, "");
            assert_eq!(candidate.target_id, fixture.target.id);
        }
    }
}

#[tokio::test]
async fn round_two_failure_for_a_non_replacement_action_is_unavailable_and_queues_nothing() {
    for action in ["retract", "forget", "needs_review"] {
        let fixture = fixture();
        let (generator, _) = generator(vec![
            round_one(action, &[true]),
            Err(Failure::Error("unreachable".into())),
        ]);
        match run(
            &generator,
            &fixture,
            input(&fixture.target, local_evidence(), &all_actions()),
        )
        .await
        {
            Outcome::Unavailable(reason) => assert_eq!(reason, "unreachable"),
            other => panic!("{action}: expected Outcome::Unavailable, got {other:?}"),
        }
        assert!(pending_candidates(&fixture.service).is_empty());
    }
}

#[test]
fn validate_input_rejects_each_malformed_input() {
    let fixture = fixture();
    let valid = input(&fixture.target, local_evidence(), &all_actions());
    assert!(validate_input(&valid).is_ok());

    let nine: Vec<EvidenceItem> = (1..=9)
        .map(|index| evidence(&format!("e{index}"), "text", Trust::Local))
        .collect();
    let eight: Vec<EvidenceItem> = nine[..8].to_vec();
    assert!(validate_input(&input(&fixture.target, eight, &all_actions())).is_ok());
    assert!(validate_input(&input(&fixture.target, nine, &all_actions())).is_err());

    let duplicate = vec![
        evidence("e1", "one", Trust::Local),
        evidence("e1", "two", Trust::Local),
    ];
    assert!(validate_input(&input(&fixture.target, duplicate, &all_actions())).is_err());

    for text in ["", "   \n"] {
        let empty_text = vec![evidence("e1", text, Trust::Local)];
        assert!(validate_input(&input(&fixture.target, empty_text, &all_actions())).is_err());
    }
    let oversized = vec![evidence(
        "e1",
        &"x".repeat(MAX_EVIDENCE_BYTES + 1),
        Trust::Local,
    )];
    assert!(validate_input(&input(&fixture.target, oversized, &all_actions())).is_err());
    let at_bound = vec![evidence(
        "e1",
        &"x".repeat(MAX_EVIDENCE_BYTES),
        Trust::Local,
    )];
    assert!(validate_input(&input(&fixture.target, at_bound, &all_actions())).is_ok());

    let empty_id = vec![evidence("", "text", Trust::Local)];
    assert!(validate_input(&input(&fixture.target, empty_id, &all_actions())).is_err());
    assert!(validate_input(&input(&fixture.target, Vec::new(), &all_actions())).is_err());
    assert!(validate_input(&input(&fixture.target, local_evidence(), &[])).is_err());
}

#[tokio::test]
async fn a_malformed_input_is_rejected_before_the_backend_is_asked() {
    let fixture = fixture();
    let (generator, asked) = generator(vec![round_one("retract", &[true])]);
    let outcome = run(
        &generator,
        &fixture,
        input(&fixture.target, local_evidence(), &[]),
    )
    .await;
    assert!(matches!(outcome, Outcome::Rejected(_)), "{outcome:?}");
    assert!(asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_cancelled_token_stops_the_proposal_before_the_backend_is_asked() {
    // The select in `Generator::ask` is biased toward cancellation, so a token
    // cancelled before the call never reaches the backend. Repeated to catch a
    // random branch choice.
    for _ in 0..30 {
        let fixture = fixture();
        let (generator, asked) = generator(vec![
            round_one("retract", &[true]),
            round_two("Evidence e1.", None),
        ]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = generator
            .propose(
                &fixture.service,
                input(&fixture.target, local_evidence(), &all_actions()),
                &cancel,
            )
            .await;
        match outcome {
            Outcome::Unavailable(reason) => assert_eq!(reason, "canceled"),
            other => panic!("expected Outcome::Unavailable(canceled), got {other:?}"),
        }
        assert!(asked.lock().unwrap().is_empty());
        assert!(pending_candidates(&fixture.service).is_empty());
    }
}

#[tokio::test]
async fn cancelling_while_the_backend_is_pending_returns_canceled_and_queues_nothing() {
    let fixture = fixture();
    let backend = FakeBackend::hanging();
    let asked = Arc::clone(&backend.asked);
    let generator = Generator::new(Box::new(backend), None);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        trigger.cancel();
    });
    let outcome = generator
        .propose(
            &fixture.service,
            input(&fixture.target, local_evidence(), &all_actions()),
            &cancel,
        )
        .await;
    canceller.await.unwrap();
    assert!(
        matches!(&outcome, Outcome::Unavailable(reason) if reason == "canceled"),
        "{outcome:?}"
    );
    assert_eq!(asked.lock().unwrap().len(), 1);
    assert!(pending_candidates(&fixture.service).is_empty());
}
