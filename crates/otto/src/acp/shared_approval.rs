//! Transport adaptation only; native approval owns decisions and persistence.
use super::{
    Backend, Connection, INTERNAL_ERROR, Reply, error, invalid_params, result, unknown_session,
};
use crate::app::ApprovalDecision;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Input {
    pub session_id: String,
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub option_id: String,
    #[serde(default)]
    pub feedback: String,
    #[serde(default)]
    pub after: i64,
}

impl Connection {
    pub(super) async fn shared_approval(&self, method: &str, params: Value) -> Reply {
        let input: Input = serde_json::from_value(params)
            .map_err(|_| invalid_params("invalid approval request"))?;
        match &self.backend {
            Backend::Local(local) => {
                let session = local
                    .session(&input.session_id)
                    .ok_or_else(unknown_session)?;
                let ctrl = &session.controller;
                let scope = ctrl.approval_scope();
                let service = ctrl.approval_service();
                let fail = |e: crate::approval::Error| error(INTERNAL_ERROR, e.to_string());
                match method {
                    "_otto/approvals/list" => result(ctrl.approval_snapshot().map_err(fail)?),
                    "_otto/approvals/watch" => result(
                        service
                            .events(&scope.workspace, Some(&scope.session_id), input.after)
                            .map_err(fail)?,
                    ),
                    "_otto/approvals/get" => result(
                        service
                            .get(&input.request_id, &scope.workspace, Some(&scope.session_id))
                            .map_err(fail)?,
                    ),
                    "_otto/approvals/withdraw" => {
                        let r = service
                            .get(&input.request_id, &scope.workspace, Some(&scope.session_id))
                            .map_err(fail)?;
                        result(
                            service
                                .withdraw(&r.id, &r.scope, input.revision)
                                .map_err(fail)?,
                        )
                    }
                    "_otto/approvals/decide" => {
                        let allow = match input.option_id.as_str() {
                            "approve" => true,
                            "reject" => false,
                            _ => return Err(invalid_params("unknown approval option")),
                        };
                        let r = service
                            .get(&input.request_id, &scope.workspace, Some(&scope.session_id))
                            .map_err(fail)?;
                        let decided = service
                            .decide(
                                &r.id,
                                &r.scope,
                                input.revision,
                                allow,
                                &input.feedback,
                                &crate::approval::Actor::local("acp-client"),
                            )
                            .map_err(fail)?;
                        result(service.finish_decision(decided).await.map_err(fail)?)
                    }
                    _ => Err(invalid_params("unknown approval operation")),
                }
            }
            Backend::Attach(relay) => relay
                .client
                .shared_approval(
                    method,
                    &input.session_id,
                    &input.request_id,
                    input.revision,
                    &input.option_id,
                    &input.feedback,
                    input.after,
                )
                .await
                .map_err(|e| error(INTERNAL_ERROR, e.to_string())),
        }
    }

    pub(super) async fn confirmation(
        &self,
        ctrl: &crate::app::Controller,
        request: &crate::approval::Request,
        cancel: &CancellationToken,
    ) -> ApprovalDecision {
        let capabilities = self
            .client_capabilities
            .lock()
            .expect("ACP capabilities")
            .clone();
        if capabilities.pointer("/_meta/otto/approvals").is_some() {
            return match ctrl
                .approval_service()
                .wait(&request.id, &request.scope, cancel)
                .await
            {
                Ok(r) if r.state == crate::approval::State::Approved => ApprovalDecision::Allow,
                Ok(r) if r.state == crate::approval::State::Withdrawn && cancel.is_cancelled() => {
                    ApprovalDecision::Cancelled
                }
                _ => ApprovalDecision::Deny,
            };
        }
        if capabilities
            .pointer("/clientCapabilities/elicitation/form")
            .is_none_or(Value::is_null)
        {
            self.send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":request.scope.session_id,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":format!("Approval pending: {}\\n{}\\nThis client cannot display confirmation. Use Otto's approval UI or _otto/approvals/decide for request {}.",request.title,request.subject.details,request.id)}}}}));
            return match ctrl
                .approval_service()
                .wait(&request.id, &request.scope, cancel)
                .await
            {
                Ok(r) if r.state == crate::approval::State::Approved => ApprovalDecision::Allow,
                _ => ApprovalDecision::Deny,
            };
        }
        let (id,reply)=self.send_request("elicitation/create",json!({"sessionId":request.scope.session_id,
            "mode":"form","message":format!("{}\\n{}\\n{}",request.title,request.subject.details,request.reason),
            "requestedSchema":{"type":"object","properties":{"approved":{"type":"boolean"},"feedback":{"type":"string"}},"required":["approved"]},
            "_meta":{"otto":{"approvalId":request.id,"revision":request.revision}}}));
        let answer = tokio::select! {
            ()=cancel.cancelled()=>None,
            _=ctrl.approval_service().wait(&request.id,&request.scope,cancel)=>None,
            frame=reply=>frame.ok(),
        };
        self.forget_request(id);
        if let Some(frame) = answer {
            let action = frame.pointer("/result/action").and_then(Value::as_str);
            if matches!(action, Some("accept" | "decline")) {
                let allow = action == Some("accept")
                    && frame
                        .pointer("/result/content/approved")
                        .and_then(Value::as_bool)
                        == Some(true);
                let feedback = frame
                    .pointer("/result/content/feedback")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let _ =
                    ctrl.decide_shared_approval(&request.id, allow, feedback, "acp-elicitation");
            }
        } else {
            self.send(
                json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":id}}),
            );
        }
        match ctrl
            .approval_service()
            .wait(&request.id, &request.scope, cancel)
            .await
        {
            Ok(r) if r.state == crate::approval::State::Approved => ApprovalDecision::Allow,
            _ => ApprovalDecision::Deny,
        }
    }
}

impl Connection {
    pub(super) async fn approval_mode(&self, method: &str, params: Value) -> Reply {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Input {
            session_id: String,
            automatic: Option<bool>,
        }
        let input: Input = serde_json::from_value(params)
            .map_err(|_| invalid_params("invalid approval mode request"))?;
        let automatic = if method.ends_with("/set") {
            Some(
                input
                    .automatic
                    .ok_or_else(|| invalid_params("automatic boolean is required"))?,
            )
        } else {
            if input.automatic.is_some() {
                return Err(invalid_params("get takes only sessionId"));
            }
            None
        };
        let mode = match &self.backend {
            Backend::Local(local) => {
                let session = local
                    .session(&input.session_id)
                    .ok_or_else(unknown_session)?;
                let ctrl = &session.controller;
                if let Some(automatic) = automatic {
                    ctrl.approval_service()
                        .set_auto_mode(&ctrl.approval_scope(), automatic)
                        .map_err(|e| error(INTERNAL_ERROR, e.to_string()))?;
                }
                ctrl.approval_service()
                    .auto_mode(&ctrl.approval_scope())
                    .map_err(|e| error(INTERNAL_ERROR, e.to_string()))?
            }
            Backend::Attach(relay) => relay
                .client
                .approval_mode(&input.session_id, automatic)
                .await
                .map_err(|e| error(INTERNAL_ERROR, e.to_string()))?,
        };
        result(json!({"automatic":mode}))
    }
}
