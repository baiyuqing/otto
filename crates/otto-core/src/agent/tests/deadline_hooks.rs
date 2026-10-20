//! Agent tests: deadline stops and hook rewriting or denial.

use super::*;

#[test]
fn deadline_tool_stop_is_typed_and_not_started() {
    let execution = stopped_tool_execution(OperationStopReason::Deadline);
    assert_eq!(
        execution.outcome,
        OperationOutcome {
            disposition: OperationDisposition::DeadlineExceeded,
            effect_certainty: EffectCertainty::NotStarted,
            stop_reason: Some(OperationStopReason::Deadline),
        }
    );
    assert_eq!(
        execution.result.content,
        ProviderError::DeadlineExceeded.to_string()
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn deadline_before_provider_dispatch_is_a_typed_error() {
    let provider = ScriptedProvider::new(Vec::new());
    let agent = Agent::new(
        provider,
        EchoExecutor,
        MemorySession::default(),
        test_options(),
    );
    let control = StoppedControl {
        token: CancellationToken::new(),
        reason: OperationStopReason::Deadline,
    };
    let error = agent
        .run_with_control("hello", &mut |_| {}, &control)
        .await
        .expect_err("deadline must stop dispatch");
    assert!(matches!(
        error,
        AgentError::Provider(ProviderError::DeadlineExceeded)
    ));
    assert!(
        agent
            .provider()
            .requests
            .lock()
            .expect("requests")
            .is_empty()
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_reports_cancellation_as_an_agent_error() {
    let agent = Agent::new(
        CancellingProvider,
        EchoExecutor,
        MemorySession::new(),
        test_options(),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut events = Vec::new();
    let error = agent
        .run("hello", &mut |event| events.push(event), &cancel)
        .await
        .expect_err("cancellation was not reported");
    assert!(matches!(
        error,
        AgentError::Provider(ProviderError::Cancelled)
    ));
    assert_eq!(
        events,
        vec![
            Event::AgentStarted,
            Event::ProviderApiCall {
                operation_id: OperationId::new("op_2").expect("operation id"),
                kind: ProviderOperationKind::Ordinary,
                provider: "fake".into(),
                model: "test-model".into(),
                duration: std::time::Duration::ZERO,
                attempts: 0,
                status: ApiStatus::Canceled,
                outcome: OperationOutcome {
                    disposition: OperationDisposition::Cancelled,
                    effect_certainty: EffectCertainty::NotStarted,
                    stop_reason: Some(OperationStopReason::UserCancellation),
                },
            },
            Event::AgentError {
                message: "provider call was cancelled".into()
            },
        ]
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_rejects_a_non_assistant_provider_response() {
    let provider = ScriptedProvider::new(vec![(
        Vec::new(),
        Response {
            message: Message {
                role: Role::User,
                blocks: vec![Block::text("wrong")],
                ..Message::default()
            },
            ..Response::default()
        },
    )]);
    let agent = Agent::new(provider, EchoExecutor, MemorySession::new(), test_options());
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("non-assistant response accepted");
    assert_eq!(
        error.to_string(),
        "invalid provider response: assistant role is required"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn run_sends_the_session_transcript_and_tool_definitions() {
    let provider = ScriptedProvider::new(vec![(Vec::new(), assistant_text())]);
    let agent = Agent::new(provider, EchoExecutor, MemorySession::new(), test_options());
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let requests = agent.provider().requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "test-model");
    assert_eq!(requests[0].system_prompt, "be brief");
    assert_eq!(requests[0].messages.len(), 1);
    assert_eq!(requests[0].messages[0].text(), "hello");
    assert_eq!(requests[0].tools, EchoExecutor.definitions());
}
#[derive(Default)]
struct TestHooks {
    events: Mutex<Vec<(String, serde_json::Value)>>,
    deny: bool,
    after_fail: bool,
}
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
impl hooks::Hooks for TestHooks {
    async fn run(
        &self,
        event: &str,
        data: serde_json::Value,
        _: &dyn OperationControl,
    ) -> Result<hooks::Response, String> {
        self.events.lock().unwrap().push((event.into(), data));
        match event {
            "before_prompt" => Ok(hooks::Response {
                context: Some("plugin context".into()),
                ..Default::default()
            }),
            "before_tool" if self.deny => Ok(hooks::Response {
                decision: Some(hooks::Decision::Deny),
                reason: Some("denied".into()),
                ..Default::default()
            }),
            "before_tool" => Ok(hooks::Response {
                arguments: Some(serde_json::json!({"value":2})),
                ..Default::default()
            }),
            "after_tool" if self.after_fail => Err("failed after hook".into()),
            "after_tool" => Ok(hooks::Response {
                content: Some("filtered".into()),
                ..Default::default()
            }),
            _ => Ok(Default::default()),
        }
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn hooks_rewrite_before_execution_preserve_effects_and_share_session_lifecycle() {
    let hooks = Arc::new(TestHooks::default());
    let mut options = test_options();
    options.hooks = Some(hooks.clone());
    let agent = Agent::new(
        ScriptedProvider::new(vec![
            (Vec::new(), assistant_tool_call()),
            (Vec::new(), assistant_text()),
            (Vec::new(), assistant_text()),
        ]),
        EchoExecutor,
        MemorySession::new(),
        options,
    );
    let mut events = Vec::new();
    agent
        .run("first", &mut |e| events.push(e), &CancellationToken::new())
        .await
        .unwrap();
    agent
        .run("second", &mut |e| events.push(e), &CancellationToken::new())
        .await
        .unwrap();
    agent.end_hooks().await;
    agent.end_hooks().await;
    let recorded = hooks.events.lock().unwrap();
    assert_eq!(
        recorded.iter().map(|(e, _)| e.as_str()).collect::<Vec<_>>(),
        vec![
            "session_start",
            "before_prompt",
            "before_tool",
            "after_tool",
            "turn_end",
            "before_prompt",
            "turn_end",
            "session_end"
        ]
    );
    assert_eq!(
        recorded.iter().find(|(e, _)| e == "after_tool").unwrap().1["content"],
        r#"{"value":2}"#
    );
    assert!(
        !recorded
            .iter()
            .find(|(e, _)| e == "before_prompt")
            .unwrap()
            .1["turn_id"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert!(
        agent
            .provider
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.system_prompt.contains("Untrusted plugin context"))
    );
    assert!(events.iter().any(|e|matches!(e,Event::ToolCallFinished{result,outcome,..} if result.content=="filtered" && outcome.effect_certainty==EffectCertainty::Completed)));
    assert!(
        agent
            .session
            .messages()
            .iter()
            .any(|m| m.role == Role::Tool && m.blocks.iter().any(|b| b.text == "filtered"))
    );
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn hook_denial_is_not_started_and_after_failure_keeps_the_tool_result() {
    for deny in [true, false] {
        let hooks = Arc::new(TestHooks {
            deny,
            after_fail: true,
            ..Default::default()
        });
        let mut options = test_options();
        options.hooks = Some(hooks.clone());
        let agent = Agent::new(
            ScriptedProvider::new(vec![
                (Vec::new(), assistant_tool_call()),
                (Vec::new(), assistant_text()),
            ]),
            EchoExecutor,
            MemorySession::new(),
            options,
        );
        let mut events = Vec::new();
        agent
            .run("run", &mut |e| events.push(e), &CancellationToken::new())
            .await
            .unwrap();
        let terminal = events
            .iter()
            .find_map(|e| {
                if let Event::ToolCallFinished {
                    result, outcome, ..
                } = e
                {
                    Some((result, outcome))
                } else {
                    None
                }
            })
            .unwrap();
        if deny {
            assert_eq!(terminal.1.effect_certainty, EffectCertainty::NotStarted);
            assert_eq!(terminal.0.content, "denied");
            assert!(
                !hooks
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(e, _)| e == "after_tool")
            );
        } else {
            assert_eq!(terminal.1.effect_certainty, EffectCertainty::Completed);
            assert_eq!(terminal.0.content, r#"{"value":2}"#);
            assert!(!terminal.0.is_error);
            assert!(events.iter().any(
                |e| matches!(e,Event::Notification{text,..} if text.contains("failed after hook"))
            ));
        }
    }
}
