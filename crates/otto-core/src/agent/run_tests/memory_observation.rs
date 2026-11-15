//! Run-loop tests: turn observation by the memory binding.

use super::*;

struct ObservingMemory {
    observes: bool,
    error: Option<String>,
    observed: Mutex<Vec<(String, serde_json::Value)>>,
}

impl ObservingMemory {
    fn new(observes: bool, error: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            observes,
            error: error.map(str::to_owned),
            observed: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl MemoryRecall for ObservingMemory {
    async fn recall(
        &self,
        _: &RecallRequest,
        _: &CancellationToken,
    ) -> Result<RecallResult, MemoryError> {
        Ok(RecallResult::default())
    }

    fn observes_turns(&self) -> bool {
        self.observes
    }

    async fn observe_turn(
        &self,
        turn_id: &str,
        turn: &serde_json::Value,
        _: &CancellationToken,
    ) -> Result<(), MemoryError> {
        self.observed
            .lock()
            .unwrap()
            .push((turn_id.to_owned(), turn.clone()));
        self.error
            .clone()
            .map_or(Ok(()), |message| Err(MemoryError(message)))
    }
}

async fn run_tool_turn(memory: Arc<ObservingMemory>) -> (Result<(), AgentError>, Vec<Event>) {
    let mut opts = options();
    opts.memory = Some(memory);
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("ok"),
        ]),
        EchoExecutor::default(),
        MemorySession::new(),
        opts,
    );
    let mut events = Vec::new();
    let result = agent
        .run(
            "hello",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await;
    (result, events)
}

#[tokio::test]
async fn an_observing_binding_receives_the_turn_without_hooks() {
    let memory = ObservingMemory::new(true, None);
    let (result, _) = run_tool_turn(memory.clone()).await;
    result.expect("run");
    let observed = memory.observed.lock().unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].0, "op_1");
    let roles_and_types: Vec<_> = observed[0].1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| {
            message["blocks"].as_array().unwrap().iter().map(|block| {
                format!(
                    "{}:{}",
                    message["role"].as_str().unwrap(),
                    block["type"].as_str().unwrap()
                )
            })
        })
        .collect();
    assert_eq!(
        roles_and_types,
        [
            "user:text",
            "assistant:tool_call",
            "tool:tool_result",
            "assistant:text"
        ]
    );
    assert_eq!(observed[0].1["messages"][0]["blocks"][0]["text"], "hello");
}

#[tokio::test]
async fn a_binding_that_does_not_observe_receives_no_turn() {
    let memory = ObservingMemory::new(false, None);
    let (result, _) = run_tool_turn(memory.clone()).await;
    result.expect("run");
    assert!(memory.observed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_observation_is_a_warning_and_the_run_succeeds() {
    let memory = ObservingMemory::new(true, Some("observe down"));
    let (result, events) = run_tool_turn(memory.clone()).await;
    result.expect("run");
    assert_eq!(memory.observed.lock().unwrap().len(), 1);
    assert!(events.iter().any(
        |event| matches!(event, Event::MemoryWarning { message } if message == "observe down")
    ));
}
