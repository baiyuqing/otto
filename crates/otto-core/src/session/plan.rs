//! Versioned `otto.plan` records: the model's current plan, a short list of
//! steps with a status each. Every record replaces the whole plan; the latest
//! valid record on the active path is the plan in force. The records never
//! enter model context themselves.

use serde::{Deserialize, Serialize};

pub const PLAN_CUSTOM_TYPE: &str = "otto.plan";
pub const PLAN_SCHEMA_VERSION: u32 = 1;
/// Most steps one plan holds.
pub const MAX_PLAN_STEPS: usize = 20;
/// Most characters in one step's text.
pub const MAX_PLAN_STEP_CHARS: usize = 200;

/// Where one step stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub text: String,
    pub status: PlanStatus,
}

#[derive(Serialize, Deserialize)]
struct PlanRecord {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    steps: Vec<PlanStep>,
}

/// Checks the plan rules: at most [`MAX_PLAN_STEPS`] steps, each with one
/// line of 1 to [`MAX_PLAN_STEP_CHARS`] characters, and at most one step
/// `in_progress`. The error names the rule broken.
pub fn validate_plan(steps: &[PlanStep]) -> Result<(), String> {
    if steps.len() > MAX_PLAN_STEPS {
        return Err(format!(
            "a plan has at most {MAX_PLAN_STEPS} steps; got {}",
            steps.len()
        ));
    }
    for (index, step) in steps.iter().enumerate() {
        let number = index + 1;
        if step.text.trim().is_empty() {
            return Err(format!("step {number} has no text"));
        }
        if step.text.contains(['\n', '\r']) {
            return Err(format!("step {number} text must be one line"));
        }
        let chars = step.text.chars().count();
        if chars > MAX_PLAN_STEP_CHARS {
            return Err(format!(
                "step {number} text has {chars} characters; at most {MAX_PLAN_STEP_CHARS}"
            ));
        }
    }
    let in_progress = steps
        .iter()
        .filter(|step| step.status == PlanStatus::InProgress)
        .count();
    if in_progress > 1 {
        return Err(format!(
            "at most one step may be in_progress; got {in_progress}"
        ));
    }
    Ok(())
}

/// Encodes a plan as `otto.plan` data.
pub fn encode_plan(steps: &[PlanStep]) -> String {
    serde_json::to_string(&PlanRecord {
        schema_version: PLAN_SCHEMA_VERSION,
        steps: steps.to_vec(),
    })
    .expect("plan record serializes")
}

/// Decodes `otto.plan` data; `None` for a malformed record, another schema
/// version, or a plan that breaks [`validate_plan`]. Unknown fields are
/// ignored.
pub fn decode_plan(data: &str) -> Option<Vec<PlanStep>> {
    let record = serde_json::from_str::<PlanRecord>(data).ok()?;
    (record.schema_version == PLAN_SCHEMA_VERSION && validate_plan(&record.steps).is_ok())
        .then_some(record.steps)
}

/// One line saying where the plan stands, such as
/// `2 of 5 completed; in progress: write tests`.
pub fn plan_summary(steps: &[PlanStep]) -> String {
    if steps.is_empty() {
        return "no plan".into();
    }
    let completed = steps
        .iter()
        .filter(|step| step.status == PlanStatus::Completed)
        .count();
    let mut summary = format!("{completed} of {} completed", steps.len());
    if let Some(step) = steps
        .iter()
        .find(|step| step.status == PlanStatus::InProgress)
    {
        summary.push_str("; in progress: ");
        summary.push_str(&step.text);
    }
    summary
}

/// The plan as a checklist, one step per line: `[x]` completed, `[>]` in
/// progress, `[ ]` pending.
pub fn plan_checklist(steps: &[PlanStep]) -> String {
    steps
        .iter()
        .map(|step| {
            let mark = match step.status {
                PlanStatus::Completed => "[x]",
                PlanStatus::InProgress => "[>]",
                PlanStatus::Pending => "[ ]",
            };
            format!("{mark} {}", step.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(text: &str, status: PlanStatus) -> PlanStep {
        PlanStep {
            text: text.into(),
            status,
        }
    }

    #[test]
    fn exact_record_json_round_trips() {
        let steps = vec![
            step("read", PlanStatus::Completed),
            step("write", PlanStatus::InProgress),
        ];
        let encoded = encode_plan(&steps);
        assert_eq!(
            encoded,
            r#"{"schemaVersion":1,"steps":[{"text":"read","status":"completed"},{"text":"write","status":"in_progress"}]}"#
        );
        assert_eq!(decode_plan(&encoded), Some(steps));
        assert_eq!(
            decode_plan(r#"{"schemaVersion":1,"steps":[]}"#),
            Some(Vec::new())
        );
    }

    #[test]
    fn invalid_records_and_plans_are_rejected() {
        assert_eq!(decode_plan("{}"), None);
        assert_eq!(decode_plan(r#"{"schemaVersion":2,"steps":[]}"#), None);
        assert_eq!(
            decode_plan(r#"{"schemaVersion":1,"steps":[{"text":"a","status":"done"}]}"#),
            None
        );

        let too_many = vec![step("a", PlanStatus::Pending); MAX_PLAN_STEPS + 1];
        assert!(validate_plan(&too_many).unwrap_err().contains("at most 20"));
        assert!(
            validate_plan(&[step(" ", PlanStatus::Pending)])
                .unwrap_err()
                .contains("no text")
        );
        assert!(
            validate_plan(&[step("a\nb", PlanStatus::Pending)])
                .unwrap_err()
                .contains("one line")
        );
        let long = "x".repeat(MAX_PLAN_STEP_CHARS + 1);
        assert!(
            validate_plan(&[step(&long, PlanStatus::Pending)])
                .unwrap_err()
                .contains("201")
        );
        assert!(
            validate_plan(&[step(&"é".repeat(MAX_PLAN_STEP_CHARS), PlanStatus::Pending)]).is_ok()
        );
        assert!(
            validate_plan(&[
                step("a", PlanStatus::InProgress),
                step("b", PlanStatus::InProgress)
            ])
            .unwrap_err()
            .contains("in_progress")
        );
        // A record that breaks the rules is not a plan either.
        let two_active = encode_plan(&[
            step("a", PlanStatus::InProgress),
            step("b", PlanStatus::InProgress),
        ]);
        assert_eq!(decode_plan(&two_active), None);
    }

    #[test]
    fn summary_and_checklist() {
        let steps = vec![
            step("read", PlanStatus::Completed),
            step("write", PlanStatus::InProgress),
            step("ship", PlanStatus::Pending),
        ];
        assert_eq!(plan_summary(&steps), "1 of 3 completed; in progress: write");
        assert_eq!(plan_summary(&[]), "no plan");
        assert_eq!(plan_checklist(&steps), "[x] read\n[>] write\n[ ] ship");
    }
}
