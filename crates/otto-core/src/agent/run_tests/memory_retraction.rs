//! Run tests: retraction notices and per-step filtering of recalled records.

use super::*;
use crate::session::{
    CompactionCheckpoint, CompactionDetails, MEMORY_RECALL_CUSTOM_TYPE, MemoryRecordRef,
    encode_memory_recall,
};

const NOTICE_TAG: &str = "<memory_retraction";

fn record(id: &str, revision: &str, text: &str) -> Record {
    Record {
        id: id.into(),
        revision: Some(revision.into()),
        scope: Scope {
            namespace: "user".into(),
            id: "u1".into(),
        },
        kind: "fact".into(),
        key: id.into(),
        text: text.into(),
        standing: false,
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn unreferenced_recall_skips_lifecycle_and_keeps_content() {
    let memory = Arc::new(FakeMemory::failing_lifecycle(
        vec![record("", "", "UNREFERENCED-MARKER")],
        "empty identities cannot be inspected",
    ));
    let agent = agent_over(vec![Turn::text("done")], MemorySession::new(), &memory);
    let events = run_turn(&agent, "hello").await;
    assert_eq!(memory.lifecycle_call_count(), 0);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::MemoryWarning { .. }))
    );
    assert!(request_text(&agent, 0).contains("UNREFERENCED-MARKER"));
}

fn record_a() -> Record {
    record("rec-a", "3", "MARKER-A prefers vim")
}

fn record_b() -> Record {
    record("rec-b", "1", "MARKER-B prefers emacs")
}

type TestAgent = Agent<FakeProvider, EchoExecutor, MemorySession>;

fn agent_over(turns: Vec<Turn>, session: MemorySession, memory: &Arc<FakeMemory>) -> TestAgent {
    Agent::new(
        FakeProvider::new(turns),
        EchoExecutor::default(),
        session,
        Options {
            memory: Some(memory.clone()),
            ..options()
        },
    )
}

async fn run_turn(agent: &TestAgent, input: &str) -> Vec<Event> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            input,
            &mut |event| sink.lock().expect("events").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    collect(&events)
}

/// The text of every message of the `index`-th ordinary request.
fn request_text(agent: &TestAgent, index: usize) -> String {
    agent.provider().normal_requests()[index]
        .messages
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>()
        .join("\n---\n")
}

fn assert_no_notice_block(text: &str) {
    assert!(
        !text.contains(NOTICE_TAG),
        "unexpected notice block: {text}"
    );
    assert!(
        !text.contains("no longer valid evidence"),
        "unexpected preamble: {text}"
    );
}

/// Copies the messages and recall references of `source` into a new session,
/// as a reload from storage does.
async fn reloaded(source: &MemorySession) -> MemorySession {
    let session = MemorySession::new();
    let references = source.memory_references();
    for message in source.messages() {
        let id = message.id.clone();
        let is_user = message.role == Role::User;
        session.append(message).await.expect("append message");
        if let Some(turn) = references
            .iter()
            .find(|turn| is_user && turn.user_message_id == id)
        {
            session
                .append_custom(
                    MEMORY_RECALL_CUSTOM_TYPE,
                    &encode_memory_recall(&turn.records),
                )
                .await
                .expect("append recall record");
        }
    }
    session
}

// Fails at the notice assertion if selection ignores the superseded status,
// at the text assertions if the retired record is replayed or the new one is
// not recalled, at the last loop if a notice is persisted.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_record_superseded_between_turns_gets_a_notice_and_its_replacement_is_recalled() {
    let memory = Arc::new(FakeMemory::with_records(vec![record_a()]));
    let agent = agent_over(
        vec![Turn::text("first"), Turn::text("second")],
        MemorySession::new(),
        &memory,
    );
    run_turn(&agent, "one").await;
    assert_no_notice_block(&request_text(&agent, 0));

    memory.set_records(vec![record_b()]);
    memory.set_status_at(
        "rec-a",
        RecordStatus::Superseded {
            replacement_id: "rec-b".into(),
        },
        Some("4"),
    );
    run_turn(&agent, "two").await;

    let second = request_text(&agent, 1);
    assert!(
        second.contains(
            r#"<memory_retraction id="rec-a" revision="3" current_revision="4" reason="superseded" replacement_id="rec-b"/>"#
        ),
        "{second}"
    );
    assert!(
        second.contains("MARKER-B"),
        "the replacement was not recalled: {second}"
    );
    assert!(
        !second.contains("MARKER-A"),
        "the retired text was replayed: {second}"
    );
    for message in agent.session().messages() {
        assert!(
            !message.text().contains(NOTICE_TAG),
            "a notice was persisted"
        );
        assert!(
            !message.text().contains("MARKER-B"),
            "recalled text was persisted"
        );
    }
}

// Fails if a retired record that no turn or checkpoint references produces a
// notice.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_retired_record_that_was_never_recalled_produces_no_notice() {
    let memory = Arc::new(FakeMemory::with_records(vec![record_a()]));
    let agent = agent_over(
        vec![Turn::text("first"), Turn::text("second")],
        MemorySession::new(),
        &memory,
    );
    run_turn(&agent, "one").await;
    memory.set_status("rec-c", RecordStatus::Retracted);
    run_turn(&agent, "two").await;
    assert_no_notice_block(&request_text(&agent, 1));
}

// Fails if the persisted `otto.memory_recall` references are not read when a
// new agent starts over a stored session.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_resumed_session_gets_a_notice_from_its_persisted_references() {
    let first_memory = Arc::new(FakeMemory::with_records(vec![record_a()]));
    let first = agent_over(
        vec![Turn::text("first")],
        MemorySession::new(),
        &first_memory,
    );
    run_turn(&first, "one").await;
    let session = reloaded(first.session()).await;
    assert_eq!(
        session.memory_references(),
        first.session().memory_references()
    );

    let resumed_memory = Arc::new(FakeMemory::with_records(Vec::new()));
    resumed_memory.set_status_at("rec-a", RecordStatus::Retracted, Some("3"));
    let resumed = Agent::new(
        FakeProvider::new(vec![Turn::text("second")]),
        EchoExecutor::default(),
        session,
        Options {
            memory: Some(resumed_memory.clone()),
            new_id: Box::new({
                let counter = AtomicUsize::new(0);
                move || format!("resumed-{}", counter.fetch_add(1, Ordering::SeqCst))
            }),
            ..options()
        },
    );
    run_turn(&resumed, "two").await;

    let text = request_text(&resumed, 0);
    assert!(
        text.contains(r#"<memory_retraction id="rec-a" revision="3" current_revision="3" reason="retracted"/>"#),
        "{text}"
    );
    assert!(!text.contains("MARKER-A"), "{text}");
}

/// Forgets `rec-a` in the shared memory binding when the tool runs.
struct ForgettingExecutor {
    memory: Arc<FakeMemory>,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl ToolExecutor for ForgettingExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }

    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        self.memory.set_status("rec-a", RecordStatus::Forgotten);
        EchoExecutor::default().execute(call, control).await
    }
}

// Fails at the second-request assertions if the recalled set is not
// re-filtered before every provider step, at the call count if the filter
// re-runs retrieval.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_record_forgotten_inside_the_tool_loop_leaves_the_next_step() {
    let memory = Arc::new(FakeMemory::with_records(vec![record_a()]));
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("done"),
        ]),
        ForgettingExecutor {
            memory: memory.clone(),
        },
        MemorySession::new(),
        Options {
            memory: Some(memory.clone()),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");

    let requests = agent.provider().normal_requests();
    let text = |index: usize| {
        requests[index]
            .messages
            .iter()
            .map(Message::text)
            .collect::<Vec<_>>()
            .join("\n---\n")
    };
    assert!(text(0).contains("MARKER-A"), "{}", text(0));
    assert_no_notice_block(&text(0));
    let second = text(1);
    assert!(!second.contains("MARKER-A"), "{second}");
    assert!(!second.contains("<memory id=\"rec-a\""), "{second}");
    assert!(
        second.contains(r#"id="rec-a""#) && second.contains(r#"reason="forgotten""#),
        "{second}"
    );
    assert_eq!(memory.calls.load(Ordering::SeqCst), 1, "recall ran again");
}

// Fails if the latest checkpoint's `details.memory_records` is not part of
// the referenced set, since no visible user turn references rec-a.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_checkpoint_reference_gets_a_notice_when_no_visible_turn_names_the_record() {
    let session = MemorySession::new();
    for (index, text) in ["question 0", "question 1"].into_iter().enumerate() {
        for (role, id, body) in [
            (Role::User, format!("u{index}"), text),
            (Role::Assistant, format!("a{index}"), "ok"),
        ] {
            session
                .append(Message {
                    id,
                    role,
                    blocks: vec![Block::text(body)],
                    ..Message::default()
                })
                .await
                .expect("append");
        }
    }
    session
        .append_compaction(CompactionCheckpoint {
            summary: "summary of question 0".into(),
            first_kept_entry_id: "u1".into(),
            tokens_before: 10,
            usage: None,
            details: CompactionDetails {
                memory_records: vec![MemoryRecordRef {
                    id: "rec-a".into(),
                    revision: Some("3".into()),
                }],
                ..CompactionDetails::default()
            },
            created_at: clock(),
        })
        .await
        .expect("compaction");
    // The first message after a checkpoint must have a normal role, so the
    // capability update the run appends cannot come first.
    for (role, id) in [(Role::User, "u2"), (Role::Assistant, "a2")] {
        session
            .append(Message {
                id: id.into(),
                role,
                blocks: vec![Block::text("after the checkpoint")],
                ..Message::default()
            })
            .await
            .expect("append after the checkpoint");
    }
    assert!(
        session.memory_references().is_empty(),
        "no turn references a record"
    );

    let memory = Arc::new(FakeMemory::with_records(Vec::new()));
    memory.set_status("rec-a", RecordStatus::Retracted);
    let agent = agent_over(vec![Turn::text("ok")], session, &memory);
    run_turn(&agent, "next").await;

    let text = request_text(&agent, 0);
    assert!(
        text.contains(r#"<memory_retraction id="rec-a" revision="3" reason="retracted"/>"#),
        "{text}"
    );
}

// Fails if an unchanged revision produces a notice, or if a higher active
// revision produces none.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn an_updated_record_gets_a_notice_and_an_unchanged_record_does_not() {
    let memory = Arc::new(FakeMemory::with_records(vec![record_a()]));
    let agent = agent_over(
        vec![
            Turn::text("first"),
            Turn::text("second"),
            Turn::text("third"),
        ],
        MemorySession::new(),
        &memory,
    );
    run_turn(&agent, "one").await;

    memory.set_status_at("rec-a", RecordStatus::Active, Some("3"));
    run_turn(&agent, "two").await;
    assert_no_notice_block(&request_text(&agent, 1));

    memory.set_status_at("rec-a", RecordStatus::Active, Some("4"));
    run_turn(&agent, "three").await;
    let third = request_text(&agent, 2);
    assert!(
        third.contains(
            r#"<memory_retraction id="rec-a" revision="3" current_revision="4" reason="updated"/>"#
        ),
        "{third}"
    );
}

// Fails if a lifecycle error is swallowed without a warning, fails the turn,
// or removes the recalled text from the request.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failing_lifecycle_query_warns_and_keeps_the_recalled_record() {
    let memory = Arc::new(FakeMemory::failing_lifecycle(
        vec![record_a()],
        "lifecycle unavailable",
    ));
    let agent = agent_over(vec![Turn::text("ok")], MemorySession::new(), &memory);
    let events = run_turn(&agent, "hello").await;

    assert!(events.contains(&Event::MemoryWarning {
        message: "lifecycle unavailable".into()
    }));
    let text = request_text(&agent, 0);
    assert!(text.contains("MARKER-A"), "{text}");
    assert_no_notice_block(&text);
    assert!(memory.lifecycle_call_count() >= 1);
}

// Fails if the report text of the memory section omits the notice block the
// last request carried.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_context_report_memory_section_lists_the_notice() {
    let memory = Arc::new(FakeMemory::with_records(vec![record_a()]));
    let agent = agent_over(
        vec![Turn::text("first"), Turn::text("second")],
        MemorySession::new(),
        &memory,
    );
    run_turn(&agent, "one").await;
    memory.set_records(Vec::new());
    memory.set_status_at("rec-a", RecordStatus::Retracted, Some("3"));
    run_turn(&agent, "two").await;

    let report = agent.context_report();
    let section = report
        .sections
        .iter()
        .find(|section| section.kind == SectionKind::Memory)
        .expect("memory section");
    let text: String = section
        .items
        .iter()
        .map(|item| item.text.as_str())
        .collect();
    assert!(
        text.contains(r#"<memory_retraction id="rec-a" revision="3" current_revision="3" reason="retracted"/>"#),
        "{text}"
    );
}
