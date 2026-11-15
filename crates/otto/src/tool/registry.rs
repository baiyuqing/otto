//! The tool registry.
//!
//! A [`Registry`] owns a fixed set of tools, keyed by the name each one
//! advertises, and serves them through [`otto_core::tool::ToolExecutor`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use otto_core::model::{
    EffectCertainty, OperationDisposition, OperationOutcome, OperationStopReason, ToolDefinition,
};
use otto_core::operation::OperationControl;
use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};

use super::{CONTEXT_CANCELED, Tool};

/// A check run around every registered tool call. `before` can refuse the
/// call before the tool runs; `after` can flag a call that ran but whose
/// effects the caller should not trust. `after` receives the retry safety
/// the tool declares, so a guard can skip work that only a call with
/// effects needs. Both return operator-facing text on failure.
pub(crate) trait CallGuard: Send + Sync {
    fn before(&self) -> Result<(), String>;
    fn after(&self, retry_safety: otto_core::model::RetrySafety) -> Result<(), String>;
}

/// A named, ordered set of tools.
pub struct Registry {
    authorization_run: Option<Arc<Mutex<String>>>,
    ordered: Vec<Box<dyn Tool + Send + Sync>>,
    by_name: HashMap<String, usize>,
    guard: Option<Arc<dyn CallGuard>>,
    approval: Option<(
        Arc<crate::approval::Service>,
        otto_core::approval::Scope,
        Option<Arc<crate::session::origin::TurnOrigin>>,
    )>,
}

impl Registry {
    pub fn new(tools: Vec<Box<dyn Tool + Send + Sync>>) -> Result<Self, String> {
        let mut by_name = HashMap::with_capacity(tools.len());
        for (index, tool) in tools.iter().enumerate() {
            let name = tool.definition().name;
            if by_name.insert(name.clone(), index).is_some() {
                return Err(format!("duplicate tool: {name}"));
            }
        }
        Ok(Self {
            authorization_run: None,
            ordered: tools,
            by_name,
            guard: None,
            approval: None,
        })
    }

    pub(crate) fn with_approval(
        self,
        service: Arc<crate::approval::Service>,
        scope: otto_core::approval::Scope,
    ) -> Self {
        self.with_approval_origin(service, scope, None)
    }

    pub(crate) fn with_approval_origin(
        mut self,
        service: Arc<crate::approval::Service>,
        scope: otto_core::approval::Scope,
        origin: Option<Arc<crate::session::origin::TurnOrigin>>,
    ) -> Self {
        self.approval = Some((service, scope, origin));
        self
    }

    pub(crate) fn with_guard(mut self, guard: Arc<dyn CallGuard>) -> Self {
        self.guard = Some(guard);
        self
    }
    pub(crate) fn with_authorization_run(mut self, run: Arc<Mutex<String>>) -> Self {
        self.authorization_run = Some(run);
        self
    }

    pub fn lookup(&self, name: &str) -> Option<&(dyn Tool + Send + Sync)> {
        self.by_name
            .get(name)
            .map(|&index| self.ordered[index].as_ref())
    }

    pub fn tools(&self) -> &[Box<dyn Tool + Send + Sync>] {
        &self.ordered
    }
}

impl Registry {
    pub(crate) fn approval_binding(
        &self,
    ) -> Option<(
        Arc<crate::approval::Service>,
        otto_core::approval::Scope,
        Option<Arc<crate::session::origin::TurnOrigin>>,
    )> {
        self.approval.clone()
    }
    pub(crate) async fn execute_with_scope(
        &self,
        call: ToolCall<'_>,
        control: &dyn OperationControl,
        scope_override: Option<&otto_core::approval::Scope>,
    ) -> ToolExecution {
        if let Some(reason) = control.admission_stop_reason() {
            return stopped_not_started(reason);
        }

        let Some(tool) = self.lookup(call.name) else {
            return not_started(ToolResult::unknown_tool(call.name));
        };

        if let Some(message) =
            otto_core::tool::redaction::argument_error(call.name, call.arguments.get())
        {
            return not_started(ToolResult::error(message));
        }

        if let Some(guard) = &self.guard
            && let Err(text) = guard.before()
        {
            return not_started(ToolResult::error(text));
        }

        let mut authorization = None;
        if let Some((service, scope, presentation_origin)) = &self.approval {
            let mut effective_scope = scope_override.unwrap_or(scope).clone();
            if let Some(run) = &self.authorization_run {
                effective_scope.run_id = run.lock().expect("authorization run").clone();
            }
            let scope = &effective_scope;
            let spec = match tool
                .approval_spec_async(call.arguments, control.cancellation_token())
                .await
            {
                Ok(spec) => spec,
                Err(result) => return not_started(result),
            };
            if let Some(spec) = spec {
                let version = spec.version.clone();
                let origin = presentation_origin.as_ref().and_then(|origin| origin.get());
                let mut request = match service.create_with_origin(
                    crate::approval::Proposal {
                        kind: otto_core::approval::Kind::Authorization,
                        authority: spec.authority,
                        scope: scope.clone(),
                        subject: otto_core::approval::Subject {
                            tool_call_id: call.tool_call_id.into(),
                            source: spec.source,
                            id: call.operation_id.to_string(),
                            version,
                            details: spec.details,
                        },
                        title: spec.title,
                        reason: spec.reason,
                        persistent: spec.persistent,
                        blocking: true,
                        expires_at: Some(chrono::Utc::now().timestamp() + 600),
                    },
                    origin
                        .as_ref()
                        .map_or("", crate::session::origin::Origin::as_str),
                ) {
                    Ok(r) => r,
                    Err(e) => return not_started(ToolResult::error(e.to_string())),
                };
                loop {
                    request = match wait_for_authorization(
                        service,
                        &request,
                        control.cancellation_token(),
                    )
                    .await
                    {
                        Ok(request) => request,
                        Err(error) => return not_started(ToolResult::error(error)),
                    };
                    request = match service.revalidate_authorization(&request) {
                        Ok(request) => request,
                        Err(error) => return not_started(ToolResult::error(error.to_string())),
                    };
                    if request.state == otto_core::approval::State::Pending {
                        continue;
                    }
                    if request.state != otto_core::approval::State::Approved {
                        let reason = request
                            .review
                            .as_ref()
                            .and_then(|review| review.assessment.as_ref())
                            .map(|assessment| assessment.reason.as_str())
                            .or_else(|| {
                                request
                                    .decision
                                    .as_ref()
                                    .map(|decision| decision.feedback.as_str())
                            })
                            .filter(|reason| !reason.trim().is_empty())
                            .unwrap_or("The requested authority was not granted.");
                        return not_started(ToolResult::error(format!(
                            "Operation was not approved: {reason} Choose a safer path or obtain a concrete user decision before continuing."
                        )));
                    }
                    if let Some(reason) = control.admission_stop_reason() {
                        return stopped_not_started(reason);
                    }
                    if let Some(guard) = &self.guard
                        && let Err(error) = guard.before()
                    {
                        return not_started(ToolResult::error(error));
                    }
                    match tool
                        .approval_spec_async(call.arguments, control.cancellation_token())
                        .await
                    {
                        Ok(Some(current))
                            if current.details == request.subject.details
                                && current.persistent == request.persistent
                                && current.authority == request.authority
                                && current.source == request.subject.source
                                && current.version == request.subject.version => {}
                        _ => {
                            let _ = service.supersede_unstarted(&request);
                            return not_started(ToolResult::error(
                                "Approval subject changed before dispatch; request approval again.",
                            ));
                        }
                    }
                    if let Err(e) = service.application(
                        &request.id,
                        &request.scope,
                        otto_core::approval::ApplicationState::NotStarted,
                        otto_core::approval::ApplicationState::Applying,
                        "",
                    ) {
                        if matches!(e, crate::approval::Error::Conflict)
                            && let Ok(current) = service.revalidate_authorization(&request)
                            && current.application
                                == otto_core::approval::ApplicationState::NotStarted
                            && current.state != otto_core::approval::State::Approved
                        {
                            request = current;
                            continue;
                        }
                        return not_started(ToolResult::error(e.to_string()));
                    }
                    break;
                }
                if let Err(error) = tool
                    .apply_approval(call.arguments, control.cancellation_token(), &request)
                    .await
                {
                    let _ = service.application(
                        &request.id,
                        &request.scope,
                        otto_core::approval::ApplicationState::Applying,
                        otto_core::approval::ApplicationState::Failed,
                        "Authorization could not be applied.",
                    );
                    return not_started(ToolResult::error(error));
                }
                authorization = Some(request);
            }
        }
        let result = if let Some(request) = &authorization {
            tool.execute_approved(call, control.cancellation_token(), request)
                .await
        } else {
            tool.execute_call(call, control.cancellation_token()).await
        };
        let mut execution = settle_tool_result(result);

        if let Some(guard) = &self.guard
            && let Err(text) = guard.after(tool.retry_safety())
        {
            execution.result.content = format!("{text}\n\n{}", execution.result.content);
            execution.result.is_error = true;
            execution.outcome = outcome(OperationDisposition::Error, EffectCertainty::Unknown);
        }

        if let Some(request) = authorization
            && let Some((service, _, _)) = &self.approval
        {
            let state = if execution.outcome.effect_certainty == EffectCertainty::Unknown {
                otto_core::approval::ApplicationState::Unknown
            } else if execution.result.is_error {
                otto_core::approval::ApplicationState::Failed
            } else {
                otto_core::approval::ApplicationState::Applied
            };
            let _ = service.application(
                &request.id,
                &request.scope,
                otto_core::approval::ApplicationState::Applying,
                state,
                if execution.result.is_error {
                    "Operation did not complete successfully."
                } else {
                    ""
                },
            );
        }
        execution
    }
}

async fn wait_for_authorization(
    service: &crate::approval::Service,
    request: &otto_core::approval::Request,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<otto_core::approval::Request, String> {
    let wait = service.wait(&request.id, &request.scope, cancel);
    tokio::pin!(wait);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
    loop {
        tokio::select! {
            result = &mut wait => return result.map_err(|error| error.to_string()),
            _ = tick.tick(), if request.scope.run_id.starts_with("schedule:") => {
                let current = service.get(&request.id, &request.scope.workspace, Some(&request.scope.session_id))
                    .map_err(|error| error.to_string())?;
                if current.state == otto_core::approval::State::Pending
                    && current.review.as_ref().is_none_or(|review| review.status == otto_core::approval::ReviewStatus::HumanRequired)
                {
                    return Err(format!("Human authority is required for approval {}; this unattended operation was not started. The exact request remains available for review.", current.id));
                }
            }
        }
    }
}

fn outcome(
    disposition: OperationDisposition,
    effect_certainty: EffectCertainty,
) -> OperationOutcome {
    OperationOutcome {
        disposition,
        effect_certainty,
        stop_reason: None,
    }
}

fn not_started(result: ToolResult) -> ToolExecution {
    ToolExecution {
        result,
        outcome: outcome(OperationDisposition::Error, EffectCertainty::NotStarted),
    }
}

fn stopped_not_started(reason: OperationStopReason) -> ToolExecution {
    let (text, disposition) = match reason {
        OperationStopReason::Deadline => (
            "operation deadline exceeded",
            OperationDisposition::DeadlineExceeded,
        ),
        _ => (CONTEXT_CANCELED, OperationDisposition::Cancelled),
    };
    let mut execution = not_started(ToolResult::error(text));
    execution.outcome.disposition = disposition;
    execution.outcome.stop_reason = Some(reason);
    execution
}

/// Tool overrides may narrow a completed result to a more conservative
/// certainty. They may never upgrade an uncertain result to `Completed`.
fn settle_tool_result(mut result: ToolResult) -> ToolExecution {
    let default = outcome(
        if result.is_error {
            OperationDisposition::Error
        } else {
            OperationDisposition::Succeeded
        },
        EffectCertainty::Completed,
    );
    let settled = match result.outcome_override.take() {
        Some(override_outcome)
            if override_outcome.effect_certainty != EffectCertainty::Completed =>
        {
            override_outcome
        }
        _ => default,
    };
    ToolExecution {
        result,
        outcome: settled,
    }
}

#[async_trait::async_trait]
impl ToolExecutor for Registry {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.ordered.iter().map(|tool| tool.definition()).collect()
    }

    fn retry_safety(&self, name: &str) -> otto_core::model::RetrySafety {
        self.lookup(name)
            .map(|tool| tool.retry_safety())
            .unwrap_or(otto_core::model::RetrySafety::NonIdempotent)
    }

    fn batch_control(
        &self,
        name: &str,
        execution: &ToolExecution,
    ) -> otto_core::approval::BatchControl {
        if self.approval.is_some()
            && (name == "bash" || name.starts_with(super::mcp::NAME_PREFIX))
            && execution.result.is_error
            && execution.outcome.effect_certainty == EffectCertainty::NotStarted
        {
            return otto_core::approval::BatchControl::Replan;
        }
        self.lookup(name)
            .map_or_default(|tool| tool.batch_control())
    }

    fn yields_turn(&self, name: &str, execution: &ToolExecution) -> bool {
        !execution.result.is_error
            && execution.outcome.disposition == OperationDisposition::Succeeded
            && execution.outcome.effect_certainty == EffectCertainty::Completed
            && self.lookup(name).is_some_and(|tool| tool.yields_turn())
    }

    async fn execute(&self, call: ToolCall<'_>, control: &dyn OperationControl) -> ToolExecution {
        self.execute_with_scope(call, control, None).await
    }
}

#[cfg(test)]
mod tests;
