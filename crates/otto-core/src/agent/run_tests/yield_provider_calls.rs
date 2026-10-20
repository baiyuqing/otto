//! Run-loop tests: yielding, edit advice and provider call records.

use super::*;

struct YieldExecutor {
    calls: Arc<AtomicUsize>,
    inbox: Option<Arc<Inbox>>,
    fail: bool,
    replan: bool,
}
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for YieldExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }
    fn yields_turn(&self, _: &str, _: &crate::tool::ToolExecution) -> bool {
        true
    }
    fn batch_control(
        &self,
        _: &str,
        _: &crate::tool::ToolExecution,
    ) -> crate::approval::BatchControl {
        if self.replan {
            crate::approval::BatchControl::Replan
        } else {
            crate::approval::BatchControl::Continue
        }
    }
    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(inbox) = &self.inbox {
            inbox.push(Notification {
                kind: Some(NotificationKind::TaskFinished),
                text: "child completed during yield".into(),
                ..Default::default()
            });
        }
        if self.fail {
            crate::tool::ToolExecution::completed(ToolResult::error("repair this failure"))
        } else {
            EchoExecutor::default().execute(call, control).await
        }
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn yielding_settles_the_entire_batch_without_another_provider_request() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls()]),
        YieldExecutor {
            calls: calls.clone(),
            inbox: None,
            fail: false,
            replan: false,
        },
        MemorySession::new(),
        options(),
    );
    agent
        .run("delegate and yield", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(agent.provider().requests().len(), 1);
    assert_eq!(
        agent
            .session()
            .messages()
            .iter()
            .filter(|m| m.role == Role::Tool)
            .count(),
        2
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn completion_racing_with_yield_is_processed_without_an_extra_user_prompt() {
    let inbox = Arc::new(Inbox::new(None));
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("y", "{}"),
            Turn::text("collected child result"),
        ]),
        YieldExecutor {
            calls: Arc::default(),
            inbox: Some(inbox.clone()),
            fail: false,
            replan: false,
        },
        MemorySession::new(),
        Options {
            mailbox: inbox,
            ..options()
        },
    );
    agent
        .run("delegate", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(agent.provider().requests().len(), 2);
    assert!(
        agent.provider().requests()[1]
            .messages
            .iter()
            .any(|m| m.text().contains("child completed during yield"))
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn failures_and_approval_replanning_take_precedence_over_yield() {
    for (fail, replan) in [(true, false), (false, true)] {
        let agent = Agent::new(
            FakeProvider::new(vec![
                Turn::tool_call("y", "{}"),
                Turn::text("handled feedback"),
            ]),
            YieldExecutor {
                calls: Arc::default(),
                inbox: None,
                fail,
                replan,
            },
            MemorySession::new(),
            options(),
        );
        agent
            .run("work", &mut |_| {}, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(agent.provider().requests().len(), 2);
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_yielded_turn_can_resume_from_a_later_child_notification() {
    let inbox = Arc::new(Inbox::new(None));
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::tool_call("y", "{}"), Turn::text("resumed")]),
        YieldExecutor {
            calls: Arc::default(),
            inbox: None,
            fail: false,
            replan: false,
        },
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    agent
        .run("work", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(agent.provider().requests().len(), 1);
    inbox.push(Notification {
        kind: Some(NotificationKind::TaskFinished),
        text: "later result".into(),
        ..Default::default()
    });
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(agent.provider().requests().len(), 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn steering_racing_with_yield_is_preserved() {
    let steering = Arc::new(crate::agent::steering::Steering::default());
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("y", "{}"),
            Turn::text("handled guidance"),
        ]),
        YieldExecutor {
            calls: Arc::default(),
            inbox: None,
            fail: false,
            replan: false,
        },
        MemorySession::new(),
        Options {
            steering: steering.clone(),
            ..options()
        },
    );
    let mut submitted = false;
    agent
        .run(
            "work",
            &mut |event| {
                if matches!(event, Event::ToolCallFinished { .. }) && !submitted {
                    steering.submit("new direction").unwrap();
                    submitted = true;
                }
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(agent.provider().requests().len(), 2);
    assert!(
        agent.provider().requests()[1]
            .messages
            .iter()
            .any(|m| m.text().contains("new direction"))
    );
}

struct EditAdviceExecutor;
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for EditAdviceExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "edit".into(),
            ..Default::default()
        }]
    }
    async fn execute(
        &self,
        _: crate::tool::ToolCall<'_>,
        _: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        crate::tool::ToolExecution::completed(ToolResult {
            content: "edited file.rs".into(),
            ..Default::default()
        })
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn batching_advice_is_persisted_with_the_third_successful_edit() {
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::named_tool_call(
                "edit",
                "e1",
                r#"{"path":"file.rs","old_text":"a","new_text":"A"}"#,
            ),
            Turn::named_tool_call(
                "edit",
                "e2",
                r#"{"path":"file.rs","old_text":"b","new_text":"B"}"#,
            ),
            Turn::named_tool_call(
                "edit",
                "e3",
                r#"{"path":"file.rs","old_text":"c","new_text":"C"}"#,
            ),
            Turn::text("done"),
        ]),
        EditAdviceExecutor,
        MemorySession::new(),
        options(),
    );
    agent
        .run("edit", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    let results: Vec<_> = agent
        .session()
        .messages()
        .into_iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| {
            assert_eq!(message.blocks.len(), 1);
            message.blocks.into_iter().next().unwrap()
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert_eq!(
        results
            .iter()
            .map(|block| block.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["e1", "e2", "e3"]
    );
    assert!(
        results
            .iter()
            .all(|block| block.block_type == BlockType::ToolResult && !block.is_error)
    );
    assert!(!results[0].text.contains("Batching advice"));
    assert!(!results[1].text.contains("Batching advice"));
    assert!(results[2].text.contains("Batching advice"));
    assert!(results[2].text.contains("This edit succeeded"));
}

// -- provider call records -------------------------------------------------

/// Options whose clock advances 100 ms on every read.
fn ticking_options() -> Options {
    let tick = std::sync::atomic::AtomicI64::new(0);
    Options {
        now: Box::new(move || {
            clock() + chrono::TimeDelta::milliseconds(100 * tick.fetch_add(1, Ordering::SeqCst))
        }),
        ..options()
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_retried_request_appends_one_provider_call_record() {
    use crate::session::ProviderCallKind;
    let mut turn = Turn::text("final").with_events(vec![
        StreamEvent::ReasoningDelta {
            text: "abandoned".into(),
        },
        StreamEvent::Retry {
            attempt: 2,
            max_attempts: 3,
            delay: std::time::Duration::from_secs(2),
            reason: "connection interrupted".into(),
            interrupted_output: true,
        },
        StreamEvent::TextDelta {
            text: "final".into(),
        },
    ]);
    let usage = Usage {
        input_tokens: 40,
        output_tokens: 3,
        cached_input_tokens: 10,
    };
    turn.outcome
        .as_mut()
        .expect("scripted response")
        .message
        .usage = Some(usage);
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        TurnRecordingSession::default(),
        ticking_options(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");

    let calls = agent.session().calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.kind, ProviderCallKind::Ordinary);
    assert_eq!(call.attempts, 2);
    assert_eq!(call.usage, Some(usage));
    assert_eq!(call.finish_reason, Some(FinishReason::Stop));
    // The clock reads 100 ms apart: start, reasoning, retry, text, end.
    assert_eq!(call.retries.len(), 1);
    assert_eq!(call.retries[0].at_ms, 200);
    assert_eq!(call.retries[0].delay_ms, 2000);
    assert_eq!(call.retries[0].reason, "connection interrupted");
    // The reasoning delta before the retry does not count.
    assert_eq!(call.first_output_ms, Some(300));
    assert_eq!(call.duration_ms, 400);
    assert_eq!(call.started_at % 100, 0);
    assert!(call.started_at >= clock().timestamp_millis());
    let request = call.request.as_ref().expect("request estimate");
    assert!(request.system_prompt_tokens > 0);
    assert!(request.message_tokens > 0);
    assert_eq!(request.memory_tokens, 0);
    assert!(request.memory_record_ids.is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn provider_call_record_lists_the_recalled_memory_records() {
    let mut records = one_record();
    records.push(Record {
        id: "rec-2".into(),
        key: "shell".into(),
        text: "prefers zsh".into(),
        ..records[0].clone()
    });
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        TurnRecordingSession::default(),
        Options {
            memory: Some(Arc::new(FakeMemory::with_records(records))),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");

    let calls = agent.session().calls();
    let request = calls[0].request.as_ref().expect("request estimate");
    assert_eq!(request.memory_record_ids, ["rec-1", "rec-2"]);
    assert!(request.memory_tokens > 0);
    // The memory message is counted once, as memory.
    let user_only = crate::agent::context_estimate::estimate_message(
        &agent.provider().requests()[0].messages[1],
    );
    assert_eq!(request.message_tokens, user_only);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_compaction_appends_a_provider_call_record_of_its_kind() {
    use crate::session::ProviderCallKind;
    let mut turn = Turn::summary(&structured_summary());
    turn.events.push(StreamEvent::Retry {
        attempt: 2,
        max_attempts: 4,
        delay: std::time::Duration::from_secs(1),
        reason: "connection interrupted".into(),
        interrupted_output: false,
    });
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        TurnRecordingSession {
            inner: seeded_session().await,
            ..TurnRecordingSession::default()
        },
        options(),
    );
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("compact");

    let calls = agent.session().calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].kind, ProviderCallKind::LocalCompaction);
    assert_eq!(calls[0].attempts, 2);
    assert_eq!(calls[0].retries.len(), 1);
    assert!(calls[0].request.as_ref().expect("estimate").message_tokens > 0);
}
