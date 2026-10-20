//! Run-loop tests: the progress guard and provider recovery byte bounds.

use super::*;

// -- progress guard --------------------------------------------------------

/// Whether every assistant tool call in the transcript has its result, in
/// order. A turn the guard stops must still leave a well-formed transcript.
fn every_tool_call_has_its_result(messages: &[Message]) -> bool {
    let mut pending: Option<&str> = None;
    for message in messages {
        for block in &message.blocks {
            match block.block_type {
                BlockType::ToolCall if pending.is_none() => {
                    pending = Some(block.tool_call_id.as_str());
                }
                BlockType::ToolResult => {
                    if pending != Some(block.tool_call_id.as_str()) {
                        return false;
                    }
                    pending = None;
                }
                _ => {}
            }
        }
    }
    pending.is_none()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn identical_calls_stop_the_turn_with_no_progress() {
    let turns: Vec<Turn> = (0..ProgressPolicy::default().repeated_calls)
        .map(|index| Turn::tool_call(&format!("call-{index}"), r#"{"path":"a.txt"}"#))
        .collect();
    let expected = ProgressPolicy::default().repeated_calls;
    let agent = Agent::new(
        FakeProvider::new(turns),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let error = agent
        .run("go", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("a repeated call stops the turn");

    match error {
        AgentError::NoProgress(stall) => {
            assert_eq!(stall.kind, StallKind::RepeatedCall);
            assert_eq!(stall.tool, "echo");
            assert_eq!(stall.count, expected);
        }
        other => panic!("unexpected error: {other:?}"),
    }
    // One provider request per scripted call, and not one more: the turn
    // stopped before asking the model again.
    assert_eq!(agent.provider().normal_requests().len(), expected as usize);
    assert!(
        every_tool_call_has_its_result(&agent.session().messages()),
        "the stopped turn left a tool call without its result"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failing_streak_stops_the_turn_with_no_progress() {
    let limit = 3;
    // Distinct arguments, so only the failure streak can stop this turn.
    let turns: Vec<Turn> = (0..limit)
        .map(|index| {
            Turn::named_tool_call(
                "missing",
                &format!("call-{index}"),
                &format!(r#"{{"n":{index}}}"#),
            )
        })
        .collect();
    let agent = Agent::new(
        FakeProvider::new(turns),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            progress: ProgressPolicy {
                repeated_calls: 0,
                consecutive_failures: limit,
            },
            ..options()
        },
    );
    let error = agent
        .run("go", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("a failing streak stops the turn");

    match error {
        AgentError::NoProgress(stall) => {
            assert_eq!(stall.kind, StallKind::RepeatedFailure);
            assert_eq!(stall.tool, "missing");
            assert_eq!(stall.count, limit);
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(agent.provider().normal_requests().len(), limit as usize);
    assert!(every_tool_call_has_its_result(&agent.session().messages()));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn alternating_failed_edits_and_unchanged_reads_stop_without_breaking_resume() {
    struct FailedEditAndRead;

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl ToolExecutor for FailedEditAndRead {
        fn definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }

        fn retry_safety(&self, name: &str) -> crate::model::RetrySafety {
            if name == "read" {
                crate::model::RetrySafety::ReadOnly
            } else {
                crate::model::RetrySafety::NonIdempotent
            }
        }

        async fn execute(
            &self,
            call: crate::tool::ToolCall<'_>,
            _: &dyn OperationControl,
        ) -> crate::tool::ToolExecution {
            let failed = call.name == "edit";
            let mut execution = crate::tool::ToolExecution::completed(if failed {
                ToolResult::error("old_text was not found")
            } else {
                ToolResult {
                    content: "unchanged file".into(),
                    ..ToolResult::default()
                }
            });
            if failed {
                execution.outcome.effect_certainty = crate::model::EffectCertainty::NotStarted;
            }
            execution
        }
    }

    for with_hooks in [false, true] {
        let mut turns: Vec<Turn> = (0..6)
            .map(|index| {
                Turn::named_tool_call(
                    if index % 2 == 0 { "edit" } else { "read" },
                    &format!("call-{index}"),
                    r#"{"path":"a.txt"}"#,
                )
            })
            .collect();
        turns.push(Turn::text("continued"));
        let agent = Agent::new(
            FakeProvider::new(turns),
            FailedEditAndRead,
            MemorySession::new(),
            Options {
                progress: ProgressPolicy {
                    repeated_calls: 3,
                    consecutive_failures: 12,
                },
                hooks: with_hooks.then(|| {
                    Arc::new(CompactionHooks::default()) as Arc<dyn crate::agent::hooks::Hooks>
                }),
                ..options()
            },
        );
        let outcome = agent
            .run("go", &mut |_| {}, &CancellationToken::new())
            .await;
        if with_hooks {
            outcome.expect("hook effects are not covered by tool safety");
        } else {
            assert!(
                matches!(outcome, Err(AgentError::NoProgress(stall)) if stall.kind == StallKind::RepeatedCycle && stall.count == 3)
            );
            assert_eq!(agent.provider().normal_requests().len(), 6);
            agent
                .run("continue", &mut |_| {}, &CancellationToken::new())
                .await
                .unwrap();
        }
        assert!(every_tool_call_has_its_result(&agent.session().messages()));
        assert_eq!(
            agent.session().messages().last().unwrap().text(),
            "continued"
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn distinct_calls_never_stop_a_long_turn() {
    // More calls than either limit, all different: a long but productive turn
    // must run to its natural end.
    let mut turns: Vec<Turn> = (0..20)
        .map(|index| Turn::tool_call(&format!("call-{index}"), &format!(r#"{{"n":{index}}}"#)))
        .collect();
    let calls = turns.len();
    turns.push(Turn::text("done"));
    let agent = Agent::new(
        FakeProvider::new(turns),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run("go", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("distinct calls make progress");

    assert_eq!(agent.provider().normal_requests().len(), calls + 1);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_disabled_policy_runs_a_repeating_turn_to_its_natural_end() {
    let mut turns: Vec<Turn> = (0..20)
        .map(|index| Turn::tool_call(&format!("call-{index}"), r#"{"path":"a.txt"}"#))
        .collect();
    turns.push(Turn::text("done"));
    let agent = Agent::new(
        FakeProvider::new(turns),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            progress: ProgressPolicy::disabled(),
            ..options()
        },
    );
    agent
        .run("go", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("a disabled guard never stops a turn");

    assert_eq!(agent.provider().normal_requests().len(), 21);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn provider_recovery_discards_redaction_tails_and_persists_only_the_final_answer() {
    let retry = StreamEvent::Retry {
        attempt: 2,
        max_attempts: 7,
        delay: std::time::Duration::from_secs(1),
        reason: "connection interrupted".into(),
        interrupted_output: true,
    };
    let turn = Turn::text("final answer").with_events(vec![
        StreamEvent::ReasoningDelta {
            text: "abandoned reasoning".into(),
        },
        StreamEvent::TextDelta {
            text: "abandoned answer".into(),
        },
        retry,
        StreamEvent::TextDelta {
            text: "final answer".into(),
        },
    ]);
    let agent = Agent::with_redactor(
        FakeProvider::new(vec![turn]),
        EchoExecutor {
            content: None,
            persisted: None,
        },
        MemorySession::new(),
        options(),
        Redactor::new(&["long-configured-sensitive-value".into()]),
    );
    let mut events = Vec::new();
    agent
        .run(
            "hi",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let retry = events
        .iter()
        .position(|event| matches!(event, Event::ProviderRetry { .. }))
        .unwrap();
    let after: String = events[retry + 1..]
        .iter()
        .filter_map(|event| match event {
            Event::TextDelta { text } | Event::ReasoningDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(after, "final answer");
    let messages = agent.session().messages();
    assert_eq!(messages.last().unwrap().text(), "final answer");
    assert!(!format!("{messages:?}").contains("abandoned"));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_recovery_resets_the_per_response_byte_bound() {
    let turn = Turn::text("done").with_events(vec![
        StreamEvent::TextDelta {
            text: "partial".into(),
        },
        StreamEvent::Retry {
            attempt: 2,
            max_attempts: 7,
            delay: std::time::Duration::ZERO,
            reason: "connection interrupted".into(),
            interrupted_output: true,
        },
        StreamEvent::TextDelta {
            text: "done".into(),
        },
    ]);
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor {
            content: None,
            persisted: None,
        },
        MemorySession::new(),
        options(),
    );
    let answer = agent
        .complete_text(
            &text_request("hi", 8),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(answer.text, "done");
    assert!(agent.session().messages().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn compaction_recovery_resets_the_per_response_byte_bound() {
    let summary = structured_summary();
    let turn = Turn::summary(&summary).with_events(vec![
        StreamEvent::TextDelta {
            text: "x".repeat(crate::agent::summary::SUMMARY_MAXIMUM_BYTES),
        },
        StreamEvent::Retry {
            attempt: 2,
            max_attempts: 7,
            delay: std::time::Duration::ZERO,
            reason: "connection interrupted".into(),
            interrupted_output: true,
        },
        StreamEvent::TextDelta {
            text: summary.clone(),
        },
    ]);
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        seeded_session().await,
        options(),
    );
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        agent
            .session()
            .latest_compaction()
            .unwrap()
            .summary
            .starts_with(&summary)
    );
}
