//! Run-loop tests: turn records and input ids.

use super::*;

// -- turn records ----------------------------------------------------------

/// A [`MemorySession`] that also keeps every `otto.turn` record it is given.
#[derive(Default)]
pub(super) struct TurnRecordingSession {
    pub(super) inner: MemorySession,
    pub(super) turns: Mutex<Vec<crate::session::TurnRecord>>,
    pub(super) calls: Mutex<Vec<crate::session::ProviderCallRecord>>,
    /// Every custom record as `(customType, data)`, in append order.
    pub(super) customs: Mutex<Vec<(String, String)>>,
}

impl TurnRecordingSession {
    fn turns(&self) -> Vec<crate::session::TurnRecord> {
        self.turns.lock().expect("turns").clone()
    }

    pub(super) fn calls(&self) -> Vec<crate::session::ProviderCallRecord> {
        self.calls.lock().expect("calls").clone()
    }
}

#[async_trait::async_trait]
impl Session for TurnRecordingSession {
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
        self.inner.append_operation_fact(fact).await
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
        self.customs
            .lock()
            .expect("customs")
            .push((custom_type.to_owned(), data.to_owned()));
        if custom_type == crate::session::TURN_CUSTOM_TYPE {
            self.turns
                .lock()
                .expect("turns")
                .push(crate::session::decode_turn_record(data).expect("valid turn record"));
        }
        if custom_type == crate::session::PROVIDER_CALL_CUSTOM_TYPE {
            self.calls.lock().expect("calls").push(
                crate::session::decode_provider_call(data).expect("valid provider call record"),
            );
        }
        self.inner.append_custom(custom_type, data).await
    }
}

fn started_trigger(
    record: &crate::session::TurnRecord,
) -> (crate::session::TurnTrigger, u32, Option<String>) {
    match record {
        crate::session::TurnRecord::Started {
            trigger,
            attempt,
            resumes,
            ..
        } => (*trigger, *attempt, resumes.clone()),
        other => panic!("expected a start record, got {other:?}"),
    }
}

fn ended_outcome(record: &crate::session::TurnRecord) -> crate::session::TurnOutcome {
    match record {
        crate::session::TurnRecord::Ended { outcome, .. } => *outcome,
        other => panic!("expected an end record, got {other:?}"),
    }
}

#[tokio::test]
async fn every_turn_writes_a_start_and_a_matching_end() {
    use crate::session::{TurnOutcome, TurnTrigger};
    let provider = FakeProvider::new(vec![
        Turn::text("ok"),
        Turn {
            events: Vec::new(),
            outcome: Err(ProviderError::Other("boom".into())),
        },
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        TurnRecordingSession::default(),
        options(),
    );
    agent
        .run("first", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("completed turn");
    agent
        .run("second", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("failed turn");
    let cancel = CancellationToken::new();
    cancel.cancel();
    agent
        .run("third", &mut |_| {}, &cancel)
        .await
        .expect_err("cancelled turn");

    let turns = agent.session().turns();
    assert_eq!(turns.len(), 6, "{turns:?}");
    for (index, outcome) in [
        TurnOutcome::Completed,
        TurnOutcome::Failed,
        TurnOutcome::Cancelled,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            started_trigger(&turns[index * 2]),
            (TurnTrigger::User, 1, None)
        );
        assert_eq!(ended_outcome(&turns[index * 2 + 1]), outcome);
    }
    let ended_error = |record: &crate::session::TurnRecord| match record {
        crate::session::TurnRecord::Ended { error, .. } => error.clone(),
        other => panic!("expected an end record, got {other:?}"),
    };
    assert_eq!(ended_error(&turns[1]), None);
    assert!(
        ended_error(&turns[3]).is_some_and(|error| error.contains("boom")),
        "{:?}",
        turns[3]
    );
    assert_eq!(ended_error(&turns[5]), None);
    assert_eq!(agent.session().interrupted_turn(), None);
}

/// A session whose last turn started and never ended, as process loss
/// leaves it.
async fn interrupted_session() -> TurnRecordingSession {
    let session = TurnRecordingSession::default();
    session
        .inner
        .append_custom(
            crate::session::TURN_CUSTOM_TYPE,
            &crate::session::encode_turn_record(&crate::session::TurnRecord::started(
                "op_old",
                crate::session::TurnTrigger::User,
                1,
                None,
            )),
        )
        .await
        .expect("seed start record");
    session
}

#[tokio::test]
async fn a_wake_turn_after_an_interrupted_turn_is_its_recovery() {
    use crate::session::TurnTrigger;
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        kind: Some(NotificationKind::Message),
        text: "The previous Otto process stopped".into(),
        ..Notification::default()
    });
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("resumed")]),
        EchoExecutor::default(),
        interrupted_session().await,
        Options {
            mailbox: inbox,
            ..options()
        },
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("recovery turn");
    let turns = agent.session().turns();
    assert_eq!(
        started_trigger(&turns[0]),
        (TurnTrigger::Recovery, 2, Some("op_old".to_string()))
    );
    assert_eq!(agent.session().interrupted_turn(), None);
}

#[tokio::test]
async fn a_prompt_after_an_interrupted_turn_starts_a_new_chain() {
    use crate::session::TurnTrigger;
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        interrupted_session().await,
        options(),
    );
    agent
        .run("go on", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("prompt turn");
    assert_eq!(
        started_trigger(&agent.session().turns()[0]),
        (TurnTrigger::User, 1, None)
    );
}

// -- input ids --------------------------------------------------------------

#[tokio::test]
async fn a_notification_already_in_the_transcript_is_acknowledged_without_a_second_message() {
    let session = MemorySession::new();
    let delivered = Message {
        role: Role::Context,
        context_type: "task_notification".into(),
        display: true,
        blocks: vec![Block::text("done")],
        ..Message::default()
    };
    session
        .append_input_if_absent("in_1", delivered)
        .await
        .expect("first delivery");
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        text: "done".into(),
        id: "in_1".into(),
        ..Notification::default()
    });
    inbox.push(Notification {
        text: "new".into(),
        id: "in_2".into(),
        ..Notification::default()
    });
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        session,
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("wake turn");
    assert!(inbox.is_empty(), "both entries are acknowledged");
    let ids: Vec<String> = agent
        .session()
        .messages()
        .into_iter()
        .filter(|message| !message.input_id.is_empty())
        .map(|message| message.input_id)
        .collect();
    assert_eq!(ids, ["in_1", "in_2"]);
}

#[test]
fn enqueue_skips_an_id_already_queued() {
    let inbox = Inbox::default();
    let first = inbox
        .enqueue(Notification {
            id: "in_1".into(),
            ..Notification::default()
        })
        .expect("enqueue");
    let again = inbox
        .enqueue(Notification {
            id: "in_1".into(),
            text: "retry".into(),
            ..Notification::default()
        })
        .expect("enqueue");
    assert_eq!(first, again);
    assert_eq!(inbox.len(), 1);
}

#[tokio::test]
async fn a_recovery_turn_delivers_the_interrupted_turns_steering_once() {
    let journal = Arc::new(Inbox::default());
    journal.push(Notification {
        kind: Some(NotificationKind::UserMessage),
        text: "also update the docs".into(),
        id: "steer:op_old:1".into(),
        scope: "op_old".into(),
        ..Notification::default()
    });
    let steering = Arc::new(crate::agent::steering::Steering::default());
    steering.set_journal(journal.clone());
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        kind: Some(NotificationKind::Message),
        text: "The previous Otto process stopped".into(),
        ..Notification::default()
    });
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("resumed")]),
        EchoExecutor::default(),
        interrupted_session().await,
        Options {
            mailbox: inbox,
            steering,
            ..options()
        },
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("recovery turn");
    let steered: Vec<Message> = agent
        .session()
        .messages()
        .into_iter()
        .filter(|message| message.role == Role::User)
        .collect();
    assert_eq!(steered.len(), 1);
    assert_eq!(steered[0].text(), "also update the docs");
    assert_eq!(steered[0].input_id, "steer:op_old:1");
    assert!(journal.is_empty());
}

#[tokio::test]
async fn the_next_input_id_marks_one_runs_user_message() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("one"), Turn::text("two")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent.set_next_input_id(Some("req-1".into()));
    agent
        .run("first", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("first run");
    agent
        .run("second", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("second run");
    let users: Vec<(String, String)> = agent
        .session()
        .messages()
        .into_iter()
        .filter(|message| message.role == Role::User)
        .map(|message| (message.text(), message.input_id))
        .collect();
    assert_eq!(
        users,
        [
            ("first".to_string(), "req-1".to_string()),
            ("second".to_string(), String::new())
        ]
    );
    assert!(agent.session().has_input("req-1"));
}

#[tokio::test]
async fn a_followup_step_message_carries_its_input_id() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("done")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent.set_next_input_id(Some("followup:task:1".into()));
    agent
        .run_followup_with_system_context_control(
            "check the branch",
            "",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("follow-up step");
    let followup = agent
        .session()
        .messages()
        .into_iter()
        .find(|message| message.context_type == "agent_followup")
        .expect("agent_followup message");
    assert_eq!(followup.input_id, "followup:task:1");
    assert!(agent.session().has_input("followup:task:1"));
}
