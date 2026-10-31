use super::*;

/// Two runners built on one session share its resources, registry, inbox
/// and MCP servers; the second build connects nothing new.
#[tokio::test]
async fn two_runner_builds_on_one_session_share_its_resources() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());

    let first = builder
        .build_runner_quiet(&session, &runtime())
        .await
        .expect("first runner");
    let resources_first = session.resources().expect("resources after first build");
    let second = builder
        .build_runner_quiet(&session, &runtime())
        .await
        .expect("second runner");
    let resources_second = session.resources().expect("resources after second build");

    assert!(Arc::ptr_eq(resources_first, resources_second));
    assert!(Arc::ptr_eq(&first.mcp(), &second.mcp()));
    assert!(Arc::ptr_eq(
        first.tasks().expect("first tasks"),
        second.tasks().expect("second tasks")
    ));
    assert!(Arc::ptr_eq(first.mailbox(), second.mailbox()));
    // Closing a superseded runner leaves the session's resources open.
    first.close();
    assert!(!resources_second.is_closed());
}

/// Two sessions of one builder own separate resources, registries and
/// inboxes.
#[tokio::test]
async fn two_sessions_of_one_builder_own_separate_resources() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session_a = SharedSession::memory(Header::default());
    let session_b = SharedSession::memory(Header::default());

    let runner_a = builder
        .build_runner_quiet(&session_a, &runtime())
        .await
        .expect("runner a");
    let runner_b = builder
        .build_runner_quiet(&session_b, &runtime())
        .await
        .expect("runner b");

    let resources_a = session_a.resources().expect("resources a");
    let resources_b = session_b.resources().expect("resources b");
    assert!(!Arc::ptr_eq(resources_a, resources_b));
    assert!(!Arc::ptr_eq(
        runner_a.tasks().expect("tasks a"),
        runner_b.tasks().expect("tasks b")
    ));
    assert!(!Arc::ptr_eq(runner_a.mailbox(), runner_b.mailbox()));
    assert!(!Arc::ptr_eq(&runner_a.mcp(), &runner_b.mcp()));
    session_a.close_resources().await;
    assert!(resources_a.is_closed());
    assert!(!resources_b.is_closed());
}

/// The session owns its resources, so they hold it weakly: dropping the last
/// session handle and the runner releases the transcript.
#[tokio::test]
async fn dropping_the_session_and_runner_releases_the_transcript() {
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());
    let transcript = Arc::downgrade(session.handle());

    let runner = builder
        .build_runner_quiet(&session, &runtime())
        .await
        .expect("runner");
    runner.close();
    drop(runner);
    drop(session);

    assert!(transcript.upgrade().is_none());
}

/// Whether a session has a task registry depends on `[agents]` only. A first
/// runner without a sub-agent executor (here the scripted test provider, like
/// a closed redaction boundary in production) still creates the registry, so
/// a later runner with a real provider can start sub-agents.
#[tokio::test]
async fn a_first_runner_without_a_subagent_executor_still_creates_the_task_registry() {
    use crate::runtime::builder::TEST_PROVIDERS;
    use crate::subagent::doubles_tests::FakeProvider;
    struct ProviderGuard(String);
    impl Drop for ProviderGuard {
        fn drop(&mut self) {
            TEST_PROVIDERS
                .lock()
                .unwrap()
                .retain(|(home, _)| home != &self.0);
        }
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let builder = builder(dir.path());
    let session = SharedSession::memory(Header::default());
    let home = dir.path().to_string_lossy().into_owned();
    TEST_PROVIDERS
        .lock()
        .unwrap()
        .push((home.clone(), FakeProvider::new()));
    let provider_guard = ProviderGuard(home);

    let scripted = builder
        .build_runner_quiet(&session, &runtime())
        .await
        .expect("scripted runner");
    let tasks_first = session
        .resources()
        .and_then(|resources| resources.tasks().cloned())
        .expect("task registry created by a runner without an executor");
    drop(provider_guard);
    let provider_backed = builder
        .build_runner_quiet(&session, &runtime())
        .await
        .expect("provider-backed runner");

    assert!(Arc::ptr_eq(
        &tasks_first,
        provider_backed.tasks().expect("tasks of the second runner")
    ));
    assert!(
        provider_backed
            .definitions()
            .iter()
            .any(|definition| definition.name == "agent"),
        "the provider-backed runner offers the agent tool"
    );
    scripted.close();
    provider_backed.close();
}

/// The second runner build on a session spawns no MCP server process: both
/// runners hold the same `SessionMcp` and the start log is unchanged.
#[tokio::test]
async fn second_runner_build_spawns_no_further_mcp_server_process() {
    use crate::runtime::mcp_server_fixture_tests::shell_mcp_server_with_start_log;
    let dir = tempfile::tempdir().expect("temp dir");
    let start_log = dir.path().join("mcp-starts.log");
    let mut builder = builder(dir.path());
    builder.mcp = McpRuntime {
        enabled: true,
        call_timeout_secs: 5,
        connect_timeout_secs: 5,
        servers: vec![shell_mcp_server_with_start_log(
            "fixture",
            dir.path(),
            Some(&start_log),
        )],
    };
    let session = SharedSession::memory(Header::default());

    let first = builder
        .build_runner_quiet(&session, &runtime())
        .await
        .expect("first runner");
    // One connect may spawn the server more than once (the protocol probe
    // falls back to the legacy handshake), so compare counts across builds.
    let starts_after_first = std::fs::read_to_string(&start_log).expect("start log");
    assert!(starts_after_first.lines().count() >= 1);
    let second = builder
        .build_runner_quiet(&session, &runtime())
        .await
        .expect("second runner");

    assert!(Arc::ptr_eq(&first.mcp(), &second.mcp()));
    let starts_after_second = std::fs::read_to_string(&start_log).expect("start log");
    assert_eq!(starts_after_second, starts_after_first);
    let status = first.mcp().status();
    assert_eq!(status.len(), 1);
    assert!(
        matches!(
            status[0].state,
            crate::mcp::ServerState::Connected { tools: 1 }
        ),
        "{:?}",
        status[0].state
    );
    // MCP tools are offered through the router tools, not one tool each.
    for runner in [&first, &second] {
        let names: Vec<String> = runner.definitions().into_iter().map(|d| d.name).collect();
        for router in ["mcp_search_tools", "mcp_call_tool"] {
            assert!(
                names.iter().any(|name| name == router),
                "{router} in {names:?}"
            );
        }
    }
    session.close_resources().await;
}
