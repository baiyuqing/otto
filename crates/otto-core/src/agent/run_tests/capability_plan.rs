//! Run-loop tests: capability notices and plan updates.

use super::*;

// -- capability notice -----------------------------------------------------

const NOTICE_OPEN: &str = "<runtime_capability_update>";

fn recorded_tools(session: &MemorySession) -> Vec<String> {
    session.observed_capabilities().expect("record").tools
}

async fn session_with_record(tools: &str) -> MemorySession {
    let session = MemorySession::new();
    session
        .append_custom(
            crate::session::CAPABILITIES_CUSTOM_TYPE,
            &format!(r#"{{"tools":{tools}}}"#),
        )
        .await
        .expect("seed record");
    session
}

fn notices(messages: &[Message]) -> Vec<&Message> {
    messages
        .iter()
        .filter(|message| message.context_type == crate::session::CAPABILITY_UPDATE_CONTEXT_TYPE)
        .collect()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_new_session_gets_a_record_and_no_notice() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run("hi", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert!(notices(&agent.session().messages()).is_empty());
    assert_eq!(recorded_tools(agent.session()), vec!["echo".to_owned()]);
    assert_eq!(
        agent.provider().normal_requests()[0].system_prompt,
        "be brief"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_differing_record_appends_one_notice_before_the_user_message() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one"), Turn::text("two")]),
        EchoExecutor::default(),
        session_with_record(r#"["web_search"]"#).await,
        options(),
    );
    agent
        .run("hi", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let messages = agent.session().messages();
    assert_eq!(messages[0].role, Role::Context);
    assert!(messages[0].display);
    assert_eq!(messages[1].role, Role::User);
    let notice = messages[0].text();
    assert!(notice.starts_with(NOTICE_OPEN));
    assert!(notice.contains("\nAdded: echo\nRemoved: web_search\n"));
    assert_eq!(recorded_tools(agent.session()), vec!["echo".to_owned()]);

    agent
        .run("again", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(notices(&agent.session().messages()).len(), 1);
    let requests = agent.provider().normal_requests();
    for request in &requests {
        assert_eq!(request.system_prompt, "be brief");
        assert_eq!(request.messages[0].text(), notice);
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_changed_definition_digest_appends_one_notice() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one"), Turn::text("two")]),
        EchoExecutor::default(),
        session_with_record(r#"["echo"],"definitions":{"echo":"0000000000000000"}"#).await,
        options(),
    );
    for prompt in ["hi", "again"] {
        agent
            .run(prompt, &mut |_| {}, &CancellationToken::new())
            .await
            .expect("run");
    }
    let messages = agent.session().messages();
    let found = notices(&messages);
    assert_eq!(found.len(), 1);
    assert!(found[0].text().contains("\nChanged definitions: echo\n"));
    let recorded = agent.session().observed_capabilities().expect("record");
    assert_ne!(
        recorded.definitions.expect("digests")["echo"],
        "0000000000000000"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn history_without_a_record_gets_the_legacy_notice_once() {
    let session = MemorySession::new();
    for (role, text) in [(Role::User, "old"), (Role::Assistant, "old reply")] {
        session
            .append(Message {
                role,
                blocks: vec![Block::text(text)],
                ..Message::default()
            })
            .await
            .expect("append");
    }
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one"), Turn::text("two")]),
        EchoExecutor::default(),
        session,
        options(),
    );
    for prompt in ["hi", "again"] {
        agent
            .run(prompt, &mut |_| {}, &CancellationToken::new())
            .await
            .expect("run");
    }
    let messages = agent.session().messages();
    let found = notices(&messages);
    assert_eq!(found.len(), 1);
    assert!(found[0].text().contains("was not recorded"));
    assert!(found[0].text().contains("Current tools: echo\n"));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_matching_record_appends_nothing() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one")]),
        EchoExecutor::default(),
        session_with_record(r#"["echo"]"#).await,
        options(),
    );
    agent
        .run("hi", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(agent.session().messages().len(), 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_summarized_notice_is_carried_in_the_capability_updates_block() {
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

    // A matching record, so the run between the compactions adds no notice.
    let session = session_with_record(r#"["echo"]"#).await;
    session
        .append(crate::session::context::new_context_message(
            "n0",
            crate::session::CAPABILITY_UPDATE_CONTEXT_TYPE,
            true,
            "<runtime_capability_update>\nAdded: agent\n</runtime_capability_update>".into(),
            clock(),
            None,
        ))
        .await
        .expect("notice");
    exchange(&session, 0, "first").await;
    exchange(&session, 1, "second").await;
    exchange(&session, 2, "third").await;
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::summary(&structured_summary()),
            Turn::text("ok"),
            Turn::summary(&structured_summary()),
        ]),
        EchoExecutor::default(),
        session,
        options(),
    );
    let block = "<capability-updates>\n\"\\u003cruntime_capability_update\\u003e\\nAdded: agent\\n\\u003c/runtime_capability_update\\u003e\"\n</capability-updates>";
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("first compaction");
    agent
        .run("next", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run after compaction");
    let next_request_text: String = agent.provider().normal_requests()[0]
        .messages
        .iter()
        .map(Message::text)
        .collect();
    assert!(next_request_text.contains(block), "{next_request_text}");

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
    assert_eq!(summary.matches(block).count(), 1, "{summary}");
    let second_request = agent.provider().summary_requests()[1].messages[0].text();
    assert!(
        !second_request.contains("capability-updates"),
        "the carried block reached the summarizer: {second_request}"
    );
}

// -- plan ------------------------------------------------------------------

/// A session one `Arc` shares between the agent and a tool.
struct SharedMemory(Arc<MemorySession>);

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl Session for SharedMemory {
    fn memory_references(&self) -> Vec<crate::session::TurnMemoryReferences> {
        self.0.memory_references()
    }

    fn messages(&self) -> Vec<Message> {
        self.0.messages()
    }
    async fn append(&self, message: Message) -> Result<(), crate::session::SessionError> {
        self.0.append(message).await
    }
    fn latest_compaction(&self) -> Option<crate::session::CompactionMetadata> {
        self.0.latest_compaction()
    }
    async fn append_compaction(
        &self,
        checkpoint: crate::session::CompactionCheckpoint,
    ) -> Result<crate::session::CompactionMetadata, crate::session::SessionError> {
        self.0.append_compaction(checkpoint).await
    }
    async fn append_operation_fact(
        &self,
        fact: crate::session::OperationFact,
    ) -> Result<(), crate::session::SessionError> {
        self.0.append_operation_fact(fact).await
    }
    fn operation_history(&self) -> Vec<crate::session::OperationFact> {
        self.0.operation_history()
    }
    fn operation_ledger(&self) -> crate::session::OperationLedger {
        self.0.operation_ledger()
    }
    fn observed_capabilities(&self) -> Option<crate::session::Capabilities> {
        self.0.observed_capabilities()
    }
    fn interrupted_turn(&self) -> Option<crate::session::InterruptedTurn> {
        self.0.interrupted_turn()
    }
    fn plan(&self) -> Vec<crate::session::PlanStep> {
        self.0.plan()
    }
    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: Message,
    ) -> Result<crate::session::AppendOutcome, crate::session::SessionError> {
        self.0.append_input_if_absent(input_id, message).await
    }
    fn has_input(&self, input_id: &str) -> bool {
        self.0.has_input(input_id)
    }
    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), crate::session::SessionError> {
        self.0.append_custom(custom_type, data).await
    }
}

/// An `echo` tool that writes its `steps` argument as the session plan.
struct PlanWriter(Arc<MemorySession>);

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for PlanWriter {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }

    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        _control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        #[derive(serde::Deserialize)]
        struct Args {
            steps: Vec<crate::session::PlanStep>,
        }
        let steps = serde_json::from_str::<Args>(call.arguments.get())
            .expect("steps")
            .steps;
        self.0
            .append_custom(
                crate::session::PLAN_CUSTOM_TYPE,
                &crate::session::encode_plan(&steps),
            )
            .await
            .expect("plan");
        crate::tool::ToolExecution::completed(ToolResult::default())
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_tool_call_that_changes_the_plan_emits_plan_updated_once() {
    let memory = Arc::new(MemorySession::new());
    let steps = r#"{"steps":[{"text":"ship","status":"in_progress"}]}"#;
    let provider = FakeProvider::new(vec![
        Turn::tool_call("call-1", steps),
        // The same plan again changes nothing and emits nothing.
        Turn::tool_call("call-2", steps),
        Turn::text("done"),
    ]);
    let agent = Agent::new(
        provider,
        PlanWriter(Arc::clone(&memory)),
        SharedMemory(Arc::clone(&memory)),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    agent
        .run(
            "plan it",
            &mut |event| sink.lock().unwrap().push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    let updates: Vec<_> = collect(&events)
        .into_iter()
        .filter_map(|event| match event {
            Event::PlanUpdated { steps } => Some(steps),
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0][0].text, "ship");
    let names = names(&collect(&events));
    let finished = names
        .iter()
        .position(|name| *name == "tool_call_finished")
        .unwrap();
    assert_eq!(names[finished + 1], "plan_updated");
}
