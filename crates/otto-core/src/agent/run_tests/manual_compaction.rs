//! Run-loop tests: manual and remote compaction, checkpoints and task-registry close.

use super::*;

// -- manual compaction -----------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_manual_compaction_persists_the_summary_and_emits_the_plan_first() {
    let mut turn = Turn::summary(&structured_summary());
    turn.events.push(StreamEvent::Retry {
        attempt: 2,
        max_attempts: 4,
        delay: std::time::Duration::from_secs(1),
        reason: "connection interrupted".into(),
        interrupted_output: false,
    });
    let provider = FakeProvider::new(vec![turn]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let result = agent
        .compact(
            "focus on the parser",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("compact");

    assert!(!result.checkpoint_id.is_empty());
    assert!(!result.noop);
    assert!(result.usage_present);
    assert_eq!(result.usage.output_tokens, 7);
    assert_eq!(
        names(&collect(&events)),
        [
            "compaction_started",
            "compaction_planned",
            "provider_retry",
            "provider_api_call",
            "compaction_completed"
        ]
    );
    let latest = agent.session().latest_compaction().expect("a checkpoint");
    assert!(latest.summary.contains("## Goal"));
    assert!(
        agent.provider().summary_requests()[0]
            .system_prompt
            .contains("focus on the parser"),
        "the focus was not added to the prompt"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn remote_compaction_persists_opaque_state_and_reuses_only_the_new_tail() {
    let provider = FakeProvider::new(vec![
        Turn::remote_compaction("compact-1", "opaque-secret"),
        Turn::text("continued"),
    ])
    .with_remote_compaction();
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session_with_echo_capabilities().await,
        options(),
    );
    let mut events = Vec::new();
    let result = agent
        .compact(
            "",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("remote compact");

    assert_eq!(result.checkpoint_id, "compact-1");
    assert_eq!(result.tokens_before, Some(50));
    assert_eq!(result.estimated_tokens_after, None);
    assert!(result.usage_present);
    assert!(result.cached_input_tokens_present);
    let checkpoint = agent.session().remote_compaction().expect("checkpoint");
    assert_eq!(checkpoint.provider, "fake");
    assert_eq!(checkpoint.encrypted_content, "opaque-secret");
    assert!(agent.session().post_remote_compaction_messages().is_empty());
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ProviderApiCall {
            kind: crate::agent::ProviderOperationKind::RemoteCompaction,
            attempts: 1,
            ..
        }
    )));

    agent
        .run("next", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("continued turn");
    let requests = agent.provider().requests();
    assert!(requests[0].request_remote_compaction);
    assert!(requests[0].remote_compaction.is_none());
    assert_eq!(requests[1].remote_compaction, Some(checkpoint));
    assert!(!requests[1].request_remote_compaction);
    assert_eq!(requests[1].messages.len(), 1);
    assert_eq!(requests[1].messages[0].text(), "next");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn remote_compaction_accepts_a_streamed_reasoning_summary() {
    let provider = FakeProvider::new(vec![
        Turn::remote_compaction("compact-1", "opaque").with_events(vec![
            StreamEvent::ReasoningDelta {
                text: "thinking".into(),
            },
        ]),
    ])
    .with_remote_compaction();
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session_with_echo_capabilities().await,
        options(),
    );

    let result = agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("remote compact");

    assert_eq!(result.checkpoint_id, "compact-1");
    assert!(agent.session().remote_compaction().is_some());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_remote_tail_usage_anchors_the_estimate_after_an_older_local_checkpoint() {
    let session = seeded_session_with_echo_capabilities().await;
    session
        .append_compaction(crate::session::CompactionCheckpoint {
            summary: "local summary".into(),
            first_kept_entry_id: "u2".into(),
            created_at: clock(),
            ..crate::session::CompactionCheckpoint::default()
        })
        .await
        .expect("local checkpoint");
    for (id, role) in [("u3", Role::User), ("a3", Role::Assistant)] {
        session
            .append(Message {
                id: id.into(),
                role,
                blocks: vec![Block::text("z")],
                ..Message::default()
            })
            .await
            .expect("append");
    }
    session
        .append_remote_compaction(RemoteCompaction {
            provider: "fake".into(),
            id: "compact-1".into(),
            encrypted_content: "opaque".into(),
        })
        .await
        .expect("remote checkpoint");
    session
        .append(Message {
            id: "u4".into(),
            role: Role::User,
            blocks: vec![Block::text("q")],
            ..Message::default()
        })
        .await
        .expect("append");
    session
        .append(Message {
            id: "a4".into(),
            role: Role::Assistant,
            blocks: vec![Block::text("a")],
            usage: Some(Usage {
                input_tokens: 200_000,
                output_tokens: 1,
                cached_input_tokens: 0,
            }),
            ..Message::default()
        })
        .await
        .expect("append");
    let provider = FakeProvider::new(vec![
        Turn::remote_compaction("compact-2", "opaque-2"),
        Turn::text("ok"),
    ])
    .with_remote_compaction();
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        session,
        Options {
            compaction: CompactionSettings {
                auto: true,
                hard_input_window: 1_000_000,
                working_window: 100_000,
                reserve_tokens: 16_384,
                keep_recent_tokens: 20_000,
            },
            ..options()
        },
    );

    // Only the first request matters here: the compacted turn's later
    // ordinary request has an empty tail.
    let _ = agent
        .run("next", &mut |_| {}, &CancellationToken::new())
        .await;

    // The reported 200,000 input tokens cross the soft trigger, so the turn
    // compacts before its ordinary request.
    assert!(agent.provider().requests()[0].request_remote_compaction);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_manual_compaction_with_nothing_to_compact_is_a_bounded_no_op() {
    let provider = FakeProvider::new(Vec::new());
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let result = agent
        .compact(
            "",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("a no-op is not an error");
    assert!(result.noop);
    assert!(result.checkpoint_id.is_empty());
    assert_eq!(
        names(&collect(&events)),
        ["compaction_started", "compaction_completed"]
    );
    assert!(agent.provider().requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_summary_response_that_streams_past_its_bound_is_rejected() {
    let provider = FakeProvider::new(vec![Turn::summary(&structured_summary()).with_events(
        vec![StreamEvent::TextDelta {
            text: "x".repeat(200_000),
        }],
    )]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        options(),
    );
    let error = agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the stream is rejected");
    assert_eq!(
        error.to_string(),
        "invalid compaction summary: streamed response exceeded its bound or attempted a tool call"
    );
    assert!(agent.session().latest_compaction().is_none());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_summary_missing_a_heading_is_rejected_before_the_append() {
    let provider = FakeProvider::new(vec![Turn::summary("## Goal\nonly one heading")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        options(),
    );
    let error = agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the summary is rejected");
    assert!(
        error.is_invalid_compaction_summary(),
        "unexpected error: {error}"
    );
    assert!(agent.session().latest_compaction().is_none());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn summarized_user_messages_are_carried_verbatim_across_compactions() {
    async fn exchange(session: &MemorySession, index: usize, text: &str) {
        for (id, role, text) in [
            (format!("u{index}"), Role::User, text),
            (format!("a{index}"), Role::Assistant, "ok"),
        ] {
            session
                .append(Message {
                    id,
                    role,
                    blocks: vec![Block::text(text)],
                    ..Message::default()
                })
                .await
                .expect("append");
        }
    }

    let session = MemorySession::new();
    exchange(&session, 0, "gpt-5.6 returns HTTP 400").await;
    exchange(&session, 1, "second").await;
    exchange(&session, 2, "third").await;
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::summary(&structured_summary()),
            Turn::summary(&structured_summary()),
        ]),
        EchoExecutor::default(),
        session,
        options(),
    );
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("first compaction");
    exchange(agent.session(), 3, "fourth").await;
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("second compaction");

    let summary = agent
        .session()
        .latest_compaction()
        .expect("a checkpoint")
        .summary;
    assert!(
        summary.ends_with(
            "\n\n<user-messages>\n\"gpt-5.6 returns HTTP 400\"\n\"second\"\n\"third\"\n</user-messages>"
        ),
        "{summary}"
    );
    let second_request = agent.provider().summary_requests()[1].messages[0].text();
    assert!(
        !second_request.contains("gpt-5.6 returns HTTP 400")
            && !second_request.contains("user-messages"),
        "the carried block reached the summarizer: {second_request}"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn cancellation_before_the_append_commits_nothing() {
    let provider = FakeProvider::new(vec![Turn::summary(&structured_summary())]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        options(),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = agent
        .compact("", &mut |_| {}, &cancel)
        .await
        .expect_err("cancellation stops the compaction");
    assert!(error.is_cancelled(), "unexpected error: {error}");
    assert!(agent.session().latest_compaction().is_none());
    assert!(agent.provider().requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_compaction_with_no_request_sizer_is_rejected() {
    let provider = FakeProvider::new(Vec::new());
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            request_sizer: None,
            ..options()
        },
    );
    let error = agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("sizing is required");
    assert_eq!(
        error.to_string(),
        "invalid compaction summary: invalid compaction summary request: request sizing is unavailable"
    );
    assert!(agent.provider().requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_compacted_context_still_over_the_hard_trigger_stops_the_dispatch() {
    // The system prompt alone is over the hard trigger, and compaction never
    // touches it, so the retry cannot bring the request back under.
    let provider = FakeProvider::new(vec![Turn::summary(&structured_summary())]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            system_prompt: "p".repeat(90_000),
            compaction: CompactionSettings {
                auto: true,
                hard_input_window: 20_000,
                working_window: 150,
                reserve_tokens: 0,
                keep_recent_tokens: 0,
            },
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the dispatch stops");
    assert_eq!(
        error.to_string(),
        crate::agent::overflow::AUTOMATIC_COMPACTION_STILL_TOO_LARGE_MESSAGE,
        "{error:?}"
    );
    assert_eq!(agent.provider().summary_requests().len(), 1);
    assert!(agent.provider().normal_requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_later_dispatch_over_the_hard_trigger_reports_the_used_attempt() {
    // The first dispatch spends the one proactive attempt. A very large tool
    // result then pushes the second dispatch past the hard trigger.
    let provider = FakeProvider::new(vec![
        Turn::summary(&structured_summary()),
        Turn::tool_call("c1", r#"{"value":1}"#),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some("z".repeat(60_000)),
            persisted: None,
        },
        seeded_session().await,
        Options {
            compaction: tight_compaction(),
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the second dispatch stops");
    assert_eq!(
        error.to_string(),
        crate::agent::overflow::AUTOMATIC_COMPACTION_ATTEMPT_USED_MESSAGE
    );
    assert_eq!(agent.provider().summary_requests().len(), 1);
    assert_eq!(agent.provider().normal_requests().len(), 1);
}
