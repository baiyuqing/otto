//! Conformance checks for a `MemoryExtension` that the host supplies to the startup composition.

use super::*;
use otto::app::approval::ApprovalDecision;
use otto::fixtures::runtime_session::{Options, Runtime};
use otto::runtime::startup::Disabled;

fn options(root: &std::path::Path, url: &str, memory: &str, service: Arc<External>) -> Options {
    let home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(home.join(".config/otto")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let origin = url::Url::parse(url).unwrap().origin().ascii_serialization();
    std::fs::write(
        home.join(".config/otto/config.toml"),
        format!(
            "default_model = \"test/offline-test\"\n[providers.test]\ntype = \
            \"openai-compatible\"\nbase_url = \"{url}\"\napi_key_env = \"TEST_KEY\"\ntrusted_origins = [\"{origin}\"]\n\
            allow_http_origins = [\"{origin}\"]\n[providers.test.models.offline-test]\n[sandbox]\ndriver = \
            \"off\"\n[server]\nworkspace_roots = [\"{}\"]\n{memory}\n",
            root.to_string_lossy()
        ),
    )
    .unwrap();
    Options {
        workspace,
        environment: vec![
            ("HOME".to_string(), home.to_string_lossy().into_owned()),
            ("TEST_KEY".to_string(), "offline-placeholder".to_string()),
        ],
        memory_extension: Some(otto::runtime::wiring::MemoryExtension::owned(
            service,
            context().scopes[0].clone(),
        )),
        disabled: Disabled {
            bash: true,
            subagents: true,
            ..Disabled::default()
        },
    }
}

#[test]
fn an_injected_extension_uses_neutral_prompt_context_and_releases_only_session_bindings() {
    // Startup composes a large future; give the integration host its own stack.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(extension_conformance())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn extension_conformance() {
    let (url, requests) = scripted_provider::serve(scripted_provider::Script {
        replies: vec![scripted_provider::text_reply("done")],
        served: Arc::new(AtomicUsize::new(0)),
    });
    let directory = tempfile::tempdir().unwrap();
    let service = Arc::new(External::new(&[]));
    let otto = Runtime::start(options(directory.path(), &url, "", service.clone()))
        .await
        .unwrap();
    let first = otto.new_session(None).await.unwrap();
    first
        .prompt(
            "recall project context",
            &mut |_| {},
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap();
    assert_eq!(service.count(Operation::Recall), 1);
    let bodies = requests.lock().unwrap().clone();
    let request: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    let content = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(content.contains("neutral external memory"), "{content}");
    assert!(content.contains(otto_core::agent::memory::MEMORY_CONTEXT_PREAMBLE));
    assert!(!content.contains(otto_core::agent::memory::STANDING_CONTEXT_PREAMBLE));
    let tools = request["tools"].as_array().unwrap();
    assert!(!tools.iter().any(|tool| {
        ["remember", "forget"].contains(&tool["function"]["name"].as_str().unwrap_or_default())
    }));
    let first_context = first.memory_context(Origin::Model);
    first.close().await.unwrap();
    assert_eq!(service.close_calls.load(Ordering::SeqCst), 0);
    let another_workspace = directory.path().join("workspace/another-workspace");
    std::fs::create_dir(&another_workspace).unwrap();
    let second = otto.new_session(Some(&another_workspace)).await.unwrap();
    let second_context = second.memory_context(Origin::Model);
    assert_ne!(first_context.workspace_id, second_context.workspace_id);
    assert_ne!(first_context.write_scope, second_context.write_scope);
    let selected = second.memory_service().unwrap();
    selected
        .recall(
            &RecallRequest {
                scope_filter: None,
                context: second_context.clone(),
                query: "context".into(),
                limit: 3,
                token_budget: 100,
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(service.contexts.lock().unwrap()[1], second_context);
    second.close().await.unwrap();
    assert_eq!(service.count(Operation::Forget), 0);
    assert_eq!(service.close_calls.load(Ordering::SeqCst), 0);
    otto.close().await.unwrap();
    otto.close().await.unwrap();
    assert_eq!(service.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        selected
            .recall(
                &RecallRequest {
                    scope_filter: None,
                    context: context(),
                    query: "after close".into(),
                    limit: 3,
                    token_budget: 100
                },
                &CancellationToken::new()
            )
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Closed
    );

    let disabled = Arc::new(External::new(&[]));
    let otto = Runtime::start(options(
        directory.path(),
        &url,
        "[memory]\nenabled = false",
        disabled.clone(),
    ))
    .await
    .unwrap();
    let session = otto.new_session(None).await.unwrap();
    assert!(session.memory_service().is_none());
    otto.close().await.unwrap();
    assert_eq!(disabled.count(Operation::Recall), 0);

    let unknown_encryption = Arc::new(External::new(&[]));
    let result = Runtime::start(options(
        directory.path(),
        &url,
        "[memory]\nrequire_encryption = true",
        unknown_encryption.clone(),
    ))
    .await;
    assert!(result.is_err());
    assert_eq!(unknown_encryption.count(Operation::Recall), 0);

    let closed_boundary = Arc::new(External::new(&[]));
    let mut closed_options = options(directory.path(), &url, "", closed_boundary.clone());
    let home = directory.path().join("home").to_string_lossy().into_owned();
    closed_options.environment = vec![
        ("HOME".to_string(), home),
        ("TEST_KEY".to_string(), "offline-placeholder".to_string()),
        (
            "LARGE_TOKEN".to_string(),
            "z".repeat(otto_core::safetext::MAX_SECRET_BYTES + 1),
        ),
    ];
    let result = Runtime::start(closed_options).await;
    if let Ok(otto) = result {
        let session = otto.new_session(None).await.unwrap();
        assert!(
            session.memory_service().is_none(),
            "extension must not reopen an incomplete redaction boundary"
        );
        otto.close().await.unwrap();
    }
    assert_eq!(closed_boundary.count(Operation::Recall), 0);

    let mut hostile = External::new(&[]);
    hostile.wrong_scope = true;
    let hostile = Arc::new(hostile);
    let otto = Runtime::start(options(directory.path(), &url, "", hostile.clone()))
        .await
        .unwrap();
    let session = otto.new_session(None).await.unwrap();
    session
        .prompt(
            "reject unauthorized memories",
            &mut |_| {},
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap();
    assert_eq!(hostile.count(Operation::Recall), 1);
    let latest: serde_json::Value =
        serde_json::from_str(requests.lock().unwrap().last().unwrap()).unwrap();
    assert!(
        !latest["messages"]
            .to_string()
            .contains("neutral external memory")
    );
    otto.close().await.unwrap();

    let borrowed = Arc::new(External::new(&[]));
    let mut borrowed_options = options(directory.path(), &url, "", borrowed.clone());
    borrowed_options.memory_extension = Some(otto::runtime::wiring::MemoryExtension::new(
        borrowed.clone(),
        context().scopes[0].clone(),
    ));
    let otto = Runtime::start(borrowed_options).await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    session.close().await.unwrap();
    otto.close().await.unwrap();
    assert_eq!(
        borrowed.close_calls.load(Ordering::SeqCst),
        0,
        "a borrowed provider belongs to the host that supplied it"
    );

    let mut unavailable_close = External::new(&[]);
    unavailable_close.fail_close = true;
    let unavailable_close = Arc::new(unavailable_close);
    let otto = Runtime::start(options(
        directory.path(),
        &url,
        "",
        unavailable_close.clone(),
    ))
    .await
    .unwrap();
    let session = otto.new_session(None).await.unwrap();
    session.close().await.unwrap();
    assert_eq!(unavailable_close.close_calls.load(Ordering::SeqCst), 0);
    assert!(otto.close().await.unwrap_err().starts_with("close: "));
    assert_eq!(unavailable_close.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(unavailable_close.count(Operation::Forget), 0);
}
