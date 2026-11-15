//! Run-loop tests: proactive, idle and overflow-recovery compaction.

use super::*;

// -- automatic compaction --------------------------------------------------

/// Windows small enough that a short transcript crosses the soft trigger.
pub(super) fn tight_compaction() -> CompactionSettings {
    CompactionSettings {
        auto: true,
        // Large enough that the summary request, which carries the full
        // summarization system prompt, still fits under the hard budget.
        hard_input_window: 4_000,
        working_window: 150,
        reserve_tokens: 0,
        keep_recent_tokens: 0,
    }
}

/// A session with three turns, enough for a compaction boundary to exist.
pub(super) async fn seeded_session() -> MemorySession {
    let session = MemorySession::new();
    for index in 0..3 {
        session
            .append(Message {
                id: format!("u{index}"),
                role: Role::User,
                blocks: vec![Block::text("x".repeat(80))],
                ..Message::default()
            })
            .await
            .expect("append");
        session
            .append(Message {
                id: format!("a{index}"),
                role: Role::Assistant,
                blocks: vec![Block::text("y".repeat(80))],
                ..Message::default()
            })
            .await
            .expect("append");
    }
    session
}

async fn record_echo_capabilities(session: &MemorySession) {
    let definitions = EchoExecutor::default().definitions();
    let capabilities = crate::session::Capabilities {
        tools: definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect(),
        definitions: Some(
            definitions
                .iter()
                .map(|definition| {
                    (
                        definition.name.clone(),
                        crate::agent::capabilities::definition_digest(definition),
                    )
                })
                .collect(),
        ),
        version: None,
    };
    session
        .append_custom(
            crate::session::CAPABILITIES_CUSTOM_TYPE,
            &crate::session::encode_capabilities(&capabilities),
        )
        .await
        .expect("record capabilities");
}

pub(super) async fn seeded_session_with_echo_capabilities() -> MemorySession {
    let session = seeded_session().await;
    record_echo_capabilities(&session).await;
    session
}

#[tokio::test]
async fn a_request_under_the_soft_trigger_is_not_compacted() {
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            compaction: CompactionSettings {
                auto: true,
                hard_input_window: 1_000_000,
                working_window: 900_000,
                reserve_tokens: 16_384,
                keep_recent_tokens: 20_000,
            },
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert!(agent.provider().summary_requests().is_empty());
}

#[tokio::test]
async fn crossing_the_soft_trigger_compacts_before_the_request() {
    let provider = FakeProvider::new(vec![Turn::summary(&structured_summary()), Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: tight_compaction(),
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
        .expect("run");
    assert_eq!(agent.provider().summary_requests().len(), 1);
    let emitted = names(&collect(&events));
    assert!(emitted.contains(&"compaction_started"));
    assert!(emitted.contains(&"compaction_planned"));
    assert!(emitted.contains(&"compaction_completed"));
    assert!(
        agent
            .session()
            .latest_compaction()
            .is_some_and(|latest| !latest.id.is_empty()),
        "no checkpoint was written"
    );
}

#[tokio::test]
async fn proactive_compaction_is_attempted_once_per_turn() {
    // Both provider dispatches sit above the soft trigger, but only the first
    // one may compact.
    let provider = FakeProvider::new(vec![
        Turn::summary(&structured_summary()),
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            content: Some("z".repeat(1_200)),
            persisted: None,
        },
        seeded_session().await,
        Options {
            compaction: tight_compaction(),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(agent.provider().summary_requests().len(), 1);
    assert_eq!(agent.provider().normal_requests().len(), 2);
}

#[tokio::test]
async fn a_soft_compaction_failure_warns_and_sends_the_original_request() {
    let provider = FakeProvider::new(vec![Turn::failure("summary call failed"), Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: tight_compaction(),
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
    assert!(collect(&events).contains(&Event::CompactionWarning {
        message: format!(
            "{}: compaction summary provider request failed: summary call failed",
            crate::agent::overflow::AUTOMATIC_COMPACTION_WARNING_MESSAGE
        )
    }));
    assert_eq!(agent.provider().normal_requests().len(), 1);
}

#[tokio::test]
async fn a_summary_response_with_reasoning_writes_a_checkpoint() {
    let mut summary = Turn::summary(&structured_summary());
    summary.events = vec![
        StreamEvent::ReasoningDelta {
            text: "**Summarizing**".into(),
        },
        StreamEvent::TextDelta {
            text: structured_summary(),
        },
    ];
    if let Ok(response) = summary.outcome.as_mut() {
        response
            .message
            .blocks
            .insert(0, Block::reasoning("**Summarizing**"));
    }
    let provider = FakeProvider::new(vec![summary, Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: tight_compaction(),
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
        .expect("run");
    assert!(
        !collect(&events)
            .iter()
            .any(|event| matches!(event, Event::CompactionWarning { .. }))
    );
    let compaction = agent.session().latest_compaction().expect("checkpoint");
    assert!(compaction.summary.starts_with(structured_summary().trim()));
    assert!(!compaction.summary.contains("**Summarizing**"));
}

#[tokio::test]
async fn a_compaction_failure_at_the_hard_limit_stops_the_turn() {
    let provider = FakeProvider::new(vec![Turn::failure("summary call failed")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            // The hard trigger is below the transcript, so a failed
            // compaction stops the dispatch instead of warning.
            compaction: CompactionSettings {
                hard_input_window: 150,
                ..tight_compaction()
            },
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the turn stops");
    assert_eq!(
        error.to_string(),
        crate::agent::overflow::AUTOMATIC_COMPACTION_HARD_FAILURE_MESSAGE
    );
    assert!(agent.provider().normal_requests().is_empty());
}

#[tokio::test]
async fn unknown_limits_never_compact_proactively() {
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
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
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert!(agent.provider().summary_requests().is_empty());
}

/// A seeded session whose last assistant message finished `age_minutes`
/// before the test clock and reported `input_tokens` of prompt usage. The usage
/// anchors the request estimate near `input_tokens` plus a few tens of tokens.
async fn idle_session(age_minutes: i64, input_tokens: i64) -> MemorySession {
    let mut messages = seeded_session().await.messages();

    let last = messages.last_mut().expect("seeded assistant message");
    last.created_at = clock() - chrono::TimeDelta::minutes(age_minutes);
    last.usage = Some(Usage {
        input_tokens,
        ..Usage::default()
    });
    let rebuilt = MemorySession::new();
    for message in messages {
        rebuilt.append(message).await.expect("append");
    }
    record_echo_capabilities(&rebuilt).await;
    rebuilt
}

/// Runs one turn on an idle session and returns the reasons of the completed
/// compactions plus the summary request count. The soft trigger is 200 tokens.
async fn run_idle_turn(age_minutes: i64, input_tokens: i64) -> (Vec<String>, usize) {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary(&structured_summary()), Turn::text("ok")]),
        EchoExecutor::default(),
        idle_session(age_minutes, input_tokens).await,
        Options {
            compaction: CompactionSettings {
                working_window: 200,
                ..tight_compaction()
            },
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
        .expect("run");
    let reasons = collect(&events)
        .into_iter()
        .filter_map(|event| match event {
            Event::CompactionCompleted { compaction } => Some(compaction.reason.name().to_owned()),
            _ => None,
        })
        .collect();
    (reasons, agent.provider().summary_requests().len())
}

#[tokio::test]
async fn an_idle_cache_with_a_half_full_context_compacts_before_the_request() {
    // Estimate is about 135 tokens: above 100 (half the soft trigger), below 200.
    let (reasons, summaries) = run_idle_turn(61, 100).await;
    assert_eq!(reasons, ["idle"]);
    assert_eq!(summaries, 1);
}

#[tokio::test]
async fn a_recent_response_does_not_trigger_idle_compaction() {
    let (reasons, summaries) = run_idle_turn(59, 100).await;
    assert!(reasons.is_empty());
    assert_eq!(summaries, 0);
}

#[tokio::test]
async fn an_idle_cache_with_a_small_context_is_not_compacted() {
    // Estimate is about 45 tokens, below 100 (half the soft trigger).
    let (reasons, summaries) = run_idle_turn(61, 10).await;
    assert!(reasons.is_empty());
    assert_eq!(summaries, 0);
}

// -- overflow recovery -----------------------------------------------------

#[tokio::test]
async fn a_context_overflow_is_retried_once_after_compaction() {
    let provider = FakeProvider::new(vec![
        Turn::overflow(),
        Turn::summary(&structured_summary()),
        Turn::text("ok"),
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
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("the retry succeeds");
    assert_eq!(agent.provider().summary_requests().len(), 1);
    assert_eq!(agent.provider().normal_requests().len(), 2);
}

#[tokio::test]
async fn an_overflow_after_visible_text_is_not_retried() {
    let provider = FakeProvider::new(vec![Turn::overflow().with_events(vec![
        StreamEvent::TextDelta {
            text: "partial answer".into(),
        },
    ])]);
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
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the overflow is reported");
    assert!(error.to_string().contains("context window exceeded"));
    assert!(agent.provider().summary_requests().is_empty());
}

#[tokio::test]
async fn an_overflow_after_visible_reasoning_is_not_retried() {
    assert_overflow_after_visible_delta_is_not_retried(StreamEvent::ReasoningDelta {
        text: "visible reasoning that is long enough to clear redaction buffering".into(),
    })
    .await;
}

#[tokio::test]
async fn an_overflow_after_a_tool_call_delta_is_not_retried() {
    assert_overflow_after_visible_delta_is_not_retried(StreamEvent::ToolCallDelta {
        tool_call_id: "call-1".into(),
        tool_name: "echo".into(),
        arguments: r#"{"value":"partial"}"#.into(),
    })
    .await;
}

async fn assert_overflow_after_visible_delta_is_not_retried(event: StreamEvent) {
    let provider = FakeProvider::new(vec![Turn::overflow().with_events(vec![event])]);
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
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the overflow is reported");
    assert!(error.to_string().contains("context window exceeded"));
    assert!(agent.provider().summary_requests().is_empty());
    assert_eq!(agent.provider().normal_requests().len(), 1);
}

#[tokio::test]
async fn a_second_overflow_after_the_retry_stops_the_turn() {
    let provider = FakeProvider::new(vec![
        Turn::overflow(),
        Turn::summary(&structured_summary()),
        Turn::overflow(),
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
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the turn stops");
    assert_eq!(
        error.to_string(),
        crate::agent::overflow::OVERFLOW_RETRY_FAILURE_MESSAGE
    );
}

#[tokio::test]
async fn an_overflow_with_nothing_to_compact_returns_the_original_error() {
    let provider = FakeProvider::new(vec![Turn::overflow()]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: true,
                // Everything is inside the retained window, so there is no
                // prefix left to summarize.
                keep_recent_tokens: 1_000_000,
                ..CompactionSettings::default()
            },
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("the overflow is reported");
    assert!(
        error.to_string().contains("context window exceeded"),
        "{error:?}"
    );
    let completed = collect(&events)
        .into_iter()
        .find_map(|event| match event {
            Event::CompactionCompleted { compaction } => Some(compaction),
            _ => None,
        })
        .expect("a completion event");
    assert!(completed.noop, "the no-op completion was not reported");
    assert!(agent.provider().summary_requests().is_empty());
}

#[tokio::test]
async fn automatic_compaction_disabled_skips_both_paths() {
    let provider = FakeProvider::new(vec![Turn::overflow()]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: CompactionSettings {
                auto: false,
                hard_input_window: 200,
                working_window: 100,
                reserve_tokens: 40,
                keep_recent_tokens: 0,
            },
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the overflow is reported");
    assert!(error.to_string().contains("context window exceeded"));
    assert!(agent.provider().summary_requests().is_empty());
}
