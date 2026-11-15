//! Run tests: every user turn persists one `otto.memory_recall` record.

use super::memory_context::one_record;
use super::*;
use crate::session::{MemoryRecordRef, TurnMemoryReferences};

fn user_message_ids(session: &MemorySession) -> Vec<String> {
    session
        .messages()
        .iter()
        .filter(|message| message.role == Role::User)
        .map(|message| message.id.clone())
        .collect()
}

#[tokio::test]
async fn a_turn_without_a_memory_binding_writes_an_empty_record() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert_eq!(
        agent.session().memory_references(),
        vec![TurnMemoryReferences {
            user_message_id: user_message_ids(agent.session())[0].clone(),
            records: Vec::new(),
        }]
    );
}

#[tokio::test]
async fn a_failed_recall_writes_an_empty_record_and_still_warns() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::failing("store unavailable"))),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("events").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");
    assert!(collect(&events).contains(&Event::MemoryWarning {
        message: "store unavailable".into()
    }));
    assert_eq!(
        agent.session().memory_references(),
        vec![TurnMemoryReferences {
            user_message_id: user_message_ids(agent.session())[0].clone(),
            records: Vec::new(),
        }]
    );
}

#[tokio::test]
async fn two_turns_write_two_records_anchored_to_their_own_user_messages() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("first"), Turn::text("second")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::with_records(one_record()))),
            ..options()
        },
    );
    let cancel = CancellationToken::new();
    agent.run("one", &mut |_| {}, &cancel).await.expect("run");
    agent.run("two", &mut |_| {}, &cancel).await.expect("run");

    let user_ids = user_message_ids(agent.session());
    assert_eq!(user_ids.len(), 2);
    assert_ne!(user_ids[0], user_ids[1]);
    let anchored: Vec<String> = agent
        .session()
        .memory_references()
        .into_iter()
        .map(|references| references.user_message_id)
        .collect();
    assert_eq!(anchored, user_ids);
}

#[tokio::test]
async fn the_tool_loop_writes_one_record_per_turn() {
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::tool_call("c1", r#"{"value":1}"#),
            Turn::text("ok"),
        ]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            memory: Some(Arc::new(FakeMemory::with_records(one_record()))),
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let references = agent.session().memory_references();
    assert_eq!(references.len(), 1);
    assert_eq!(
        references[0].records,
        vec![MemoryRecordRef {
            id: "rec-1".into(),
            revision: Some("3".into()),
        }]
    );
}
