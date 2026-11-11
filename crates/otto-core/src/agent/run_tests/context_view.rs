//! Live strategy behavior through the ordinary provider/tool turn loop.

use super::*;
use crate::agent::context_view::ContextView;
use crate::config::context_strategy::ContextStrategy;

struct ViewProvider {
    ordinary: FakeProvider,
    compressions: AtomicUsize,
    compression_requests: Mutex<Vec<Request>>,
    summary: String,
}

impl ViewProvider {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            ordinary: FakeProvider::new(turns).with_remote_compaction(),
            compressions: AtomicUsize::new(0),
            compression_requests: Mutex::default(),
            summary: "user: exact-marker; assistant: earlier work".into(),
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Provider for ViewProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.ordinary.capabilities()
    }

    async fn complete(
        &self,
        request: &Request,
        emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        if request.system_prompt == crate::view_tree::SYSTEM_PROMPT {
            self.compressions.fetch_add(1, Ordering::SeqCst);
            self.compression_requests
                .lock()
                .unwrap()
                .push(request.clone());
            return ProviderSettlement::succeeded(Turn::text(&self.summary).outcome.unwrap(), 1);
        }
        self.ordinary.complete(request, emit, control).await
    }
}

struct ViewExecutor {
    context: Arc<ContextView>,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for ViewExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        crate::view_tree::tool_definitions()
            .into_iter()
            .take(1)
            .collect()
    }
    fn retry_safety(&self, _: &str) -> crate::model::RetrySafety {
        crate::model::RetrySafety::ReadOnly
    }
    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        _: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        let args: serde_json::Value = serde_json::from_str(call.arguments.get()).unwrap();
        let result = match self
            .context
            .zoom(args["id"].as_u64().unwrap(), args["n"].as_u64().unwrap())
        {
            Ok(content) => ToolResult {
                content,
                ..ToolResult::default()
            },
            Err(error) => ToolResult::error(error),
        };
        crate::tool::ToolExecution::completed(result)
    }
}

fn view_options(context: Arc<ContextView>) -> Options {
    Options {
        context_strategy: ContextStrategy::ViewTree,
        context_view: context,
        compaction: CompactionSettings {
            keep_recent_tokens: 80,
            ..CompactionSettings::default()
        },
        ..options()
    }
}

async fn source_session() -> MemorySession {
    let session = MemorySession::new();
    session
        .append(Message {
            id: "old-user".into(),
            role: Role::User,
            blocks: vec![Block::text(format!(
                "exact-marker {}",
                "old evidence ".repeat(300)
            ))],
            ..Message::default()
        })
        .await
        .unwrap();
    session
        .append(Message {
            id: "old-assistant".into(),
            role: Role::Assistant,
            blocks: vec![Block::text("Earlier work is complete.")],
            usage: Some(Usage {
                input_tokens: 1_000_000,
                ..Usage::default()
            }),
            ..Message::default()
        })
        .await
        .unwrap();
    session
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn view_tree_projects_old_history_and_zoom_reads_exact_source_in_normal_turns() {
    let context = Arc::new(ContextView::default());
    let agent = Agent::new(
        ViewProvider::new(vec![
            Turn::named_tool_call("zoom", "zoom-old", r#"{"id":0,"n":1}"#),
            Turn::text("answer"),
        ]),
        ViewExecutor {
            context: context.clone(),
        },
        source_session().await,
        view_options(context),
    );
    agent
        .run(
            "Find exact-marker in earlier work",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = agent.provider.ordinary.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].messages[0].context_type, "view_tree");
    assert!(requests[0].system_prompt.contains("Summaries are lossy"));
    assert!(requests[0].remote_compaction.is_none());
    assert!(!requests[0].request_remote_compaction);
    assert!(
        requests[0]
            .messages
            .iter()
            .all(|message| message.usage.is_none())
    );
    assert!(requests[1].messages.iter().any(|message| {
        message.role == Role::Tool
            && message
                .blocks
                .iter()
                .any(|block| block.text.contains("exact-marker"))
    }));
    assert!(agent.session.messages()[0].text().contains("old evidence"));
    assert!(agent.session.latest_compaction().is_none());
    assert!(
        !agent
            .session
            .messages()
            .iter()
            .any(|message| message.context_type == "view_tree")
    );
    crate::session::context::pending_tool_calls(&requests[1].messages).unwrap();
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn cache_survives_next_turn_and_a_fresh_cache_rebuilds_from_history() {
    let context = Arc::new(ContextView::default());
    let agent = Agent::new(
        ViewProvider::new(vec![Turn::text("ok"), Turn::text("ok")]),
        ViewExecutor {
            context: context.clone(),
        },
        source_session().await,
        view_options(context),
    );
    agent
        .run("first", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    let calls = agent.provider.compressions.load(Ordering::SeqCst);
    assert!(calls > 0);
    agent
        .run("second", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(agent.provider.compressions.load(Ordering::SeqCst), calls);
    let context = Arc::new(ContextView::default());
    let mut resumed_options = view_options(context.clone());
    let counter = AtomicUsize::new(0);
    resumed_options.new_id =
        Box::new(move || format!("resumed-{}", counter.fetch_add(1, Ordering::SeqCst)));
    let resumed = Agent::new(
        ViewProvider::new(vec![Turn::text("resumed")]),
        ViewExecutor {
            context: context.clone(),
        },
        agent.session,
        resumed_options,
    );
    resumed
        .run("resume", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert!(resumed.provider.compressions.load(Ordering::SeqCst) > 0);
    assert_eq!(
        resumed.provider.ordinary.requests()[0].messages[0].context_type,
        "view_tree"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn invalid_summaries_stop_after_three_attempts_without_changing_history() {
    let mut provider = ViewProvider::new(vec![Turn::text("must not send")]);
    provider.summary = "x".repeat(513);
    let context = Arc::new(ContextView::default());
    let agent = Agent::new(
        provider,
        ViewExecutor {
            context: context.clone(),
        },
        source_session().await,
        view_options(context),
    );
    let error = agent
        .run("continue", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("three attempts"));
    assert_eq!(agent.provider.compressions.load(Ordering::SeqCst), 3);
    assert!(agent.provider.ordinary.requests().is_empty());
    assert!(agent.session.messages()[0].text().contains("exact-marker"));
    assert!(agent.session.latest_compaction().is_none());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn compaction_remains_default_and_does_not_build_a_tree() {
    let agent = Agent::new(
        ViewProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        source_session().await,
        options(),
    );
    agent
        .run("continue", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(agent.provider.compressions.load(Ordering::SeqCst), 0);
    assert!(
        !agent.provider.ordinary.requests()[0]
            .messages
            .iter()
            .any(|message| message.context_type == "view_tree")
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn view_overflow_retries_once_and_preserves_visible_output_rules() {
    for visible in [false, true] {
        let first = if visible {
            Turn::overflow().with_events(vec![StreamEvent::TextDelta {
                text: "partial".into(),
            }])
        } else {
            Turn::overflow()
        };
        let context = Arc::new(ContextView::default());
        let agent = Agent::new(
            ViewProvider::new(vec![first, Turn::overflow(), Turn::text("unused")]),
            ViewExecutor {
                context: context.clone(),
            },
            source_session().await,
            view_options(context),
        );
        assert!(
            agent
                .run("continue", &mut |_| {}, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(
            agent.provider.ordinary.requests().len(),
            if visible { 1 } else { 2 }
        );
        assert!(agent.session.latest_compaction().is_none());
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn compression_and_zoom_share_the_existing_redaction_boundary() {
    let context = Arc::new(ContextView::default());
    let agent = Agent::with_redactor(
        ViewProvider::new(vec![
            Turn::named_tool_call("zoom", "zoom-redacted", r#"{"id":0,"n":1}"#),
            Turn::text("ok"),
        ]),
        ViewExecutor {
            context: context.clone(),
        },
        source_session().await,
        view_options(context),
        Redactor::new(&["exact-marker".into()]),
    );
    agent
        .run("continue", &mut |_| {}, &CancellationToken::new())
        .await
        .unwrap();
    let summaries = agent.provider.compression_requests.lock().unwrap();
    assert!(!summaries.is_empty());
    for request in summaries
        .iter()
        .chain(agent.provider.ordinary.requests().iter())
    {
        assert!(
            !serde_json::to_string(&request.messages)
                .unwrap()
                .contains("exact-marker")
        );
    }
    assert!(
        agent
            .session
            .messages()
            .iter()
            .filter(|message| message.role == Role::Tool)
            .flat_map(|message| &message.blocks)
            .all(|block| !block.text.contains("exact-marker"))
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn canceled_view_turn_does_not_start_compression() {
    let context = Arc::new(ContextView::default());
    let agent = Agent::new(
        ViewProvider::new(vec![]),
        ViewExecutor {
            context: context.clone(),
        },
        source_session().await,
        view_options(context),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(agent.run("continue", &mut |_| {}, &cancel).await.is_err());
    assert_eq!(agent.provider.compressions.load(Ordering::SeqCst), 0);
    assert!(agent.provider.ordinary.requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn live_only_tool_output_stays_in_the_raw_tail_and_out_of_summaries() {
    let context = Arc::new(ContextView::default());
    let session = source_session().await;
    let mut call = Turn::named_tool_call("read", "output-call", "{}")
        .outcome
        .unwrap()
        .message;
    call.id = "before-output".into();
    session.append(call).await.unwrap();
    session
        .append(Message {
            id: "saved-output".into(),
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_name: "read".into(),
                tool_call_id: "output-call".into(),
                text: "saved placeholder".into(),
                ..Block::default()
            }],
            ..Message::default()
        })
        .await
        .unwrap();
    session
        .append(Message {
            id: "after-output".into(),
            role: Role::Assistant,
            blocks: vec![Block::text("recent ".repeat(200))],
            ..Message::default()
        })
        .await
        .unwrap();
    let mut options = view_options(context.clone());
    options.compaction.keep_recent_tokens = 1;
    let agent = Agent::new(
        ViewProvider::new(vec![Turn::text("ok")]),
        ViewExecutor { context },
        session,
        options,
    );
    let mut state = super::super::overflow::RunDispatchState::default();
    state
        .tool_result_overlay
        .insert("output-call".into(), "live-only-output".into());
    agent
        .dispatch_normal_provider_step(&mut |_| {}, &mut state, "", &CancellationToken::new())
        .await
        .unwrap();
    let requests = agent.provider.ordinary.requests();
    assert!(
        requests[0]
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .flat_map(|message| &message.blocks)
            .any(|block| block.text == "live-only-output")
    );
    assert!(
        !serde_json::to_string(&agent.session.messages())
            .unwrap()
            .contains("live-only-output")
    );
    for request in agent.provider.compression_requests.lock().unwrap().iter() {
        assert!(
            !serde_json::to_string(&request.messages)
                .unwrap()
                .contains("live-only-output")
        );
    }
}
