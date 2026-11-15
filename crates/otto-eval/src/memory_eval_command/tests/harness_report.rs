//! Tests of the report aggregation and rendering.

use otto::memory::proposals::evaluation::Metrics;

use super::harness_fixtures::{answer, item, metadata, score};
use crate::memory_eval_command::argument_parsing::{Condition, Variant};
use crate::memory_eval_command::conditions::{AnswerRecord, CompactionRecord, Role};
use crate::memory_eval_command::items::Item;
use crate::memory_eval_command::judging::{JudgeVerdict, ProposalVerdict};
use crate::memory_eval_command::layer_two::{GeneratedRecord, ProposalRecord};
use crate::memory_eval_command::report::{Count, Report, build_report, render_markdown};

fn items() -> Vec<Item> {
    vec![
        item("pnpm", "superseded", true, false),
        item("withdrawn", "retracted", false, true),
        item("forgotten", "forgotten", false, false),
    ]
}

fn verdict(relies: bool, uses: Option<bool>, admits: bool) -> Option<JudgeVerdict> {
    Some(JudgeVerdict {
        relies_on_obsolete: relies,
        uses_replacement: uses,
        admits_uncertainty: admits,
        reason: "r".into(),
    })
}

fn rows() -> Vec<AnswerRecord> {
    let (native, tool, summary) = (
        Variant::NativeRecall,
        Variant::ToolResult,
        Variant::SummaryWithReferences,
    );
    let mut rows = Vec::new();

    let mut r1 = answer("pnpm", native, Condition::A, Role::Task);
    r1.rules.continued_reliance = true;
    r1.rules.uses_replacement = Some(true);
    r1.requests.marker_in_request = true;
    r1.judge = verdict(true, Some(true), false);
    rows.push(r1);

    let mut r2 = answer("pnpm", native, Condition::A, Role::Task);
    r2.repetition = 1;
    r2.error = "boom".into();
    rows.push(r2);

    let mut r3 = answer("withdrawn", native, Condition::A, Role::Task);
    r3.rules.admits_uncertainty = true;
    r3.requests.marker_in_request = true;
    r3.judge = verdict(false, None, false);
    rows.push(r3);

    let mut r4 = answer("forgotten", native, Condition::A, Role::Task);
    r4.rules.continued_reliance = true;
    r4.rules.answer_leakage = true;
    r4.judge = verdict(false, None, false);
    rows.push(r4);

    let mut r5 = answer("pnpm", native, Condition::A, Role::Control);
    r5.rules.control_utility = Some(true);
    r5.rules.control_collateral = Some(false);
    r5.rules.control_abstention = Some(false);
    rows.push(r5);

    let mut r6 = answer("pnpm", native, Condition::A, Role::Control);
    r6.repetition = 1;
    r6.error = "x".into();
    rows.push(r6);

    let mut r7 = answer("pnpm", tool, Condition::A, Role::Task);
    r7.rules.uses_replacement = Some(false);
    r7.judge_error = "bad judge".into();
    rows.push(r7);

    for (repetition, compaction) in [
        (
            0,
            CompactionRecord {
                noop: false,
                summary: "s".into(),
                summary_holds_obsolete_term: true,
            },
        ),
        (
            1,
            CompactionRecord {
                noop: true,
                summary: String::new(),
                summary_holds_obsolete_term: false,
            },
        ),
        (
            2,
            CompactionRecord {
                noop: false,
                summary: "t".into(),
                summary_holds_obsolete_term: false,
            },
        ),
    ] {
        let mut row = answer("pnpm", summary, Condition::D, Role::Task);
        row.repetition = repetition;
        row.compaction = Some(compaction);
        rows.push(row);
    }
    rows
}

fn count(count: usize, of: usize) -> Count {
    Count { count, of }
}

#[test]
fn layer_three_counts_and_sample_sizes_are_pinned() {
    let report = build_report(metadata(), &items(), &rows(), &[]);
    let keys: Vec<&str> = report.layer_three.keys().map(String::as_str).collect();
    assert_eq!(keys, ["A/traceable", "A/untraceable", "D/traceable"]);

    let traceable = &report.layer_three["A/traceable"];
    assert_eq!(traceable.answers, 4);
    assert_eq!(traceable.errors, 2);
    assert_eq!(traceable.continued_reliance_rule, count(2, 3));
    assert_eq!(traceable.continued_reliance_judge, count(1, 3));
    assert_eq!(traceable.uses_replacement_rule, count(1, 1));
    assert_eq!(traceable.uses_replacement_judge, count(1, 1));
    assert_eq!(traceable.admits_uncertainty_rule, count(1, 1));
    assert_eq!(traceable.admits_uncertainty_judge, count(0, 1));
    assert_eq!(traceable.request_leakage, count(2, 6));
    assert_eq!(traceable.answer_leakage, count(1, 1));
    assert_eq!(traceable.control_utility, count(1, 1));
    assert_eq!(traceable.control_collateral, count(0, 1));
    assert_eq!(traceable.control_abstention, count(0, 1));
    assert_eq!(traceable.rule_judge_disagreement, count(1, 3));

    let untraceable = &report.layer_three["A/untraceable"];
    assert_eq!(untraceable.answers, 1);
    assert_eq!(untraceable.errors, 0);
    assert_eq!(untraceable.continued_reliance_rule, count(0, 1));
    assert_eq!(untraceable.continued_reliance_judge, count(0, 0));
    assert_eq!(untraceable.uses_replacement_rule, count(0, 1));
    assert_eq!(untraceable.uses_replacement_judge, count(0, 0));
    assert_eq!(untraceable.admits_uncertainty_rule, count(0, 0));
    assert_eq!(untraceable.request_leakage, count(0, 1));
    assert_eq!(untraceable.answer_leakage, count(0, 0));
    assert_eq!(untraceable.control_utility, count(0, 0));
    assert_eq!(untraceable.rule_judge_disagreement, count(0, 0));

    let rebuilt = &report.layer_three["D/traceable"];
    assert_eq!(rebuilt.answers, 3);
    assert_eq!(rebuilt.continued_reliance_rule, count(0, 3));
}

#[test]
fn rebuild_counts_exclude_noop_compactions_from_the_term_count() {
    let report = build_report(metadata(), &items(), &rows(), &[]);
    assert_eq!(report.rebuild.compactions, 3);
    assert_eq!(report.rebuild.noop, 1);
    assert_eq!(report.rebuild.summary_holds_obsolete_term, count(1, 2));
    assert_eq!(report.items, ["pnpm", "withdrawn", "forgotten"]);
}

fn proposal(name: &str, action_correct: bool, reasonable: Option<bool>) -> ProposalRecord {
    ProposalRecord {
        name: name.to_owned(),
        category: "distinction".into(),
        generated: GeneratedRecord::default(),
        score: score(name, action_correct),
        judge: reasonable.map(|action_reasonable| ProposalVerdict {
            action_reasonable,
            evidence_cited_correctly: true,
            replacement_preserves_valid_claims: None,
            reason: "r".into(),
        }),
        judge_error: String::new(),
    }
}

#[test]
fn layer_two_section_counts_judged_rows_and_agreement() {
    let proposals = [
        proposal("a", true, Some(true)),
        proposal("b", true, Some(false)),
        proposal("c", false, None),
    ];
    let report = build_report(metadata(), &items(), &[], &proposals);
    let section = report.layer_two.expect("layer 2 section");
    assert_eq!(section.proposals, 3);
    assert_eq!(section.judged, 2);
    assert_eq!(section.judge_action_reasonable, count(1, 2));
    assert_eq!(section.judge_rule_agreement, count(1, 2));
    let scores: Vec<_> = proposals.iter().map(|row| row.score.clone()).collect();
    assert_eq!(section.metrics_text, Metrics::from_scores(&scores).render());
}

#[test]
fn without_proposals_there_is_no_layer_two_section() {
    let report = build_report(metadata(), &items(), &rows(), &[]);
    assert_eq!(report.layer_two, None);
    let text = render_markdown(&report);
    assert!(text.contains("## Layer 2\n\nNo layer 2 rows."), "{text}");
}

#[test]
fn markdown_holds_the_headings_the_counts_and_the_closing_paragraph() {
    let proposals = [proposal("a", true, Some(true))];
    let report = build_report(metadata(), &items(), &rows(), &proposals);
    let text = render_markdown(&report);
    for expected in [
        "# Memory retraction evaluation\n",
        "- run id: 20261106T080000Z\n",
        "- model: profile-model\n",
        "- thinking: unset\n",
        "- repeat: 2\n",
        "- judge: on\n",
        "- max calls: 2000\n",
        "- usage: 7 provider calls, 700 input tokens, 70 cached tokens, 17 output tokens\n",
        "- conditions: A, B, C, D\n",
        "- judge prompt version: memory-retraction-judge/1\n",
        "## Layer 3\n",
        "### A/traceable\n",
        "| continued reliance, rule | 2/3 |\n",
        "| continued reliance, judge | 1/3 |\n",
        "| marker in a request | 2/6 |\n",
        "### D/traceable\n",
        "## Rebuild (condition D)\n",
        "| rebuilt summaries holding an obsolete term | 1/2 |\n",
        "## Layer 2\n",
        "| judge: action reasonable | 1/1 |\n",
        "No threshold is defined",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in\n{text}");
    }
    assert!(text.find("### A/traceable").unwrap() < text.find("### A/untraceable").unwrap());
    assert!(text.trim_end().ends_with("whether a result passes."));
}

#[test]
fn the_report_round_trips_through_json() {
    let proposals = [proposal("a", true, Some(true))];
    let report = build_report(metadata(), &items(), &rows(), &proposals);
    let json = serde_json::to_string(&report).unwrap();
    let parsed: Report = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, report);
}
