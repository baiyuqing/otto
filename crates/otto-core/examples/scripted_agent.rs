//! Runs one agent turn offline: a scripted provider requests the `add` tool,
//! then answers with text. No network, filesystem or clock access is used.

use std::collections::VecDeque;
use std::sync::Mutex;

use otto_core::agent::{Agent, Event, Options};
use otto_core::model::{
    Block, BlockType, EffectCertainty, FinishReason, Message, OperationDisposition,
    OperationOutcome, Role, ToolDefinition,
};
use otto_core::operation::OperationControl;
use otto_core::provider::{
    Provider, ProviderError, ProviderSettlement, Request, Response, StreamEvent, StreamSink,
};
use otto_core::session::MemorySession;
use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

/// Returns the scripted responses in order, one per provider call.
struct ScriptedProvider {
    responses: Mutex<VecDeque<Message>>,
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        _request: &Request,
        emit: StreamSink<'_>,
        _control: &dyn OperationControl,
    ) -> ProviderSettlement {
        match self.responses.lock().expect("script").pop_front() {
            Some(message) => {
                let text = message.text();
                if !text.is_empty() {
                    emit(StreamEvent::TextDelta { text });
                }
                let response = Response {
                    message,
                    ..Response::default()
                };
                ProviderSettlement::succeeded(response, 1)
            }
            None => ProviderSettlement::failed(
                ProviderError::Other("script exhausted".into()),
                0,
                EffectCertainty::NotStarted,
            ),
        }
    }
}

/// One tool, `add`, which sums the integers `a` and `b`.
struct Calculator;

#[async_trait::async_trait]
impl ToolExecutor for Calculator {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let schema =
            r#"{"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}}}"#;
        vec![ToolDefinition {
            name: "add".into(),
            description: "adds two integers a and b".into(),
            parameters: Some(RawValue::from_string(schema.into()).expect("schema")),
        }]
    }

    async fn execute(&self, call: ToolCall<'_>, _control: &dyn OperationControl) -> ToolExecution {
        let args: serde_json::Value =
            serde_json::from_str(call.arguments.get()).unwrap_or_default();
        let (a, b) = (args["a"].as_i64(), args["b"].as_i64());
        if call.name != "add" || a.is_none() || b.is_none() {
            return ToolExecution {
                result: ToolResult::unknown_tool(call.name),
                outcome: OperationOutcome {
                    disposition: OperationDisposition::Error,
                    effect_certainty: EffectCertainty::NotStarted,
                    stop_reason: None,
                },
            };
        }
        let sum = a.unwrap_or_default() + b.unwrap_or_default();
        ToolExecution::completed(ToolResult {
            content: sum.to_string(),
            ..ToolResult::default()
        })
    }
}

fn assistant(finish_reason: FinishReason, blocks: Vec<Block>) -> Message {
    Message {
        role: Role::Assistant,
        finish_reason: Some(finish_reason),
        blocks,
        ..Message::default()
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let tool_call = Block {
        block_type: BlockType::ToolCall,
        tool_call_id: "call-1".into(),
        tool_name: "add".into(),
        arguments: Some(RawValue::from_string(r#"{"a":2,"b":40}"#.into()).expect("arguments")),
        ..Block::default()
    };
    let provider = ScriptedProvider {
        responses: Mutex::new(VecDeque::from([
            assistant(FinishReason::ToolCalls, vec![tool_call]),
            assistant(FinishReason::Stop, vec![Block::text("2 + 40 = 42")]),
        ])),
    };

    // otto-core has no clock and no randomness: the caller supplies both.
    let counter = std::sync::atomic::AtomicUsize::new(0);
    let options = Options {
        model: "scripted".into(),
        provider_name: "scripted".into(),
        system_prompt: "You are a calculator.".into(),
        now: Box::new(|| chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("in range")),
        new_id: Box::new(move || {
            format!(
                "id-{}",
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            )
        }),
        ..Options::default()
    };

    let agent = Agent::new(provider, Calculator, MemorySession::new(), options);
    let mut emit = |event: Event| match event {
        Event::ToolCallStarted {
            tool_name,
            arguments,
            ..
        } => println!("tool call: {tool_name}({arguments})"),
        Event::ToolCallFinished { result, .. } => println!("tool result: {}", result.content),
        Event::TextDelta { text } => println!("assistant: {text}"),
        _ => {}
    };
    let outcome = agent
        .run("What is 2 + 40?", &mut emit, &CancellationToken::new())
        .await;
    println!("turn finished: {outcome:?}");
}
