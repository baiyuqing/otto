//! Scheduled prompt runs and the local worker's frontend hooks.
//!
//! A scheduled prompt runs as the first user message of a new session with
//! every approval request denied. Its final reply is the delivery text.

use std::sync::Arc;

use otto_core::agent::Event;
use tokio_util::sync::CancellationToken;

use super::Controller;
use super::approval::{ApprovalDecision, Step, Stop};
use crate::runtime::builder::Builder;
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
    completed_text_bytes: usize,
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
                    self.completed_text_bytes = self.reply.len();
                }
                self.after_tool = false;
                self.reply.push_str(text);
            }
            Event::ProviderUsage { .. } => self.completed_text_bytes = self.reply.len(),
            Event::ProviderRetry {
                interrupted_output: true,
                ..
            } => {
                self.reply.truncate(self.completed_text_bytes);
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
    if let Ok(runner) = controller.runner() {
        runner.set_scheduled_authorization_run("unbound");
    }
    let mut collector = ReplyCollector::default();
    let result = controller
        .run_with_channel(
            Step::Scheduled(text),
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

/// Installs creation-time original user evidence, bound to the immutable item.
/// Generated scheduled prompt text itself supplies no authorization.
pub(crate) async fn inherit_authority(
    controller: &Controller,
    item: &crate::schedule::Item,
) -> Result<(), String> {
    let builder = controller.builder();
    let repository = builder.storage.sessions()?;
    let root = builder.storage.session_root(&builder.session_root)?;
    let authority_json = repository
        .custom_data_for(
            &root,
            &item.workspace,
            &item.session_id,
            &format!("otto.schedule.{}", item.label()),
        )
        .ok_or_else(|| "Scheduled creation authorization is unavailable.".to_string())?;
    let mut authority: crate::schedule::tools::ScheduledAuthority =
        serde_json::from_str(&authority_json)
            .map_err(|_| "Scheduled creation authorization is invalid.".to_string())?;
    if authority.body_digest != crate::approval::digest(&item.body)
        || authority.source_session != item.session_id
    {
        return Err("Scheduled task changed since it was authorized.".into());
    }
    authority.context.instructions.push(format!(
        "This unattended task is limited to the approved scheduled prompt: {}. \
        Original user messages cannot authorize effects beyond this scheduled \
        scope.",
        item.body
    ));
    let source_scope = otto_core::approval::Scope {
        workspace: item.workspace.clone(),
        session_id: item.session_id.clone(),
        task_id: String::new(),
        run_id: String::new(),
    };
    let settings = builder
        .approvals
        .settings(&source_scope)
        .map_err(|error| error.to_string())?;
    let target = controller.approval_scope();
    let current = builder
        .approvals
        .settings(&target)
        .map_err(|error| error.to_string())?;
    builder
        .approvals
        .set_settings(
            &target,
            current.revision,
            settings.policy,
            settings.reviewer,
        )
        .map_err(|error| error.to_string())?;
    controller
        .runner()?
        .install_authorization_context(authority.context)
        .await?;
    controller
        .runner()?
        .set_scheduled_authorization_run(&item.label());
    Ok(())
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
    controller.close_resources().await;
    let _ = controller.close();
    outcome
}

/// Runs prompts in new sessions with the runtime `controller` currently uses.
pub struct ControllerRunner(pub Arc<Controller>);

#[async_trait::async_trait]
impl PromptRunner for ControllerRunner {
    async fn run_item(
        &self,
        item: &crate::schedule::Item,
        text: &str,
        cancel: &CancellationToken,
    ) -> RunOutcome {
        let runtime = match self.0.current_runtime() {
            Ok(runtime) => runtime,
            Err(error) => return RunOutcome::FailedBeforeTools(error),
        };
        let controller = match Controller::create(Arc::clone(self.0.builder()), &runtime).await {
            Ok(controller) => controller,
            Err(error) => return RunOutcome::FailedBeforeTools(error),
        };
        let _ = inherit_authority(&controller, item).await;
        let outcome = run(&controller, text, cancel).await;
        controller.close_resources().await;
        let _ = controller.close();
        outcome
    }
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
    use super::*;

    #[test]
    fn provider_recovery_discards_only_the_current_reply_fragment() {
        let mut collector = ReplyCollector::default();
        collector.observe(&Event::TextDelta {
            text: "earlier ✓  ".into(),
        });
        collector.observe(&Event::ProviderUsage {
            usage: Default::default(),
            present: false,
            cached_input_tokens_present: false,
        });
        collector.after_tool = true;
        for text in ["partial 一", "partial 二"] {
            collector.observe(&Event::TextDelta { text: text.into() });
            collector.observe(&Event::ProviderRetry {
                operation_id: otto_core::model::OperationId::new("retry-test").unwrap(),
                attempt: 2,
                max_attempts: 7,
                delay: std::time::Duration::ZERO,
                reason: "connection interrupted".into(),
                interrupted_output: true,
            });
        }
        collector.observe(&Event::TextDelta {
            text: "final".into(),
        });
        assert_eq!(collector.reply(), "earlier ✓\n\nfinal");
    }

    #[tokio::test]
    async fn scheduled_lineage_uses_frozen_human_evidence_and_rejects_changed_task() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let source = crate::cli::fixture_tests::controller(workspace.path(), sessions.path()).await;
        let runner = source.runner().unwrap();
        runner
            .record_user_authorization("Check the approved build later; do not publish it.")
            .await
            .unwrap();
        let store = Arc::new(crate::schedule::Store::open_in_memory().unwrap());
        let origin = Arc::new(crate::schedule::tools::TurnOrigin::default());
        origin.set(Some(Origin::terminal()));
        let context = crate::schedule::tools::Context {
            store: Some(store.clone()),
            origin,
            session_id: source.info().session_id,
            workspace: source.info().workspace,
        };
        let tools =
            crate::schedule::tools::tools_with_session(&context, runner.session().handle().clone());
        let args = serde_json::value::RawValue::from_string(
            r#"{"prompt":"Check build status without publishing","seconds":60}"#.into(),
        )
        .unwrap();
        let result = tools[0].execute(&args, &CancellationToken::new()).await;
        assert!(!result.is_error, "{}", result.content);
        let item = store
            .list(&Origin::terminal(), chrono::Utc::now())
            .unwrap()
            .remove(0);
        // Changes after creation must not expand unattended authority.
        runner
            .record_user_authorization("A separate new task permits publication.")
            .await
            .unwrap();
        source
            .approval_service()
            .set_mode(
                &source.approval_scope(),
                otto_core::approval::ApprovalMode::Manual,
            )
            .unwrap();
        let runtime = source.current_runtime().unwrap();
        let target = Controller::create(source.builder().clone(), &runtime)
            .await
            .unwrap();
        inherit_authority(&target, &item).await.unwrap();
        let saved = target
            .runner()
            .unwrap()
            .session()
            .handle()
            .custom_data(crate::approval::reviewer::CONTEXT_CUSTOM_TYPE)
            .unwrap();
        let frozen: crate::approval::reviewer::ReviewContext =
            serde_json::from_str(&saved).unwrap();
        assert_eq!(
            frozen.trusted_user_messages,
            vec!["Check the approved build later; do not publish it."]
        );
        assert!(!saved.contains("separate new task permits"));
        assert_eq!(
            target
                .approval_service()
                .settings(&target.approval_scope())
                .unwrap()
                .reviewer,
            otto_core::approval::ApprovalReviewer::User
        );
        let mut changed = item.clone();
        changed.body = "Publish instead".into();
        assert!(inherit_authority(&target, &changed).await.is_err());
        target.close().unwrap();
        source.close().unwrap();
    }
    #[tokio::test]
    async fn a_scheduled_delivery_is_a_notice_and_rings_the_bell_once() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let controller =
            crate::cli::fixture_tests::controller(workspace.path(), sessions.path()).await;
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
