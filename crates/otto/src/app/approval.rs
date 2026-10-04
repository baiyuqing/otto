//! The Bash elevation and persistent-read approval loop `otto acp` and `otto serve` share.
//!
//! One call runs a first step (a prompt or a wake turn). When the step ends
//! with a Bash permission request, the loop asks the caller's decision
//! callback. `Allow` applies the grant and runs the retry
//! prompt; `Deny` ends the turn normally; `Cancelled` ends it as a cancelled
//! prompt. The controller is idle while the callback waits, so other
//! operations are not blocked by it; a frontend that must keep the session
//! busy holds its own admission rule.

use std::future::Future;

use otto_core::agent::{AgentError, Event, EventSink};
use otto_core::model::Block;
use tokio_util::sync::CancellationToken;

use super::{Controller, WakeOperation};

/// The first step of a turn.
pub enum Step<'a> {
    Text(&'a str),
    Image(&'a str, Block),
    Wake(WakeOperation<'a>),
}

/// The command and optional persistent read path a turn waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub tool_call_id: String,
    pub command: String,
    pub justification: String,
    pub read_path: String,
}

/// What the decision callback returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    /// Any answer other than allow, including a timeout.
    Deny,
    /// The turn's cancellation token fired during the wait.
    Cancelled,
}

/// How a turn that did not fail ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    EndTurn,
    Cancelled,
}

impl Controller {
    /// Runs `first` and every approval retry to its end. `emit` receives every
    /// event of every step. An error is the failing step's error, or the
    /// (already redacted) text `approve_bash` returned.
    pub async fn run_with_approvals<F, Fut>(
        &self,
        first: Step<'_>,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        mut decide: F,
    ) -> Result<Stop, AgentError>
    where
        F: FnMut(ApprovalRequest) -> Fut,
        Fut: Future<Output = ApprovalDecision>,
    {
        let session_id = self.info().session_id;
        let mut first = Some(first);
        let mut retry = String::new();
        loop {
            let mut found = None;
            let outcome = {
                let mut sink = |event: Event| {
                    if let Event::ToolCallFinished {
                        tool_name,
                        tool_call_id,
                        result,
                        ..
                    } = &event
                        && let Some(request) =
                            crate::tool::bash::parse_approval_request(tool_name, result)
                    {
                        found = Some((tool_call_id.clone(), request));
                    }
                    emit(event);
                };
                self.step_with_shared_approvals(
                    first.take(),
                    &retry,
                    &mut sink,
                    cancel,
                    &mut decide,
                )
                .await
            };
            if cancel.is_cancelled() {
                return Ok(Stop::Cancelled);
            }
            outcome?;
            let Some((tool_call_id, request)) = found else {
                return Ok(Stop::EndTurn);
            };
            let pending = self
                .builder()
                .bash_approvals
                .as_ref()
                .and_then(|approvals| approvals.pending_command(&session_id, &request.id));
            let Some(command) = pending else {
                return Ok(Stop::EndTurn);
            };
            let decision = decide(ApprovalRequest {
                approval_id: request.id.clone(),
                tool_call_id,
                command,
                justification: request.justification,
                read_path: self
                    .bash_approvals()
                    .ok()
                    .and_then(|a| a.pending_read_path(&session_id, &request.id))
                    .unwrap_or_default(),
            })
            .await;
            if cancel.is_cancelled() {
                let _ = self.deny_tool(&request.id);
                return Ok(Stop::Cancelled);
            }
            match decision {
                ApprovalDecision::Cancelled => {
                    let _ = self.deny_tool(&request.id);
                    return Ok(Stop::Cancelled);
                }
                ApprovalDecision::Deny => {
                    let _ = self.deny_tool(&request.id);
                    return Ok(Stop::EndTurn);
                }
                ApprovalDecision::Allow => {
                    match self.approve_tool(&request.id).await {
                        Ok(prompt) => retry = prompt,
                        Err(message) => {
                            // A decided request whose grant failed must not
                            // remain reserved after its frontend waiter ends.
                            let _ = self.deny_tool(&request.id);
                            return Err(AgentError::Other(message));
                        }
                    }
                }
            }
        }
    }

    async fn step_with_shared_approvals<F, Fut>(
        &self,
        first: Option<Step<'_>>,
        retry: &str,
        emit: EventSink<'_>,
        cancel: &CancellationToken,
        decide: &mut F,
    ) -> Result<(), AgentError>
    where
        F: FnMut(ApprovalRequest) -> Fut,
        Fut: Future<Output = ApprovalDecision>,
    {
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let mut sink = |event| {
            let _ = events.send(event);
        };
        let run = async {
            match first {
                Some(Step::Text(text)) => self.prompt(text, &mut sink, cancel).await,
                Some(Step::Image(text, image)) => {
                    self.prompt_with_image(text, image, &mut sink, cancel).await
                }
                Some(Step::Wake(wake)) => wake.run(&mut sink, cancel).await,
                None => self.prompt(retry, &mut sink, cancel).await,
            }
        };
        tokio::pin!(run);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
        let mut presented = std::collections::HashSet::new();
        let scope = self.approval_scope();
        loop {
            tokio::select! {
                result = &mut run => {
                    while let Ok(event) = received.try_recv() { emit(event); }
                    return result;
                },
                Some(event) = received.recv() => emit(event),
                _ = tick.tick() => {
                    let pending = self.approval_service().snapshot(&scope.workspace,Some(&scope.session_id))
                        .map_err(|e| AgentError::Other(e.to_string()))?;
                    if let Some(request) = pending.requests.into_iter().find(|r| r.blocking && presented.insert(r.id.clone())) {
                        let decision = decide(ApprovalRequest {
                            approval_id: request.id.clone(),tool_call_id: request.subject.tool_call_id.clone(),
                            command:serde_json::from_str::<serde_json::Value>(&request.subject.details).ok().and_then(|input|input["command"].as_str().map(str::to_string)).unwrap_or_else(||format!("{}\n{}",request.title,request.subject.details)),
                            justification:request.reason.clone(),read_path:serde_json::from_str::<serde_json::Value>(&request.subject.details).ok().and_then(|v|v["read_path"].as_str().map(str::to_string)).unwrap_or_default(),
                        }).await;
                        drop(self.approval_control.lock().await);
                        if decision == ApprovalDecision::Cancelled || cancel.is_cancelled() {
                            let _ = self.approval_service().withdraw(&request.id,&request.scope,request.revision);
                        } else {
                            let _ = self.decide_shared_approval(&request.id,decision == ApprovalDecision::Allow,"","frontend");
                        }
                    }
                }
            }
        }
    }

    /// `message` with provider secrets removed, for the current runtime.
    pub fn redact_error(&self, message: &str) -> String {
        let runtime = self.current_runtime().ok();
        self.builder.redact_error(message, runtime.as_ref())
    }
}
