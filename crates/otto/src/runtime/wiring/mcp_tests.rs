use super::*;
use crate::runtime::mcp_server_fixture_tests::shell_mcp_server;
use otto_core::config::{McpRuntime, McpServerRuntime};

fn builder_with_servers(root: &std::path::Path, servers: Vec<McpServerRuntime>) -> Builder {
    let mut builder = crate::fixtures::builder(root, &root.join("sessions"));
    builder.mcp = McpRuntime {
        enabled: true,
        call_timeout_secs: 5,
        connect_timeout_secs: 1,
        servers,
    };
    builder
}

fn stdio_server(
    name: &str,
    enabled: bool,
    command: &str,
    cwd: &std::path::Path,
) -> McpServerRuntime {
    McpServerRuntime {
        name: name.to_string(),
        enabled,
        transport: McpTransport::Stdio {
            command: command.to_string(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: cwd.to_string_lossy().into_owned(),
        },
        secrets: Vec::new(),
    }
}

#[test]
fn connecting_servers_report_configuration_order_without_dialing() {
    let directory = tempfile::tempdir().expect("directory");
    let builder = builder_with_servers(
        directory.path(),
        vec![
            stdio_server(
                "slow",
                true,
                "otto-mcp-test-nonexistent-command",
                directory.path(),
            ),
            stdio_server(
                "off",
                false,
                "otto-mcp-test-nonexistent-command",
                directory.path(),
            ),
        ],
    );

    let status = builder.connecting_mcp_servers().status();
    assert_eq!(status.len(), 2);
    assert_eq!(status[0].name, "slow");
    assert_eq!(status[0].transport, "stdio");
    assert_eq!(status[0].state, mcp::ServerState::Connecting);
    assert_eq!(status[1].name, "off");
    assert_eq!(status[1].state, mcp::ServerState::Disabled);
}

#[tokio::test]
async fn a_disabled_server_is_reported_without_connecting() {
    let directory = tempfile::tempdir().expect("directory");
    let builder = builder_with_servers(
        directory.path(),
        vec![stdio_server(
            "example",
            false,
            "otto-mcp-test-nonexistent-command",
            directory.path(),
        )],
    );

    let mut warnings = Vec::new();
    let (tools, connected, servers) = builder.connect_mcp(65536, &mut warnings).await;
    assert!(tools.is_empty());
    assert!(connected.is_empty());
    let status = servers.status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].name, "example");
    assert_eq!(status[0].transport, "stdio");
    assert_eq!(status[0].state, mcp::ServerState::Disabled);
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[tokio::test]
async fn a_command_that_cannot_be_spawned_is_reported_as_failed_and_does_not_stop_the_runner() {
    let directory = tempfile::tempdir().expect("directory");
    let builder = builder_with_servers(
        directory.path(),
        vec![stdio_server(
            "broken",
            true,
            "otto-mcp-test-nonexistent-command",
            directory.path(),
        )],
    );

    let mut warnings = Vec::new();
    let (tools, connected, servers) = builder.connect_mcp(65536, &mut warnings).await;
    assert!(tools.is_empty());
    assert!(connected.is_empty());
    let status = servers.status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].name, "broken");
    assert!(
        matches!(status[0].state, mcp::ServerState::Failed(_)),
        "{:?}",
        status[0].state
    );
    let text = String::from_utf8(warnings).expect("utf-8 warnings");
    assert!(text.contains("broken"), "{text:?}");
    assert!(text.starts_with("warning: "), "{text:?}");
}

struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_connected_and_a_failed_server_each_write_their_event() {
    let directory = tempfile::tempdir().expect("directory");
    let mut builder = builder_with_servers(
        directory.path(),
        vec![
            shell_mcp_server("good", directory.path()),
            stdio_server(
                "broken",
                true,
                "otto-mcp-test-nonexistent-command",
                directory.path(),
            ),
        ],
    );
    let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
    builder.shared_mut().logger = Arc::new(otto_platform::log::Logger::new(Box::new(CapturedLog(
        Arc::clone(&bytes),
    ))));

    let mut warnings = Vec::new();
    let (_tools, _connected, servers) = builder.connect_mcp(65536, &mut warnings).await;
    let status = servers.status();
    let shown = match &status[1].state {
        mcp::ServerState::Failed(message) => message.clone(),
        other => panic!("{other:?}"),
    };
    let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let connected = text
        .lines()
        .find(|line| line.contains("msg=mcp_server_connected"))
        .unwrap_or_else(|| panic!("no connected event in:\n{text}"));
    for field in ["level=INFO", "server=good", "transport=stdio", "tools=1"] {
        assert!(connected.contains(field), "{field} missing in {connected}");
    }
    let failed = text
        .lines()
        .find(|line| line.contains("msg=mcp_server_failed"))
        .unwrap_or_else(|| panic!("no failed event in:\n{text}"));
    for field in ["level=WARN", "server=broken", "transport=stdio"] {
        assert!(failed.contains(field), "{field} missing in {failed}");
    }
    let quoted = serde_json::to_string(&shown).unwrap();
    assert!(
        failed.contains(&format!("error={quoted}")) || failed.contains(&format!("error={shown}")),
        "the logged error must equal the /mcp text {shown:?}: {failed}"
    );
    servers.close().await;
}

#[tokio::test]
async fn a_server_whose_secrets_exceed_redaction_limits_fails_without_connecting() {
    let directory = tempfile::tempdir().expect("directory");
    let mut server = stdio_server(
        "oversized",
        true,
        "otto-mcp-test-nonexistent-command",
        directory.path(),
    );
    server.secrets = vec!["x".repeat(9 * 1024)];
    let builder = builder_with_servers(directory.path(), vec![server]);

    let mut warnings = Vec::new();
    let (tools, connected, servers) = builder.connect_mcp(65536, &mut warnings).await;
    assert!(tools.is_empty());
    assert!(connected.is_empty());
    let status = servers.status();
    assert_eq!(status.len(), 1);
    assert_eq!(
        status[0].state,
        mcp::ServerState::Failed(SECRETS_EXCEED_REDACTION_LIMITS.to_string())
    );
    let text = String::from_utf8(warnings).expect("utf-8 warnings");
    assert!(text.contains(SECRETS_EXCEED_REDACTION_LIMITS), "{text:?}");
}

#[tokio::test]
async fn connection_results_are_reported_in_configuration_order() {
    let directory = tempfile::tempdir().expect("directory");
    let builder = builder_with_servers(
        directory.path(),
        vec![
            stdio_server(
                "first",
                true,
                "otto-mcp-test-first-missing",
                directory.path(),
            ),
            stdio_server(
                "second",
                true,
                "otto-mcp-test-second-missing",
                directory.path(),
            ),
        ],
    );

    let mut warnings = Vec::new();
    let (tools, connected, servers) = builder.connect_mcp(65536, &mut warnings).await;
    assert!(tools.is_empty());
    assert!(connected.is_empty());
    let status = servers.status();
    assert_eq!(status.len(), 2);
    assert_eq!(status[0].name, "first");
    assert_eq!(status[1].name, "second");
    let text = String::from_utf8(warnings).expect("utf-8 warnings");
    let first = text.find("first").expect("first warning");
    let second = text.find("second").expect("second warning");
    assert!(first < second, "{text}");
}

#[test]
fn a_secret_in_a_connect_failure_message_is_redacted() {
    // `connect_mcp` cannot be driven end to end with a real connect
    // failure that echoes a secret (a spawn error's text is OS-owned,
    // not server-controlled); this drives the extracted pure
    // `failure_message` helper `connect_mcp` calls in its `Err(error)`
    // arm instead.
    let secrets = vec!["SECRET123".to_owned()];
    let marker = dynamic_redaction_marker(&secrets).expect("marker");
    let error = mcp::CallError::Transport("token=SECRET123".to_owned());
    let message = failure_message(&error, &secrets, &marker);
    assert!(!message.contains("SECRET123"), "{message:?}");
    assert!(message.contains("token="), "{message:?}");
}

#[test]
fn mcp_adapter_tools_is_empty_for_no_connected_servers() {
    assert!(mcp_adapter_tools(&[], 65536).is_empty());
}

#[test]
fn lazy_mcp_exposes_router_tools_instead_of_remote_tool_schemas() {
    let server: Arc<dyn mcp::ToolServer> = Arc::new(FakeToolServer {
        name: "notion".into(),
    });
    let connected = vec![ConnectedServer {
        server,
        tool_infos: vec![tool_info("notion-fetch"), tool_info("notion-ai-search")],
        secrets: Vec::new(),
        bearer: None,
        tool_names: vec![
            "mcp__notion__notion-fetch".to_string(),
            "mcp__notion__notion-ai-search".to_string(),
        ],
    }];

    let names = mcp_adapter_tools(&connected, 65536)
        .into_iter()
        .map(|tool| tool.definition().name)
        .collect::<Vec<_>>();

    assert_eq!(names, vec!["mcp_search_tools", "mcp_call_tool"]);
    assert!(!names.iter().any(|name| name.starts_with("mcp__notion__")));
}

// --- cross-server tool-name collisions (Finding 2) ---
//
// `connect_mcp` itself is not exercised end to end here: producing a
// real collision needs two servers that actually answer `tools/list`,
// which needs a live connection the offline test rule forbids. Instead
// this drives `tools_for` with two fake `ToolServer`s the way
// `connect_mcp` does, then feeds the results through the extracted pure
// `dedup_cross_server` helper `connect_mcp` also calls.

struct FakeToolServer {
    name: String,
}

#[async_trait::async_trait]
impl mcp::ToolServer for FakeToolServer {
    fn name(&self) -> &str {
        &self.name
    }

    async fn call(
        &self,
        _tool: &str,
        _arguments: serde_json::Value,
        _cancel: &CancellationToken,
    ) -> Result<mcp::CallOutcome, mcp::CallError> {
        Ok(mcp::CallOutcome::default())
    }
}

fn tool_info(name: &str) -> mcp::ToolInfo {
    mcp::ToolInfo {
        name: name.to_owned(),
        title: None,
        description: None,
        input_schema: None,
    }
}

#[test]
fn a_cross_server_collision_is_skipped_with_a_warning_and_the_child_set_matches() {
    // Server "a" tool "b__c" and server "a__b" tool "c" both sanitize to
    // "mcp__a__b__c": `tools_for`'s per-server dedup cannot see this, so
    // it builds both tools without a warning.
    let server_a: Arc<dyn mcp::ToolServer> = Arc::new(FakeToolServer { name: "a".into() });
    let server_a_b: Arc<dyn mcp::ToolServer> = Arc::new(FakeToolServer {
        name: "a__b".into(),
    });
    let (tools_a, warnings_a) = tools_for(
        Arc::clone(&server_a),
        &[tool_info("b__c")],
        65536,
        Vec::new(),
        None,
    );
    assert!(warnings_a.is_empty(), "{warnings_a:?}");
    let (tools_b, warnings_b) = tools_for(
        Arc::clone(&server_a_b),
        &[tool_info("c")],
        65536,
        Vec::new(),
        None,
    );
    assert!(warnings_b.is_empty(), "{warnings_b:?}");
    assert_eq!(tools_a[0].definition().name, "mcp__a__b__c");
    assert_eq!(tools_b[0].definition().name, "mcp__a__b__c");

    let mut registered = HashSet::new();
    let (keep_a, dedup_warnings_a) = dedup_cross_server("a", &tools_a, &mut registered);
    let (keep_b, dedup_warnings_b) = dedup_cross_server("a__b", &tools_b, &mut registered);

    assert_eq!(keep_a, vec![0], "the first server's tool is registered");
    assert!(dedup_warnings_a.is_empty(), "{dedup_warnings_a:?}");
    assert!(
        keep_b.is_empty(),
        "the second server's colliding tool is skipped"
    );
    assert_eq!(dedup_warnings_b.len(), 1);
    assert_eq!(
        dedup_warnings_b[0],
        "mcp a__b: skipping tool \"c\": name \"mcp__a__b__c\" is already provided by another server"
    );
}
