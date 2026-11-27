//! Parent-owned durable goal transitions.
use std::sync::Arc;

use otto_core::agent::redactor::Redactor;
use otto_core::goal::{Execution, Goal, Status};
use otto_core::model::{OperationDisposition, Role, ToolDefinition};
use otto_core::session::Session;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use tokio_util::sync::CancellationToken;

use super::{Tool, definition, error_result, preflight_error, text_result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    revision: Option<u64>,
    objective: Option<String>,
    criteria: Option<Vec<String>>,
    note: Option<String>,
    evidence: Option<Vec<String>>,
    user_message_id: Option<String>,
    user_quote: Option<String>,
    max_retries: Option<u8>,
}

pub struct ManageGoal {
    session: Arc<dyn Session + Send + Sync>,
    redactor: Redactor,
}

impl ManageGoal {
    pub fn new(session: Arc<dyn Session + Send + Sync>, values: &[String]) -> Self {
        Self {
            session,
            redactor: Redactor::new(values),
        }
    }

    fn user_reference(&self, args: &Args) -> bool {
        let (Some(id), Some(quote)) = (&args.user_message_id, &args.user_quote) else {
            return false;
        };
        !quote.trim().is_empty()
            && self.session.messages().iter().any(|m| {
                m.role == Role::User
                    && (m.id == *id || m.input_id == *id)
                    && m.text().contains(quote)
            })
    }

    fn evidence_exists(&self, reference: &str) -> bool {
        if let Some(id) = reference.strip_prefix("operation:") {
            return self
                .session
                .operation_ledger()
                .operation_for_tool_call(id)
                .is_some_and(|record| {
                    !record.corrupt
                        && record.terminal.as_ref().is_some_and(|outcome| {
                            outcome.disposition == OperationDisposition::Succeeded
                        })
                });
        }
        if let Some(id) = reference.strip_prefix("user:") {
            return self
                .session
                .messages()
                .iter()
                .any(|m| m.role == Role::User && m.id == id && !m.text().trim().is_empty());
        }
        false
    }
}

#[async_trait::async_trait]
impl Tool for ManageGoal {
    fn definition(&self) -> ToolDefinition {
        definition(
            "manage_goal",
            "Record durable parent goal state without running work. Draft a bounded objective; ask if acceptance is unclear. \
             Activate only when observable criteria and user opt-in are clear. For draft, activate, pause, cancel, and resume, \
             cite the real user input ID and exact quote expressing that intention. For achieved, provide one reference per criterion in order: \
             operation:<successful tool-call ID> or user:<message ID> for explicit acceptance. Inspect the evidence for actual coverage. \
             Retry with a materially different approach; the retry cap persists. For any existing goal, supply its current revision. \
             Returns updated state or an error; a failed persistence operation may require session recovery.",
            json!({
                "type":"object", "additionalProperties":false,
                "properties":{
                    "action":{"type":"string","enum":["draft","activate","achieved","blocked","retry_exhausted","awaiting_user","failed","retry","pause","cancel","resume"]},
                    "revision":{"type":"integer","minimum":1},
                    "objective":{"type":"string"},"criteria":{"type":"array","items":{"type":"string"}},
                    "note":{"type":"string"},"evidence":{"type":"array","items":{"type":"string"}},
                    "user_message_id":{"type":"string"},"user_quote":{"type":"string"},"max_retries":{"type":"integer","minimum":0,"maximum":10}
                }, "required":["action"]
            }),
        )
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return preflight_error(super::CONTEXT_CANCELED);
        }
        let args: Args = match serde_json::from_str(arguments.get()) {
            Ok(v) => v,
            Err(e) => return preflight_error(e),
        };
        let previous = self.session.goal();
        if previous.as_ref().map(|g| g.revision) != args.revision {
            return preflight_error("goal revision mismatch; reload the current goal");
        }
        let clean = |s: &str| self.redactor.redact_string(s.trim());
        let note = args.note.as_deref().map(clean).unwrap_or_default();
        let authorized = self.user_reference(&args);
        let mut goal = if args.action == "draft" {
            if previous.as_ref().is_some_and(|g| g.status == Status::Open) {
                return preflight_error("cancel the open goal before replacing it");
            }
            if !authorized {
                return preflight_error("reference the user message requesting the goal");
            }
            let Some(objective) = args.objective.as_deref() else {
                return preflight_error("objective is required");
            };
            let mut goal = Goal::draft(clean(objective));
            if let Some(old) = &previous {
                let Some(revision) = old.revision.checked_add(1) else {
                    return preflight_error("goal revision exhausted");
                };
                goal.revision = revision;
            }
            goal.criteria = args
                .criteria
                .as_ref()
                .map(|v| v.iter().map(|s| clean(s)).collect())
                .unwrap_or_default();
            if let Some(max) = args.max_retries {
                if max > 10 {
                    return preflight_error("max_retries must be at most 10");
                }
                goal.max_retries = max;
            }
            goal
        } else {
            let Some(mut goal) = previous.clone() else {
                return preflight_error("draft a goal first");
            };
            goal.revision = match goal.revision.checked_add(1) {
                Some(v) => v,
                None => return preflight_error("goal revision exhausted"),
            };
            match args.action.as_str() {
                "activate" if goal.status == Status::Draft && authorized => {
                    if let Some(criteria) = &args.criteria {
                        goal.criteria = criteria.iter().map(|s| clean(s)).collect();
                    }
                    goal.status = Status::Open;
                    goal.execution = Execution::Ready;
                }
                "achieved" if goal.needs_disposition() => {
                    let Some(evidence) = &args.evidence else {
                        return preflight_error("one evidence reference per criterion is required");
                    };
                    if evidence.len() != goal.criteria.len()
                        || !evidence.iter().all(|s| self.evidence_exists(s))
                    {
                        return preflight_error(
                            "each criterion needs an existing successful operation or user acceptance reference",
                        );
                    }
                    goal.evidence = evidence.iter().map(|s| clean(s)).collect();
                    goal.status = Status::Achieved;
                    goal.execution = Execution::Ended;
                }
                "blocked" if goal.needs_disposition() && !note.is_empty() => {
                    goal.execution = Execution::Blocked;
                    goal.note = note;
                }
                "retry_exhausted"
                    if goal.status == Status::Open
                        && matches!(
                            goal.execution,
                            Execution::Ready | Execution::Blocked | Execution::Interrupted
                        )
                        && goal.retries == goal.max_retries
                        && !note.is_empty() =>
                {
                    goal.execution = Execution::RetryExhausted;
                    goal.note = note;
                }
                "awaiting_user" if goal.needs_disposition() && !note.is_empty() => {
                    goal.execution = Execution::AwaitingUser;
                    goal.note = note;
                }
                "failed" if goal.needs_disposition() && !note.is_empty() => {
                    goal.status = Status::Failed;
                    goal.execution = Execution::Ended;
                    goal.note = note;
                }
                "retry"
                    if goal.status == Status::Open
                        && matches!(
                            goal.execution,
                            Execution::Blocked | Execution::Interrupted
                        )
                        && goal.retries == goal.max_retries =>
                {
                    goal.execution = Execution::RetryExhausted;
                }
                "retry"
                    if goal.status == Status::Open
                        && matches!(
                            goal.execution,
                            Execution::Blocked | Execution::Interrupted
                        )
                        && goal.retries < goal.max_retries
                        && !note.is_empty()
                        && note != goal.note =>
                {
                    goal.retries += 1;
                    goal.execution = Execution::Ready;
                    goal.note = note;
                }
                "pause"
                    if goal.status == Status::Open
                        && authorized
                        && goal.execution != Execution::Paused =>
                {
                    goal.execution = Execution::Paused;
                    goal.note = note;
                }
                "cancel" if matches!(goal.status, Status::Draft | Status::Open) && authorized => {
                    goal.status = Status::Cancelled;
                    goal.execution = Execution::Ended;
                    goal.note = if note.is_empty() {
                        "Cancelled at user request".into()
                    } else {
                        note
                    };
                }
                "resume"
                    if goal.status == Status::Open
                        && matches!(
                            goal.execution,
                            Execution::Paused
                                | Execution::AwaitingUser
                                | Execution::Blocked
                                | Execution::Interrupted
                        )
                        && authorized =>
                {
                    goal.execution = Execution::Ready;
                    goal.note = note;
                }
                _ => {
                    return preflight_error(
                        "invalid transition, missing note, retry limit, or missing explicit user reference; goal unchanged",
                    );
                }
            }
            goal
        };
        if goal.execution != Execution::WaitingChildren {
            goal.waiting_children.clear();
        }
        if !goal.is_valid() {
            return preflight_error(
                "invalid goal: check objective, criteria, evidence and length limits",
            );
        }
        match self
            .session
            .compare_and_set_goal(previous.as_ref(), &goal)
            .await
        {
            Ok(true) => text_result(format!(
                "Goal updated: {}",
                serde_json::to_string(&goal).unwrap_or_default()
            )),
            Ok(false) => preflight_error("goal changed concurrently; reload before retrying"),
            Err(e) => error_result(format!(
                "goal save failed; recover session before retrying: {e}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::session::MemorySession;

    #[tokio::test]
    async fn goal_retry_limit_stops_new_attempts_without_failing_the_goal() {
        let session = Arc::new(MemorySession::new());
        let mut goal = Goal::draft("Verify export".into());
        goal.criteria = vec!["Export tests pass".into()];
        goal.status = Status::Open;
        goal.execution = Execution::Interrupted;
        goal.max_retries = 1;
        assert!(session.compare_and_set_goal(None, &goal).await.unwrap());
        let tool = ManageGoal::new(session.clone(), &[]);
        let cancel = CancellationToken::default();
        let retry = |revision| {
            RawValue::from_string(
                json!({"action":"retry", "revision":revision, "note":"Use a clean fixture"})
                    .to_string(),
            )
            .unwrap()
        };
        assert!(!tool.execute(&retry(1), &cancel).await.is_error);
        let previous = session.goal().unwrap();
        assert_eq!(previous.retries, 1);
        assert_eq!(previous.execution, Execution::Ready);
        let mut interrupted = previous.clone();
        interrupted.execution = Execution::Interrupted;
        assert!(
            session
                .compare_and_set_goal(Some(&previous), &interrupted)
                .await
                .unwrap()
        );
        tool.execute(&retry(previous.revision), &cancel).await;
        let stopped = session.goal().unwrap();
        assert_eq!(stopped.execution, Execution::RetryExhausted);
        assert_eq!(stopped.status, Status::Open);
        assert!(
            tool.execute(&retry(stopped.revision), &cancel)
                .await
                .is_error
        );
        assert_eq!(session.goal(), Some(stopped));
    }
}
