//! Pure validation for model-submitted verification verdicts.
//!
//! This module deliberately knows nothing about commands or tool output text.
//! Native callers supply typed, durably persisted tool-result facts from the
//! current session; the validator only checks references and consistency.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::{EffectCertainty, OperationDisposition};

pub const MAX_CLAIM_CHARS: usize = 2_000;
pub const MAX_EVIDENCE_ITEMS: usize = 32;
pub const MAX_LIMITATIONS: usize = 16;
pub const MAX_LIMITATION_CHARS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Passed,
    Failed,
    Blocked,
    NotRun,
    Inconclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRelation {
    Supports,
    Contradicts,
    Blocker,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceReference {
    pub tool_call_id: String,
    pub relation: EvidenceRelation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub claim: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub evidence: Vec<EvidenceReference>,
    #[serde(default)]
    pub limitations: Vec<String>,
}

/// One authoritative persisted ToolResult fact. Output content is omitted on
/// purpose so validation cannot depend on command names or output semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceFact {
    pub tool_call_id: String,
    pub tool_name: String,
    pub disposition: OperationDisposition,
    pub effect_certainty: EffectCertainty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AcceptedEvidence {
    pub tool_call_id: String,
    pub relation: EvidenceRelation,
    pub tool_name: String,
    pub disposition: OperationDisposition,
    pub effect_certainty: EffectCertainty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AcceptedVerdict {
    pub verdict: Verdict,
    pub claim: String,
    pub evidence: Vec<AcceptedEvidence>,
    pub limitations: Vec<String>,
}

/// Validates bounds, resolves every evidence id, and applies only deterministic
/// relation/outcome consistency rules. It does not judge semantic sufficiency.
pub fn validate(submission: Submission, facts: &[EvidenceFact]) -> Result<AcceptedVerdict, String> {
    validate_text("claim", &submission.claim, MAX_CLAIM_CHARS)?;
    if submission.evidence.len() > MAX_EVIDENCE_ITEMS {
        return Err(format!(
            "evidence has {} items; maximum is {MAX_EVIDENCE_ITEMS}",
            submission.evidence.len()
        ));
    }
    if submission.limitations.len() > MAX_LIMITATIONS {
        return Err(format!(
            "limitations has {} items; maximum is {MAX_LIMITATIONS}",
            submission.limitations.len()
        ));
    }
    for (index, limitation) in submission.limitations.iter().enumerate() {
        validate_text(
            &format!("limitations[{index}]"),
            limitation,
            MAX_LIMITATION_CHARS,
        )?;
    }

    let by_id: BTreeMap<&str, &EvidenceFact> = facts
        .iter()
        .map(|fact| (fact.tool_call_id.as_str(), fact))
        .collect();
    let mut seen = BTreeSet::new();
    let mut accepted = Vec::with_capacity(submission.evidence.len());
    for reference in &submission.evidence {
        if reference.tool_call_id.trim().is_empty() {
            return Err("evidence tool_call_id must not be blank".into());
        }
        if !seen.insert(reference.tool_call_id.as_str()) {
            return Err(format!(
                "duplicate evidence tool_call_id {:?}; cite each call once",
                reference.tool_call_id
            ));
        }
        let fact = by_id.get(reference.tool_call_id.as_str()).ok_or_else(|| {
            format!(
                "evidence tool_call_id {:?} is not a prior persisted typed ToolResult in this session",
                reference.tool_call_id
            )
        })?;
        if reference.relation == EvidenceRelation::Supports
            && (fact.disposition != OperationDisposition::Succeeded
                || fact.effect_certainty != EffectCertainty::Completed)
        {
            return Err(format!(
                "supporting evidence {:?} must have disposition succeeded and completed effects (found {:?}, {:?})",
                reference.tool_call_id, fact.disposition, fact.effect_certainty
            ));
        }
        accepted.push(AcceptedEvidence {
            tool_call_id: reference.tool_call_id.clone(),
            relation: reference.relation,
            tool_name: fact.tool_name.clone(),
            disposition: fact.disposition,
            effect_certainty: fact.effect_certainty,
        });
    }

    let count = |relation| {
        submission
            .evidence
            .iter()
            .filter(|item| item.relation == relation)
            .count()
    };
    let supports = count(EvidenceRelation::Supports);
    let contradicts = count(EvidenceRelation::Contradicts);
    let blockers = count(EvidenceRelation::Blocker);
    match submission.verdict {
        Verdict::Passed if supports == 0 => {
            return Err("passed verdict requires at least one supports evidence item".into());
        }
        Verdict::Passed if contradicts > 0 || blockers > 0 => {
            return Err("passed verdict cannot include contradicts or blocker evidence".into());
        }
        Verdict::Failed if contradicts == 0 => {
            return Err("failed verdict requires at least one contradicts evidence item".into());
        }
        Verdict::Failed if blockers > 0 => {
            return Err("failed verdict cannot include blocker evidence".into());
        }
        Verdict::Blocked if blockers == 0 => {
            return Err("blocked verdict requires at least one blocker evidence item".into());
        }
        Verdict::Blocked if contradicts > 0 => {
            return Err("blocked verdict cannot include contradicts evidence".into());
        }
        Verdict::NotRun if !submission.evidence.is_empty() => {
            return Err("not_run verdict requires zero evidence items".into());
        }
        Verdict::Inconclusive
            if submission.evidence.is_empty() && submission.limitations.is_empty() =>
        {
            return Err("inconclusive verdict requires evidence or at least one limitation".into());
        }
        _ => {}
    }

    Ok(AcceptedVerdict {
        verdict: submission.verdict,
        claim: submission.claim,
        evidence: accepted,
        limitations: submission.limitations,
    })
}

fn validate_text(label: &str, value: &str, maximum: usize) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{label} must not be blank"));
    }
    let count = value.chars().count();
    if count > maximum {
        return Err(format!(
            "{label} has {count} characters; maximum is {maximum}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(disposition: OperationDisposition, effect_certainty: EffectCertainty) -> EvidenceFact {
        EvidenceFact {
            tool_call_id: "call-1".into(),
            tool_name: "arbitrary_tool".into(),
            disposition,
            effect_certainty,
        }
    }

    fn submission(verdict: Verdict, relation: EvidenceRelation) -> Submission {
        Submission {
            claim: "the behavior works".into(),
            verdict,
            evidence: vec![EvidenceReference {
                tool_call_id: "call-1".into(),
                relation,
            }],
            limitations: Vec::new(),
        }
    }

    #[test]
    fn accepts_pass_from_completed_success_without_inspecting_names_or_output() {
        let accepted = validate(
            submission(Verdict::Passed, EvidenceRelation::Supports),
            &[fact(
                OperationDisposition::Succeeded,
                EffectCertainty::Completed,
            )],
        )
        .expect("completed success supports pass");
        assert_eq!(accepted.evidence[0].tool_name, "arbitrary_tool");
    }

    #[test]
    fn failed_execution_cannot_support_pass() {
        let error = validate(
            submission(Verdict::Passed, EvidenceRelation::Supports),
            &[fact(
                OperationDisposition::Error,
                EffectCertainty::Completed,
            )],
        )
        .expect_err("failed execution is not supporting evidence");
        assert!(error.contains("disposition succeeded"), "{error}");
    }

    #[test]
    fn rejects_missing_and_duplicate_evidence() {
        let missing = validate(
            submission(Verdict::Failed, EvidenceRelation::Contradicts),
            &[],
        )
        .expect_err("missing evidence is rejected");
        assert!(missing.contains("not a prior persisted typed ToolResult"));

        let mut duplicate = submission(Verdict::Failed, EvidenceRelation::Contradicts);
        duplicate.evidence.push(EvidenceReference {
            tool_call_id: "call-1".into(),
            relation: EvidenceRelation::Context,
        });
        let error = validate(
            duplicate,
            &[fact(
                OperationDisposition::Error,
                EffectCertainty::KnownNoEffect,
            )],
        )
        .expect_err("duplicate id is rejected across relations");
        assert!(error.contains("duplicate evidence"));
    }

    #[test]
    fn enforces_not_run_and_inconclusive_rules() {
        let error = validate(
            submission(Verdict::NotRun, EvidenceRelation::Context),
            &[fact(
                OperationDisposition::Succeeded,
                EffectCertainty::Completed,
            )],
        )
        .expect_err("not_run cannot cite evidence");
        assert!(error.contains("zero evidence"));

        let accepted = validate(
            Submission {
                claim: "not attempted".into(),
                verdict: Verdict::NotRun,
                evidence: Vec::new(),
                limitations: Vec::new(),
            },
            &[],
        )
        .expect("not_run without evidence is valid");
        assert_eq!(accepted.verdict, Verdict::NotRun);

        let error = validate(
            Submission {
                claim: "unclear".into(),
                verdict: Verdict::Inconclusive,
                evidence: Vec::new(),
                limitations: Vec::new(),
            },
            &[],
        )
        .expect_err("empty inconclusive is rejected");
        assert!(error.contains("evidence or at least one limitation"));
    }
}
