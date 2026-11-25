//! Run-loop tests: the provider contract and the tool loop.

use super::*;

// -- provider contract -----------------------------------------------------

#[tokio::test]
async fn text_deltas_and_usage_reach_the_event_sink() {
    let mut turn = Turn::text("hi");
    turn.events = vec![
        StreamEvent::TextDelta { text: "hi".into() },
        StreamEvent::TextDelta {
            text: " there".into(),
        },
    ];
    if let Ok(response) = &mut turn.outcome {
        response.message.usage = Some(Usage {
            input_tokens: 11,
            output_tokens: 3,
            cached_input_tokens: 2,
            ..Default::default()
        });
    }
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    let seen = collect(&events);
    assert_eq!(
        names(&seen),
        [
            "agent_started",
            "text_delta",
            "text_delta",
            "provider_api_call",
            "provider_usage",
            "agent_finished"
        ]
    );
    assert!(seen.contains(&Event::ProviderUsage {
        usage: Usage {
            input_tokens: 11,
            output_tokens: 3,
            cached_input_tokens: 2,
            ..Default::default()
        },
        present: true,
        cached_input_tokens_present: false,
    }));
    let Event::ProviderApiCall {
        provider,
        model,
        status,
        ..
    } = seen
        .iter()
        .find(|event| matches!(event, Event::ProviderApiCall { .. }))
        .expect("an api call event")
        .clone()
    else {
        unreachable!()
    };
    assert_eq!((provider.as_str(), model.as_str()), ("fake", "test-model"));
    assert_eq!(status, crate::agent::ApiStatus::Ok);
}

#[tokio::test]
async fn a_response_without_usage_still_emits_the_event_marked_absent() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("hi")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    // The event is always emitted, with the absence marked by `present`.
    assert!(collect(&events).contains(&Event::ProviderUsage {
        usage: Usage::default(),
        present: false,
        cached_input_tokens_present: false,
    }));
}

#[tokio::test]
async fn a_negative_usage_count_is_rejected() {
    let mut turn = Turn::text("hi");
    if let Ok(response) = &mut turn.outcome {
        response.message.usage = Some(Usage {
            input_tokens: -1,
            output_tokens: 0,
            cached_input_tokens: 0,
            ..Default::default()
        });
    }
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the usage is rejected");
    assert!(
        error.to_string().starts_with("invalid provider response"),
        "{error}"
    );
}

#[tokio::test]
async fn the_thinking_setting_is_sent_to_the_provider() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("hi")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            thinking: "high".into(),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(agent.provider().requests()[0].thinking, "high");
}

#[tokio::test]
async fn a_provider_failure_ends_the_run_and_is_reported_once() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::failure("upstream refused")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("the failure is returned");
    assert!(error.to_string().contains("upstream refused"), "{error}");
    let seen = collect(&events);
    assert_eq!(
        seen.iter()
            .filter(|event| matches!(event, Event::AgentError { .. }))
            .count(),
        1
    );
    let Event::ProviderApiCall { status, .. } = seen
        .iter()
        .find(|event| matches!(event, Event::ProviderApiCall { .. }))
        .expect("an api call event")
        .clone()
    else {
        unreachable!()
    };
    assert_eq!(status, crate::agent::ApiStatus::Error);
}

// -- tool loop -------------------------------------------------------------

/// Returns two tool calls in one assistant message.
pub(super) fn two_tool_calls() -> Turn {
    let call = |id: &str, value: i32| Block {
        block_type: BlockType::ToolCall,
        tool_call_id: id.into(),
        tool_name: "echo".into(),
        arguments: Some(raw(&format!(r#"{{"value":{value}}}"#))),
        ..Block::default()
    };
    Turn {
        events: Vec::new(),
        outcome: Ok(Response {
            message: Message {
                role: Role::Assistant,
                finish_reason: Some(FinishReason::ToolCalls),
                blocks: vec![call("c1", 1), call("c2", 2)],
                ..Message::default()
            },
            ..Response::default()
        }),
    }
}

#[tokio::test]
async fn several_tool_calls_run_in_order_and_emit_paired_events() {
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls(), Turn::text("done")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    let started: Vec<String> = collect(&events)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolCallStarted { tool_call_id, .. } => Some(tool_call_id),
            _ => None,
        })
        .collect();
    assert_eq!(started, ["c1", "c2"]);
    let finished = collect(&events)
        .into_iter()
        .filter(|event| matches!(event, Event::ToolCallFinished { .. }))
        .count();
    assert_eq!(finished, 2);
    let results: Vec<String> = agent
        .session()
        .messages()
        .into_iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| message.blocks[0].text.clone())
        .collect();
    assert_eq!(results, [r#"{"value":1}"#, r#"{"value":2}"#]);
}

#[tokio::test]
async fn an_unknown_tool_is_persisted_as_an_error_result() {
    let unknown = Turn {
        events: Vec::new(),
        outcome: Ok(Response {
            message: Message {
                role: Role::Assistant,
                finish_reason: Some(FinishReason::ToolCalls),
                blocks: vec![Block {
                    block_type: BlockType::ToolCall,
                    tool_call_id: "c1".into(),
                    tool_name: "missing".into(),
                    arguments: Some(raw("{}")),
                    ..Block::default()
                }],
                ..Message::default()
            },
            ..Response::default()
        }),
    };
    let agent = Agent::new(
        FakeProvider::new(vec![unknown, Turn::text("done")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("the run continues past an unknown tool");
    let result = agent
        .session()
        .messages()
        .into_iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result");
    assert!(result.blocks[0].is_error);
    assert!(result.blocks[0].text.contains("missing"));
}

#[tokio::test]
async fn cancellation_persists_the_first_result_and_skips_the_rest() {
    let cancel = CancellationToken::new();
    let child = cancel.clone();
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls()]),
        CancellingExecutor { cancel: child },
        MemorySession::new(),
        options(),
    );
    let error = agent
        .run("hello", &mut |_| {}, &cancel)
        .await
        .expect_err("the run is cancelled");
    assert!(error.is_cancelled(), "{error}");
    let results: Vec<Message> = agent
        .session()
        .messages()
        .into_iter()
        .filter(|message| message.role == Role::Tool)
        .collect();
    assert_eq!(
        results.len(),
        2,
        "both tool calls need a persisted result to keep the transcript valid"
    );
    assert!(
        results[1].blocks[0].is_error,
        "the skipped call was not marked as an error"
    );
}

/// Cancels the run from inside the first tool call.
pub(super) struct CancellingExecutor {
    pub(super) cancel: CancellationToken,
}

#[async_trait::async_trait]
impl ToolExecutor for CancellingExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }

    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        _control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        self.cancel.cancel();
        crate::tool::ToolExecution::completed(ToolResult {
            content: call.arguments.get().to_owned(),
            persisted_content: None,
            content_scope: Default::default(),
            is_error: false,
            outcome_override: None,
        })
    }
}
