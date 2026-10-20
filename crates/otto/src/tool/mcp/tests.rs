
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
        None,
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

fn catalog_entry(server: Arc<dyn ToolServer>, info: ToolInfo) -> McpCatalogEntry {
    let prefixed_name = prefixed_name(server.name(), &info.name).expect("prefixed name");
    McpCatalogEntry {
        server,
        info,
        prefixed_name,
        max_output_bytes: 1024,
        secrets: Vec::new(),
        bearer: None,
    }
}

#[tokio::test]
async fn router_search_returns_compact_matches_without_full_schema() {
    let server = FakeServer::new("notion", ok(CallOutcome::default()));
    let mut search_info = info("notion-ai-search");
    search_info.description = Some("Search Notion pages and connected sources".to_string());
    search_info.input_schema = Some(serde_json::json!({
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "large": {"type": "string", "description": "this schema should not appear in search results"}
        }
    }));
    let fetch_info = info("notion-fetch");
    let tools = router_tools(
        vec![
            catalog_entry(server.clone() as Arc<dyn ToolServer>, search_info),
            catalog_entry(server as Arc<dyn ToolServer>, fetch_info),
        ],
        1024,
    );
    let search = tools
        .iter()
        .find(|tool| tool.definition().name == "mcp_search_tools")
        .expect("search tool");

    let result = run(search.as_ref(), r#"{"query":"search"}"#).await;

    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("mcp__notion__notion-ai-search"),
        "{}",
        result.content
    );
    assert!(
        result.content.contains("Search Notion"),
        "{}",
        result.content
    );
    assert!(
        !result.content.contains("inputSchema"),
        "{}",
        result.content
    );
    assert!(!result.content.contains("large"), "{}", result.content);
    assert!(
        !result.content.contains("notion-fetch"),
        "{}",
        result.content
    );
}

#[test]
fn router_call_requires_approval_for_the_inner_tool() {
    let server = FakeServer::new("notion", ok(text_outcome("page contents")));
    let mut entry = catalog_entry(server.clone() as Arc<dyn ToolServer>, info("notion-fetch"));
    entry.secrets = vec!["sk-secret-value".to_owned()];
    let tools = router_tools(vec![entry], 1024);
    let call = tools
        .iter()
        .find(|tool| tool.definition().name == "mcp_call_tool")
        .expect("call tool");
    let raw = |text: &str| RawValue::from_string(text.to_owned()).unwrap();

    let spec = call
        .approval_spec(&raw(
            r#"{"tool":"mcp__notion__notion-fetch","arguments":{"id":"page-1"}}"#,
        ))
        .unwrap()
        .expect("router call needs approval");
    assert_eq!(spec.source, "mcp");
    assert_eq!(spec.title, "Authorize mcp__notion__notion-fetch?");
    assert_eq!(spec.version, crate::approval::digest(r#"{"id":"page-1"}"#));
    assert!(
        server.seen.lock().unwrap().is_none(),
        "approval must not call the tool"
    );

    let unknown = call
        .approval_spec(&raw(r#"{"tool":"mcp__notion__nope"}"#))
        .err()
        .expect("unknown tool");
    assert!(unknown.content.contains("unknown MCP tool"), "{unknown:?}");

    let leaking = call
        .approval_spec(&raw(
            r#"{"tool":"mcp__notion__notion-fetch","arguments":{"id":"sk-secret-value"}}"#,
        ))
        .err()
        .expect("credential in arguments");
    assert!(
        leaking.content.contains("expose credentials"),
        "{leaking:?}"
    );
}

#[tokio::test]
async fn router_call_forwards_to_the_selected_remote_tool() {
    let server = FakeServer::new("notion", ok(text_outcome("page contents")));
    let tools = router_tools(
        vec![catalog_entry(
            server.clone() as Arc<dyn ToolServer>,
            info("notion-fetch"),
        )],
        1024,
    );
    let call = tools
        .iter()
        .find(|tool| tool.definition().name == "mcp_call_tool")
        .expect("call tool");

    let result = run(
        call.as_ref(),
        r#"{"tool":"mcp__notion__notion-fetch","arguments":{"id":"page-1"}}"#,
    )
    .await;

    assert_eq!(result.content, "page contents");
    let seen = server.seen.lock().unwrap().clone().expect("remote call");
    assert_eq!(seen.0, "notion-fetch");
    assert_eq!(seen.1, serde_json::json!({"id": "page-1"}));
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
    let (tools, warnings) = tools_for(
        server,
        std::slice::from_ref(&too_long),
        1024,
        Vec::new(),
        None,
    );
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
    let (tools, warnings) = tools_for(server, &[a, b, c], 1024, Vec::new(), None);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].definition().name, "mcp__s__distinct");
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|warning| warning.contains("collides")));
}

#[test]
fn tools_for_preserves_input_order() {
    let server = FakeServer::new("s", ok(CallOutcome::default()));
    let (tools, warnings) = tools_for(
        server,
        &[info("first"), info("second")],
        1024,
        Vec::new(),
        None,
    );
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
    let (mut tools, warnings) = tools_for(
        server,
        &[info("t")],
        1024,
        vec!["SECRET123".to_owned()],
        None,
    );
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
    assert!(result.outcome_override.is_none());
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
    assert_eq!(
        result.outcome_override.unwrap().effect_certainty,
        otto_core::model::EffectCertainty::Unknown
    );
}

#[tokio::test]
async fn a_secret_in_a_transport_error_message_is_redacted() {
    let server = FakeServer::new("s", Err(CallError::Transport("token=SECRET123".to_owned())));
    let (mut tools, warnings) = tools_for(
        server,
        &[info("t")],
        1024,
        vec!["SECRET123".to_owned()],
        None,
    );
    assert!(warnings.is_empty());
    let tool = tools.pop().unwrap();
    let result = run(&tool, "{}").await;
    assert!(result.is_error);
    assert!(!result.content.contains("SECRET123"), "{result:?}");
    assert!(result.content.contains("token="), "{result:?}");
}

#[tokio::test]
async fn a_timeout_is_reported_with_the_server_name() {
    let server = FakeServer::new("s", Err(CallError::Timeout));
    let tool = tool(server, &info("t"), 1024);
    let result = run(&tool, "{}").await;
    assert!(result.is_error);
    assert_eq!(result.content, "mcp s: timed out");
    let outcome = result.outcome_override.unwrap();
    assert_eq!(
        outcome.disposition,
        otto_core::model::OperationDisposition::DeadlineExceeded
    );
    assert_eq!(
        outcome.effect_certainty,
        otto_core::model::EffectCertainty::Unknown
    );
}

#[tokio::test]
async fn a_cancelled_call_error_is_the_fixed_cancellation_text() {
    let server = FakeServer::new("s", Err(CallError::Cancelled));
    let tool = tool(server, &info("t"), 1024);
    let result = run(&tool, "{}").await;
    assert!(result.is_error);
    assert_eq!(result.content, CONTEXT_CANCELED);
    let outcome = result.outcome_override.unwrap();
    assert_eq!(
        outcome.disposition,
        otto_core::model::OperationDisposition::Cancelled
    );
    assert_eq!(
        outcome.effect_certainty,
        otto_core::model::EffectCertainty::Unknown
    );
}

#[tokio::test]
async fn an_interrupted_collateral_call_is_interrupted_unknown() {
    let server = FakeServer::new("s", Err(CallError::Interrupted));
    let tool = tool(server, &info("t"), 1024);
    let result = run(&tool, "{}").await;
    let outcome = result.outcome_override.expect("typed outcome");
    assert_eq!(outcome.disposition, OperationDisposition::Interrupted);
    assert_eq!(outcome.effect_certainty, EffectCertainty::Unknown);
    assert_eq!(
        outcome.stop_reason,
        Some(OperationStopReason::TransportLost)
    );
}

#[tokio::test]
async fn a_stopping_server_rejects_before_dispatch() {
    let server = FakeServer::new("s", Err(CallError::Stopping));
    let tool = tool(server, &info("t"), 1024);
    let result = run(&tool, "{}").await;
    let outcome = result.outcome_override.expect("typed outcome");
    assert_eq!(outcome.disposition, OperationDisposition::Error);
    assert_eq!(outcome.effect_certainty, EffectCertainty::NotStarted);
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

// --- OAuth bearer secrets (Finding 6) ---

struct FakeBearer {
    secrets: Vec<String>,
}

#[async_trait::async_trait]
impl BearerSource for FakeBearer {
    async fn bearer(&self, _cancel: &CancellationToken) -> Result<String, CallError> {
        Err(CallError::NeedsLogin)
    }

    async fn refresh(
        &self,
        _rejected: &str,
        _cancel: &CancellationToken,
    ) -> Result<String, CallError> {
        Err(CallError::NeedsLogin)
    }

    fn secrets(&self) -> Vec<String> {
        self.secrets.clone()
    }
}

#[tokio::test]
async fn oauth_bearer_tokens_are_redacted_from_results_though_never_configured_as_secrets() {
    let token = "oauth-secret-token";
    let server = FakeServer::new("s", ok(text_outcome(&format!("your token is {token}"))));
    let bearer: Arc<dyn BearerSource> = Arc::new(FakeBearer {
        secrets: vec![token.to_owned()],
    });
    let (mut tools, warnings) = tools_for(
        server,
        std::slice::from_ref(&info("t")),
        1024,
        Vec::new(),
        Some(bearer),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    let tool = tools.pop().expect("one tool built");
    let result = run(&tool, "{}").await;
    assert!(!result.content.contains(token), "{:?}", result.content);
}
