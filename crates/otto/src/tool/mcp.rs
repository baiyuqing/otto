//! The `Tool` adapter for one MCP tool.
//!
//! Design: `docs/specs/2026-09-19-mcp-design.md` ("Tool naming" and "Result
//! mapping"). One [`McpTool`] wraps one tool advertised by one server's
//! `tools/list` and forwards `execute` to [`crate::mcp::ToolServer::call`].
//!
//! Ownership: an `McpTool` holds an `Arc<dyn ToolServer>` shared with every
//! other tool on the same server, plus its own name, description, schema and
//! redaction secrets, all copied out of the untrusted `ToolInfo` at
//! construction so a later change to the server's advertised list cannot
//! reach an already-built tool. Concurrency: [`Tool::execute`] takes `&self`
//! and calls through to the shared server, which may run concurrently with
//! other tools on it. Security: the tool's name, description, schema and
//! result text are untrusted server data; the name is sanitized and length
//! capped before it reaches the model, and the result text is redacted and
//! capped the same way the built-in tools are.

use std::sync::Arc;

use otto_core::model::ToolDefinition;
use otto_core::safetext::dynamic_redaction_marker;
use otto_core::tool::ToolResult;
use serde_json::Value;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::result::{capped_text_result, redact_exact_text};
use super::{CONTEXT_CANCELED, Tool, error_result};
use crate::mcp::{CallError, CallOutcome, ContentBlock, ToolInfo, ToolServer};

/// Every registered MCP tool name starts with this.
pub const NAME_PREFIX: &str = "mcp__";
/// `safe_prompt_tool_name` in `crate::cli::prompt` rejects anything longer.
pub const MAX_TOOL_NAME_BYTES: usize = 64;
/// The advertised description is cut to this many bytes, on a char boundary.
pub const MAX_DESCRIPTION_BYTES: usize = 1024;

/// The fallback schema for a tool that advertises no usable `inputSchema`.
const FALLBACK_SCHEMA: &str = r#"{"type":"object"}"#;

/// Builds `mcp__<server>__<tool>`, sanitizing every byte of `tool` outside
/// `[A-Za-z0-9_-]` to `_`. Returns `None` when the result would exceed
/// [`MAX_TOOL_NAME_BYTES`]; truncating is not attempted because two different
/// tool names could then truncate to the same prefixed name.
pub fn prefixed_name(server: &str, tool: &str) -> Option<String> {
    let sanitized: String = tool
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
                byte as char
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{NAME_PREFIX}{server}__{sanitized}");
    if name.len() > MAX_TOOL_NAME_BYTES {
        None
    } else {
        Some(name)
    }
}

/// Cuts `text` to at most `limit` bytes, on a char boundary.
fn truncate_on_char_boundary(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// `[<server>] ` followed by the tool's description, falling back to its
/// title and then its remote name, capped at [`MAX_DESCRIPTION_BYTES`].
fn build_description(server: &str, info: &ToolInfo) -> String {
    let body = info
        .description
        .as_deref()
        .filter(|text| !text.is_empty())
        .or_else(|| info.title.as_deref().filter(|text| !text.is_empty()))
        .unwrap_or(&info.name);
    truncate_on_char_boundary(&format!("[{server}] {body}"), MAX_DESCRIPTION_BYTES)
}

/// The server's `inputSchema` verbatim when it is a JSON object, else the
/// fallback `{"type": "object"}`.
fn build_parameters(info: &ToolInfo) -> String {
    match &info.input_schema {
        Some(value) if value.is_object() => value.to_string(),
        _ => FALLBACK_SCHEMA.to_owned(),
    }
}

/// One MCP tool, adapted to the `Tool` contract.
pub struct McpTool {
    server: Arc<dyn ToolServer>,
    name: String,
    remote_name: String,
    description: String,
    parameters: String,
    max_output_bytes: usize,
    secrets: Vec<String>,
    marker: String,
}

impl McpTool {
    fn new(
        server: Arc<dyn ToolServer>,
        name: String,
        info: &ToolInfo,
        max_output_bytes: usize,
        secrets: Vec<String>,
    ) -> Self {
        let description = build_description(server.name(), info);
        let parameters = build_parameters(info);
        let marker = dynamic_redaction_marker(&secrets).unwrap_or_default();
        Self {
            server,
            name,
            remote_name: info.name.clone(),
            description,
            parameters,
            max_output_bytes,
            secrets,
            marker,
        }
    }

    fn server_name(&self) -> &str {
        self.server.name()
    }

    /// Parses the call arguments, mapping the shapes the model produces for
    /// "no arguments" to an empty object; any other non-object is rejected.
    fn parse_arguments(&self, arguments: &RawValue) -> Result<Value, ToolResult> {
        let raw = arguments.get().trim();
        if raw.is_empty() || raw == "null" {
            return Ok(Value::Object(serde_json::Map::new()));
        }
        match serde_json::from_str::<Value>(raw) {
            Ok(value) if value.is_object() => Ok(value),
            _ => Err(error_result(format!(
                "mcp {}: arguments must be a JSON object",
                self.server_name()
            ))),
        }
    }

    fn map_outcome(&self, outcome: CallOutcome) -> ToolResult {
        let text = render_outcome_text(&outcome, self.server_name());
        let redacted = redact_exact_text(&text, &self.secrets, &self.marker);
        let mut result = capped_text_result(&redacted, self.max_output_bytes);
        result.is_error = outcome.is_error;
        result
    }

    fn map_error(&self, error: CallError) -> ToolResult {
        let server = self.server_name();
        match error {
            CallError::Cancelled => error_result(CONTEXT_CANCELED),
            CallError::NeedsLogin => error_result(format!(
                "mcp {server}: authorization required; run 'otto mcp login {server}'"
            )),
            other => error_result(format!("mcp {server}: {other}")),
        }
    }
}

/// Renders a `tools/call` outcome as the text the model sees, following the
/// design's "Result mapping" section.
fn render_outcome_text(outcome: &CallOutcome, server: &str) -> String {
    if !outcome.content.is_empty() {
        outcome
            .content
            .iter()
            .map(render_block)
            .collect::<Vec<_>>()
            .join("\n")
    } else if let Some(structured) = &outcome.structured_content {
        serde_json::to_string(structured).unwrap_or_default()
    } else if outcome.is_error {
        format!("mcp {server}: tool reported an error")
    } else {
        String::new()
    }
}

fn render_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Image {
            mime_type,
            data_len,
        } => {
            format!("[image {mime_type}, {data_len} bytes base64 omitted]")
        }
        ContentBlock::Audio {
            mime_type,
            data_len,
        } => {
            format!("[audio {mime_type}, {data_len} bytes base64 omitted]")
        }
        ContentBlock::ResourceLink { uri, name, title } => {
            let label = title
                .as_deref()
                .filter(|text| !text.is_empty())
                .unwrap_or(name);
            format!("[resource {uri}] {label}")
        }
        ContentBlock::Resource {
            uri,
            mime_type,
            text,
            blob_len,
        } => match text {
            Some(text) => text.clone(),
            None => {
                let mime = mime_type.as_deref().unwrap_or("unknown");
                format!("[resource {uri}, {mime}, {blob_len} bytes blob omitted]")
            }
        },
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: Some(
                RawValue::from_string(self.parameters.clone()).expect("schema is valid JSON"),
            ),
        }
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let args = match self.parse_arguments(arguments) {
            Ok(args) => args,
            Err(result) => return result,
        };
        match self.server.call(&self.remote_name, args, cancel).await {
            Ok(outcome) => self.map_outcome(outcome),
            Err(error) => self.map_error(error),
        }
    }
}

/// Builds adapters for every tool `server` advertised, in input order.
/// Skips, with one warning each, a tool whose prefixed name would exceed
/// [`MAX_TOOL_NAME_BYTES`], and both tools of a same-name collision after
/// sanitizing.
pub fn tools_for(
    server: Arc<dyn ToolServer>,
    tools: &[ToolInfo],
    max_output_bytes: usize,
    secrets: Vec<String>,
) -> (Vec<McpTool>, Vec<String>) {
    let server_name = server.name().to_owned();
    let mut warnings = Vec::new();
    let mut names: Vec<Option<String>> = Vec::with_capacity(tools.len());
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for info in tools {
        match prefixed_name(&server_name, &info.name) {
            Some(name) => {
                *counts.entry(name.clone()).or_insert(0) += 1;
                names.push(Some(name));
            }
            None => {
                warnings.push(format!(
                    "mcp {server_name}: skipping tool {:?}: name exceeds {MAX_TOOL_NAME_BYTES} bytes after prefixing",
                    info.name
                ));
                names.push(None);
            }
        }
    }

    let mut built = Vec::with_capacity(tools.len());
    for (info, name) in tools.iter().zip(names) {
        let Some(name) = name else { continue };
        if counts[&name] > 1 {
            warnings.push(format!(
                "mcp {server_name}: skipping tool {:?}: sanitized name {name:?} collides with another tool on this server",
                info.name
            ));
            continue;
        }
        built.push(McpTool::new(
            Arc::clone(&server),
            name,
            info,
            max_output_bytes,
            secrets.clone(),
        ));
    }
    (built, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::{run, run_cancelled};
    use std::sync::Mutex;

    struct FakeServer {
        name: String,
        response: Mutex<Option<Result<CallOutcome, CallError>>>,
        seen: Mutex<Option<(String, Value)>>,
    }

    impl FakeServer {
        fn new(name: &str, response: Result<CallOutcome, CallError>) -> Arc<Self> {
            Arc::new(Self {
                name: name.to_owned(),
                response: Mutex::new(Some(response)),
                seen: Mutex::new(None),
            })
        }
    }

    #[async_trait::async_trait]
    impl ToolServer for FakeServer {
        fn name(&self) -> &str {
            &self.name
        }

        async fn call(
            &self,
            tool: &str,
            arguments: Value,
            _cancel: &CancellationToken,
        ) -> Result<CallOutcome, CallError> {
            *self.seen.lock().unwrap() = Some((tool.to_owned(), arguments));
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("test calls the fake at most once")
        }
    }

    fn info(name: &str) -> ToolInfo {
        ToolInfo {
            name: name.to_owned(),
            title: None,
            description: None,
            input_schema: None,
        }
    }

    fn tool(server: Arc<dyn ToolServer>, info: &ToolInfo, max_output_bytes: usize) -> McpTool {
        let (mut tools, warnings) = tools_for(
            server,
            std::slice::from_ref(info),
            max_output_bytes,
            Vec::new(),
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        tools.pop().expect("one tool built")
    }

    fn ok(outcome: CallOutcome) -> Result<CallOutcome, CallError> {
        Ok(outcome)
    }

    fn text_outcome(text: &str) -> CallOutcome {
        CallOutcome {
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            structured_content: None,
            is_error: false,
        }
    }

    // --- naming ---

    #[test]
    fn sanitizes_bytes_outside_the_allowed_set() {
        assert_eq!(
            prefixed_name("gh", "list.issues"),
            Some("mcp__gh__list_issues".to_owned())
        );
        assert_eq!(
            prefixed_name("gh", "a/b c"),
            Some("mcp__gh__a_b_c".to_owned())
        );
    }

    #[test]
    fn a_name_at_the_byte_limit_is_kept_and_one_over_is_rejected() {
        let server = "s";
        // "mcp__s__" is 8 bytes; pad the tool name to land exactly on 64.
        let tool = "t".repeat(MAX_TOOL_NAME_BYTES - "mcp__s__".len());
        let name = prefixed_name(server, &tool).expect("fits exactly");
        assert_eq!(name.len(), MAX_TOOL_NAME_BYTES);

        let too_long = format!("{tool}x");
        assert_eq!(prefixed_name(server, &too_long), None);
    }

    #[test]
    fn tools_for_skips_a_too_long_name_with_a_warning() {
        let server = FakeServer::new("s", ok(CallOutcome::default()));
        let too_long = info(&"t".repeat(MAX_TOOL_NAME_BYTES));
        let (tools, warnings) =
            tools_for(server, std::slice::from_ref(&too_long), 1024, Vec::new());
        assert!(tools.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(&too_long.name), "{warnings:?}");
        assert!(warnings[0].contains("exceeds"), "{warnings:?}");
    }

    #[test]
    fn tools_for_skips_both_tools_of_a_sanitized_collision() {
        let server = FakeServer::new("s", ok(CallOutcome::default()));
        let a = info("a.b");
        let b = info("a/b");
        let c = info("distinct");
        let (tools, warnings) = tools_for(server, &[a, b, c], 1024, Vec::new());
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].definition().name, "mcp__s__distinct");
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|warning| warning.contains("collides")));
    }

    #[test]
    fn tools_for_preserves_input_order() {
        let server = FakeServer::new("s", ok(CallOutcome::default()));
        let (tools, warnings) =
            tools_for(server, &[info("first"), info("second")], 1024, Vec::new());
        assert!(warnings.is_empty());
        assert_eq!(
            tools
                .iter()
                .map(|t| t.definition().name)
                .collect::<Vec<_>>(),
            vec!["mcp__s__first", "mcp__s__second"]
        );
    }

    // --- definition ---

    #[test]
    fn description_falls_back_from_description_to_title_to_remote_name() {
        let server = FakeServer::new("s", ok(CallOutcome::default()));

        let mut with_description = info("t");
        with_description.description = Some("does a thing".to_owned());
        with_description.title = Some("Title".to_owned());
        assert_eq!(
            tool(server.clone(), &with_description, 1024)
                .definition()
                .description,
            "[s] does a thing"
        );

        let mut with_title_only = info("t");
        with_title_only.title = Some("Title".to_owned());
        assert_eq!(
            tool(server.clone(), &with_title_only, 1024)
                .definition()
                .description,
            "[s] Title"
        );

        let neither = info("t");
        assert_eq!(
            tool(server.clone(), &neither, 1024)
                .definition()
                .description,
            "[s] t"
        );

        let mut empty_description = info("t");
        empty_description.description = Some(String::new());
        assert_eq!(
            tool(server, &empty_description, 1024)
                .definition()
                .description,
            "[s] t"
        );
    }

    #[test]
    fn a_long_description_is_cut_at_1024_bytes_on_a_char_boundary() {
        let server = FakeServer::new("s", ok(CallOutcome::default()));
        // A multi-byte character sits right at the cut point so a byte-only
        // cut would split it.
        let mut long = info("t");
        long.description = Some(format!("{}\u{20ac}{}", "a".repeat(1022), "b".repeat(10)));
        let description = tool(server, &long, 1024).definition().description;
        assert!(description.len() <= MAX_DESCRIPTION_BYTES);
        assert!(String::from_utf8(description.into_bytes()).is_ok());
    }

    #[test]
    fn schema_falls_back_to_a_bare_object_when_absent_or_not_an_object() {
        let server = FakeServer::new("s", ok(CallOutcome::default()));

        let no_schema = info("t");
        let parameters = tool(server.clone(), &no_schema, 1024)
            .definition()
            .parameters
            .unwrap();
        assert_eq!(parameters.get(), FALLBACK_SCHEMA);

        let mut array_schema = info("t");
        array_schema.input_schema = Some(serde_json::json!([1, 2]));
        let parameters = tool(server.clone(), &array_schema, 1024)
            .definition()
            .parameters
            .unwrap();
        assert_eq!(parameters.get(), FALLBACK_SCHEMA);

        let mut object_schema = info("t");
        object_schema.input_schema = Some(serde_json::json!({"type": "object", "properties": {}}));
        let parameters = tool(server, &object_schema, 1024)
            .definition()
            .parameters
            .unwrap();
        let value: Value = serde_json::from_str(parameters.get()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"type": "object", "properties": {}})
        );
    }

    // --- content block rendering ---

    #[tokio::test]
    async fn text_blocks_render_verbatim_and_join_with_newlines() {
        let outcome = CallOutcome {
            content: vec![
                ContentBlock::Text {
                    text: "one".to_owned(),
                },
                ContentBlock::Text {
                    text: "two".to_owned(),
                },
            ],
            ..Default::default()
        };
        let server = FakeServer::new("s", ok(outcome));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert_eq!(result.content, "one\ntwo");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn image_and_audio_blocks_report_mime_and_length_only() {
        let outcome = CallOutcome {
            content: vec![
                ContentBlock::Image {
                    mime_type: "image/png".to_owned(),
                    data_len: 42,
                },
                ContentBlock::Audio {
                    mime_type: "audio/wav".to_owned(),
                    data_len: 7,
                },
            ],
            ..Default::default()
        };
        let server = FakeServer::new("s", ok(outcome));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert_eq!(
            result.content,
            "[image image/png, 42 bytes base64 omitted]\n[audio audio/wav, 7 bytes base64 omitted]"
        );
    }

    #[tokio::test]
    async fn resource_link_prefers_title_over_name() {
        let outcome = CallOutcome {
            content: vec![ContentBlock::ResourceLink {
                uri: "file:///a".to_owned(),
                name: "a.txt".to_owned(),
                title: Some("A file".to_owned()),
            }],
            ..Default::default()
        };
        let server = FakeServer::new("s", ok(outcome));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert_eq!(result.content, "[resource file:///a] A file");
    }

    #[tokio::test]
    async fn resource_link_falls_back_to_name_without_a_title() {
        let outcome = CallOutcome {
            content: vec![ContentBlock::ResourceLink {
                uri: "file:///a".to_owned(),
                name: "a.txt".to_owned(),
                title: None,
            }],
            ..Default::default()
        };
        let server = FakeServer::new("s", ok(outcome));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert_eq!(result.content, "[resource file:///a] a.txt");
    }

    #[tokio::test]
    async fn an_embedded_resource_prefers_its_text() {
        let outcome = CallOutcome {
            content: vec![ContentBlock::Resource {
                uri: "file:///a".to_owned(),
                mime_type: Some("text/plain".to_owned()),
                text: Some("body".to_owned()),
                blob_len: 99,
            }],
            ..Default::default()
        };
        let server = FakeServer::new("s", ok(outcome));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert_eq!(result.content, "body");
    }

    #[tokio::test]
    async fn an_embedded_resource_without_text_reports_its_blob_length() {
        let outcome = CallOutcome {
            content: vec![ContentBlock::Resource {
                uri: "file:///a".to_owned(),
                mime_type: Some("image/png".to_owned()),
                text: None,
                blob_len: 99,
            }],
            ..Default::default()
        };
        let server = FakeServer::new("s", ok(outcome));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert_eq!(
            result.content,
            "[resource file:///a, image/png, 99 bytes blob omitted]"
        );
    }

    // --- structured content fallback ---

    #[tokio::test]
    async fn empty_content_falls_back_to_structured_content_as_json() {
        let outcome = CallOutcome {
            content: Vec::new(),
            structured_content: Some(serde_json::json!({"ok": true})),
            is_error: false,
        };
        let server = FakeServer::new("s", ok(outcome));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        let value: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
    }

    // --- is_error propagation ---

    #[tokio::test]
    async fn is_error_is_carried_through_with_content_present() {
        let outcome = CallOutcome {
            content: vec![ContentBlock::Text {
                text: "boom".to_owned(),
            }],
            structured_content: None,
            is_error: true,
        };
        let server = FakeServer::new("s", ok(outcome));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert!(result.is_error);
        assert_eq!(result.content, "boom");
    }

    #[tokio::test]
    async fn an_error_outcome_with_no_content_gets_a_placeholder_message() {
        let outcome = CallOutcome {
            content: Vec::new(),
            structured_content: None,
            is_error: true,
        };
        let server = FakeServer::new("s", ok(outcome));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert!(result.is_error);
        assert_eq!(result.content, "mcp s: tool reported an error");
    }

    // --- redaction and cap ---

    #[tokio::test]
    async fn a_secret_echoed_by_the_server_is_redacted() {
        let outcome = text_outcome("token=SECRET123 done");
        let server = FakeServer::new("s", ok(outcome));
        let (mut tools, warnings) =
            tools_for(server, &[info("t")], 1024, vec!["SECRET123".to_owned()]);
        assert!(warnings.is_empty());
        let tool = tools.pop().unwrap();
        let result = run(&tool, "{}").await;
        assert!(!result.content.contains("SECRET123"), "{result:?}");
        assert!(result.content.contains("token="), "{result:?}");
        assert!(result.content.contains("done"), "{result:?}");
    }

    #[tokio::test]
    async fn output_beyond_the_cap_is_truncated() {
        let outcome = text_outcome("0123456789");
        let server = FakeServer::new("s", ok(outcome));
        let tool = tool(server, &info("t"), 5);
        let result = run(&tool, "{}").await;
        assert!(result.content.contains("[truncated:"), "{result:?}");
        assert!(result.content.starts_with("01234"), "{result:?}");
    }

    // --- errors ---

    #[tokio::test]
    async fn an_rpc_error_is_reported_with_the_server_name() {
        let server = FakeServer::new(
            "s",
            Err(CallError::Rpc {
                code: -32601,
                message: "not found".to_owned(),
            }),
        );
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert!(result.is_error);
        assert_eq!(result.content, "mcp s: -32601 not found");
    }

    #[tokio::test]
    async fn a_transport_error_is_reported_with_the_server_name() {
        let server = FakeServer::new(
            "s",
            Err(CallError::Transport("child exited (1)".to_owned())),
        );
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert!(result.is_error);
        assert_eq!(result.content, "mcp s: child exited (1)");
    }

    #[tokio::test]
    async fn a_timeout_is_reported_with_the_server_name() {
        let server = FakeServer::new("s", Err(CallError::Timeout));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert!(result.is_error);
        assert_eq!(result.content, "mcp s: timed out");
    }

    #[tokio::test]
    async fn a_cancelled_call_error_is_the_go_context_canceled_text() {
        let server = FakeServer::new("s", Err(CallError::Cancelled));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert!(result.is_error);
        assert_eq!(result.content, CONTEXT_CANCELED);
    }

    #[tokio::test]
    async fn needs_login_names_the_server_twice() {
        let server = FakeServer::new("s", Err(CallError::NeedsLogin));
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "{}").await;
        assert!(result.is_error);
        assert_eq!(
            result.content,
            "mcp s: authorization required; run 'otto mcp login s'"
        );
    }

    // --- cancellation and arguments ---

    #[tokio::test]
    async fn a_cancelled_token_never_reaches_the_server() {
        let server = FakeServer::new("s", ok(CallOutcome::default()));
        let fake = server.clone();
        let tool = tool(server, &info("t"), 1024);
        let result = run_cancelled(&tool, "{}").await;
        assert!(result.is_error);
        assert_eq!(result.content, CONTEXT_CANCELED);
        assert!(fake.seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn null_arguments_become_an_empty_object() {
        // "" and whitespace-only text are handled the same way in
        // `parse_arguments` (the design's wording covers both), but neither
        // is constructible as a `RawValue`, which validates its text as JSON
        // on construction; `null` is the one reachable case.
        let server = FakeServer::new("s", ok(CallOutcome::default()));
        let fake = server.clone();
        let tool = tool(server, &info("t"), 1024);
        let result = run(&tool, "null").await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            fake.seen.lock().unwrap().as_ref().unwrap().1,
            serde_json::json!({})
        );
    }

    #[tokio::test]
    async fn non_object_arguments_are_rejected() {
        for arguments in ["[1,2]", "\"str\"", "1", "true"] {
            let server = FakeServer::new("s", ok(CallOutcome::default()));
            let fake = server.clone();
            let tool = tool(server, &info("t"), 1024);
            let result = run(&tool, arguments).await;
            assert!(result.is_error, "{arguments:?}: {result:?}");
            assert_eq!(result.content, "mcp s: arguments must be a JSON object");
            assert!(fake.seen.lock().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn the_remote_tool_name_is_passed_through_unprefixed() {
        let server = FakeServer::new("s", ok(CallOutcome::default()));
        let fake = server.clone();
        let tool = tool(server, &info("list.issues"), 1024);
        run(&tool, r#"{"a":1}"#).await;
        let seen = fake.seen.lock().unwrap();
        let (name, arguments) = seen.as_ref().unwrap();
        assert_eq!(name, "list.issues");
        assert_eq!(*arguments, serde_json::json!({"a": 1}));
    }
}
