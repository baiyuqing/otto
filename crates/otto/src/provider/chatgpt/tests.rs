use super::*;
use crate::auth::Credentials;
use crate::faults::http::{self as testserver, sse_response};
use otto_core::model::{Block, BlockType, FinishReason, Message, Role, ToolDefinition, Usage};
use otto_core::provider::StreamEvent;
use tokio_util::sync::CancellationToken;

mod auth_recovery;
mod catalog;
mod retry;
mod stream;

struct StoppedControl {
    token: CancellationToken,
    reason: OperationStopReason,
}

impl StoppedControl {
    fn deadline() -> Self {
        let token = CancellationToken::new();
        token.cancel();
        Self {
            token,
            reason: OperationStopReason::Deadline,
        }
    }
}

impl OperationControl for StoppedControl {
    fn cancellation_token(&self) -> &CancellationToken {
        &self.token
    }

    fn remaining(&self) -> Option<Duration> {
        Some(Duration::ZERO)
    }

    fn stop_reason(&self) -> Option<OperationStopReason> {
        Some(self.reason)
    }
}

const CANNED_STREAM: &str = concat!(
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n",
    "\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"world\"}\n",
    "\n",
    "event: response.output_item.added\n",
    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_abc\",\"name\":\"get_time\"}}\n",
    "\n",
    "event: response.function_call_arguments.delta\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{\\\"tz\\\":\"}\n",
    "\n",
    "event: response.function_call_arguments.delta\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"\\\"utc\\\"}\"}\n",
    "\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n",
    "\n",
);

/// A token source that never refreshes: the zero expiry never expires, so
/// no endpoint is contacted.
fn static_tokens(access_token: &str) -> TokenSource {
    TokenSource::new(
        Credentials {
            access_token: access_token.to_owned(),
            ..Credentials::default()
        },
        std::path::PathBuf::from("/nonexistent/chatgpt.json"),
        CancellationToken::new(),
    )
}

fn model_request() -> Request {
    Request {
        model: "gpt-5-codex".to_owned(),
        system_prompt: "sys".to_owned(),
        messages: vec![Message {
            role: Role::User,
            blocks: vec![Block {
                block_type: BlockType::Text,
                text: "hi".to_owned(),
                ..Block::default()
            }],
            ..Message::default()
        }],
        tools: vec![ToolDefinition {
            name: "get_time".to_owned(),
            description: "get the time".to_owned(),
            parameters: serde_json::value::RawValue::from_string(r#"{"type":"object"}"#.to_owned())
                .ok(),
        }],
        ..Request::default()
    }
}

async fn complete(
    client: &Client,
    request: &Request,
) -> (otto_core::provider::ProviderSettlement, Vec<StreamEvent>) {
    let mut events = Vec::new();
    let result = {
        let mut sink = |event: StreamEvent| events.push(event);
        client
            .complete(request, &mut sink, &CancellationToken::new())
            .await
    };
    (result, events)
}

#[test]
fn serialized_request_size_matches_the_exact_wire_payload() {
    let client = Client::new(static_tokens("token"), "acct-1");
    let request = model_request();
    let expected = serde_json::to_vec(&otto_core::openairesponses::protocol::build_request(
        &request,
    ))
    .unwrap()
    .len();
    assert_eq!(client.serialized_request_size(&request).unwrap(), expected);
}

/// A stream in which the backend ran its own web search: the hosted call's
/// output items and a citation annotation must not disturb the text.
const HOSTED_SEARCH_STREAM: &str = concat!(
    "event: response.output_item.added\n",
    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"in_progress\"}}\n",
    "\n",
    "event: response.web_search_call.searching\n",
    "data: {\"type\":\"response.web_search_call.searching\",\"item_id\":\"ws_1\"}\n",
    "\n",
    "event: response.output_item.done\n",
    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"query\":\"rust\"}}}\n",
    "\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Rust is a language.\"}\n",
    "\n",
    "event: response.output_text.annotation.added\n",
    "data: {\"type\":\"response.output_text.annotation.added\",\"annotation\":{\"type\":\"url_citation\",\"url\":\"https://www.rust-lang.org/\"}}\n",
    "\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n",
    "\n",
);

#[tokio::test]
async fn hosted_web_search_is_advertised_only_when_enabled_and_its_items_are_ignored() {
    let server = testserver::spawn(|_| sse_response(HOSTED_SEARCH_STREAM)).await;
    let off = trusted_test_client(&server.url, static_tokens("test-token"), "acct-1");
    let on = trusted_test_client(&server.url, static_tokens("test-token"), "acct-1")
        .with_hosted_web_search(true);

    let (settlement, _) = complete(&off, &model_request()).await;
    settlement.result.unwrap();
    let (settlement, _) = complete(&on, &model_request()).await;
    let response = settlement.result.unwrap();

    let sent = server.requests();
    let tools = |index: usize| -> Vec<serde_json::Value> {
        let body: serde_json::Value = serde_json::from_str(&sent[index].body).unwrap();
        body["tools"].as_array().unwrap().clone()
    };
    assert!(tools(0).iter().all(|tool| tool["type"] == "function"));
    let enabled = tools(1);
    assert_eq!(
        enabled.last().unwrap(),
        &serde_json::json!({"type": "web_search"})
    );
    assert_eq!(enabled.len(), tools(0).len() + 1);

    assert_eq!(
        response.message.blocks.len(),
        1,
        "no tool call for a hosted search"
    );
    assert_eq!(response.message.blocks[0].block_type, BlockType::Text);
    assert_eq!(response.message.blocks[0].text, "Rust is a language.");
    assert_eq!(response.message.finish_reason, Some(FinishReason::Stop));
    // The size estimate counts the extra tool.
    assert_eq!(
        on.serialized_request_size(&model_request()).unwrap(),
        sent[1].body.len()
    );
}

fn redaction_marker(values: &[String]) -> String {
    let mut collector = otto_core::safetext::SecretCollector::new();
    for value in values {
        assert!(collector.add(value));
    }
    otto_core::safetext::dynamic_redaction_marker(&collector.values()).unwrap()
}

// Mock endpoints receive explicit test-owned origin grants, never production bypasses.
fn trusted_test_client(base_url: &str, tokens: TokenSource, account_id: &str) -> Client {
    let origin = url::Url::parse(base_url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_default();
    match otto_core::config::endpoint::TrustedEndpoint::authorize(
        base_url,
        "chatgpt",
        std::slice::from_ref(&origin),
        std::slice::from_ref(&origin),
    ) {
        Ok(endpoint) => Client::new_trusted(&endpoint, tokens, account_id),
        Err(_) => Client::with_base_url(base_url, tokens, account_id),
    }
}
