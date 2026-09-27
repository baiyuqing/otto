//! `list_models`: the model ids the session's OpenAI-compatible endpoint
//! reports at `GET {base_url}/models`.
//!
//! The runtime builder registers it only for the `openai-compatible`
//! provider, so a model id the model writes into a reply or a config file can
//! come from the endpoint instead of from the model's training data.
//!
//! Ownership: the tool shares the session's [`Client`] through an `Arc`.
//! Concurrency: `execute` takes `&self`; the client holds no mutable state.
//! Cancellation and errors: the request races the turn token, and every
//! failure is an in-band error result carrying the client's redacted text.

use std::sync::Arc;

use otto_core::model::ToolDefinition;
use otto_core::provider::ProviderError;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::result::{capped_text_result, decode_strict_json};
use super::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};
use crate::provider::openaicompat::Client;

const DESCRIPTION: &str = "List the model ids this session's provider endpoint accepts, one per line, as the endpoint reports them at GET {base_url}/models. Call it before writing a model id into a reply, a config file, or an agent call; do not write a model id from memory.";

/// The text returned when the endpoint lists no models.
const NO_MODELS: &str = "the endpoint reported no models";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListModelsArgs {}

/// The schema advertised for `list_models`.
pub fn list_models_definition() -> ToolDefinition {
    definition(
        "list_models",
        DESCRIPTION,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
    )
}

/// Lists the endpoint's model ids.
pub struct ListModelsTool {
    client: Arc<Client>,
    max_output: usize,
}

impl ListModelsTool {
    pub fn new(client: Arc<Client>, max_output: usize) -> Self {
        Self { client, max_output }
    }
}

#[async_trait::async_trait]
impl Tool for ListModelsTool {
    fn definition(&self) -> ToolDefinition {
        list_models_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if let Err(message) = decode_strict_json::<ListModelsArgs>(arguments.get(), &[]) {
            return error_result(message);
        }
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        match self.client.list_models(cancel).await {
            Ok(ids) if ids.is_empty() => text_result(NO_MODELS),
            Ok(ids) => capped_text_result(&ids.join("\n"), self.max_output),
            Err(ProviderError::Cancelled) => error_result(CONTEXT_CANCELED),
            Err(error) => error_result(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::tool::testutil::{MAX_OUTPUT_BYTES, run, run_cancelled};

    /// Serves one fixed HTTP response to every connection and returns the base
    /// URL. The accept loop ends with the test's runtime.
    async fn serve(status: u16, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let base_url = format!("http://{}", listener.local_addr().expect("an address"));
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut head = Vec::new();
                let mut buffer = [0u8; 1024];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => head.extend_from_slice(&buffer[..read]),
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        base_url
    }

    fn tool(base_url: &str) -> ListModelsTool {
        ListModelsTool::new(Arc::new(Client::new(base_url, "key")), MAX_OUTPUT_BYTES)
    }

    #[tokio::test]
    async fn lists_the_endpoint_ids_one_per_line() {
        let base_url = serve(200, r#"{"data":[{"id":"gpt-5.6"},{"id":"deepseek-chat"}]}"#).await;
        let result = run(&tool(&base_url), "{}").await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "deepseek-chat\ngpt-5.6");
    }

    #[tokio::test]
    async fn an_empty_list_says_so() {
        let base_url = serve(200, r#"{"data":[]}"#).await;
        let result = run(&tool(&base_url), "{}").await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, NO_MODELS);
    }

    #[tokio::test]
    async fn a_provider_failure_is_an_in_band_error() {
        let result = run(&tool("not a url"), "{}").await;
        assert!(result.is_error);
        assert_eq!(result.content, "invalid OpenAI-compatible base URL");
    }

    #[tokio::test]
    async fn unknown_arguments_and_cancellation_are_rejected() {
        let result = run(&tool("not a url"), r#"{"filter":"gpt"}"#).await;
        assert!(result.is_error);
        assert_eq!(result.content, r#"json: unknown field "filter""#);

        let result = run_cancelled(&tool("not a url"), "{}").await;
        assert!(result.is_error);
        assert_eq!(result.content, CONTEXT_CANCELED);
    }
}
