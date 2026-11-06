//! Run tests: compaction summaries affected by retired memory records are
//! marked in the request and rebuilt by a compaction.

use super::*;
use crate::agent::memory_retraction::{SUMMARY_INSTRUCTIONS_HEADING, SUMMARY_INSTRUCTIONS_RULE};
use crate::session::{
    CompactionCheckpoint, CompactionDetails, MEMORY_RECALL_CUSTOM_TYPE, MemoryRecordRef,
    MemoryReferenceCoverage, encode_memory_recall,
};

const SUMMARY_LINE_TAG: &str = "<memory_summary_affected";

type TestAgent = Agent<FakeProvider, EchoExecutor, MemorySession>;

fn rec_a_ref() -> MemoryRecordRef {
    MemoryRecordRef {
        id: "rec-a".into(),
        revision: Some("3".into()),
    }
}

fn record_a() -> Record {
    Record {
        id: "rec-a".into(),
        revision: Some("3".into()),
        scope: Scope {
            namespace: "user".into(),
            id: "u1".into(),
        },
        kind: "fact".into(),
        key: "rec-a".into(),
        text: "MARKER-A prefers vim".into(),
        standing: false,
    }
}

/// Keeps only the last turn verbatim, so every earlier turn is summarized.
fn summarizing_options(memory: &Arc<FakeMemory>) -> Options {
    Options {
        memory: Some(memory.clone()),
        compaction: CompactionSettings {
            keep_recent_tokens: 0,
            ..CompactionSettings::default()
        },
        ..options()
    }
}

fn agent_over(turns: Vec<Turn>, session: MemorySession, memory: &Arc<FakeMemory>) -> TestAgent {
    Agent::new(
        FakeProvider::new(turns),
        EchoExecutor::default(),
        session,
        summarizing_options(memory),
    )
}

/// Appends a user message, its recall record when `records` is `Some`, and an
/// assistant reply, in the order the agent writes them.
async fn turn(session: &MemorySession, index: usize, records: Option<&[MemoryRecordRef]>) {
    session
        .append(Message {
            id: format!("u{index}"),
            role: Role::User,
            blocks: vec![Block::text(format!("question {index}"))],
            ..Message::default()
        })
        .await
        .expect("append user");
    if let Some(records) = records {
        session
            .append_custom(MEMORY_RECALL_CUSTOM_TYPE, &encode_memory_recall(records))
            .await
            .expect("append recall record");
    }
    session
        .append(Message {
            id: format!("a{index}"),
            role: Role::Assistant,
            blocks: vec![Block::text("ok")],
            ..Message::default()
        })
        .await
        .expect("append assistant");
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

async fn compact_events(agent: &TestAgent) -> Vec<Event> {
    let mut events = Vec::new();
    agent
        .compact(
            "",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("compaction");
    events
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

fn memory_warnings(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::MemoryWarning { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn affected_line(checkpoint_id: &str) -> String {
    format!(
        r#"<memory_summary_affected checkpoint="{checkpoint_id}" record="rec-a" reason="retracted"/>"#
    )
}

/// An agent whose session was compacted once while `rec-a` was active. The
/// checkpoint names `rec-a@3`; the retained last turn names it too. Provider
/// script after the summary: `follow_up`.
async fn compacted_agent(follow_up: Vec<Turn>) -> (TestAgent, Arc<FakeMemory>, String) {
    let memory = Arc::new(FakeMemory::with_records(vec![record_a()]));
    let session = MemorySession::new();
    turn(&session, 0, Some(&[rec_a_ref()])).await;
    turn(&session, 1, Some(&[])).await;
    turn(&session, 2, Some(&[])).await;
    let mut turns = vec![Turn::summary(&structured_summary())];
    turns.extend(follow_up);
    let agent = agent_over(turns, session, &memory);
    let events = compact_events(&agent).await;
    assert!(memory_warnings(&events).is_empty());
    let checkpoint = agent.session().latest_compaction().expect("checkpoint");
    assert_eq!(checkpoint.details.memory_records, vec![rec_a_ref()]);
    assert!(checkpoint.details.memory_retracted.is_empty());
    // The first message after a checkpoint must have a normal role, so the
    // capability update a run appends cannot come first.
    turn(agent.session(), 3, Some(&[])).await;
    (agent, memory, checkpoint.id)
}

// Fails at the line assertion if the checkpoint's references are not
// intersected with the lifecycle, at the warning count if the warning repeats
// per provider step (two steps here) or is missing.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_summary_that_used_a_retired_record_is_marked_and_warned_once() {
    let (agent, memory, id) = compacted_agent(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("done"),
    ])
    .await;
    memory.set_records(Vec::new());
    memory.set_status_at("rec-a", RecordStatus::Retracted, Some("3"));
    let events = run_turn(&agent, "next").await;

    for step in 0..2 {
        let text = request_text(&agent, step);
        assert!(text.contains(&affected_line(&id)), "step {step}: {text}");
    }
    let warnings = memory_warnings(&events);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    for part in [id.as_str(), "rec-a", "/compact"] {
        assert!(warnings[0].contains(part), "{part}: {}", warnings[0]);
    }
}

// Fails at the instruction assertions if the retired record does not reach the
// summary system prompt, at the details assertions if the union is not stored,
// at the last assertion if the rebuilt summary is still reported as affected.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_compaction_rebuilds_the_summary_with_the_retraction_and_clears_the_marker() {
    let (agent, memory, first_id) = compacted_agent(vec![
        Turn::text("t1"),
        Turn::summary(&structured_summary()),
        Turn::text("t2"),
    ])
    .await;
    memory.set_records(Vec::new());
    memory.set_status_at("rec-a", RecordStatus::Retracted, Some("3"));
    run_turn(&agent, "next").await;
    assert!(request_text(&agent, 0).contains(&affected_line(&first_id)));

    compact_events(&agent).await;
    let summary_requests = agent.provider().summary_requests();
    assert_eq!(summary_requests.len(), 2);
    let prompt = &summary_requests[1].system_prompt;
    assert!(prompt.contains(SUMMARY_INSTRUCTIONS_HEADING), "{prompt}");
    let line = prompt
        .lines()
        .find(|line| line.starts_with(r#"<memory_retraction id="rec-a""#))
        .unwrap_or_else(|| panic!("no retraction line: {prompt}"));
    assert!(line.contains(r#"reason="retracted""#), "{line}");
    assert!(prompt.contains(SUMMARY_INSTRUCTIONS_RULE), "{prompt}");

    let rebuilt = agent.session().latest_compaction().expect("checkpoint");
    assert_ne!(rebuilt.id, first_id);
    assert_eq!(rebuilt.details.memory_retracted, vec![rec_a_ref()]);
    assert!(rebuilt.details.memory_records.contains(&rec_a_ref()));

    run_turn(&agent, "after").await;
    let text = request_text(&agent, 1);
    assert!(!text.contains(SUMMARY_LINE_TAG), "{text}");
}

// Fails if a compaction without retired references adds the retraction block
// to the summary request or records a retracted reference.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_compaction_without_retired_references_adds_no_instructions() {
    let (agent, _memory, _id) = compacted_agent(Vec::new()).await;
    let summary_requests = agent.provider().summary_requests();
    assert_eq!(summary_requests.len(), 1);
    let prompt = &summary_requests[0].system_prompt;
    assert!(!prompt.contains(SUMMARY_INSTRUCTIONS_HEADING), "{prompt}");
    assert!(!prompt.contains("<memory_retraction"), "{prompt}");
    let checkpoint = agent.session().latest_compaction().expect("checkpoint");
    assert!(checkpoint.details.memory_retracted.is_empty());
}

// Fails at the first assertion if a checkpoint with unknown coverage gets no
// line when a visible turn names a retired record, at the second if the
// warning does not say the coverage is unknown.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_summary_with_unknown_coverage_is_marked_when_a_visible_turn_names_a_retired_record() {
    let memory = Arc::new(FakeMemory::with_records(vec![record_a()]));
    let agent = agent_over(
        vec![
            Turn::summary(&structured_summary()),
            Turn::text("first"),
            Turn::text("second"),
        ],
        seeded_session().await,
        &memory,
    );
    compact_events(&agent).await;
    let checkpoint = agent.session().latest_compaction().expect("checkpoint");
    assert_eq!(
        checkpoint.details.memory_coverage,
        MemoryReferenceCoverage::Unknown
    );
    turn(agent.session(), 3, Some(&[])).await;
    run_turn(&agent, "one").await;
    assert!(!request_text(&agent, 0).contains(SUMMARY_LINE_TAG));

    memory.set_records(Vec::new());
    memory.set_status_at("rec-a", RecordStatus::Retracted, Some("3"));
    let events = run_turn(&agent, "two").await;

    let text = request_text(&agent, 1);
    let expected = format!(
        r#"<memory_summary_affected checkpoint="{}" coverage="unknown"/>"#,
        checkpoint.id
    );
    assert!(text.contains(&expected), "{text}");
    let warnings = memory_warnings(&events);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("unknown memory coverage"),
        "{}",
        warnings[0]
    );
}

// Fails if a provider-managed compaction in force is not reported as an
// untraceable summary when a visible turn names a retired record.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_remote_compaction_in_force_is_marked_with_unknown_coverage() {
    let memory = Arc::new(FakeMemory::with_records(vec![record_a()]));
    let provider = FakeProvider::new(vec![
        Turn::remote_compaction("compact-1", "opaque"),
        Turn::text("first"),
        Turn::text("second"),
    ])
    .with_remote_compaction();
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        seeded_session_with_echo_capabilities().await,
        summarizing_options(&memory),
    );
    compact_events(&agent).await;
    assert!(agent.session().remote_compaction().is_some());
    run_turn(&agent, "one").await;

    memory.set_records(Vec::new());
    memory.set_status_at("rec-a", RecordStatus::Retracted, Some("3"));
    run_turn(&agent, "two").await;

    let requests = agent.provider().requests();
    let last = requests.last().expect("request");
    let text: String = last.messages.iter().map(Message::text).collect();
    assert!(
        text.contains(r#"<memory_summary_affected checkpoint="remote" coverage="unknown"/>"#),
        "{text}"
    );
}

// Fails if a lifecycle error during compaction is not reported as a memory
// warning, aborts the compaction, adds the retraction block to the summary
// request, or drops the previous checkpoint's retracted references.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_failing_lifecycle_during_compaction_warns_and_keeps_the_previous_retractions() {
    let memory = Arc::new(FakeMemory::failing_lifecycle(
        vec![record_a()],
        "lifecycle unavailable",
    ));
    let session = MemorySession::new();
    turn(&session, 0, Some(&[rec_a_ref()])).await;
    turn(&session, 1, Some(&[])).await;
    let earlier = MemoryRecordRef {
        id: "rec-z".into(),
        revision: Some("1".into()),
    };
    session
        .append_compaction(CompactionCheckpoint {
            summary: "summary of question 0".into(),
            first_kept_entry_id: "u1".into(),
            tokens_before: 10,
            usage: None,
            details: CompactionDetails {
                memory_records: vec![rec_a_ref(), earlier.clone()],
                memory_retracted: vec![earlier.clone()],
                memory_coverage: MemoryReferenceCoverage::Complete,
                ..CompactionDetails::default()
            },
            created_at: clock(),
        })
        .await
        .expect("compaction");
    turn(&session, 2, Some(&[])).await;
    turn(&session, 3, Some(&[])).await;
    let agent = agent_over(vec![Turn::summary(&structured_summary())], session, &memory);
    let events = compact_events(&agent).await;

    assert!(
        memory_warnings(&events).contains(&"lifecycle unavailable".to_owned()),
        "{events:?}"
    );
    assert!(memory.lifecycle_call_count() >= 1);
    let summary_requests = agent.provider().summary_requests();
    assert_eq!(summary_requests.len(), 1);
    assert!(
        !summary_requests[0]
            .system_prompt
            .contains(SUMMARY_INSTRUCTIONS_HEADING)
    );
    let rebuilt = agent.session().latest_compaction().expect("checkpoint");
    assert_eq!(rebuilt.details.memory_retracted, vec![earlier]);
}

// Fails if an active record produces a summary line or a memory warning.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn no_retired_record_gives_no_summary_line_and_no_warning() {
    let (agent, _memory, _id) = compacted_agent(vec![Turn::text("ok")]).await;
    let events = run_turn(&agent, "next").await;
    let text = request_text(&agent, 0);
    assert!(!text.contains(SUMMARY_LINE_TAG), "{text}");
    assert!(memory_warnings(&events).is_empty(), "{events:?}");
}
