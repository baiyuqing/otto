use super::*;
use crate::agent::inbox::Inbox;
use crate::session::MemorySession;

#[tokio::test]
async fn memory_session_keeps_the_session_contract() {
    session(&MemorySession::new, None).await.unwrap();
}

#[test]
fn inbox_keeps_the_mailbox_contract() {
    mailbox(&|| Arc::new(Inbox::default()), None).unwrap();
}

/// A wrapper that forgets one forward: the check that guards it must
/// name the broken guarantee.
struct Forgets(MemorySession, Forgotten);

#[derive(Clone, Copy, PartialEq)]
enum Forgotten {
    Inputs,
    Plan,
    MemoryReferences,
}

#[async_trait::async_trait]
impl Session for Forgets {
    fn memory_references(&self) -> Vec<crate::session::TurnMemoryReferences> {
        if self.1 == Forgotten::MemoryReferences {
            return Vec::new();
        }
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
        checkpoint: CompactionCheckpoint,
    ) -> Result<crate::session::CompactionMetadata, crate::session::SessionError> {
        self.0.append_compaction(checkpoint).await
    }
    async fn append_operation_fact(
        &self,
        fact: OperationFact,
    ) -> Result<(), crate::session::SessionError> {
        self.0.append_operation_fact(fact).await
    }
    fn operation_ledger(&self) -> crate::session::OperationLedger {
        self.0.operation_ledger()
    }
    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), crate::session::SessionError> {
        self.0.append_custom(custom_type, data).await
    }
    fn observed_capabilities(&self) -> Option<Capabilities> {
        self.0.observed_capabilities()
    }
    fn interrupted_turn(&self) -> Option<InterruptedTurn> {
        self.0.interrupted_turn()
    }

    fn plan(&self) -> Vec<crate::session::PlanStep> {
        if self.1 == Forgotten::Plan {
            return Vec::new();
        }
        self.0.plan()
    }
    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: Message,
    ) -> Result<AppendOutcome, crate::session::SessionError> {
        if self.1 != Forgotten::Inputs {
            return self.0.append_input_if_absent(input_id, message).await;
        }
        self.0
            .append(message)
            .await
            .map(|()| AppendOutcome::Appended)
    }
    fn has_input(&self, input_id: &str) -> bool {
        self.0.has_input(input_id)
    }
}

#[tokio::test]
async fn a_wrapper_that_drops_a_guarantee_fails_the_named_check() {
    for (forgotten, check) in [
        (Forgotten::Inputs, "session.input_id"),
        (Forgotten::Plan, "session.plan"),
        (Forgotten::MemoryReferences, "session.memory_references"),
    ] {
        let failure = session(&|| Forgets(MemorySession::new(), forgotten), None)
            .await
            .expect_err("the broken wrapper must fail");
        assert_eq!(failure.check, check, "{failure}");
    }
}

/// A mailbox that ignores acknowledgements fails `mailbox.fifo`.
#[derive(Debug, Default)]
struct IgnoresAck(Inbox);

impl Mailbox for IgnoresAck {
    fn enqueue(&self, notification: Notification) -> Result<u64, crate::mailbox::MailboxError> {
        Mailbox::enqueue(&self.0, notification)
    }
    fn queued(&self) -> Vec<Entry> {
        self.0.queued()
    }
    fn ack(&self, _sequence: u64) -> Option<Notification> {
        None
    }
    fn remove(&self, task_id: &str, kind: NotificationKind) -> Option<Notification> {
        self.0.remove(task_id, kind)
    }
    fn snapshot(&self) -> Vec<Notification> {
        self.0.snapshot()
    }
    fn len(&self) -> usize {
        self.0.len()
    }
    fn load(&self, entries: Vec<Entry>) {
        self.0.load(entries);
    }
}

#[test]
fn a_mailbox_that_ignores_ack_fails_the_named_check() {
    let failure = mailbox(&|| Arc::new(IgnoresAck::default()), None)
        .expect_err("the broken mailbox must fail");
    assert_eq!(failure.check, "mailbox.fifo", "{failure}");
}
