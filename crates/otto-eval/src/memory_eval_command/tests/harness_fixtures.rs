//! Hand-built items, answers and metadata for the harness tests.

use chrono::{TimeZone, Utc};
use otto::memory::proposals::evaluation::{Abstention, LayerOne, Score};

use crate::eval_command::model_calls::TokenUsage;
use crate::memory_eval_command::argument_parsing::{Condition, Variant};
use crate::memory_eval_command::conditions::{AnswerRecord, RequestFacts, Role, case_key};
use crate::memory_eval_command::items::Item;
use crate::memory_eval_command::report::RunMetadata;

/// An item whose texts have fixed lengths: the prior texts hold 55 bytes, the record 26, the
/// replacement 23 (absent without one), the control record 16, the task 22, the control task 14.
pub(super) fn item(
    name: &str,
    retirement: &str,
    with_replacement: bool,
    uncertainty: bool,
) -> Item {
    let replacement = if with_replacement {
        serde_json::json!({"id": "rec-new", "revision": "2", "text": "Package manager is pnpm"})
    } else {
        serde_json::Value::Null
    };
    let value = serde_json::json!({
        "name": name,
        "retirement": retirement,
        "record": {
            "id": "rec-old", "revision": "1", "kind": "fact", "key": "package_manager",
            "scope_id": "project-a", "text": "Package manager is npm ZK1", "marker": "ZK1"
        },
        "replacement": replacement,
        "obsolete_terms": ["npm"],
        "prior": {
            "user": "user turn", "assistant_neutral": "neutral reply", "assistant": "claims npm",
            "tool_name": "bash", "tool_input": "cat x", "tool_output": "npm", "summary": "summary npm"
        },
        "task": "Which package manager?",
        "checks": {
            "must_contain_any": if with_replacement { vec!["pnpm"] } else { vec![] },
            "uncertainty_expected": uncertainty
        },
        "control": {
            "record": {
                "id": "rec-ctl", "revision": "1", "kind": "fact", "key": "deploy",
                "scope_id": "project-b", "text": "Deploy with make"
            },
            "task": "How to deploy?",
            "must_contain_any": ["make"],
            "must_not_contain": ["pnpm"]
        }
    });
    serde_json::from_value(value).expect("fixture item")
}

/// A row with default rule results and request facts.
pub(super) fn answer(
    item: &str,
    variant: Variant,
    condition: Condition,
    role: Role,
) -> AnswerRecord {
    AnswerRecord {
        key: case_key(item, variant, condition, 0, role),
        item: item.to_owned(),
        variant,
        condition,
        repetition: 0,
        role,
        traceable: variant.traceable(),
        answer: String::new(),
        error: String::new(),
        usage: TokenUsage::default(),
        requests: RequestFacts::default(),
        compaction: None,
        rules: Default::default(),
        judge: None,
        judge_error: String::new(),
    }
}

pub(super) fn metadata() -> RunMetadata {
    let time = Utc.with_ymd_and_hms(2026, 11, 6, 8, 0, 0).unwrap();
    RunMetadata {
        run_id: "20261106T080000Z".into(),
        started_at: time,
        finished_at: time,
        otto_version: "0.0.0".into(),
        provider: "openai-compatible".into(),
        model: "profile-model".into(),
        thinking: String::new(),
        items_version: "memory-retraction-items/1".into(),
        labels_version: "memory-proposal-labels/1".into(),
        proposal_prompt_version: "proposal/1".into(),
        judge_prompt_version: "memory-retraction-judge/1".into(),
        layer_two_judge_prompt_version: "memory-proposal-judge/1".into(),
        conditions: Condition::ALL.to_vec(),
        variants: Variant::ALL.to_vec(),
        repeat: 2,
        judge: true,
        max_calls: 2000,
        usage: TokenUsage {
            provider_calls: 7,
            input_tokens: 700,
            cached_tokens: 70,
            output_tokens: 17,
        },
    }
}

/// A score with the given action correctness and nothing else set.
pub(super) fn score(name: &str, action_correct: bool) -> Score {
    Score {
        name: name.to_owned(),
        category: "distinction".into(),
        action: None,
        unavailable: None,
        layer_one: LayerOne::NotRun,
        action_correct,
        abstention: Abstention::NotApplicable,
        erroneous_retirement: false,
        unsupported_inversion: false,
        scope_violation: false,
        claims_preserved: None,
        cited_unsupporting: false,
        missed_supporting: false,
        leakage: false,
    }
}
