//! Adversarial fixtures from `testdata/memory-eval/proposals/adversarial.jsonl`,
//! one JSON object per line, driven through the generator with a scripted
//! backend.

use serde::Deserialize;

use super::*;

const ADVERSARIAL_CASES: &str =
    include_str!("../../../../../../testdata/memory-eval/proposals/adversarial.jsonl");

#[derive(Debug, Deserialize)]
struct FixtureEvidence {
    id: String,
    text: String,
    trust: String,
}

#[derive(Debug, Deserialize)]
struct FixtureRoundOne {
    action: String,
    supports: Vec<bool>,
}

#[derive(Debug, Deserialize)]
struct FixtureRoundTwo {
    explanation: String,
    #[serde(default)]
    replacement: Option<String>,
    /// Answers to questions that were not asked, which must be ignored.
    #[serde(default)]
    extra: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct FixtureCase {
    name: String,
    record_text: String,
    evidence: Vec<FixtureEvidence>,
    round_one: FixtureRoundOne,
    #[serde(default)]
    round_two: Option<FixtureRoundTwo>,
    /// `rejected`, `proposed`, `no_change` or `unavailable`.
    expected: String,
    /// A substring of the rejection reason.
    #[serde(default)]
    expected_detail: Option<String>,
    /// The candidate action of a `proposed` outcome.
    #[serde(default)]
    expected_action: Option<String>,
    /// A permitted action set narrower than all six.
    #[serde(default)]
    permitted: Option<Vec<String>>,
}

fn trust(name: &str) -> Trust {
    match name {
        "local" => Trust::Local,
        "external" => Trust::External,
        other => panic!("unknown trust {other}"),
    }
}

fn cases() -> Vec<FixtureCase> {
    ADVERSARIAL_CASES
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect()
}

#[test]
fn the_fixture_file_names_every_case_once_and_covers_each_outcome() {
    let cases = cases();
    let mut names: Vec<&str> = cases.iter().map(|case| case.name.as_str()).collect();
    names.sort_unstable();
    let count = names.len();
    names.dedup();
    assert_eq!(names.len(), count, "duplicate case names");
    assert!(count >= 8, "{count} cases");
    for outcome in ["rejected", "proposed", "no_change"] {
        assert!(
            cases.iter().any(|case| case.expected == outcome),
            "no {outcome} case"
        );
    }
}

#[tokio::test]
async fn adversarial_cases_end_in_the_expected_outcome() {
    for case in cases() {
        let fixture = fixture_with_text(&case.record_text);
        let mut rounds = vec![round_one(&case.round_one.action, &case.round_one.supports)];
        if let Some(second) = &case.round_two {
            let mut answers =
                round_two(&second.explanation, second.replacement.as_deref()).unwrap();
            for (key, value) in &second.extra {
                answers.insert(key.clone(), answer(value));
            }
            rounds.push(Ok(answers));
        }
        let (generator, _) = generator(rounds);
        let evidence = case
            .evidence
            .iter()
            .map(|item| evidence(&item.id, &item.text, trust(&item.trust)))
            .collect();
        let permitted = case.permitted.as_ref().map_or_else(all_actions, |names| {
            names
                .iter()
                .map(|name| ProposalAction::parse(name).expect("action"))
                .collect()
        });
        let outcome = run(
            &generator,
            &fixture,
            input(&fixture.target, evidence, &permitted),
        )
        .await;
        let pending = pending_candidates(&fixture.service);
        let name = &case.name;
        match (case.expected.as_str(), outcome) {
            ("rejected", Outcome::Rejected(reason)) => {
                if let Some(detail) = &case.expected_detail {
                    assert!(reason.contains(detail.as_str()), "{name}: {reason}");
                }
                assert!(
                    pending.is_empty(),
                    "{name}: a rejected proposal queued a candidate"
                );
            }
            ("no_change", Outcome::NoChange { .. }) => {
                assert!(pending.is_empty(), "{name}: no_change queued a candidate");
            }
            ("unavailable", Outcome::Unavailable(_)) => {
                assert!(pending.is_empty(), "{name}: unavailable queued a candidate");
            }
            ("proposed", Outcome::Proposed(candidate)) => {
                let action = case.expected_action.as_deref().expect("expected_action");
                assert_eq!(candidate.action.as_str(), action, "{name}");
                assert_eq!(pending.len(), 1, "{name}");
                // Identity and scope never come from the model.
                assert_eq!(candidate.proposed.scope, fixture.target.scope, "{name}");
                assert_eq!(candidate.target_id, fixture.target.id, "{name}");
                assert_eq!(candidate.base_revision, fixture.target.revision, "{name}");
                if candidate.action.carries_content() {
                    assert_eq!(candidate.proposed.kind, fixture.target.kind, "{name}");
                    assert_eq!(candidate.proposed.key, fixture.target.key, "{name}");
                } else {
                    assert_eq!(candidate.proposed.text, "", "{name}");
                }
                if let Some(second) = &case.round_two {
                    assert_eq!(candidate.reason, second.explanation.trim(), "{name}");
                }
            }
            (expected, other) => panic!("{name}: expected {expected}, got {other:?}"),
        }
    }
}
