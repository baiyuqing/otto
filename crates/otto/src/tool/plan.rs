//! `update_plan`: the model states and updates its plan for the current work
//! as an `otto.plan` session record.

use std::sync::Arc;

use super::{Tool, definition, error_result, preflight_error, text_result};
use otto_core::agent::redactor::Redactor;
use otto_core::model::ToolDefinition;
use otto_core::session::{
    MAX_PLAN_STEP_CHARS, MAX_PLAN_STEPS, PLAN_CUSTOM_TYPE, PlanStep, Session, encode_plan,
    plan_summary, validate_plan,
};
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

pub(crate) const UPDATE_PLAN_TOOL: &str = "update_plan";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    steps: Vec<PlanStep>,
}

/// Replaces the session plan with the steps it is given.
pub struct UpdatePlan {
    session: Arc<dyn Session + Send + Sync>,
    redactor: Redactor,
}

impl UpdatePlan {
    pub fn new(session: Arc<dyn Session + Send + Sync>, redaction_values: &[String]) -> Self {
        Self {
            session,
            redactor: Redactor::new(redaction_values),
        }
    }
}

// Whole-plan replacement has the same effect on repetition, but is not ReadOnly
// and therefore does not qualify for automatic recovery replay.
pub(crate) const UPDATE_PLAN_RETRY: (&str, otto_core::model::RetrySafety) =
    ("update_plan", otto_core::model::RetrySafety::Idempotent);

#[async_trait::async_trait]
impl Tool for UpdatePlan {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        UPDATE_PLAN_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            UPDATE_PLAN_TOOL,
            &format!(
                "Record your plan for the current work as a checklist the user can \
                    follow, replacing any earlier plan. Use it for work of three or more \
                    steps, not for single-step requests: keep exactly one step in_progress \
                    while you work and mark each step completed as soon as it is done, \
                    sending the update in the same batch as the next tool call. It records \
                    progress only and runs nothing; use task_queue to schedule a later \
                    prompt and request_approval to ask the user to approve an approach. \
                    steps: 0 to {MAX_PLAN_STEPS} items in order, each with one line of 1 to \
                    {MAX_PLAN_STEP_CHARS} characters of text and a status of pending, \
                    in_progress or completed; at most one in_progress. An empty list clears \
                    the plan. Returns a one-line summary such as \"Plan updated: 2 of 5 \
                    completed; in progress: <step>.\", or an error naming the broken rule, \
                    in which case the plan is unchanged. The plan is saved with the session \
                    and shown to the user."
            ),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "steps": {
                        "type": "array",
                        "maxItems": MAX_PLAN_STEPS,
                        "description": "The whole plan in order; an empty list clears it.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "text": {
                                    "type": "string",
                                    "minLength": 1,
                                    "maxLength": MAX_PLAN_STEP_CHARS,
                                    "description": "What the step does, on one line."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["text", "status"]
                        }
                    }
                },
                "required": ["steps"]
            }),
        )
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return preflight_error(super::CONTEXT_CANCELED);
        }
        let mut args: Args = match serde_json::from_str(arguments.get()) {
            Ok(args) => args,
            Err(error) => return preflight_error(error),
        };
        for step in &mut args.steps {
            step.text = self.redactor.redact_string(step.text.trim());
        }
        if let Err(error) = validate_plan(&args.steps) {
            return preflight_error(format!("{error}; the plan is unchanged"));
        }
        match self
            .session
            .append_custom(PLAN_CUSTOM_TYPE, &encode_plan(&args.steps))
            .await
        {
            Ok(()) if args.steps.is_empty() => text_result("Plan cleared."),
            Ok(()) => text_result(format!("Plan updated: {}.", plan_summary(&args.steps))),
            Err(error) => error_result(format!("the plan could not be saved: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::session::{MemorySession, PlanStatus};

    fn tool() -> (UpdatePlan, Arc<MemorySession>) {
        let session = Arc::new(MemorySession::new());
        (
            UpdatePlan::new(
                Arc::clone(&session) as Arc<dyn Session + Send + Sync>,
                &["s3cret-value".to_string()],
            ),
            session,
        )
    }

    async fn call(tool: &UpdatePlan, arguments: serde_json::Value) -> ToolResult {
        let raw = RawValue::from_string(arguments.to_string()).unwrap();
        tool.execute(&raw, &CancellationToken::new()).await
    }

    #[tokio::test]
    async fn a_valid_plan_replaces_the_session_plan() {
        let (tool, session) = tool();
        let result = call(
            &tool,
            json!({"steps":[
                {"text":"read the code","status":"completed"},
                {"text":" write the fix ","status":"in_progress"},
                {"text":"run the tests","status":"pending"}
            ]}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            result.content,
            "Plan updated: 1 of 3 completed; in progress: write the fix."
        );
        let plan = session.plan();
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[1].text, "write the fix");
        assert_eq!(plan[1].status, PlanStatus::InProgress);

        let cleared = call(&tool, json!({"steps":[]})).await;
        assert_eq!(cleared.content, "Plan cleared.");
        assert!(session.plan().is_empty());
    }

    #[tokio::test]
    async fn an_invalid_plan_is_refused_and_the_plan_is_unchanged() {
        let (tool, session) = tool();
        call(&tool, json!({"steps":[{"text":"keep","status":"pending"}]})).await;
        let too_many: Vec<_> = (0..=MAX_PLAN_STEPS)
            .map(|n| json!({"text": format!("step {n}"), "status": "pending"}))
            .collect();
        for (arguments, rule) in [
            (json!({"steps": too_many}), "at most 20 steps"),
            (json!({"steps":[{"text":"","status":"pending"}]}), "no text"),
            (
                json!({"steps":[{"text":"a\nb","status":"pending"}]}),
                "one line",
            ),
            (
                json!({"steps":[{"text":"a","status":"in_progress"},{"text":"b","status":"in_progress"}]}),
                "in_progress",
            ),
            (
                json!({"steps":[{"text":"a","status":"done"}]}),
                "unknown variant",
            ),
            (json!({}), "missing field"),
        ] {
            let result = call(&tool, arguments).await;
            assert!(result.is_error, "{rule}: {}", result.content);
            assert!(result.content.contains(rule), "{rule}: {}", result.content);
        }
        assert_eq!(session.plan()[0].text, "keep");
    }

    #[tokio::test]
    async fn step_text_is_redacted_before_it_is_saved() {
        let (tool, session) = tool();
        call(
            &tool,
            json!({"steps":[{"text":"use s3cret-value","status":"pending"}]}),
        )
        .await;
        assert!(!session.plan()[0].text.contains("s3cret-value"));
    }
}
