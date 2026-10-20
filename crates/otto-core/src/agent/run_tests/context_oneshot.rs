//! Run-loop tests: context reports, one-shot completion, approval barriers and compaction hooks.

use super::*;

// -- context report --------------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_context_report_lists_what_the_next_request_sends() {
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            system_prompt: "base\n## Skills\nnone".into(),
            system_prompt_parts: vec![
                ("Base".into(), "base\n".into()),
                ("Skills".into(), "## Skills\nnone".into()),
            ],
            memory: Some(Arc::new(FakeMemory::with_records(one_record()))),
            compaction: CompactionSettings {
                auto: true,
                hard_input_window: 200_000,
                working_window: 100_000,
                reserve_tokens: 1_000,
                keep_recent_tokens: 10_000,
            },
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");

    let report = agent.context_report();
    assert_eq!(report.model, "test-model");
    assert_eq!(report.context_window, 200_000);
    assert_eq!(report.compaction_threshold, 99_000);

    let section = |kind: SectionKind| {
        report
            .sections
            .iter()
            .find(|section| section.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} section"))
    };
    let prompt = section(SectionKind::SystemPrompt);
    let labels: Vec<&str> = prompt.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, ["Base", "Skills"]);
    let text: String = prompt.items.iter().map(|i| i.text.as_str()).collect();
    assert_eq!(text, "base\n## Skills\nnone");

    let tools = section(SectionKind::Tools);
    assert_eq!(tools.items.len(), 1);
    assert_eq!(tools.items[0].label, "echo");
    assert!(tools.items[0].text.contains(r#""type":"object""#));

    // user, assistant tool call, tool result, assistant text: the session
    // transcript, in order.
    let messages = section(SectionKind::Messages);
    assert_eq!(messages.items.len(), agent.session().messages().len());
    assert_eq!(messages.items[0].label, "#1 user");
    assert_eq!(messages.items[0].text, "hello");
    assert_eq!(messages.items[1].label, "#2 assistant echo");
    assert_eq!(messages.items[2].label, "#3 tool echo");
    assert_eq!(messages.items[3].text, "done");

    let memory = section(SectionKind::Memory);
    assert!(memory.items[0].text.contains("prefers vim"));

    for section in &report.sections {
        let sum: i64 = section.items.iter().map(|item| item.tokens).sum();
        assert_eq!(section.tokens, sum, "{:?}", section.kind);
    }
    assert!(report.estimated_total > 0);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn prompt_parts_that_do_not_match_the_prompt_fall_back_to_one_part() {
    let agent = Agent::new(
        FakeProvider::new(Vec::new()),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            system_prompt: "the real prompt".into(),
            system_prompt_parts: vec![("Base".into(), "something else".into())],
            ..options()
        },
    );
    let report = agent.context_report();
    let prompt = &report.sections[0];
    assert_eq!(prompt.kind, SectionKind::SystemPrompt);
    assert_eq!(prompt.items.len(), 1);
    assert_eq!(prompt.items[0].label, "System prompt");
    assert_eq!(prompt.items[0].text, "the real prompt");
    // No compaction settings: no window and no threshold.
    assert_eq!(report.context_window, 0);
    assert_eq!(report.compaction_threshold, 0);
    assert!(
        !report
            .sections
            .iter()
            .any(|section| section.kind == SectionKind::Memory),
        "no recall ran, so there is no memory section"
    );
}

#[test]
fn a_compaction_summary_and_mcp_tools_get_their_own_sections() {
    use crate::session::COMPACTION_CONTEXT_TYPE;
    let request = Request {
        cache_key: String::new(),
        model: "m".into(),
        system_prompt: "p".into(),
        thinking: String::new(),
        messages: vec![
            Message {
                role: Role::Context,
                context_type: COMPACTION_CONTEXT_TYPE.into(),
                blocks: vec![Block::text("[Compaction summary]\nearlier")],
                ..Message::default()
            },
            Message {
                role: Role::User,
                blocks: vec![Block::text("next")],
                ..Message::default()
            },
        ],
        tools: vec![
            ToolDefinition {
                name: "read".into(),
                ..ToolDefinition::default()
            },
            ToolDefinition {
                name: "mcp__github__search".into(),
                ..ToolDefinition::default()
            },
        ],
        remote_compaction: None,
        request_remote_compaction: false,
    };
    let sections = crate::agent::context_report::sections(&request, &[], "");
    let kinds: Vec<SectionKind> = sections.iter().map(|section| section.kind).collect();
    assert_eq!(
        kinds,
        [
            SectionKind::SystemPrompt,
            SectionKind::Tools,
            SectionKind::McpTools,
            SectionKind::CompactionSummary,
            SectionKind::Messages,
        ]
    );
    assert_eq!(sections[3].items[0].text, "[Compaction summary]\nearlier");
    assert_eq!(sections[4].items[0].label, "#2 user");
}

// -- one-shot text completion ------------------------------------------------

pub(super) fn text_request<'a>(
    user_text: &'a str,
    maximum_bytes: usize,
) -> crate::agent::oneshot::TextRequest<'a> {
    crate::agent::oneshot::TextRequest {
        system_prompt: "answer briefly",
        user_text,
        maximum_bytes,
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_sends_one_tool_less_redacted_message_and_touches_no_history() {
    const SECRET: &str = "sk-live-abcdef";
    let mut turn = Turn::summary("the answer");
    turn.events.push(StreamEvent::Retry {
        attempt: 2,
        max_attempts: 4,
        delay: std::time::Duration::from_secs(1),
        reason: "connection interrupted".into(),
        interrupted_output: false,
    });
    let agent = Agent::with_redactor(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
        Redactor::new(&[SECRET.to_owned()]),
    );

    let mut events = Vec::new();
    let response = agent
        .complete_text(
            &text_request(&format!("the key is {SECRET}"), 1024),
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("complete");

    assert_eq!(response.text, "the answer");
    assert_eq!(names(&events), ["provider_retry", "provider_api_call"]);
    assert!(
        matches!(&events[0], Event::ProviderRetry { operation_id, attempt: 2, .. } if !operation_id.as_str().is_empty())
    );
    assert!(response.usage_present);
    assert_eq!(response.usage.output_tokens, 7);
    let requests = agent.provider().requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].system_prompt, "answer briefly");
    assert_eq!(requests[0].messages.len(), 1);
    assert!(!requests[0].messages[0].text().contains(SECRET));
    assert!(agent.session.messages().is_empty(), "no history appended");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_redacts_the_response() {
    const SECRET: &str = "sk-live-abcdef";
    let provider = FakeProvider::new(vec![Turn::summary(&format!("leaked {SECRET}"))]);
    let agent = Agent::with_redactor(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
        Redactor::new(&[SECRET.to_owned()]),
    );
    let response = agent
        .complete_text(
            &text_request("hi", 1024),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("complete");
    assert!(!response.text.contains(SECRET), "{}", response.text);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_rejects_a_tool_call_a_wrong_role_and_an_oversized_answer() {
    let cases = [
        (Turn::tool_call("c1", "{}"), "non-text block"),
        (Turn::summary("0123456789"), "byte bound"),
    ];
    for (turn, expected) in cases {
        let agent = Agent::new(
            FakeProvider::new(vec![turn]),
            EchoExecutor::default(),
            MemorySession::new(),
            options(),
        );
        let error = agent
            .complete_text(
                &text_request("hi", 5),
                &mut |_| {},
                &CancellationToken::new(),
            )
            .await
            .expect_err("rejected");
        assert!(error.to_string().contains(expected), "{error}");
    }

    let mut wrong_role = Turn::summary("fine");
    if let Ok(response) = &mut wrong_role.outcome {
        response.message.role = Role::User;
    }
    let agent = Agent::new(
        FakeProvider::new(vec![wrong_role]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let error = agent
        .complete_text(
            &text_request("hi", 1024),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect_err("rejected");
    assert!(error.to_string().contains("assistant"), "{error}");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_stops_a_stream_that_runs_past_its_bound() {
    let turn = Turn::summary("x").with_events(vec![
        StreamEvent::TextDelta {
            text: "0123".into(),
        },
        StreamEvent::TextDelta {
            text: "4567".into(),
        },
    ]);
    let agent = Agent::new(
        FakeProvider::new(vec![turn]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let error = agent
        .complete_text(
            &text_request("hi", 5),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect_err("stopped");
    assert!(error.to_string().contains("exceeded"), "{error}");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn complete_text_does_not_call_the_provider_when_already_cancelled_or_boundary_closed() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary("never")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = agent
        .complete_text(&text_request("hi", 1024), &mut |_| {}, &cancel)
        .await
        .expect_err("cancelled");
    assert!(error.is_cancelled(), "{error}");

    let closed = Agent::with_redactor(
        FakeProvider::new(vec![Turn::summary("never")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
        Redactor::with_completeness(&[], false),
    );
    assert!(!closed.allows_dynamic_content());
    let error = closed
        .complete_text(
            &text_request("hi", 1024),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect_err("closed");
    assert!(error.to_string().contains("redaction boundary"), "{error}");
    assert!(agent.provider().requests().is_empty());
    assert!(closed.provider().requests().is_empty());
}

struct ApprovalBarrierExecutor(Arc<AtomicUsize>);
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for ApprovalBarrierExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }
    fn batch_control(
        &self,
        _name: &str,
        _execution: &crate::tool::ToolExecution,
    ) -> crate::approval::BatchControl {
        crate::approval::BatchControl::Replan
    }
    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        self.0.fetch_add(1, Ordering::SeqCst);
        EchoExecutor::default().execute(call, control).await
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn approval_barrier_settles_later_batch_calls_without_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls(), Turn::text("replanned")]),
        ApprovalBarrierExecutor(calls.clone()),
        MemorySession::new(),
        options(),
    );
    let mut events = Vec::new();
    agent
        .run(
            "confirm first",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(events.iter().any(|event|matches!(event,Event::ToolCallFinished{tool_call_id,outcome,..}
        if tool_call_id=="c2" && outcome.effect_certainty==crate::model::EffectCertainty::NotStarted)));
}

#[derive(Default)]
pub(super) struct CompactionHooks {
    events: Mutex<Vec<String>>,
    deny: bool,
}
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch="wasm32",async_trait::async_trait(?Send))]
impl crate::agent::hooks::Hooks for CompactionHooks {
    async fn run(
        &self,
        event: &str,
        _: serde_json::Value,
        _: &dyn OperationControl,
    ) -> Result<crate::agent::hooks::Response, String> {
        self.events.lock().unwrap().push(event.into());
        if event == "before_compaction" {
            return Ok(crate::agent::hooks::Response {
                decision: Some(if self.deny {
                    crate::agent::hooks::Decision::Deny
                } else {
                    crate::agent::hooks::Decision::Allow
                }),
                context: Some("retain the parser contract".into()),
                ..Default::default()
            });
        }
        Ok(Default::default())
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn compaction_hooks_gate_the_provider_and_observe_only_committed_checkpoints() {
    for deny in [false, true] {
        let hooks = Arc::new(CompactionHooks {
            deny,
            ..Default::default()
        });
        let mut opts = options();
        opts.hooks = Some(hooks.clone());
        let agent = Agent::new(
            FakeProvider::new(vec![Turn::summary(&structured_summary())]),
            EchoExecutor::default(),
            seeded_session().await,
            opts,
        );
        let result = agent
            .compact("parser", &mut |_| {}, &CancellationToken::new())
            .await;
        if deny {
            assert!(result.is_err());
            assert!(agent.provider().summary_requests().is_empty());
            assert_eq!(
                *hooks.events.lock().unwrap(),
                vec!["session_start", "before_compaction"]
            );
        } else {
            assert!(result.is_ok());
            assert!(
                agent.provider().summary_requests()[0]
                    .system_prompt
                    .contains("retain the parser contract")
            );
            assert_eq!(
                *hooks.events.lock().unwrap(),
                vec!["session_start", "before_compaction", "after_compaction"]
            );
        }
    }
}
