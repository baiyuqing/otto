//! Run-loop tests: session append failures and ordering of durable facts.

use super::*;

// -- durable boundaries ----------------------------------------------------

/// Fails the nth append and passes the rest through.
struct FailingSession {
    inner: MemorySession,
    fail_at: usize,
    appends: AtomicUsize,
}

impl FailingSession {
    fn new(fail_at: usize) -> Self {
        Self {
            inner: MemorySession::new(),
            fail_at,
            appends: AtomicUsize::new(0),
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Session for FailingSession {
    fn memory_references(&self) -> Vec<crate::session::TurnMemoryReferences> {
        self.inner.memory_references()
    }

    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }

    async fn append(&self, message: Message) -> Result<(), crate::session::SessionError> {
        if self.appends.fetch_add(1, Ordering::SeqCst) == self.fail_at {
            return Err(crate::session::SessionError::Persist("disk full".into()));
        }
        self.inner.append(message).await
    }

    fn latest_compaction(&self) -> Option<crate::session::CompactionMetadata> {
        self.inner.latest_compaction()
    }

    async fn append_compaction(
        &self,
        checkpoint: crate::session::CompactionCheckpoint,
    ) -> Result<crate::session::CompactionMetadata, crate::session::SessionError> {
        self.inner.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: crate::session::OperationFact,
    ) -> Result<(), crate::session::SessionError> {
        self.inner.append_operation_fact(fact).await
    }

    fn observed_capabilities(&self) -> Option<crate::session::Capabilities> {
        self.inner.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<crate::session::InterruptedTurn> {
        self.inner.interrupted_turn()
    }

    fn plan(&self) -> Vec<crate::session::PlanStep> {
        self.inner.plan()
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: crate::model::Message,
    ) -> Result<crate::session::AppendOutcome, crate::session::SessionError> {
        self.inner.append_input_if_absent(input_id, message).await
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.inner.has_input(input_id)
    }

    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), crate::session::SessionError> {
        self.inner.append_custom(custom_type, data).await
    }
}

struct OperationFailingSession {
    inner: MemorySession,
    fail_at: usize,
    fact_appends: AtomicUsize,
}

impl OperationFailingSession {
    fn new(fail_at: usize) -> Self {
        Self {
            inner: MemorySession::new(),
            fail_at,
            fact_appends: AtomicUsize::new(0),
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Session for OperationFailingSession {
    fn memory_references(&self) -> Vec<crate::session::TurnMemoryReferences> {
        self.inner.memory_references()
    }

    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }

    async fn append(&self, message: Message) -> Result<(), crate::session::SessionError> {
        self.inner.append(message).await
    }

    fn latest_compaction(&self) -> Option<crate::session::CompactionMetadata> {
        self.inner.latest_compaction()
    }

    async fn append_compaction(
        &self,
        checkpoint: crate::session::CompactionCheckpoint,
    ) -> Result<crate::session::CompactionMetadata, crate::session::SessionError> {
        self.inner.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: crate::session::OperationFact,
    ) -> Result<(), crate::session::SessionError> {
        if self.fact_appends.fetch_add(1, Ordering::SeqCst) == self.fail_at {
            return Err(crate::session::SessionError::Persist(
                "operation disk full".into(),
            ));
        }
        self.inner.append_operation_fact(fact).await
    }

    fn operation_history(&self) -> Vec<crate::session::OperationFact> {
        self.inner.operation_history()
    }

    fn operation_ledger(&self) -> crate::session::OperationLedger {
        self.inner.operation_ledger()
    }

    fn observed_capabilities(&self) -> Option<crate::session::Capabilities> {
        self.inner.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<crate::session::InterruptedTurn> {
        self.inner.interrupted_turn()
    }

    fn plan(&self) -> Vec<crate::session::PlanStep> {
        self.inner.plan()
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: crate::model::Message,
    ) -> Result<crate::session::AppendOutcome, crate::session::SessionError> {
        self.inner.append_input_if_absent(input_id, message).await
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.inner.has_input(input_id)
    }

    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), crate::session::SessionError> {
        self.inner.append_custom(custom_type, data).await
    }
}

struct CountingExecutor {
    calls: Arc<AtomicUsize>,
}

struct AsyncOrderingSession {
    inner: MemorySession,
    attempt_persisted: Arc<AtomicBool>,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Session for AsyncOrderingSession {
    fn memory_references(&self) -> Vec<crate::session::TurnMemoryReferences> {
        self.inner.memory_references()
    }

    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }
    async fn append(&self, message: Message) -> Result<(), crate::session::SessionError> {
        self.inner.append(message).await
    }
    fn latest_compaction(&self) -> Option<crate::session::CompactionMetadata> {
        self.inner.latest_compaction()
    }
    async fn append_compaction(
        &self,
        checkpoint: crate::session::CompactionCheckpoint,
    ) -> Result<crate::session::CompactionMetadata, crate::session::SessionError> {
        self.inner.append_compaction(checkpoint).await
    }
    async fn append_operation_fact(
        &self,
        fact: crate::session::OperationFact,
    ) -> Result<(), crate::session::SessionError> {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
        self.inner.append_operation_fact(fact).await?;
        self.attempt_persisted.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn operation_history(&self) -> Vec<crate::session::OperationFact> {
        self.inner.operation_history()
    }
    fn operation_ledger(&self) -> crate::session::OperationLedger {
        self.inner.operation_ledger()
    }
    fn observed_capabilities(&self) -> Option<crate::session::Capabilities> {
        self.inner.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<crate::session::InterruptedTurn> {
        self.inner.interrupted_turn()
    }

    fn plan(&self) -> Vec<crate::session::PlanStep> {
        self.inner.plan()
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: crate::model::Message,
    ) -> Result<crate::session::AppendOutcome, crate::session::SessionError> {
        self.inner.append_input_if_absent(input_id, message).await
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.inner.has_input(input_id)
    }

    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), crate::session::SessionError> {
        self.inner.append_custom(custom_type, data).await
    }
}

struct DispatchAfterFactExecutor(Arc<AtomicBool>);

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for DispatchAfterFactExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }
    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        _control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        assert!(
            self.0.load(Ordering::SeqCst),
            "tool ran before its attempt fact was durable"
        );
        crate::tool::ToolExecution::completed(ToolResult {
            content: call.arguments.get().to_owned(),
            ..ToolResult::default()
        })
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for CountingExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }

    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        _control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        self.calls.fetch_add(1, Ordering::SeqCst);
        crate::tool::ToolExecution::completed(ToolResult {
            content: call.arguments.get().to_owned(),
            ..ToolResult::default()
        })
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_attempt_fact_stops_before_tool_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::tool_call("c1", r#"{"value":1}"#)]),
        CountingExecutor {
            calls: Arc::clone(&calls),
        },
        OperationFailingSession::new(0),
        options(),
    );

    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("attempt fact failure stops the run");
    assert!(error.to_string().contains("operation attempt"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(agent.session().operation_history().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn async_operation_fact_is_durable_before_tool_dispatch() {
    let attempt_persisted = Arc::new(AtomicBool::new(false));
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        DispatchAfterFactExecutor(Arc::clone(&attempt_persisted)),
        AsyncOrderingSession {
            inner: MemorySession::new(),
            attempt_persisted: Arc::clone(&attempt_persisted),
        },
        options(),
    );

    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert!(attempt_persisted.load(Ordering::SeqCst));
    assert_eq!(agent.session().operation_history().len(), 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_terminal_fact_stops_after_dispatch_without_a_tool_result() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::tool_call("c1", r#"{"value":1}"#)]),
        CountingExecutor {
            calls: Arc::clone(&calls),
        },
        OperationFailingSession::new(1),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);

    let error = agent
        .run(
            "hello",
            &mut |event| captured.lock().expect("events").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("terminal fact failure stops the run");
    assert!(error.to_string().contains("operation terminal"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(agent.session().operation_history().len(), 1);
    let finished: Vec<_> = collect(&events)
        .into_iter()
        .filter_map(|event| match event {
            Event::ToolCallFinished {
                outcome, result, ..
            } => Some((outcome, result)),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 1);
    assert!(finished[0].1.is_error);
    assert!(finished[0].1.content.contains("outcome unknown"));
    assert_eq!(
        finished[0].0,
        crate::model::OperationOutcome {
            disposition: crate::model::OperationDisposition::Error,
            effect_certainty: crate::model::EffectCertainty::Unknown,
            stop_reason: None,
        }
    );
    assert!(
        !agent
            .session()
            .messages()
            .iter()
            .any(|message| message.role == Role::Tool)
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_user_message_append_stops_before_the_provider() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("never sent")]),
        EchoExecutor::default(),
        FailingSession::new(0),
        options(),
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the append failure stops the run");
    assert_eq!(error.to_string(), "persist user message: disk full");
    assert!(agent.provider().requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_notification_append_leaves_it_and_later_ones_queued() {
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        task_id: "t1".into(),
        kind: Some(NotificationKind::TaskFinished),
        text: "first".into(),
        usage: None,
        ..Default::default()
    });
    inbox.push(Notification {
        task_id: "t2".into(),
        kind: Some(NotificationKind::TaskFinished),
        text: "second".into(),
        usage: None,
        ..Default::default()
    });
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("never sent")]),
        EchoExecutor::default(),
        FailingSession::new(0),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    let error = agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the append failure stops the run");
    assert_eq!(error.to_string(), "persist notification: disk full");
    assert_eq!(
        inbox.len(),
        2,
        "the failed item and the one after it must stay queued"
    );
    assert!(agent.provider().requests().is_empty());

    // A later delivery over the same inbox, with a session that now accepts
    // appends, delivers each notification exactly once.
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let texts: Vec<String> = agent
        .session()
        .messages()
        .iter()
        .filter(|message| message.role == Role::Context)
        .map(|message| message.text())
        .collect();
    assert_eq!(texts, vec!["first".to_string(), "second".to_string()]);
    assert!(inbox.is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_assistant_message_is_persisted_before_the_tools_run() {
    // The second append is the assistant message that asks for the tool call.
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        EchoExecutor::default(),
        FailingSession::new(1),
        options(),
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the append failure stops the run");
    assert!(error.to_string().ends_with("disk full"), "{error}");
    assert!(
        !agent
            .session()
            .messages()
            .iter()
            .any(|message| message.role == Role::Tool),
        "a tool ran before its assistant message was durable"
    );
    assert_eq!(agent.provider().requests().len(), 1);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failed_tool_result_append_stops_before_the_next_provider_call() {
    // Appends: user, assistant, tool result.
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        EchoExecutor::default(),
        FailingSession::new(2),
        options(),
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the append failure stops the run");
    assert!(error.to_string().ends_with("disk full"), "{error}");
    assert_eq!(agent.provider().requests().len(), 1);
}
