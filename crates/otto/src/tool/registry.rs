//! The tool registry.
//!
//! A [`Registry`] owns a fixed set of tools, keyed by the name each one
//! advertises, and serves them through [`otto_core::tool::ToolExecutor`].

use std::collections::HashMap;
use std::sync::Arc;

use otto_core::model::{
    EffectCertainty, OperationDisposition, OperationOutcome, OperationStopReason, ToolDefinition,
};
use otto_core::operation::OperationControl;
use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};

use super::{CONTEXT_CANCELED, Tool};

/// A check run around every registered tool call. `before` can refuse the
/// call before the tool runs; `after` can flag a call that ran but whose
/// effects the caller should not trust. Both return operator-facing text
/// on failure.
pub trait CallGuard: Send + Sync {
    fn before(&self) -> Result<(), String>;
    fn after(&self) -> Result<(), String>;
}

/// A named, ordered set of tools.
pub struct Registry {
    ordered: Vec<Box<dyn Tool + Send + Sync>>,
    by_name: HashMap<String, usize>,
    guard: Option<Arc<dyn CallGuard>>,
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
            ordered: tools,
            by_name,
            guard: None,
        })
    }

    pub fn with_guard(mut self, guard: Arc<dyn CallGuard>) -> Self {
        self.guard = Some(guard);
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

    fn replayable(&self, name: &str) -> bool {
        self.lookup(name).is_some() && super::safety::is_replayable(name)
    }

    async fn execute(&self, call: ToolCall<'_>, control: &dyn OperationControl) -> ToolExecution {
        if let Some(reason) = control.admission_stop_reason() {
            return stopped_not_started(reason);
        }

        let Some(tool) = self.lookup(call.name) else {
            return not_started(ToolResult::unknown_tool(call.name));
        };

        if let Some(guard) = &self.guard
            && let Err(text) = guard.before()
        {
            return not_started(ToolResult::error(text));
        }

        let result = tool
            .execute(call.arguments, control.cancellation_token())
            .await;
        let mut execution = settle_tool_result(result);

        if let Some(guard) = &self.guard
            && let Err(text) = guard.after()
        {
            execution.result.content = format!("{text}\n\n{}", execution.result.content);
            execution.result.is_error = true;
            execution.outcome = outcome(OperationDisposition::Error, EffectCertainty::Unknown);
        }

        execution
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::raw;
    use crate::tool::{definition, text_result};
    use otto_core::model::{OperationId, OperationStopReason};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    struct FakeTool {
        name: &'static str,
        result: Option<ToolResult>,
        cancel_during_call: bool,
    }

    #[async_trait::async_trait]
    impl Tool for FakeTool {
        fn definition(&self) -> ToolDefinition {
            definition(self.name, "", serde_json::json!({"type": "object"}))
        }

        async fn execute(
            &self,
            _arguments: &serde_json::value::RawValue,
            cancel: &CancellationToken,
        ) -> ToolResult {
            if self.cancel_during_call {
                cancel.cancel();
            }
            self.result.clone().unwrap_or_else(|| text_result("ok"))
        }
    }

    struct CountingTool(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Tool for CountingTool {
        fn definition(&self) -> ToolDefinition {
            definition("write", "", serde_json::json!({"type": "object"}))
        }

        async fn execute(
            &self,
            _arguments: &serde_json::value::RawValue,
            _cancel: &CancellationToken,
        ) -> ToolResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            text_result("ran")
        }
    }

    fn fake(name: &'static str) -> Box<dyn Tool + Send + Sync> {
        Box::new(FakeTool {
            name,
            result: None,
            cancel_during_call: false,
        })
    }

    fn operation_id() -> OperationId {
        serde_json::from_str(r#""op_registry_test""#).unwrap()
    }

    async fn execute(registry: &Registry, name: &str, cancel: &CancellationToken) -> ToolExecution {
        let operation_id = operation_id();
        registry
            .execute(
                ToolCall {
                    operation_id: &operation_id,
                    name,
                    arguments: &raw("{}"),
                    attempt: 1,
                },
                cancel,
            )
            .await
    }

    #[tokio::test(start_paused = true)]
    async fn expired_deadline_refuses_dispatch_before_polling_the_tool() {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry =
            Registry::new(vec![Box::new(CountingTool(Arc::clone(&calls)))]).expect("registry");
        let control = crate::deadline::Control::new(crate::deadline::Deadline::after(
            std::time::Duration::ZERO,
        ));
        let operation_id = operation_id();
        let execution = registry
            .execute(
                ToolCall {
                    operation_id: &operation_id,
                    name: "write",
                    arguments: &raw("{}"),
                    attempt: 1,
                },
                &control,
            )
            .await;

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            execution.outcome.disposition,
            OperationDisposition::DeadlineExceeded
        );
        assert_eq!(
            execution.outcome.effect_certainty,
            EffectCertainty::NotStarted
        );
        assert_eq!(
            execution.outcome.stop_reason,
            Some(OperationStopReason::Deadline)
        );
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let error = Registry::new(vec![fake("read"), fake("read")])
            .err()
            .expect("a duplicate name must fail");
        assert_eq!(error, "duplicate tool: read");
    }

    #[test]
    fn definitions_lookup_and_tools_preserve_the_input_order() {
        let registry = Registry::new(vec![fake("first"), fake("second")]).unwrap();
        let definitions = registry.definitions();
        assert_eq!(definitions[0].name, "first");
        assert_eq!(definitions[1].name, "second");
        assert_eq!(
            registry.lookup("second").unwrap().definition().name,
            "second"
        );
        assert!(registry.lookup("missing").is_none());
    }

    #[tokio::test]
    async fn unknown_and_predispatch_cancel_are_not_started() {
        let registry = Registry::new(vec![fake("read")]).unwrap();
        let missing = execute(&registry, "missing", &CancellationToken::new()).await;
        assert_eq!(
            missing.outcome.effect_certainty,
            EffectCertainty::NotStarted
        );
        assert_eq!(missing.outcome.disposition, OperationDisposition::Error);

        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = execute(&registry, "read", &cancel).await;
        assert_eq!(
            cancelled.outcome.effect_certainty,
            EffectCertainty::NotStarted
        );
        assert_eq!(
            cancelled.outcome.disposition,
            OperationDisposition::Cancelled
        );
        assert_eq!(
            cancelled.outcome.stop_reason,
            Some(OperationStopReason::UserCancellation)
        );
    }

    #[tokio::test]
    async fn completed_tool_maps_result_status() {
        let registry = Registry::new(vec![fake("read")]).unwrap();
        let execution = execute(&registry, "read", &CancellationToken::new()).await;
        assert_eq!(execution.result.content, "ok");
        assert_eq!(
            execution.outcome.disposition,
            OperationDisposition::Succeeded
        );
        assert_eq!(
            execution.outcome.effect_certainty,
            EffectCertainty::Completed
        );
    }

    #[tokio::test]
    async fn a_concrete_not_started_override_is_not_upgraded_to_completed() {
        let registry = Registry::new(vec![Box::new(FakeTool {
            name: "bash",
            result: Some(ToolResult::error("invalid arguments").not_started()),
            cancel_during_call: false,
        })])
        .unwrap();

        let execution = execute(&registry, "bash", &CancellationToken::new()).await;

        assert_eq!(execution.outcome.disposition, OperationDisposition::Error);
        assert_eq!(
            execution.outcome.effect_certainty,
            EffectCertainty::NotStarted
        );
        assert!(execution.result.outcome_override.is_none());
    }

    #[tokio::test]
    async fn a_completed_override_cannot_change_the_registry_mapping() {
        let registry = Registry::new(vec![Box::new(FakeTool {
            name: "read",
            result: Some(ToolResult {
                content: "failed".into(),
                is_error: true,
                outcome_override: Some(outcome(
                    OperationDisposition::Succeeded,
                    EffectCertainty::Completed,
                )),
                ..ToolResult::default()
            }),
            cancel_during_call: false,
        })])
        .unwrap();

        let execution = execute(&registry, "read", &CancellationToken::new()).await;

        assert_eq!(execution.outcome.disposition, OperationDisposition::Error);
        assert_eq!(
            execution.outcome.effect_certainty,
            EffectCertainty::Completed
        );
    }

    #[tokio::test]
    async fn completed_tool_result_wins_over_late_cancellation() {
        let registry = Registry::new(vec![Box::new(FakeTool {
            name: "write",
            result: None,
            cancel_during_call: true,
        })])
        .unwrap();
        let cancel = CancellationToken::new();

        let execution = execute(&registry, "write", &cancel).await;

        assert_eq!(execution.result.content, "ok");
        assert_eq!(
            execution.outcome.disposition,
            OperationDisposition::Succeeded
        );
        assert_eq!(
            execution.outcome.effect_certainty,
            EffectCertainty::Completed
        );
        assert_eq!(execution.outcome.stop_reason, None);
    }

    #[tokio::test]
    async fn an_explicit_override_remains_authoritative_after_cancellation() {
        let registry = Registry::new(vec![Box::new(FakeTool {
            name: "write",
            result: Some(ToolResult::error("rejected").not_started()),
            cancel_during_call: true,
        })])
        .unwrap();
        let cancel = CancellationToken::new();

        let execution = execute(&registry, "write", &cancel).await;

        assert_eq!(execution.outcome.disposition, OperationDisposition::Error);
        assert_eq!(
            execution.outcome.effect_certainty,
            EffectCertainty::NotStarted
        );
    }

    struct FakeGuard {
        before_result: Result<(), String>,
        after_result: Result<(), String>,
    }

    impl CallGuard for FakeGuard {
        fn before(&self) -> Result<(), String> {
            self.before_result.clone()
        }

        fn after(&self) -> Result<(), String> {
            self.after_result.clone()
        }
    }

    #[tokio::test]
    async fn before_is_not_started_and_after_is_unknown() {
        let before = Registry::new(vec![fake("read")])
            .unwrap()
            .with_guard(Arc::new(FakeGuard {
                before_result: Err("lease lost".into()),
                after_result: Ok(()),
            }));
        let execution = execute(&before, "read", &CancellationToken::new()).await;
        assert_eq!(
            execution.outcome.effect_certainty,
            EffectCertainty::NotStarted
        );

        let after = Registry::new(vec![fake("read")])
            .unwrap()
            .with_guard(Arc::new(FakeGuard {
                before_result: Ok(()),
                after_result: Err("sync failed".into()),
            }));
        let execution = execute(&after, "read", &CancellationToken::new()).await;
        assert!(execution.result.is_error);
        assert_eq!(execution.result.content, "sync failed\n\nok");
        assert_eq!(execution.outcome.disposition, OperationDisposition::Error);
        assert_eq!(execution.outcome.effect_certainty, EffectCertainty::Unknown);
    }
}
