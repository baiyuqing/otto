//! Tests for remember, forget, proposals and review.

use super::*;

#[test]
fn pending_summary_counts_pending_in_scope_and_finds_the_oldest() {
    let (_directory, service) = service();
    let user = user_scope();
    let here = Scope::new(NAMESPACE_WORKSPACE, "here");
    let elsewhere = Scope::new(NAMESPACE_WORKSPACE, "elsewhere");
    let scopes = [user.clone(), here.clone()];
    assert_eq!(
        service.pending_summary(&scopes).expect("empty"),
        PendingSummary::default()
    );
    let first = service
        .propose(&model_proposal(&user, "a", "one"))
        .expect("a")
        .remove(0);
    std::thread::sleep(std::time::Duration::from_millis(5));
    service
        .propose(&model_proposal(&here, "b", "two"))
        .expect("b");
    service
        .propose(&model_proposal(&elsewhere, "c", "three"))
        .expect("c");
    let accepted = service
        .propose(&model_proposal(&here, "d", "four"))
        .expect("d")
        .remove(0);
    service
        .review(&ReviewRequest {
            reference: CandidateRef {
                scope: here,
                id: accepted.id,
            },
            decision: ReviewDecision::Accept,
            edited: None,
            target_revision: None,
        })
        .expect("accept");
    let summary = service.pending_summary(&scopes).expect("summary");
    assert_eq!(summary.count, 2);
    assert_eq!(summary.oldest, Some(first.created_at));
}

/// A keyed remember updates in place whether or not a revision is given,
/// and never leaves a duplicate behind.
#[test]
fn a_keyed_remember_creates_once_and_then_updates_in_place() {
    let (_directory, service) = service();
    let scope = user_scope();

    let created = service
        .remember(&remember(&scope, "editor", "vim"))
        .expect("create");
    assert_eq!(created.revision, 1);
    assert!(!created.id.is_empty());
    assert_eq!(created.source.origin, Some(Origin::Human));

    let conditional = service
        .remember(&RememberRequest {
            id: created.id.clone(),
            expected_revision: Some(created.revision),
            ..remember(&scope, "editor", "neovim")
        })
        .expect("conditional update");
    assert_eq!(
        (conditional.revision, conditional.text.as_str()),
        (2, "neovim")
    );
    assert_eq!(conditional.created_at, created.created_at);

    let by_key = service
        .remember(&remember(&scope, "editor", "emacs"))
        .expect("keyed update");
    assert_eq!(by_key.id, created.id);
    assert_eq!((by_key.revision, by_key.text.as_str()), (3, "emacs"));

    let listed = service
        .search(&SearchRequest {
            scopes: vec![scope],
            kinds: vec!["preference".into()],
            limit: 10,
            token_budget: 1000,
            now: Utc::now(),
            ..SearchRequest::default()
        })
        .expect("search");
    assert_eq!(
        listed.records.len(),
        1,
        "the keyed update duplicated the record"
    );
}

/// The guard runs inside the store, so this checks the service does not
/// route around it.
#[test]
fn a_remember_carrying_a_secret_is_refused() {
    let (_directory, service) = service();
    let error = service
        .remember(&RememberRequest {
            scope: user_scope(),
            kind: "secret".into(),
            text: "api_key: sk-abcdef123456".into(),
            ..RememberRequest::default()
        })
        .expect_err("a secret must not be stored");
    assert!(error.is(ErrorKind::SensitiveMemory), "error = {error}");
}

#[test]
fn forgetting_writes_a_tombstone_and_purging_backups_is_refused() {
    let (_directory, service) = service();
    let scope = user_scope();
    let created = service
        .remember(&remember(&scope, "editor", "vim"))
        .expect("create");
    let reference = RecordRef {
        scope: scope.clone(),
        id: created.id.clone(),
    };

    let purge = service
        .forget(&ForgetRequest {
            reference: reference.clone(),
            expected_revision: created.revision,
            purge_backups: true,
            confirm_purge: true,
        })
        .expect_err("purge is unsupported");
    assert!(purge.is(ErrorKind::Unsupported), "error = {purge}");

    let result = service
        .forget(&ForgetRequest {
            reference: reference.clone(),
            expected_revision: created.revision,
            purge_backups: false,
            confirm_purge: false,
        })
        .expect("forget");
    assert_eq!(result.tombstone.id, created.id);
    assert!(
        service
            .get(&reference)
            .expect_err("record is gone")
            .is(ErrorKind::NotFound)
    );
    assert_eq!(
        service.get_tombstone(&reference).expect("tombstone").id,
        created.id
    );
}

#[test]
fn reading_a_record_that_was_never_written_is_not_found() {
    let (_directory, service) = service();
    let reference = RecordRef {
        scope: user_scope(),
        id: "0123456789abcdef".into(),
    };
    assert!(
        service
            .get(&reference)
            .expect_err("no record")
            .is(ErrorKind::NotFound)
    );
}

#[tokio::test]
async fn a_successful_proposal_signals_review_availability_without_payload() {
    let (_directory, service) = service();
    let mut changed = service.review_available_changed();
    let candidates = service
        .propose(&model_proposal(
            &user_scope(),
            "editor",
            "private candidate text",
        ))
        .expect("proposal");

    changed.changed().await.expect("signal sender remains live");
    assert_eq!(*changed.borrow(), 1);
    assert_eq!(candidates.len(), 1);
}

#[test]
fn a_proposal_is_queued_only_when_the_policy_pends() {
    let (_directory, service) = service();
    let scope = user_scope();

    let human = service
        .propose(&ProposeRequest {
            source: Provenance {
                origin: Some(Origin::Human),
                ..Provenance::default()
            },
            ..model_proposal(&scope, "editor", "vim")
        })
        .expect_err("a human write is not a proposal");
    assert!(human.is(ErrorKind::InvalidRequest), "error = {human}");

    for decision in [PolicyDecision::Reject, PolicyDecision::Accept] {
        let (_directory, store) = open_temp();
        let fixed: Policy = match decision {
            PolicyDecision::Reject => |_| PolicyDecision::Reject,
            _ => |_| PolicyDecision::Accept,
        };
        let service = Service::new(store, fixed);
        let error = service
            .propose(&model_proposal(&scope, "editor", "vim"))
            .expect_err("a non-pending decision never queues");
        assert!(error.is(ErrorKind::PolicyDecision), "error = {error}");
        assert_eq!(
            error.to_string(),
            format!(
                "memory proposal was not queued for review: policy decision {:?}",
                decision.as_str()
            )
        );
    }
}

#[test]
fn an_accepted_proposal_becomes_a_record_and_a_rejected_one_does_not() {
    let (_directory, service) = service();
    let scope = user_scope();

    let queued = service
        .propose(&model_proposal(&scope, "editor", "vim"))
        .expect("propose");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].state, CandidateState::Pending);

    let accepted = service
        .review(&ReviewRequest {
            reference: CandidateRef {
                scope: scope.clone(),
                id: queued[0].id.clone(),
            },
            decision: ReviewDecision::Accept,
            edited: None,
            target_revision: None,
        })
        .expect("accept");
    let record = accepted.record.expect("an accepted create writes a record");
    assert_eq!(record.text, "vim");
    assert_eq!(accepted.candidate.decision_source, Some(Origin::Human));
    assert_eq!(
        service
            .get(&RecordRef {
                scope: scope.clone(),
                id: record.id
            })
            .expect("read")
            .text,
        "vim"
    );

    let second = service
        .propose(&model_proposal(&scope, "shell", "fish"))
        .expect("propose");
    let rejected = service
        .review(&ReviewRequest {
            reference: CandidateRef {
                scope: scope.clone(),
                id: second[0].id.clone(),
            },
            decision: ReviewDecision::Reject,
            edited: None,
            target_revision: None,
        })
        .expect("reject");
    assert!(
        rejected.record.is_none(),
        "a rejected candidate must write no record"
    );
    assert_eq!(rejected.candidate.state, CandidateState::Rejected);
}

#[test]
fn core_record_reports_the_store_revision_as_a_decimal_string() {
    let record = Record {
        id: "rec-1".into(),
        revision: 7,
        ..Record::default()
    };
    assert_eq!(
        core_record(record.clone(), false).revision.as_deref(),
        Some("7")
    );
    assert_eq!(core_record(record, true).revision.as_deref(), Some("7"));
}

#[test]
fn reviewing_a_supersede_or_correct_mints_the_replacement_id_and_retires_the_target() {
    for (action, retired_state) in [
        (
            CandidateAction::Supersede,
            crate::memory::RecordState::Superseded,
        ),
        (
            CandidateAction::Correct,
            crate::memory::RecordState::Retracted,
        ),
    ] {
        let (_directory, service) = service();
        let scope = user_scope();
        let target = service
            .remember(&remember(&scope, "editor", "vim"))
            .expect("target");
        let queued = service
            .propose(&ProposeRequest {
                action,
                target_id: target.id.clone(),
                base_revision: target.revision,
                ..model_proposal(&scope, "editor", "neovim")
            })
            .expect("propose");
        let reviewed = service
            .review(&ReviewRequest {
                reference: CandidateRef {
                    scope: scope.clone(),
                    id: queued[0].id.clone(),
                },
                decision: ReviewDecision::Accept,
                edited: None,
                target_revision: None,
            })
            .expect("accept");

        let replacement = reviewed
            .record
            .expect("an accepted replacement writes a record");
        assert!(!replacement.id.is_empty(), "{action:?}");
        assert_ne!(replacement.id, target.id, "{action:?}");
        assert_eq!(replacement.text, "neovim");
        assert_eq!(reviewed.candidate.result_record_id, replacement.id);
        assert_eq!(reviewed.candidate.result_revision, 1);
        let retired = service
            .get(&RecordRef {
                scope: scope.clone(),
                id: target.id.clone(),
            })
            .expect("target stays readable");
        assert_eq!(retired.state, retired_state, "{action:?}");
        assert_eq!(retired.revision, target.revision + 1);
    }
}
