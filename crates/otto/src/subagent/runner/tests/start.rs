//! Starting children: agent selection, models, prompts and inherited context.

use super::*;

#[tokio::test]
async fn start_unknown_agent_rejected_before_task_created() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

    let error = runner
        .start(StartRequest {
            prompt: "go".into(),
            agent: "nope".into(),
            ..StartRequest::default()
        })
        .await
        .expect_err("an unknown agent is rejected");
    assert_eq!(error.to_string(), r#"unknown agent "nope""#);
    assert!(tasks.list().is_empty());
}

#[tokio::test]
async fn start_named_agent_sets_task_agent() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.catalog = catalog(vec![definition("reviewer")]);
    let (runner, _) = runner(config);

    let task = runner
        .start(StartRequest {
            prompt: "go".into(),
            agent: "reviewer".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(task.agent, "reviewer");
    assert_eq!(wait_final(&tasks, &task.id).await.agent, "reviewer");
}

#[tokio::test]
async fn start_model_precedence() {
    for (call_model, definition_model, configured, want, source) in [
        ("call-model", "def-model", true, "call-model", "explicit"),
        ("", "def-model", true, "def-model", "definition"),
        ("", "", true, "cheap-model", "configured_first"),
        ("", "", false, "sess-model", "parent"),
    ] {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        config.template.model = "sess-model".into();
        if configured {
            config.template.subagent_models = vec!["cheap-model".into(), "strong-model".into()];
        }
        config.catalog = catalog(vec![Definition {
            name: "reviewer".into(),
            model: definition_model.into(),
            ..Definition::default()
        }]);
        let (runner, _) = runner(config);

        runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "reviewer".into(),
                model: call_model.into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;

        assert_eq!(
            provider.requests().first().expect("one request").model,
            want
        );
        let task = tasks.get("t1").expect("recorded task");
        assert_eq!(task.model, want);
        assert_eq!(task.model_source.as_str(), source);
    }
}

/// A definition's tool allowlist narrows the child registry to exactly
/// those tools, in the parent's order. The assertion reads the child's
/// system prompt, because the test prompt names the child's tools.
#[tokio::test]
async fn new_runner_tools_allowlist_parent_order() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(
        &provider,
        &tasks,
        vec![stub("grep"), stub("read"), stub("write")],
    );
    config.catalog = catalog(vec![Definition {
        name: "reviewer".into(),
        tools: Some(vec!["read".into(), "grep".into()]),
        ..Definition::default()
    }]);
    let (runner, warnings) = runner(config);
    assert!(warnings.is_empty(), "{warnings:?}");

    runner
        .start(StartRequest {
            prompt: "go".into(),
            agent: "reviewer".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    let requests = provider.requests();
    assert!(
        requests
            .first()
            .expect("one request")
            .system_prompt
            .starts_with("PARENT PROMPT tools=grep,read,agent_report\n"),
        "{}",
        requests[0].system_prompt
    );
}

#[tokio::test]
async fn new_runner_unknown_tool_warning() {
    for bad in ["bogus", "agent", "remember"] {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, vec![stub("read")]);
        config.catalog = catalog(vec![Definition {
            name: "reviewer".into(),
            tools: Some(vec!["read".into(), bad.into()]),
            ..Definition::default()
        }]);
        let (runner, warnings) = runner(config);
        assert!(
            warnings.contains(&format!("agent reviewer: unknown tool {bad:?} ignored")),
            "{warnings:?}"
        );

        runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "reviewer".into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        wait_final(&tasks, "t1").await;
        assert!(
            provider.requests()[0]
                .system_prompt
                .starts_with("PARENT PROMPT tools=read,agent_report\n"),
            "{}",
            provider.requests()[0].system_prompt
        );
    }
}

#[tokio::test]
async fn start_system_prompt_uses_definition_body() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.catalog = catalog(vec![Definition {
        name: "reviewer".into(),
        body: "You are a reviewer.".into(),
        ..Definition::default()
    }]);
    let (runner, _) = runner(config);
    runner
        .start(StartRequest {
            prompt: "go".into(),
            agent: "reviewer".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    let prompt = provider.requests()[0].system_prompt.clone();
    assert!(
        prompt.contains("<subagent_role>\nYou are a reviewer.\n</subagent_role>"),
        "{prompt}"
    );
    assert!(!prompt.contains(GENERIC_SUBAGENT_INSTRUCTION), "{prompt}");
}

#[tokio::test]
async fn start_system_prompt_empty_body_uses_generic_instruction() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.catalog = catalog(vec![definition("reviewer")]);
    let (runner, _) = runner(config);
    runner
        .start(StartRequest {
            prompt: "go".into(),
            agent: "reviewer".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    wait_final(&tasks, "t1").await;

    assert!(
        provider.requests()[0]
            .system_prompt
            .contains(GENERIC_SUBAGENT_INSTRUCTION)
    );
}

#[tokio::test]
async fn start_context_inherit_snapshot_in_first_request() {
    let parent = vec![
        user_message("u1", "first"),
        assistant_message("a1", "reply one"),
        user_message("u2", "second"),
        tool_call_message("a2", "agent"),
        // A sibling call's result, appended after the pending agent call.
        Message {
            id: "tr1".into(),
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_call_id: "call-0".into(),
                ..Block::default()
            }],
            ..Message::default()
        },
    ];
    let want_snapshot = parent[..3].to_vec();

    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, vec![stub("write"), stub("grep")]);
    let source = parent.clone();
    config.parent_session = Some(Arc::new(move || source.clone()));
    let (runner, _) = runner(config);

    let task = runner
        .start(StartRequest {
            prompt: "delegated task".into(),
            context: "inherit".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(task.context, "inherit");
    wait_final(&tasks, &task.id).await;

    let messages = provider.requests()[0].messages.clone();
    // The child has history and no tool-set record, so its first run adds
    // the legacy tool-set notice between the snapshot and the prompt.
    assert_eq!(messages.len(), want_snapshot.len() + 2, "{messages:#?}");
    assert_eq!(&messages[..want_snapshot.len()], want_snapshot.as_slice());
    let notice = &messages[want_snapshot.len()];
    assert_eq!(notice.role, Role::Context);
    assert_eq!(notice.context_type, "capability_update");
    let mut names: Vec<String> = provider.requests()[0]
        .tools
        .iter()
        .map(|definition| definition.name.clone())
        .collect();
    assert!(!names.is_empty());
    names.sort();
    let text = notice.text();
    assert!(text.contains("was not recorded"), "{text}");
    assert!(
        text.lines()
            .any(|line| line == format!("Current tools: {}", names.join(", "))),
        "{text}"
    );
    assert!(!text.contains("Added:"), "{text}");
    assert!(!text.contains("Removed:"), "{text}");
    let last = &messages[want_snapshot.len() + 1];
    assert_eq!(last.role, Role::User);
    assert_eq!(last.text(), "delegated task");
}

#[tokio::test]
async fn start_context_fresh_only_prompt() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    let consulted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&consulted);
    config.parent_session = Some(Arc::new(move || {
        flag.store(true, Ordering::SeqCst);
        Vec::new()
    }));
    let (runner, _) = runner(config);

    let task = runner
        .start(StartRequest {
            prompt: "go".into(),
            context: "fresh".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(task.context, "fresh");
    wait_final(&tasks, &task.id).await;

    assert!(
        !consulted.load(Ordering::SeqCst),
        "the parent session must not be consulted for context fresh"
    );
    let messages = provider.requests()[0].messages.clone();
    assert_eq!(messages.len(), 1, "{messages:#?}");
    assert_eq!(messages[0].text(), "go");
}

#[tokio::test]
async fn start_context_definition_inherit_honoured_and_call_override() {
    let parent = vec![
        user_message("u1", "hello"),
        tool_call_message("a1", "agent"),
    ];

    for (request_context, want_context, want_messages) in
        [("", "inherit", 2usize), ("fresh", "fresh", 1)]
    {
        let provider = FakeProvider::new();
        provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
        let tasks = Arc::new(Tasks::new());
        let mut config = test_config(&provider, &tasks, Vec::new());
        let source = parent.clone();
        config.parent_session = Some(Arc::new(move || source.clone()));
        config.catalog = catalog(vec![Definition {
            name: "reviewer".into(),
            context: "inherit".into(),
            ..Definition::default()
        }]);
        let (runner, _) = runner(config);

        let task = runner
            .start(StartRequest {
                prompt: "go".into(),
                agent: "reviewer".into(),
                context: request_context.into(),
                ..StartRequest::default()
            })
            .await
            .expect("start succeeds");
        assert_eq!(task.context, want_context);
        wait_final(&tasks, &task.id).await;
        assert_eq!(provider.requests()[0].messages.len(), want_messages);
    }
}

#[tokio::test]
async fn start_context_inherit_without_parent_session_errors() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

    let error = runner
        .start(StartRequest {
            prompt: "go".into(),
            context: "inherit".into(),
            ..StartRequest::default()
        })
        .await
        .expect_err("inherit without a parent session is rejected");
    assert_eq!(
        error.to_string(),
        "context inherit is not available in this session"
    );
    assert!(tasks.list().is_empty());
}

#[tokio::test]
async fn start_fork_before_gives_the_child_the_forked_messages_as_inherit() {
    let forked = vec![user_message("u1", "first"), assistant_message("a1", "one")];
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, vec![stub("write")]);
    let quoted = Arc::new(std::sync::Mutex::new(String::new()));
    let (source, seen) = (forked.clone(), Arc::clone(&quoted));
    config.parent_fork = Some(Arc::new(move |prompt| {
        *seen.lock().unwrap() = prompt.to_owned();
        Ok(source.clone())
    }));
    let (runner, _) = runner(config);

    let task = runner
        .start(StartRequest {
            prompt: "delegated task".into(),
            fork_before: "  second\n".into(),
            ..StartRequest::default()
        })
        .await
        .expect("start succeeds");
    assert_eq!(task.context, "inherit");
    wait_final(&tasks, &task.id).await;

    assert_eq!(*quoted.lock().unwrap(), "second");
    let messages = provider.requests()[0].messages.clone();
    assert_eq!(&messages[..forked.len()], forked.as_slice());
}

#[tokio::test]
async fn start_fork_before_errors_create_no_task() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let mut config = test_config(&provider, &tasks, Vec::new());
    config.parent_fork = Some(Arc::new(|_| Err("2 user prompts match".into())));
    let (forking, _) = runner(config);
    for (context, want) in [
        ("fresh", r#"fork_before requires context "inherit""#),
        ("", "fork_before: 2 user prompts match"),
    ] {
        let error = forking
            .start(StartRequest {
                prompt: "go".into(),
                context: context.into(),
                fork_before: "x".into(),
                ..StartRequest::default()
            })
            .await
            .expect_err("fork_before fails");
        assert_eq!(error.to_string(), want);
    }

    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));
    let error = runner
        .start(StartRequest {
            prompt: "go".into(),
            fork_before: "x".into(),
            ..StartRequest::default()
        })
        .await
        .expect_err("no saved session");
    assert_eq!(error.to_string(), "fork_before: needs a saved session");
    assert!(tasks.list().is_empty());
}

#[tokio::test]
async fn start_invalid_context_errors() {
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let (runner, _) = runner(test_config(&provider, &tasks, Vec::new()));

    let error = runner
        .start(StartRequest {
            prompt: "go".into(),
            context: "bogus".into(),
            ..StartRequest::default()
        })
        .await
        .expect_err("an invalid context is rejected");
    assert_eq!(error.to_string(), r#"context must be "fresh" or "inherit""#);
    assert!(tasks.list().is_empty());
}
