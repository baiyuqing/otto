//! Compaction tests: a checkpoint carries the memory record references of the
//! turns it summarizes.

use super::*;
use crate::session::encode_memory_recall;
use crate::session::{MEMORY_RECALL_CUSTOM_TYPE, MemoryRecordRef, MemoryReferenceCoverage};

fn record(id: &str, revision: &str) -> MemoryRecordRef {
    MemoryRecordRef {
        id: id.into(),
        revision: Some(revision.into()),
    }
}

/// Appends the user message, its recall record (when `records` is `Some`) and
/// an assistant reply, in the order the agent writes them.
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

/// Keeps only the last turn verbatim, so every earlier turn is summarized.
fn summarize_all_but_last_turn() -> Options {
    Options {
        compaction: CompactionSettings {
            keep_recent_tokens: 0,
            ..CompactionSettings::default()
        },
        ..options()
    }
}

async fn compact_once(agent: &Agent<FakeProvider, EchoExecutor, MemorySession>) {
    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("compaction");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_checkpoint_carries_the_references_of_the_summarized_turns_only() {
    let session = MemorySession::new();
    turn(&session, 0, Some(&[record("a", "1")])).await;
    turn(&session, 1, Some(&[record("b", "2")])).await;
    turn(&session, 2, Some(&[record("retained", "9")])).await;
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary(&structured_summary())]),
        EchoExecutor::default(),
        session,
        summarize_all_but_last_turn(),
    );
    compact_once(&agent).await;

    let details = agent
        .session()
        .latest_compaction()
        .expect("checkpoint")
        .details;
    assert_eq!(
        details.memory_records,
        vec![record("a", "1"), record("b", "2")]
    );
    assert_eq!(details.memory_coverage, MemoryReferenceCoverage::Complete);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn turns_without_reference_records_give_unknown_coverage() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary(&structured_summary())]),
        EchoExecutor::default(),
        seeded_session().await,
        summarize_all_but_last_turn(),
    );
    compact_once(&agent).await;

    let details = agent
        .session()
        .latest_compaction()
        .expect("checkpoint")
        .details;
    assert!(details.memory_records.is_empty());
    assert_eq!(details.memory_coverage, MemoryReferenceCoverage::Unknown);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn one_summarized_turn_without_a_record_keeps_the_others_and_gives_unknown() {
    let session = MemorySession::new();
    turn(&session, 0, None).await;
    turn(&session, 1, Some(&[record("b", "2")])).await;
    turn(&session, 2, Some(&[record("retained", "9")])).await;
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary(&structured_summary())]),
        EchoExecutor::default(),
        session,
        summarize_all_but_last_turn(),
    );
    compact_once(&agent).await;

    let details = agent
        .session()
        .latest_compaction()
        .expect("checkpoint")
        .details;
    assert_eq!(details.memory_records, vec![record("b", "2")]);
    assert_eq!(details.memory_coverage, MemoryReferenceCoverage::Unknown);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_later_checkpoint_carries_the_previous_references_forward() {
    let session = MemorySession::new();
    turn(&session, 0, Some(&[record("a", "1")])).await;
    turn(&session, 1, Some(&[record("retained", "9")])).await;
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::summary(&structured_summary()),
            Turn::summary(&structured_summary()),
        ]),
        EchoExecutor::default(),
        session,
        summarize_all_but_last_turn(),
    );
    compact_once(&agent).await;
    let first = agent
        .session()
        .latest_compaction()
        .expect("first checkpoint")
        .details;
    assert_eq!(first.memory_records, vec![record("a", "1")]);

    turn(agent.session(), 2, Some(&[record("c", "3")])).await;
    turn(agent.session(), 3, Some(&[record("retained", "9")])).await;
    compact_once(&agent).await;

    let second = agent
        .session()
        .latest_compaction()
        .expect("second checkpoint")
        .details;
    assert_eq!(
        second.memory_records,
        vec![record("a", "1"), record("c", "3"), record("retained", "9")]
    );
    assert_eq!(second.memory_coverage, MemoryReferenceCoverage::Complete);
}
