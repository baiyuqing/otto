//! Map one ACP permission request to one Otto user decision.

use super::acp_transport::PermissionHandler;
use crate::approval::{Proposal, Service};
use otto_core::agent::redactor::Redactor;
use otto_core::approval::{Authority, Kind, Scope, State, Subject};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) struct Handler {
    pub service: Arc<Service>,
    pub scope: Scope,
    pub redactor: Redactor,
    pub cancel: CancellationToken,
}

#[async_trait::async_trait]
impl PermissionHandler for Handler {
    async fn decide(&self, params: Value) -> Value {
        let cancelled = || json!({"outcome":{"outcome":"cancelled"}});
        if self.cancel.is_cancelled() || !self.redactor.allows_dynamic_content() {
            return cancelled();
        }
        let Some(tool) = params.get("toolCall").filter(|tool| tool.is_object()) else {
            return cancelled();
        };
        let Some(id) = tool
            .get("toolCallId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return cancelled();
        };
        let Some(title) = tool
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
        else {
            return cancelled();
        };
        let Some(raw_input) = tool.get("rawInput") else {
            return cancelled();
        };
        let Some(option) = params
            .get("options")
            .and_then(Value::as_array)
            .and_then(|options| options.iter().find(|item| item["kind"] == "allow_once"))
            .and_then(|item| item.get("optionId"))
            .and_then(Value::as_str)
        else {
            return cancelled();
        };
        let details = json!({"title": title, "input": raw_input}).to_string();
        if details.len() > 8192 {
            return cancelled();
        }
        let redacted_details = self.redactor.redact_string(&details);
        if redacted_details != details {
            return cancelled();
        }
        let details = redacted_details;
        let title = self.redactor.redact_string(title);
        let safe_id = crate::approval::digest(id);
        let request = self.service.create(Proposal {
            kind: Kind::Confirmation,
            authority: Authority::Boundary,
            scope: self.scope.clone(),
            subject: Subject {
                tool_call_id: safe_id.clone(),
                source: "external-acp".into(),
                id: safe_id,
                version: crate::approval::digest(&details),
                details,
            },
            title: format!("External agent: {title}"),
            reason: "An external agent requests permission for this action. Its own sandbox and permission rules apply.".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        });
        let Ok(request) = request else {
            return cancelled();
        };
        let decision = self
            .service
            .wait(&request.id, &request.scope, &self.cancel)
            .await;
        if decision.is_ok_and(|decision| decision.state == State::Approved) {
            json!({"outcome":{"outcome":"selected", "optionId":option}})
        } else {
            cancelled()
        }
    }
}

#[cfg(test)]
#[path = "acp_approval_tests.rs"]
mod tests;
