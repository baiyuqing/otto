//! Tests of the item file parser: the embedded file and one rejection per consistency rule.

use crate::memory_eval_command::items::{Item, Retirement, embedded_items, parse_items};

const ITEM_NAMES: [&str; 6] = [
    "install_command_pnpm",
    "staging_cluster_renamed",
    "api_timeout_seconds",
    "editor_preference_withdrawn",
    "deploy_window_withdrawn",
    "private_appointment_forgotten",
];

fn jsonl(items: &[Item]) -> String {
    items
        .iter()
        .map(|item| serde_json::to_string(item).expect("item serializes"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses the embedded items after `edit` changed the item at `index`; returns the error.
fn rejection(index: usize, edit: impl FnOnce(&mut Item)) -> String {
    let mut items = embedded_items().expect("embedded items");
    edit(&mut items[index]);
    parse_items(&jsonl(&items)).expect_err("the edited file must be rejected")
}

fn assert_rejected(index: usize, edit: impl FnOnce(&mut Item), expected: &[&str]) {
    let error = rejection(index, edit);
    for fragment in expected {
        assert!(error.contains(fragment), "{error:?} lacks {fragment:?}");
    }
}

#[test]
fn embedded_file_holds_the_six_items() {
    let items = embedded_items().expect("embedded items");
    let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
    assert_eq!(names, ITEM_NAMES);
    let retirements: Vec<Retirement> = items.iter().map(|item| item.retirement).collect();
    assert_eq!(
        retirements,
        [
            Retirement::Superseded,
            Retirement::Superseded,
            Retirement::Superseded,
            Retirement::Retracted,
            Retirement::Retracted,
            Retirement::Forgotten,
        ]
    );
}

#[test]
fn duplicate_name_is_rejected() {
    assert_rejected(
        1,
        |item| item.name = ITEM_NAMES[0].into(),
        &["duplicate name"],
    );
}

#[test]
fn duplicate_marker_is_rejected() {
    assert_rejected(
        1,
        |item| {
            let first = "m-30-npmi";
            item.record.text = item.record.text.replace(&item.record.marker, first);
            item.record.marker = first.into();
        },
        &["marker", "already used"],
    );
}

#[test]
fn malformed_marker_is_rejected() {
    assert_rejected(
        0,
        |item| {
            item.record.text = item.record.text.replace(&item.record.marker, "marker");
            item.record.marker = "marker".into();
        },
        &["install_command_pnpm", "record.marker"],
    );
}

#[test]
fn marker_absent_from_the_record_text_is_rejected() {
    assert_rejected(
        0,
        |item| item.record.text = "Project alpha installs with npm".into(),
        &[
            "install_command_pnpm",
            "record.text does not hold record.marker",
        ],
    );
}

#[test]
fn marker_in_a_copy_is_rejected() {
    for field in ["assistant", "tool_output", "summary"] {
        assert_rejected(
            0,
            |item| {
                let marker = item.record.marker.clone();
                let prior = &mut item.prior;
                match field {
                    "assistant" => prior.assistant.push_str(&marker),
                    "tool_output" => prior.tool_output.push_str(&marker),
                    _ => prior.summary.push_str(&marker),
                }
            },
            &[&format!("prior.{field} holds record.marker")],
        );
    }
}

#[test]
fn neutral_copy_with_an_obsolete_term_is_rejected() {
    assert_rejected(
        0,
        |item| item.prior.assistant_neutral = "Run npm install.".into(),
        &["prior.assistant_neutral holds the obsolete term"],
    );
}

#[test]
fn claim_copy_without_an_obsolete_term_is_rejected() {
    assert_rejected(
        0,
        |item| item.prior.tool_output = "Setup finished.".into(),
        &["prior.tool_output holds no obsolete term"],
    );
}

#[test]
fn superseded_without_replacement_is_rejected() {
    assert_rejected(
        0,
        |item| item.replacement = None,
        &["superseded item needs a replacement"],
    );
}

#[test]
fn replacement_on_a_retracted_item_is_rejected() {
    let replacement = embedded_items().expect("items")[0].replacement.clone();
    assert_rejected(
        3,
        |item| item.replacement = replacement,
        &["only a superseded item may have a replacement"],
    );
}

#[test]
fn empty_must_contain_any_with_a_replacement_is_rejected() {
    assert_rejected(
        0,
        |item| item.checks.must_contain_any.clear(),
        &["checks.must_contain_any is empty although a replacement exists"],
    );
}

#[test]
fn must_contain_any_without_a_replacement_is_rejected() {
    assert_rejected(
        3,
        |item| item.checks.must_contain_any = vec!["emacs".into()],
        &["checks.must_contain_any is not empty"],
    );
}

#[test]
fn uppercase_term_is_rejected() {
    assert_rejected(
        0,
        |item| item.obsolete_terms.push("NPM Install".into()),
        &["not lowercase"],
    );
    assert_rejected(
        0,
        |item| item.control.must_not_contain.push("PNPM".into()),
        &["control.must_not_contain", "not lowercase"],
    );
}

#[test]
fn control_scope_equal_to_the_record_scope_is_rejected() {
    assert_rejected(
        0,
        |item| item.control.record.scope_id = item.record.scope_id.clone(),
        &["control.record.scope_id equals record.scope_id"],
    );
}

#[test]
fn control_id_equal_to_the_record_or_replacement_id_is_rejected() {
    assert_rejected(
        0,
        |item| item.control.record.id = item.record.id.clone(),
        &["control.record.id equals"],
    );
    assert_rejected(
        0,
        |item| {
            item.control.record.id = item.replacement.as_ref().expect("replacement").id.clone();
        },
        &["control.record.id equals"],
    );
}

#[test]
fn empty_control_must_contain_any_is_rejected() {
    assert_rejected(
        0,
        |item| item.control.must_contain_any.clear(),
        &["control.must_contain_any is empty"],
    );
}

#[test]
fn obsolete_term_in_the_task_is_rejected() {
    assert_rejected(
        0,
        |item| item.task = "Is npm install still right for project alpha?".into(),
        &["task holds the obsolete term"],
    );
}

#[test]
fn malformed_line_names_its_number() {
    let items = embedded_items().expect("items");
    let mut text = jsonl(&items[..2]);
    text.push_str("\n\n{\"name\": \n");
    let error = parse_items(&text).expect_err("malformed line");
    assert!(error.contains("line 4"), "{error}");
}

#[test]
fn unknown_field_is_rejected() {
    let items = embedded_items().expect("items");
    let mut value = serde_json::to_value(&items[0]).expect("value");
    value["extra"] = serde_json::json!(1);
    let error = parse_items(&value.to_string()).expect_err("unknown field");
    assert!(
        error.contains("line 1") && error.contains("extra"),
        "{error}"
    );
}
