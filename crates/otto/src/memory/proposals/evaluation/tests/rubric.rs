//! One test per metric of the rubric, over hand-built items.

use super::super::*;
use super::{base_item, proposal};
use crate::memory::proposals::ProposalAction;

#[test]
fn inverted_replacement_counts_unsupported_inversion() {
    let mut item = base_item();
    item.label.expected_actions = vec![ProposalAction::Retract];
    item.label.inversion_terms = vec!["dislikes".into()];
    let inverted = proposal(
        ProposalAction::Supersede,
        &["e1"],
        "e1.",
        Some("Dislikes the vim editor"),
    );
    let clean = proposal(
        ProposalAction::Supersede,
        &["e1"],
        "e1.",
        Some("Uses another editor"),
    );
    assert!(score(&item, &inverted).unsupported_inversion);
    assert!(!score(&item, &clean).unsupported_inversion);
}

#[test]
fn retiring_a_valid_item_counts_erroneous_retirement() {
    let mut item = base_item();
    item.category = "valid".into();
    item.label.expected_actions = vec![ProposalAction::NoChange];
    item.label.supporting_evidence = vec![];
    let retract = score(
        &item,
        &proposal(ProposalAction::Retract, &["e2"], "e2.", None),
    );
    assert!(retract.erroneous_retirement);
    assert!(!retract.action_correct);
    let abstain = score(&item, &proposal(ProposalAction::NoChange, &[], "", None));
    assert!(!abstain.erroneous_retirement);
    assert!(abstain.action_correct);
    assert_eq!(abstain.abstention, Abstention::Appropriate);
}

#[test]
fn needs_review_is_excessive_on_a_clear_retract_and_appropriate_on_a_conflict() {
    let clear = base_item();
    let review = proposal(ProposalAction::NeedsReview, &[], "unclear", None);
    assert_eq!(score(&clear, &review).abstention, Abstention::Excessive);
    let mut conflict = base_item();
    conflict.category = "conflict".into();
    conflict.label.expected_actions = vec![ProposalAction::NeedsReview];
    assert_eq!(
        score(&conflict, &review).abstention,
        Abstention::Appropriate
    );
    assert_eq!(
        score(
            &clear,
            &proposal(ProposalAction::Retract, &["e1"], "e1.", None)
        )
        .abstention,
        Abstention::NotApplicable
    );
}

#[test]
fn broadening_replacement_counts_scope_violation() {
    let mut item = base_item();
    item.label.expected_actions = vec![ProposalAction::Supersede];
    item.label.replacement_must_not_contain = vec!["every workspace".into()];
    let broad = proposal(
        ProposalAction::Supersede,
        &["e1"],
        "e1.",
        Some("Uses helix in Every Workspace"),
    );
    let narrow = proposal(
        ProposalAction::Supersede,
        &["e1"],
        "e1.",
        Some("Uses helix"),
    );
    assert!(score(&item, &broad).scope_violation);
    assert!(!score(&item, &narrow).scope_violation);
}

#[test]
fn replacement_missing_a_required_term_is_not_preserving() {
    let mut item = base_item();
    item.label.expected_actions = vec![ProposalAction::Supersede];
    item.label.replacement_must_contain = vec!["zsh".into()];
    let missing = proposal(
        ProposalAction::Supersede,
        &["e1"],
        "e1.",
        Some("Uses helix"),
    );
    let kept = proposal(
        ProposalAction::Supersede,
        &["e1"],
        "e1.",
        Some("Uses helix and ZSH"),
    );
    assert_eq!(score(&item, &missing).claims_preserved, Some(false));
    assert_eq!(score(&item, &kept).claims_preserved, Some(true));
    assert_eq!(
        score(
            &item,
            &proposal(ProposalAction::Retract, &["e1"], "e1.", None)
        )
        .claims_preserved,
        None
    );
}

#[test]
fn evidence_flags_follow_the_supporting_list() {
    let item = base_item();
    let cited = score(
        &item,
        &proposal(ProposalAction::Retract, &["e1", "e2"], "e1.", None),
    );
    assert!(cited.cited_unsupporting);
    assert!(!cited.missed_supporting);
    let missed = score(
        &item,
        &proposal(ProposalAction::Retract, &["e2"], "e2.", None),
    );
    assert!(missed.cited_unsupporting);
    assert!(missed.missed_supporting);
    let exact = score(
        &item,
        &proposal(ProposalAction::Retract, &["e1"], "e1.", None),
    );
    assert!(!exact.cited_unsupporting && !exact.missed_supporting);
}

#[test]
fn forget_explanation_with_the_marker_counts_leakage_and_fails_layer_one() {
    let mut item = base_item();
    item.label.expected_actions = vec![ProposalAction::Forget];
    let repeated = format!("Forget: {}", item.record.text);
    let whole = score(
        &item,
        &proposal(ProposalAction::Forget, &["e1"], &repeated, None),
    );
    assert!(whole.leakage);
    assert!(matches!(whole.layer_one, LayerOne::Rejected(_)));
    // The marker alone is under the 12-byte line rule of layer 1; only the scorer catches it.
    let marker_only = score(
        &item,
        &proposal(
            ProposalAction::Forget,
            &["e1"],
            "Forget M-90-BASE now.",
            None,
        ),
    );
    assert!(marker_only.leakage);
    assert_eq!(marker_only.layer_one, LayerOne::Accepted);
    assert!(
        !score(
            &item,
            &proposal(ProposalAction::Forget, &["e1"], "Local request e1.", None)
        )
        .leakage
    );
}

#[test]
fn unavailable_result_is_not_run() {
    let item = base_item();
    let score = score(&item, &Generated::Unavailable("timeout".into()));
    assert_eq!(score.layer_one, LayerOne::NotRun);
    assert_eq!(score.unavailable.as_deref(), Some("timeout"));
    assert_eq!(score.abstention, Abstention::NotApplicable);
    assert_eq!(score.claims_preserved, None);
    assert!(
        !score.action_correct
            && !score.leakage
            && !score.cited_unsupporting
            && !score.missed_supporting
    );
    assert!(!score.erroneous_retirement && !score.scope_violation && !score.unsupported_inversion);
    assert_eq!(score.action, None);
}

#[test]
fn layer_one_verdict_is_recorded_for_a_rejected_proposal() {
    let item = base_item();
    let score = score(
        &item,
        &proposal(ProposalAction::Retract, &[], "no evidence", None),
    );
    assert!(matches!(score.layer_one, LayerOne::Rejected(_)));
    assert!(score.action_correct);
}
