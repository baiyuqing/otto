//! The generator over a scripted backend, scored and aggregated.

use tokio_util::sync::CancellationToken;

use super::super::*;
use crate::memory::proposals::tests::{FakeBackend, round_one, round_two};
use crate::memory::proposals::{Generator, ProposalAction};

use super::base_item;

async fn generate(
    item: &LabeledItem,
    rounds: Vec<crate::memory::proposals::tests::Round>,
) -> Generated {
    let generator = Generator::new(Box::new(FakeBackend::new(rounds)), None);
    Generated::from_result(
        generator
            .generate(&item.input("memory-eval"), &CancellationToken::new())
            .await,
    )
}

#[tokio::test]
async fn generator_results_are_scored_and_aggregated() {
    let retract_item = base_item();
    let mut valid_item = base_item();
    valid_item.name = "valid_item".into();
    valid_item.category = "valid".into();
    valid_item.record.marker = "m-91-vali".into();
    valid_item.record.text = "Prefers the vim editor for every project (m-91-vali)".into();
    valid_item.label.expected_actions = vec![ProposalAction::NoChange];
    valid_item.label.supporting_evidence = vec![];

    let retracted = generate(
        &retract_item,
        vec![
            round_one("retract", &[true, false]),
            round_two("Evidence e1 says the use stopped.", None),
        ],
    )
    .await;
    let wrongly_retired = generate(
        &valid_item,
        vec![
            round_one("retract", &[false, true]),
            round_two("Evidence e2.", None),
        ],
    )
    .await;
    let unreachable = generate(&valid_item, vec![]).await;
    assert!(matches!(unreachable, Generated::Unavailable(_)));

    let scores = vec![
        score(&retract_item, &retracted),
        score(&valid_item, &wrongly_retired),
        score(&valid_item, &unreachable),
    ];
    let metrics = Metrics::from_scores(&scores);
    assert_eq!(metrics.items, 3);
    assert_eq!(metrics.unavailable, 1);
    assert_eq!(metrics.action_correct, 1);
    assert_eq!(metrics.erroneous_retirement, 1);
    assert_eq!(metrics.cited_unsupporting, 1);
    assert_eq!(metrics.rejected_by_layer_one, 0);
    assert_eq!(
        metrics.per_category["distinction"],
        CategoryMetrics {
            items: 1,
            action_correct: 1
        }
    );
    assert_eq!(
        metrics.per_category["valid"],
        CategoryMetrics {
            items: 2,
            action_correct: 0
        }
    );

    let text = metrics.render();
    assert!(text.contains("action_correct: 1/3"));
    assert!(text.contains("erroneous_retirement: 1/3"));
    assert!(text.contains("unavailable: 1/3"));
    assert!(text.contains("  valid: 0/2"));
}

#[test]
fn labeled_item_round_trips_through_serde_json() {
    let mut item = base_item();
    item.label.expected_actions = vec![ProposalAction::Supersede, ProposalAction::Correct];
    item.label.reference_replacement = Some("Prefers helix".into());
    item.permitted = vec![
        ProposalAction::NoChange,
        ProposalAction::Supersede,
        ProposalAction::Correct,
    ];
    let json = serde_json::to_string(&item).expect("serialize");
    assert!(json.contains("\"supersede\"") && json.contains("\"local\""));
    let parsed: LabeledItem = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, item);
}

#[test]
fn permitted_defaults_to_every_action() {
    let mut value = serde_json::to_value(base_item()).expect("value");
    value.as_object_mut().expect("object").remove("permitted");
    let parsed: LabeledItem = serde_json::from_value(value).expect("deserialize");
    assert_eq!(parsed.permitted, ProposalAction::ALL.to_vec());
}
