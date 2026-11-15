//! Run-loop tests: the provider egress guard and redaction of events and history.

use super::*;

// -- redaction -------------------------------------------------------------

fn heuristic_credential() -> String {
    ["gh", "p_", "abcdefghijkl", "mnopqrstuvwx"].concat()
}

async fn seeded_session_with_candidate(candidate: &str) -> MemorySession {
    let session = MemorySession::new();
    for index in 0..3 {
        session
            .append(Message {
                id: format!("guard-u{index}"),
                role: Role::User,
                blocks: vec![Block::text(if index == 0 {
                    format!("API_KEY={candidate}")
                } else {
                    "historic user text".repeat(8)
                })],
                ..Message::default()
            })
            .await
            .expect("append user");
        session
            .append(Message {
                id: format!("guard-a{index}"),
                role: Role::Assistant,
                blocks: vec![Block::text("historic assistant text".repeat(8))],
                ..Message::default()
            })
            .await
            .expect("append assistant");
    }
    session
}

#[tokio::test]
async fn provider_egress_guard_redacts_ordinary_dynamic_fields_and_json() {
    let candidate = heuristic_credential();
    let email = "alice@example.com";
    let provider = FakeProvider::new(vec![
        Turn::tool_call("guard-call", &format!(r#"{{"token":"{candidate}"}}"#)),
        Turn::text("done"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor {
            persisted: None,
            content: Some(format!("PASSWORD={candidate}")),
        },
        MemorySession::new(),
        Options {
            system_prompt: format!("API_KEY={candidate}; contact {email}"),
            ..options()
        },
    );
    agent
        .run_with_system_context(
            &format!("Authorization: Bearer {candidate}"),
            &format!("TOKEN={candidate}"),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let requests = agent.provider().requests();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let rendered = format!("{request:?}");
        assert!(
            !rendered.contains(&candidate),
            "candidate reached ordinary provider request"
        );
        assert!(
            !rendered.contains(email),
            "PII reached ordinary provider request"
        );
        assert!(rendered.contains("<EMAIL_1>"), "PII placeholder is missing");
    }
}

#[tokio::test]
async fn provider_egress_guard_redacts_local_compaction_request() {
    let candidate = heuristic_credential();
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary(&structured_summary())]),
        EchoExecutor::default(),
        seeded_session_with_candidate(&candidate).await,
        options(),
    );
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("compact");
    let requests = agent.provider().requests();
    assert_eq!(requests.len(), 1);
    assert!(!format!("{:?}", requests[0]).contains(&candidate));
}

#[tokio::test]
async fn local_compaction_guard_blocks_before_planned_event_and_provider_poll() {
    let candidate = heuristic_credential();
    let session = MemorySession::new();
    for index in 0..3 {
        session
            .append(Message {
                id: format!("local-guard-u{index}"),
                role: Role::User,
                blocks: vec![Block::text("historic user text".repeat(8))],
                ..Message::default()
            })
            .await
            .expect("append user");
        session
            .append(Message {
                id: format!("local-guard-a{index}"),
                role: Role::Assistant,
                blocks: vec![Block::text("historic assistant text".repeat(8))],
                ..Message::default()
            })
            .await
            .expect("append assistant");
    }
    let diagnostics = Arc::new(CapturingDiagnostics::default());
    let captured_models = diagnostics.models.clone();
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary(&structured_summary())]),
        EchoExecutor::default(),
        session,
        Options {
            model: candidate.clone(),
            diagnostics: Some(diagnostics),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .compact(
            "",
            &mut |event| sink.lock().expect("events").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("unsafe local request must block");

    assert!(matches!(error, AgentError::ProviderEgressSecret));
    assert!(!error.to_string().contains(&candidate));
    assert!(agent.provider().requests().is_empty());
    assert!(captured_models.lock().expect("diagnostics").is_empty());
    let captured_events = collect(&events);
    assert!(
        captured_events
            .iter()
            .all(|event| !matches!(event, Event::CompactionPlanned { .. }))
    );
    assert!(
        captured_events
            .iter()
            .all(|event| !format!("{event:?}").contains(&candidate))
    );
}

#[tokio::test]
async fn provider_egress_guard_redacts_remote_compaction_request() {
    let candidate = heuristic_credential();
    let provider = FakeProvider::new(vec![Turn::remote_compaction("guard-checkpoint", "opaque")])
        .with_remote_compaction();
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session_with_candidate(&candidate).await,
        options(),
    );
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("compact");
    let requests = agent.provider().requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].request_remote_compaction);
    assert!(!format!("{:?}", requests[0]).contains(&candidate));
}

#[tokio::test]
async fn provider_egress_guard_redacts_one_shot_request() {
    let candidate = heuristic_credential();
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary("safe answer")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .complete_text(
            &text_request(&format!("PRIVATE_KEY={candidate}"), 1024),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("complete");
    let requests = agent.provider().requests();
    assert_eq!(requests.len(), 1);
    assert!(!format!("{:?}", requests[0]).contains(&candidate));
}

#[tokio::test]
async fn provider_egress_guard_refuses_structural_image_before_all_turn_sinks() {
    let candidate = heuristic_credential();
    let diagnostics = Arc::new(CapturingDiagnostics::default());
    let captured_models = diagnostics.models.clone();
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("must not run")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            diagnostics: Some(diagnostics),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .run_with_image_system_context(
            "must not be persisted before the request is guarded",
            Some(Block {
                block_type: BlockType::Image,
                data: candidate.clone(),
                mime_type: "image/png".into(),
                ..Block::default()
            }),
            "",
            &mut |event| sink.lock().expect("events").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("structural image candidate must block");

    assert!(matches!(error, AgentError::ProviderEgressSecret));
    assert!(!error.to_string().contains(&candidate));
    assert!(agent.provider().requests().is_empty());
    assert!(agent.session().messages().is_empty());
    assert!(captured_models.lock().expect("diagnostics").is_empty());
    assert!(collect(&events).is_empty());
}

#[tokio::test]
async fn remote_compaction_guard_blocks_before_planned_event_and_provider_poll() {
    let candidate = heuristic_credential();
    let session = MemorySession::new();
    session
        .append(Message {
            id: "remote-guard-user".into(),
            role: Role::User,
            blocks: vec![Block::text("historic user text")],
            ..Message::default()
        })
        .await
        .expect("seed history");
    let diagnostics = Arc::new(CapturingDiagnostics::default());
    let captured_models = diagnostics.models.clone();
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::remote_compaction("must-not-run", "opaque")])
            .with_remote_compaction(),
        EchoExecutor::default(),
        session,
        Options {
            model: candidate.clone(),
            diagnostics: Some(diagnostics),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .compact(
            "",
            &mut |event| sink.lock().expect("events").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("unsafe remote request must block");

    assert!(matches!(error, AgentError::ProviderEgressSecret));
    assert!(!error.to_string().contains(&candidate));
    assert!(agent.provider().requests().is_empty());
    assert!(captured_models.lock().expect("diagnostics").is_empty());
    let captured_events = collect(&events);
    assert!(
        captured_events
            .iter()
            .all(|event| !matches!(event, Event::CompactionPlanned { .. }))
    );
    assert!(
        captured_events
            .iter()
            .all(|event| !format!("{event:?}").contains(&candidate))
    );
}

#[tokio::test]
async fn provider_egress_guard_refuses_structural_secret_without_provider_poll() {
    let candidate = heuristic_credential();
    let diagnostics = Arc::new(CapturingDiagnostics::default());
    let captured_models = diagnostics.models.clone();
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("must not run")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            diagnostics: Some(diagnostics),
            model: candidate.clone(),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .run(
            "must not be persisted before the request is guarded",
            &mut |event| sink.lock().expect("events").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("structural candidate must block");

    assert!(matches!(error, AgentError::ProviderEgressSecret));
    assert!(!error.to_string().contains(&candidate));
    assert!(agent.provider().requests().is_empty());
    assert!(agent.session().messages().is_empty());
    assert!(captured_models.lock().expect("diagnostics").is_empty());
    let captured_events = collect(&events);
    assert!(
        captured_events
            .iter()
            .all(|event| !matches!(event, Event::ProviderApiCall { .. }))
    );
    assert!(
        captured_events
            .iter()
            .all(|event| !format!("{event:?}").contains(&candidate))
    );
}

#[tokio::test]
async fn a_credential_is_redacted_from_events_persistence_and_history() {
    const SECRET: &str = "sk-live-abcdef";
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", &format!(r#"{{"token":"{SECRET}"}}"#)),
        Turn::text("done"),
    ]);
    let agent = Agent::with_redactor(
        provider,
        EchoExecutor {
            content: Some(format!("saw {SECRET}")),
            persisted: None,
        },
        MemorySession::new(),
        options(),
        Redactor::new(&[SECRET.to_owned()]),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            &format!("use {SECRET} please"),
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    for event in collect(&events) {
        assert!(
            !format!("{event:?}").contains(SECRET),
            "the credential reached an event: {event:?}"
        );
    }
    for message in agent.session().messages() {
        assert!(
            !format!("{message:?}").contains(SECRET),
            "the credential was persisted: {message:?}"
        );
    }
    for request in agent.provider().requests() {
        assert!(
            !format!("{:?}", request.messages).contains(SECRET),
            "the credential went back to the provider"
        );
    }
}

#[tokio::test]
async fn a_provider_retry_is_forwarded_before_the_reply() {
    let mut turn = Turn::text("ok");
    turn.events.insert(
        0,
        StreamEvent::Retry {
            attempt: 2,
            max_attempts: 3,
            delay: std::time::Duration::from_millis(250),
            reason: "HTTP 503".into(),
            interrupted_output: false,
        },
    );
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor {
            content: None,
            persisted: None,
        },
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hi",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let events = collect(&events);
    let retry = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::ProviderRetry {
                    operation_id,
                    attempt: 2,
                    max_attempts: 3,
                    delay,
                    reason,
                                    interrupted_output: false,
                } if !operation_id.as_str().is_empty()
                    && *delay == std::time::Duration::from_millis(250)
                    && reason == "HTTP 503"
            )
        })
        .expect("retry forwarded");
    let text = events
        .iter()
        .position(|event| matches!(event, Event::TextDelta { .. }))
        .expect("text");
    assert!(retry < text);
}

#[tokio::test]
async fn reasoning_is_streamed_redacted_and_persisted() {
    const SECRET: &str = "sk-live-abcdef";
    let reasoning = format!("use {SECRET} then");
    let provider = FakeProvider::new(vec![
        Turn {
            events: vec![
                StreamEvent::ReasoningDelta {
                    text: "use sk-live".into(),
                },
                StreamEvent::ReasoningDelta {
                    text: "-abcdef then".into(),
                },
                StreamEvent::TextDelta { text: "ok".into() },
            ],
            outcome: Ok(Response {
                message: Message {
                    role: Role::Assistant,
                    finish_reason: Some(FinishReason::Stop),
                    blocks: vec![Block::reasoning(reasoning), Block::text("ok")],
                    ..Message::default()
                },
                ..Response::default()
            }),
        },
        Turn::text("second"),
    ]);
    let agent = Agent::with_redactor(
        provider,
        EchoExecutor {
            content: None,
            persisted: None,
        },
        MemorySession::new(),
        options(),
        Redactor::new(&[SECRET.to_owned()]),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    for prompt in ["first", "again"] {
        agent
            .run(
                prompt,
                &mut |event| sink.lock().expect("e").push(event),
                &CancellationToken::new(),
            )
            .await
            .expect("run");
    }

    let streamed: Vec<Event> = collect(&events)
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Event::ReasoningDelta { .. } | Event::TextDelta { .. }
            )
        })
        .collect();
    let reasoning_text: String = streamed
        .iter()
        .take_while(|event| matches!(event, Event::ReasoningDelta { .. }))
        .map(|event| match event {
            Event::ReasoningDelta { text } => text.as_str(),
            _ => "",
        })
        .collect();
    let redacted = format!("use {} then", crate::agent::redactor::REDACTION_MARKER);
    assert_eq!(reasoning_text, redacted);
    assert!(matches!(&streamed[streamed.len() - 2], Event::TextDelta { text } if text == "ok"));

    let first_reply = &agent.session().messages()[1];
    assert_eq!(first_reply.blocks[0], Block::reasoning(redacted));
}

#[tokio::test]
async fn an_incomplete_redactor_runs_no_provider_tool_or_session_call() {
    let provider = FakeProvider::new(vec![Turn::text("never sent")]);
    let agent = Agent::with_redactor(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
        Redactor::with_completeness(&["secret".to_owned()], false),
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
        .expect("the turn ends without doing anything");
    assert_eq!(
        names(&collect(&events)),
        ["agent_started", "agent_finished"]
    );
    assert!(agent.provider().requests().is_empty());
    assert!(agent.session().messages().is_empty());
}

#[tokio::test]
async fn an_incomplete_redactor_still_reports_cancellation() {
    let agent = Agent::with_redactor(
        FakeProvider::new(Vec::new()),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
        Redactor::with_completeness(&["secret".to_owned()], false),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = agent
        .run("hello", &mut |_| {}, &cancel)
        .await
        .expect_err("cancellation is reported");
    assert!(error.is_cancelled(), "unexpected error: {error}");
    assert!(agent.session().messages().is_empty());
}

#[tokio::test]
async fn a_secret_in_the_configuration_downgrades_the_redactor() {
    let agent = Agent::with_redactor(
        FakeProvider::new(vec![Turn::text("never sent")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            system_prompt: "the key is sk-live-abcdef".into(),
            ..options()
        },
        Redactor::new(&["sk-live-abcdef".to_owned()]),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("the turn ends without doing anything");
    assert!(
        agent.provider().requests().is_empty(),
        "a downgraded redactor still let a request through"
    );
}
