//! Parser rejections: one assertion per consistency rule.

use super::super::*;
use super::base_item;
use crate::memory::proposals::ProposalAction;

fn line(item: &LabeledItem) -> String {
    serde_json::to_string(item).expect("serialize item")
}

fn parse_error(items: &[LabeledItem]) -> String {
    let jsonl: Vec<String> = items.iter().map(line).collect();
    parse_labeled_items(&jsonl.join("\n")).expect_err("dataset must be rejected")
}

fn supersede_item() -> LabeledItem {
    let mut item = base_item();
    item.label.expected_actions = vec![ProposalAction::Supersede];
    item.label.replacement_must_contain = vec!["helix".into()];
    item.label.reference_replacement = Some("Prefers the helix editor for every project".into());
    item
}

#[test]
fn accepts_a_consistent_item_and_skips_blank_lines() {
    let jsonl = format!(
        "\n{}\n\n{}\n",
        line(&base_item()),
        line(&supersede_item_named("second", "m-91-seco"))
    );
    assert_eq!(parse_labeled_items(&jsonl).expect("parses").len(), 2);
}

fn supersede_item_named(name: &str, marker: &str) -> LabeledItem {
    let mut item = supersede_item();
    item.record.text = item.record.text.replace(&item.record.marker, marker);
    item.record.marker = marker.into();
    item.name = name.into();
    item
}

#[test]
fn rejects_duplicate_name() {
    assert!(parse_error(&[base_item(), base_item()]).contains("duplicate name"));
}

#[test]
fn rejects_marker_absent_from_text() {
    let mut item = base_item();
    item.record.text = "Prefers the vim editor for every project".into();
    assert!(parse_error(&[item]).contains("marker"));
}

#[test]
fn rejects_unknown_supporting_evidence() {
    let mut item = base_item();
    item.label.supporting_evidence = vec!["e9".into()];
    assert!(parse_error(&[item]).contains("supporting evidence e9 does not exist"));
}

#[test]
fn rejects_expected_action_outside_permitted() {
    let mut item = base_item();
    item.permitted = vec![ProposalAction::NoChange];
    assert!(parse_error(&[item]).contains("retract is not permitted"));
}

#[test]
fn rejects_supersede_without_reference_replacement() {
    let mut item = supersede_item();
    item.label.reference_replacement = None;
    assert!(parse_error(&[item]).contains("reference_replacement is required"));
}

#[test]
fn rejects_reference_replacement_with_forbidden_term() {
    let mut item = supersede_item();
    item.label.replacement_must_not_contain = vec!["editor".into()];
    assert!(parse_error(&[item]).contains("forbidden term"));
}

#[test]
fn rejects_uppercase_term() {
    let mut item = base_item();
    item.label.inversion_terms = vec!["Dislikes".into()];
    assert!(parse_error(&[item]).contains("must be lowercase"));
}

#[test]
fn rejects_reference_replacement_on_a_non_replacing_label() {
    let mut item = base_item();
    item.label.reference_replacement = Some("Something else".into());
    assert!(parse_error(&[item]).contains("allowed only with supersede or correct"));
}

#[test]
fn malformed_json_error_names_the_line_number() {
    let jsonl = format!("{}\n{{\"name\": \n", line(&base_item()));
    let error = parse_labeled_items(&jsonl).expect_err("malformed line");
    assert!(error.starts_with("line 2:"), "{error}");
}
