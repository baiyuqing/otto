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

#[test]
fn custom_read_name_cannot_inherit_builtin_retry_safety() {
    let registry = Registry::new(vec![Box::new(FakeTool {
        name: "read",
        result: None,
        cancel_during_call: false,
    })])
    .expect("registry");
    assert_eq!(
        ToolExecutor::retry_safety(&registry, "read"),
        otto_core::model::RetrySafety::NonIdempotent
    );
    assert!(!ToolExecutor::replayable(&registry, "read"));
    assert!(!ToolExecutor::replayable(&registry, "unknown"));
}

struct AsyncApprovalTool {
    calls: Arc<AtomicUsize>,
    change_on_second_call: bool,
    applied: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for AsyncApprovalTool {
    fn definition(&self) -> ToolDefinition {
        definition("async_approval", "", serde_json::json!({"type":"object"}))
    }

    async fn approval_spec_async(
        &self,
        _arguments: &serde_json::value::RawValue,
        _cancel: &CancellationToken,
    ) -> Result<Option<super::super::ApprovalSpec>, ToolResult> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(super::super::ApprovalSpec {
            version: if self.change_on_second_call && call > 0 {
                "v2"
            } else {
                "v1"
            }
            .into(),
            source: "test".into(),
            authority: otto_core::approval::Authority::Bounded,
            title: "Approve?".into(),
            reason: "test".into(),
            details: if self.change_on_second_call && call > 0 {
                "two"
            } else {
                "one"
            }
            .into(),
            persistent: false,
        }))
    }

    async fn apply_approval(
        &self,
        _arguments: &serde_json::value::RawValue,
        _cancel: &CancellationToken,
        _request: &otto_core::approval::Request,
    ) -> Result<(), String> {
        self.applied.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn execute(
        &self,
        _arguments: &serde_json::value::RawValue,
        _cancel: &CancellationToken,
    ) -> ToolResult {
        text_result("ran")
    }
}

fn approval_scope() -> otto_core::approval::Scope {
    otto_core::approval::Scope {
        workspace: "/workspace".into(),
        session_id: "session".into(),
        task_id: String::new(),
        run_id: String::new(),
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
                tool_call_id: "",
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
    let control =
        crate::deadline::Control::new(crate::deadline::Deadline::after(std::time::Duration::ZERO));
    let operation_id = operation_id();
    let execution = registry
        .execute(
            ToolCall {
                tool_call_id: "",
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
async fn async_approval_subject_is_prepared_and_revalidated() {
    let calls = Arc::new(AtomicUsize::new(0));
    let applied = Arc::new(AtomicUsize::new(0));
    let service = Arc::new(crate::approval::Service::in_memory().unwrap());
    let origin = Arc::new(crate::session::origin::TurnOrigin::default());
    origin.set(Some(
        crate::session::origin::Origin::chat("telegram", "42").unwrap(),
    ));
    let registry = Registry::new(vec![Box::new(AsyncApprovalTool {
        calls: Arc::clone(&calls),
        change_on_second_call: false,
        applied: Arc::clone(&applied),
    })])
    .unwrap()
    .with_approval_origin(service.clone(), approval_scope(), Some(origin));
    let execution = execute(&registry, "async_approval", &CancellationToken::new()).await;
    assert!(!execution.result.is_error, "{}", execution.result.content);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(applied.load(Ordering::SeqCst), 1);
    let request = service
        .events("/workspace", Some("session"), 0)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .request;
    assert_eq!(request.presentation_origin, "chat:telegram:42");
}

#[tokio::test]
async fn changed_async_approval_subject_never_reaches_effect_boundary() {
    let calls = Arc::new(AtomicUsize::new(0));
    let applied = Arc::new(AtomicUsize::new(0));
    let service = Arc::new(crate::approval::Service::in_memory().unwrap());
    let registry = Registry::new(vec![Box::new(AsyncApprovalTool {
        calls: Arc::clone(&calls),
        change_on_second_call: true,
        applied: Arc::clone(&applied),
    })])
    .unwrap()
    .with_approval(service, approval_scope());
    let execution = execute(&registry, "async_approval", &CancellationToken::new()).await;
    assert!(execution.result.is_error);
    assert!(execution.result.content.contains("subject changed"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(applied.load(Ordering::SeqCst), 0);
    assert_eq!(
        execution.outcome.effect_certainty,
        EffectCertainty::NotStarted
    );
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

    fn after(&self, _: otto_core::model::RetrySafety) -> Result<(), String> {
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

#[tokio::test]
async fn operational_placeholders_are_rejected_before_native_tool_execution() {
    let registry = Registry::new(vec![Box::new(FakeTool {
        name: "bash",
        result: None,
        cancel_during_call: true,
    })])
    .unwrap();
    let cancel = CancellationToken::new();
    let id = operation_id();
    let arguments = raw(r#"{"command":"echo \ue000"}"#);
    let execution = registry
        .execute(
            ToolCall {
                tool_call_id: "c",
                operation_id: &id,
                name: "bash",
                arguments: &arguments,
                attempt: 1,
            },
            &cancel,
        )
        .await;
    assert!(execution.result.is_error);
    assert!(execution.result.content.starts_with("redacted_input:"));
    assert_eq!(
        execution.outcome.effect_certainty,
        EffectCertainty::NotStarted
    );
    assert!(!cancel.is_cancelled(), "the concrete tool must not execute");
}
