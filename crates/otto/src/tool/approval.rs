//! Model initiation is discretionary; only admitted user handlers can decide.
use super::{Tool, definition, error_result, text_result};
use crate::approval::{Kind, Proposal, Scope, Service, State, Subject};
use async_trait::async_trait;
use otto_core::approval::BatchControl;
use otto_core::model::ToolDefinition;
use otto_core::tool::{ToolCall, ToolResult};
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct RequestApproval {
    pub service: Arc<Service>,
    pub scope: Scope,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    title: String,
    reason: String,
    #[serde(default)]
    proposal: Option<String>,
    #[serde(default)]
    subject_ref: Option<String>,
}
pub fn request_definition() -> ToolDefinition {
    definition(
        "request_approval",
        "Ask the user to approve a proposal or review a prepared action. Decide yourself when consent is needed. Supply exactly one of proposal or subject_ref. This records an approval request and returns its decision and feedback; AUTO sessions approve automatically, MANUAL sessions wait for the user; later calls in this batch will not execute. Confirmation does not grant tool permissions. Never claim approval before a recorded approval decision.",
        json!({"type":"object","additionalProperties":false,"properties":{
            "title":{"type":"string"},"reason":{"type":"string"},
            "proposal":{"type":"string"},"subject_ref":{"type":"string"}
        },"required":["title","reason"]}),
    )
}
#[async_trait]
impl Tool for RequestApproval {
    fn definition(&self) -> ToolDefinition {
        request_definition()
    }
    fn batch_control(&self) -> BatchControl {
        BatchControl::Replan
    }
    async fn execute(&self, _args: &RawValue, _cancel: &CancellationToken) -> ToolResult {
        error_result("request_approval requires a logical operation context").not_started()
    }
    async fn execute_call(&self, call: ToolCall<'_>, cancel: &CancellationToken) -> ToolResult {
        let args: Args = match serde_json::from_str(call.arguments.get()) {
            Ok(args) => args,
            Err(_) => {
                return error_result(
                    "expected title, reason and exactly one of proposal or subject_ref",
                )
                .not_started();
            }
        };
        if cancel.is_cancelled() {
            return error_result(super::CONTEXT_CANCELED).cancelled_not_started();
        }
        let request = match (args.proposal, args.subject_ref) {
            (Some(proposal), None) => self.service.create(Proposal {
                kind: Kind::Confirmation,
                scope: self.scope.clone(),
                subject: Subject {
                    tool_call_id: call.tool_call_id.into(),
                    source: "conversation".into(),
                    id: call.operation_id.to_string(),
                    version: crate::approval::digest(&proposal),
                    details: proposal,
                },
                title: args.title,
                reason: args.reason,
                persistent: false,
                blocking: true,
                expires_at: None,
            }),
            (None, Some(id)) => {
                self.service
                    .get(&id, &self.scope.workspace, Some(&self.scope.session_id))
            }
            _ => {
                return error_result("supply exactly one of proposal or subject_ref").not_started();
            }
        };
        let request = match request {
            Ok(r) if !self.scope.task_id.is_empty() && r.scope.task_id != self.scope.task_id => {
                return error_result("approval belongs to another task").not_started();
            }
            Ok(r) => r,
            Err(e) => return error_result(e).not_started(),
        };
        match self.service.wait(&request.id,&request.scope,cancel).await {
            Ok(decided) => text_result(json!({"request_id":decided.id,"decision":decided.state,
                "approved":decided.state == State::Approved,"feedback":decided.decision.map(|d| d.feedback),
                "application":decided.application}).to_string()),
            Err(e) => error_result(e).not_started(),
        }
    }
}

pub struct Control {
    pub service: Arc<Service>,
    pub scope: Scope,
    pub revoke: bool,
    pub expected_id: Option<String>,
    pub legacy: Option<Arc<super::bash::BashApprovals>>,
}
#[async_trait]
impl Tool for Control {
    fn definition(&self) -> ToolDefinition {
        definition(
            if self.revoke {
                "approval_revoke"
            } else {
                "approval_pending"
            },
            if self.revoke {
                "Withdraw an exact pending approval in this session. Cannot approve, undo execution or remove permanent grants."
            } else {
                "List pending approvals visible to this session. Decisions come from the user or the session AUTO policy."
            },
            if self.revoke {
                json!({"type":"object","additionalProperties":false,"properties":{"id":{"type":"string"}},"required":["id"]})
            } else {
                json!({"type":"object","additionalProperties":false,"properties":{},"required":[]})
            },
        )
    }
    async fn execute(&self, args: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(super::CONTEXT_CANCELED).cancelled_not_started();
        }
        if self.revoke {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Input {
                id: String,
            }
            let input: Input = match serde_json::from_str(args.get()) {
                Ok(i) => i,
                Err(_) => return error_result("expected approval id"),
            };
            if self.expected_id.as_ref().is_some_and(|id| id != &input.id) {
                return error_result("request is outside this approval dialogue");
            }
            match self.service.get(
                &input.id,
                &self.scope.workspace,
                Some(&self.scope.session_id),
            ) {
                Ok(r)
                    if !self.scope.task_id.is_empty() && r.scope.task_id != self.scope.task_id =>
                {
                    error_result("approval belongs to another task")
                }
                Ok(r) => match self.service.withdraw(&r.id, &r.scope, r.revision) {
                    Ok(_) => text_result("Approval withdrawn."),
                    Err(e) => error_result(e),
                },
                Err(crate::approval::Error::NotFound) => {
                    if let Some(legacy) = &self.legacy {
                        match legacy.revoke(&self.scope.session_id, &input.id) {
                            Ok(()) => text_result("Approval withdrawn."),
                            Err(e) => error_result(e),
                        }
                    } else {
                        error_result("approval not found")
                    }
                }
                Err(e) => error_result(e),
            }
        } else {
            if args.get().trim() != "{}" {
                return error_result("approval_pending takes no arguments");
            }
            match self
                .service
                .snapshot(&self.scope.workspace, Some(&self.scope.session_id))
            {
                Ok(snapshot) => {
                    let requests: Vec<_> = snapshot
                        .requests
                        .into_iter()
                        .filter(|request| {
                            self.scope.task_id.is_empty()
                                || request.scope.task_id == self.scope.task_id
                        })
                        .collect();
                    text_result(
                        serde_json::to_string(&requests).expect("approval requests serialize"),
                    )
                }
                Err(e) => error_result(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::model::OperationId;
    #[tokio::test]
    async fn child_controls_cannot_read_or_withdraw_another_tasks_request() {
        let service = Arc::new(Service::in_memory().unwrap());
        let mut scope = Scope {
            workspace: "/w".into(),
            session_id: "s".into(),
            task_id: "other-task".into(),
            run_id: String::new(),
        };
        let request = service
            .create(Proposal {
                kind: Kind::Confirmation,
                scope: scope.clone(),
                subject: Subject {
                    tool_call_id: String::new(),
                    source: "conversation".into(),
                    id: "other-operation".into(),
                    version: "v1".into(),
                    details: "Other task's plan".into(),
                },
                title: "Review plan".into(),
                reason: "Before proceeding".into(),
                persistent: false,
                blocking: true,
                expires_at: None,
            })
            .unwrap();
        scope.task_id = "this-task".into();
        let mut control = Control {
            service: service.clone(),
            scope,
            revoke: false,
            expected_id: None,
            legacy: None,
        };
        let cancel = CancellationToken::new();
        let pending = control
            .execute(&super::super::testutil::raw("{}"), &cancel)
            .await;
        assert_eq!(pending.content, "[]");
        control.revoke = true;
        let result = control
            .execute(
                &super::super::testutil::raw(&json!({"id": request.id}).to_string()),
                &cancel,
            )
            .await;
        assert!(result.is_error);
        assert_eq!(
            service.get(&request.id, "/w", Some("s")).unwrap().state,
            State::Pending
        );
    }
    #[tokio::test]
    async fn model_request_waits_for_a_real_decision_and_returns_feedback() {
        for allow in [true, false] {
            let service = Arc::new(Service::in_memory().unwrap());
            let scope = Scope {
                workspace: "/w".into(),
                session_id: "s".into(),
                task_id: String::new(),
                run_id: String::new(),
            };
            service.set_auto_mode(&scope, false).unwrap();
            let mut changes = service.changes();
            let tool = RequestApproval {
                service: service.clone(),
                scope: scope.clone(),
            };
            let run = tokio::spawn(async move {
                let operation = OperationId::new("confirm-operation").unwrap();
                let args = super::super::testutil::raw(
                    r#"{"title":"Review approach","reason":"Scope confirmation","proposal":"Change the parser"}"#,
                );
                tool.execute_call(
                    ToolCall {
                        tool_call_id: "call-1",
                        operation_id: &operation,
                        name: "request_approval",
                        arguments: &args,
                        attempt: 1,
                    },
                    &CancellationToken::new(),
                )
                .await
            });
            changes.changed().await.unwrap();
            let request = service
                .snapshot("/w", Some("s"))
                .unwrap()
                .requests
                .pop()
                .unwrap();
            assert!(!run.is_finished());
            assert_eq!(request.subject.tool_call_id, "call-1");
            service
                .decide(
                    &request.id,
                    &scope,
                    request.revision,
                    allow,
                    "Use the narrower scope",
                    &crate::approval::Actor::local("test-ui"),
                )
                .unwrap();
            let result = run.await.unwrap();
            let value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(value["approved"], allow);
            assert_eq!(value["feedback"], "Use the narrower scope");
        }
    }
}
