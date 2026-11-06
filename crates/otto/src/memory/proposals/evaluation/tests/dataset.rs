//! The shipped dataset: it parses, covers every category and its labels pass
//! the scorer and layer 1 when answered as labeled.

use std::collections::BTreeMap;

use super::super::*;
use crate::memory::proposals::{Proposal, ProposalAction, validate_input};

const LABELED_JSONL: &str =
    include_str!("../../../../../../../testdata/memory-eval/proposals/labeled.jsonl");
const CATEGORIES: [&str; 10] = [
    "distinction",
    "validity",
    "override",
    "partial",
    "scope",
    "conflict",
    "ambiguity",
    "duplicate",
    "adversarial",
    "valid",
];

fn items() -> Vec<LabeledItem> {
    parse_labeled_items(LABELED_JSONL).expect("labeled dataset parses")
}

#[test]
fn dataset_parses_with_expected_size_and_categories() {
    let items = items();
    assert!((24..=28).contains(&items.len()), "{} items", items.len());
    let mut per_category: BTreeMap<&str, usize> = BTreeMap::new();
    for item in &items {
        *per_category.entry(item.category.as_str()).or_default() += 1;
    }
    for category in CATEGORIES {
        let count = per_category.get(category).copied().unwrap_or(0);
        assert!(count >= 2, "category {category} has {count} items");
    }
    assert!(per_category["distinction"] >= 4);
    assert!(per_category.keys().all(|name| CATEGORIES.contains(name)));
    assert!(LABELED_DATASET_PATH.ends_with("labeled.jsonl"));
    assert_eq!(DATASET_VERSION, "memory-proposal-labels/1");
}

#[test]
fn every_item_builds_a_valid_generator_input() {
    for item in items() {
        let input = item.input("memory-eval");
        validate_input(&input).unwrap_or_else(|error| panic!("{}: {error}", item.name));
        assert_eq!(input.target.id, format!("eval-{}", item.name));
        assert_eq!(input.target.text, item.record.text);
    }
}

#[test]
fn reference_proposals_score_clean_through_layer_one() {
    for item in items() {
        let action = item.label.expected_actions[0];
        let replacement = requires_replacement(action)
            .then(|| item.label.reference_replacement.clone())
            .flatten();
        let generated = Generated::Proposal(Proposal {
            action,
            evidence_refs: item.label.supporting_evidence.clone(),
            explanation: item.label.rationale.clone(),
            replacement,
        });
        let score = score(&item, &generated);
        let name = &item.name;
        assert!(score.action_correct, "{name}: action");
        assert_eq!(score.layer_one, LayerOne::Accepted, "{name}: layer one");
        assert!(!score.unsupported_inversion, "{name}: inversion");
        assert!(!score.scope_violation, "{name}: scope");
        assert_ne!(score.claims_preserved, Some(false), "{name}: claims");
        assert!(!score.cited_unsupporting, "{name}: cited unsupporting");
        assert!(!score.missed_supporting, "{name}: missed supporting");
        assert!(!score.leakage, "{name}: leakage");
        assert_ne!(
            score.abstention,
            Abstention::Excessive,
            "{name}: abstention"
        );
    }
}

#[test]
fn forget_item_marker_is_in_the_record_text() {
    let items = items();
    let forget = items
        .iter()
        .find(|item| {
            item.label
                .expected_actions
                .contains(&ProposalAction::Forget)
        })
        .expect("forget item");
    assert!(forget.record.text.contains(&forget.record.marker));
}
