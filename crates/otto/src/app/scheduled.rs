//! Scheduled prompt runs and the local worker's frontend hooks.
//!
//! A scheduled prompt runs as the first user message of a new session with
//! every approval request denied. Its final reply is the delivery text.

use std::sync::Arc;

use otto_core::agent::Event;
use tokio_util::sync::CancellationToken;

use super::Controller;
use super::approval::{ApprovalDecision, Step, Stop};
use crate::cli::runtime_builder::Builder;
use crate::schedule::worker::{PromptRunner, Sink};
use crate::schedule::{Origin, RunOutcome};
use otto_core::config::resolve::Runtime;

const BOUNDARY_CLOSED: &str =
    "scheduled runs are unavailable while the redaction boundary is closed";

/// Collects the reply text of a turn: every text delta, with a blank line
/// between the text before and after a tool call.
#[derive(Default)]
pub(super) struct ReplyCollector {
    reply: String,
    after_tool: bool,
    tool_started: bool,
}

impl ReplyCollector {
    pub(super) fn observe(&mut self, event: &Event) {
        match event {
            Event::TextDelta { text } => {
                if self.after_tool && !self.reply.is_empty() {
                    self.reply.truncate(self.reply.trim_end().len());
                    self.reply.push_str("\n\n");
                }
                self.after_tool = false;
                self.reply.push_str(text);
            }
            Event::ToolCallStarted { .. } => {
                self.after_tool = true;
                self.tool_started = true;
            }
            _ => {}
        }
    }

    pub(super) fn reply(&self) -> &str {
        &self.reply
    }
}

/// Runs `text` as a user turn on `controller`'s current session and maps the
/// end of the turn to an outcome.
pub(crate) async fn run(
    controller: &Controller,
    text: &str,
    cancel: &CancellationToken,
) -> RunOutcome {
    let mut collector = ReplyCollector::default();
    let result = controller
        .run_with_channel(
            Step::Text(text),
            None,
            None,
            &mut |event: Event| collector.observe(&event),
            cancel,
            |_| async { ApprovalDecision::Deny },
        )
        .await;
    let failure = match result {
        Ok(Stop::EndTurn) => {
            return match controller.runner() {
                Ok(runner) => RunOutcome::Reply(runner.redact_text(collector.reply().trim())),
                Err(error) => RunOutcome::Failed(error),
            };
        }
        Ok(Stop::Cancelled) => "the run was cancelled".to_string(),
        Err(error) => error.to_string(),
    };
    if collector.tool_started {
        RunOutcome::Failed(failure)
    } else {
        RunOutcome::FailedBeforeTools(failure)
    }
}

/// Runs `text` in a new session created with `runtime`, then closes it.
pub async fn run_new_session(
    builder: Arc<Builder>,
    runtime: &Runtime,
    text: &str,
    cancel: &CancellationToken,
) -> RunOutcome {
    if !builder.boundary_allows_dynamic(Some(runtime)) {
        return RunOutcome::Failed(BOUNDARY_CLOSED.into());
    }
    let controller = match Controller::create(builder, runtime).await {
        Ok(controller) => controller,
        Err(error) => return RunOutcome::FailedBeforeTools(error),
    };
    let outcome = run(&controller, text, cancel).await;
    controller.close_mcp().await;
    let _ = controller.close();
    outcome
}

/// Runs prompts in new sessions with the runtime `controller` currently uses.
pub struct ControllerRunner(pub Arc<Controller>);

#[async_trait::async_trait]
impl PromptRunner for ControllerRunner {
    /// `workspace` is ignored: the local run role only claims items of this
    /// controller's workspace.
    async fn run(&self, _workspace: &str, text: &str, cancel: &CancellationToken) -> RunOutcome {
        let runtime = match self.0.current_runtime() {
            Ok(runtime) => runtime,
            Err(error) => return RunOutcome::FailedBeforeTools(error),
        };
        run_new_session(Arc::clone(self.0.builder()), &runtime, text, cancel).await
    }
}

/// Delivers to the REPL or TUI through the controller's notices.
pub struct NoticeSink(pub Arc<Controller>);

#[async_trait::async_trait]
impl Sink for NoticeSink {
    async fn deliver(&self, _origin: &Origin, text: &str) -> Result<(), String> {
        self.0.push_scheduled(text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_scheduled_delivery_is_a_notice_and_rings_the_bell_once() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let controller = crate::cli::testutil::controller(workspace.path(), sessions.path()).await;
        assert!(!controller.take_bell());
        controller.push_scheduled("[scheduled] drink water");
        assert!(controller.take_bell());
        assert!(!controller.take_bell());
        assert!(
            controller
                .take_notices()
                .contains(&"[scheduled] drink water".to_string())
        );
    }
}
