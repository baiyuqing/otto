//! Run-loop tests: memory recall, per-run system context, and the tool-result overlay.

use super::*;

// -- memory ----------------------------------------------------------------

pub(super) fn one_record() -> Vec<Record> {
    vec![Record {
        id: "rec-1".into(),
        scope: Scope {
            namespace: "user".into(),
            id: "u1".into(),
        },
        kind: "preference".into(),
        key: "editor".into(),
        text: "prefers vim".into(),
        standing: false,
    }]
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_rendered_memory_context_is_prepended_without_being_persisted() {
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::with_records(one_record()))),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let request = &agent.provider().normal_requests()[0];
    assert_eq!(request.messages[0].role, Role::User);
    assert!(request.messages[0].text().contains("prefers vim"));
    assert!(request.messages[0].text().contains("untrusted"));
    for message in agent.session().messages() {
        assert!(
            !message.text().contains("prefers vim"),
            "the memory block was persisted"
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn memory_context_keeps_existing_history_as_the_request_prefix() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("first reply"), Turn::text("second reply")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::with_records(one_record()))),
            ..options()
        },
    );
    let cancel = CancellationToken::new();
    agent.run("first", &mut |_| {}, &cancel).await.expect("run");
    agent
        .run("second", &mut |_| {}, &cancel)
        .await
        .expect("run");

    let requests = agent.provider().normal_requests();
    let messages = &requests[1].messages;
    assert_eq!(messages[0].text(), "first");
    assert_eq!(messages[1].text(), "first reply");
    assert!(messages[2].text().contains("prefers vim"));
    assert_eq!(messages[3].text(), "second");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn per_run_system_context_is_sent_for_one_turn_only() {
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("first done"),
            Turn::text("second done"),
        ]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let cancel = CancellationToken::new();
    agent
        .run_with_system_context("first", "transient task notes", &mut |_| {}, &cancel)
        .await
        .expect("first run");
    agent
        .run("second", &mut |_| {}, &cancel)
        .await
        .expect("second run");

    let requests = agent.provider().normal_requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].system_prompt,
        "be brief\n\ntransient task notes"
    );
    assert_eq!(
        requests[1].system_prompt,
        "be brief\n\ntransient task notes"
    );
    assert_eq!(requests[2].system_prompt, "be brief");
    for message in agent.session().messages() {
        assert!(
            !message.text().contains("transient task notes"),
            "per-run system context was persisted"
        );
    }
    let report = agent.context_report();
    let prompt = report
        .sections
        .iter()
        .find(|section| section.kind == SectionKind::SystemPrompt)
        .expect("system prompt section");
    assert_eq!(prompt.items[0].text, "be brief");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn per_run_system_context_is_sent_on_overflow_retry() {
    let provider = FakeProvider::new(vec![
        Turn::overflow(),
        Turn::summary(&structured_summary()),
        Turn::text("done"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: true,
                ..CompactionSettings::default()
            },
            ..options()
        },
    );
    agent
        .run_with_system_context(
            "hello",
            "retry-only transient context",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let normal_requests = agent.provider().normal_requests();
    assert_eq!(normal_requests.len(), 2);
    assert!(
        normal_requests
            .iter()
            .all(|request| request.system_prompt == "be brief\n\nretry-only transient context")
    );
    let summary_requests = agent.provider().summary_requests();
    assert_eq!(summary_requests.len(), 1);
    assert!(
        !summary_requests[0]
            .system_prompt
            .contains("retry-only transient context")
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn recall_runs_once_across_the_tool_loop() {
    let memory = Arc::new(FakeMemory::with_records(one_record()));
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(memory.clone()),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(memory.calls.load(Ordering::SeqCst), 1);
    for request in agent.provider().normal_requests() {
        assert!(
            request.messages[0].text().contains("prefers vim"),
            "the memory block is missing from a later request"
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_recall_warns_and_the_turn_continues() {
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::failing("store unavailable"))),
            ..options()
        },
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
        .expect("the turn continues");
    assert!(collect(&events).contains(&Event::MemoryWarning {
        message: "store unavailable".into()
    }));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn close_reports_the_memory_binding_error() {
    let memory = Arc::new(FakeMemory {
        close_error: Some("close failed".into()),
        ..FakeMemory::with_records(Vec::new())
    });
    let agent = Agent::new(
        FakeProvider::new(Vec::new()),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(memory),
            ..options()
        },
    );
    assert_eq!(
        agent.close().expect_err("close fails").to_string(),
        "close failed"
    );

    let plain = Agent::new(
        FakeProvider::new(Vec::new()),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    assert!(plain.close().is_ok(), "close without a binding is a no-op");
}

// -- tool-result overlay ---------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_placeholder_is_persisted_and_the_full_text_reaches_the_provider() {
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some("the whole file".into()),
            persisted: Some("[stored elsewhere]".into()),
        },
        MemorySession::new(),
        options(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");

    let stored = agent
        .session()
        .messages()
        .into_iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result");
    assert_eq!(stored.blocks[0].text, "[stored elsewhere]");
    let second = &agent.provider().normal_requests()[1];
    let overlaid = second
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result in the request");
    assert_eq!(overlaid.blocks[0].text, "the whole file");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_overlay_does_not_survive_the_turn() {
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("first"),
        Turn::text("second"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some("the whole file".into()),
            persisted: Some("[stored elsewhere]".into()),
        },
        MemorySession::new(),
        options(),
    );
    let cancel = CancellationToken::new();
    agent.run("first", &mut |_| {}, &cancel).await.expect("run");
    agent
        .run("second", &mut |_| {}, &cancel)
        .await
        .expect("run");

    let last = agent.provider().normal_requests().pop().expect("a request");
    let tool = last
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result");
    assert_eq!(
        tool.blocks[0].text, "[stored elsewhere]",
        "the overlay leaked into the next turn"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_empty_persisted_override_still_overlays_the_live_text() {
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some("shown".into()),
            persisted: Some(String::new()),
        },
        MemorySession::new(),
        options(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let stored = agent
        .session()
        .messages()
        .into_iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result");
    assert_eq!(stored.blocks[0].text, "");
    let second = &agent.provider().normal_requests()[1];
    let overlaid = second
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("a tool result in the request");
    assert_eq!(overlaid.blocks[0].text, "shown");
}
