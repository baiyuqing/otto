//! Tests of the `--dry-run` estimate.

use otto::memory::proposals::evaluation::LabeledItem;

use super::harness_fixtures::item;
use crate::memory_eval_command::argument_parsing::{dry_run_items, parse};
use crate::memory_eval_command::conditions::AGENT_SYSTEM_PROMPT;
use crate::memory_eval_command::layer_two::labeled_items;

fn options(values: &[&str]) -> crate::memory_eval_command::Options {
    let arguments: Vec<String> = values.iter().map(|value| (*value).to_owned()).collect();
    parse(&arguments).unwrap().options
}

fn estimate(values: &[&str], layer_three_items: &[crate::memory_eval_command::Item]) -> String {
    let labeled = labeled_items().unwrap();
    let mut output = Vec::new();
    dry_run_items(layer_three_items, &labeled, &options(values), &mut output).unwrap();
    String::from_utf8(output).unwrap()
}

fn labeled_bytes(labeled: &[LabeledItem]) -> usize {
    labeled
        .iter()
        .map(|entry| {
            entry.record.text.len() + entry.evidence.iter().map(|e| e.text.len()).sum::<usize>()
        })
        .sum()
}

#[test]
fn layer_two_estimate_counts_two_rounds_per_labeled_item() {
    let labeled = labeled_items().unwrap();
    assert_eq!(labeled.len(), 26);
    let bytes = labeled_bytes(&labeled) * 2;
    let without_judge = estimate(&["--layer", "2", "--no-judge"], &[]);
    assert_eq!(
        without_judge,
        format!(
            "items: 0\nlabeled items: 26\nlayer 3 cases: 0\nlayer 3 provider calls (estimate): 0\n\
layer 2 provider calls (estimate): 52\nprovider calls total (estimate): 52\n\
task input bytes (estimate): {bytes}\n"
        )
    );
    let with_judge = estimate(&["--layer", "2"], &[]);
    assert!(
        with_judge.contains("layer 2 provider calls (estimate): 78\n"),
        "{with_judge}"
    );
    assert!(
        with_judge.contains("provider calls total (estimate): 78\n"),
        "{with_judge}"
    );
}

#[test]
fn layer_three_estimate_counts_cases_answers_judges_and_compactions() {
    // Two items: 156 and 133 bytes of text. Conditions A and D over native_recall and
    // summary_with_references: A runs for both variants, D for the summary variant only,
    // so each item has 3 cases and one of them compacts.
    let items = [
        item("with_replacement", "superseded", true, false),
        item("without_replacement", "retracted", false, true),
    ];
    let command = [
        "--layer",
        "3",
        "--conditions",
        "A,D",
        "--variants",
        "native_recall,summary_with_references",
    ];
    let prompt = AGENT_SYSTEM_PROMPT.len();
    let bytes = 3 * 2 * (156 + prompt) + 3 * 2 * (133 + prompt);

    let mut without_judge = command.to_vec();
    without_judge.push("--no-judge");
    assert_eq!(
        estimate(&without_judge, &items),
        format!(
            "items: 2\nlabeled items: 26\nlayer 3 cases: 6\nlayer 3 provider calls (estimate): 14\n\
layer 2 provider calls (estimate): 0\nprovider calls total (estimate): 14\n\
task input bytes (estimate): {bytes}\n"
        )
    );
    let with_judge = estimate(&command, &items);
    assert!(
        with_judge.contains("layer 3 provider calls (estimate): 20\n"),
        "{with_judge}"
    );
    assert!(
        with_judge.contains("provider calls total (estimate): 20\n"),
        "{with_judge}"
    );
}

#[test]
fn repeat_multiplies_cases_and_all_layers_add_up() {
    let items = [item("with_replacement", "superseded", true, false)];
    let output = estimate(
        &[
            "--conditions",
            "A",
            "--variants",
            "tool_result",
            "--repeat",
            "3",
            "--no-judge",
        ],
        &items,
    );
    // 3 cases x 2 answers = 6 layer 3 calls; 26 x 2 = 52 layer 2 calls.
    assert!(output.contains("layer 3 cases: 3\n"), "{output}");
    assert!(
        output.contains("layer 3 provider calls (estimate): 6\n"),
        "{output}"
    );
    assert!(
        output.contains("layer 2 provider calls (estimate): 52\n"),
        "{output}"
    );
    assert!(
        output.contains("provider calls total (estimate): 58\n"),
        "{output}"
    );
}

#[test]
fn condition_d_has_no_case_for_variants_without_a_summary() {
    let items = [item("with_replacement", "superseded", true, false)];
    let output = estimate(
        &[
            "--layer",
            "3",
            "--conditions",
            "D",
            "--variants",
            "tool_result,native_recall",
        ],
        &items,
    );
    assert!(output.contains("layer 3 cases: 0\n"), "{output}");
    assert!(
        output.contains("task input bytes (estimate): 0\n"),
        "{output}"
    );
}

#[test]
fn the_embedded_items_give_the_documented_selection_arithmetic() {
    let selection = options(&[
        "--layer",
        "3",
        "--items",
        "install_command_pnpm",
        "--conditions",
        "A,D",
        "--variants",
        "native_recall,summary_with_references",
        "--no-judge",
    ]);
    let mut output = Vec::new();
    crate::memory_eval_command::argument_parsing::dry_run(&selection, &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    // One item; A for two variants and D for one: 3 cases, 3 x 2 answers + 1 compaction.
    assert!(
        output.starts_with("items: 1\nlabeled items: 26\nlayer 3 cases: 3\n"),
        "{output}"
    );
    assert!(
        output.contains("layer 3 provider calls (estimate): 7\n"),
        "{output}"
    );
    assert!(
        output.contains("layer 2 provider calls (estimate): 0\n"),
        "{output}"
    );
}
