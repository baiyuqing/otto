//! Behavioural contracts of the [`Session`] and [`Mailbox`] ports.
//!
//! Otto's crash-safety guarantees live behind these two ports: the message
//! ordering rule, exactly-once input delivery (`append_input_if_absent`), the
//! interrupted turn folded from `otto.turn`, the operation ledger that drives
//! replay, and the mailbox's FIFO, acknowledgement and deduplication rules.
//! Every implementation, and every wrapper that forwards to one, must keep
//! them. [`session`] and [`mailbox`] check that against fresh instances; with
//! a `reopen` function they also check that a durable implementation keeps
//! them across a restart.
//!
//! Each failure names a stable check, such as `session.input_id`, so a
//! backend author sees exactly which guarantee broke. The checks mutate only
//! the instances they are given. Fault injection (torn writes, lost `fsync`),
//! concurrency and performance stay with each backend's own tests.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::value::RawValue;

use crate::agent::inbox::{Entry, Notification, NotificationKind};
use crate::mailbox::Mailbox;
use crate::model::{
    Block, BlockType, EffectCertainty, FinishReason, Message, OperationDisposition, OperationId,
    OperationOutcome, RetrySafety, Role,
};
use crate::session::operation::OperationFact;
use crate::session::{
    AppendOutcome, CAPABILITIES_CUSTOM_TYPE, COMPACTION_CONTEXT_TYPE, Capabilities,
    CompactionCheckpoint, InterruptedTurn, PLAN_CUSTOM_TYPE, PlanStatus, PlanStep, Session,
    TURN_CUSTOM_TYPE, TurnOutcome, TurnRecord, TurnTrigger, encode_capabilities, encode_plan,
    encode_turn_record,
};

/// One broken guarantee: the stable check name and what was observed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{check}: {detail}")]
pub struct Failure {
    pub check: &'static str,
    pub detail: String,
}

fn ensure(check: &'static str, condition: bool, detail: impl Into<String>) -> Result<(), Failure> {
    if condition {
        Ok(())
    } else {
        Err(Failure {
            check,
            detail: detail.into(),
        })
    }
}

fn fail<E: std::fmt::Display>(check: &'static str, step: &str) -> impl FnOnce(E) -> Failure {
    let step = step.to_owned();
    move |error| Failure {
        check,
        detail: format!("{step}: {error}"),
    }
}

/// Runs every session check against sessions from `fresh`. With `reopen`
/// (close the session and open it again from the same storage) the
/// `session.reopen` check also runs.
pub async fn session<S: Session>(
    fresh: &(dyn Fn() -> S + Sync),
    reopen: Option<&(dyn Fn(S) -> S + Sync)>,
) -> Result<(), Failure> {
    session_order(&fresh()).await?;
    session_tool_sequence(&fresh()).await?;
    session_input_id(&fresh()).await?;
    session_turns(&fresh()).await?;
    session_capabilities(&fresh()).await?;
    session_plan(&fresh()).await?;
    session_operations(&fresh()).await?;
    session_compaction(&fresh()).await?;
    if let Some(reopen) = reopen {
        session_reopen(fresh(), reopen).await?;
    }
    Ok(())
}

/// Rebuilds a mailbox from the durable state of the one it is given.
pub type MailboxReopen<'a> = dyn Fn(Arc<dyn Mailbox>) -> Arc<dyn Mailbox> + 'a;

/// Runs every mailbox check against mailboxes from `fresh`. With `reopen`
/// (rebuild a mailbox from the same durable state) the `mailbox.reopen`
/// check also runs.
pub fn mailbox(
    fresh: &dyn Fn() -> Arc<dyn Mailbox>,
    reopen: Option<&MailboxReopen<'_>>,
) -> Result<(), Failure> {
    mailbox_fifo(fresh().as_ref())?;
    mailbox_dedup(fresh().as_ref())?;
    mailbox_remove(fresh().as_ref())?;
    mailbox_load(fresh().as_ref())?;
    if let Some(reopen) = reopen {
        mailbox_reopen(fresh(), reopen)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// session checks
// ---------------------------------------------------------------------------

fn at() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("in range")
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![Block::text(text)],
        created_at: at(),
        ..Message::default()
    }
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block::text(text)],
        created_at: at(),
        finish_reason: Some(FinishReason::Stop),
        ..Message::default()
    }
}

fn call(id: &str, name: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: id.into(),
            tool_name: name.into(),
            arguments: Some(RawValue::from_string("{}".into()).expect("valid JSON")),
            ..Block::default()
        }],
        created_at: at(),
        finish_reason: Some(FinishReason::ToolCalls),
        ..Message::default()
    }
}

fn result(id: &str, name: &str) -> Message {
    Message {
        role: Role::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            tool_call_id: id.into(),
            tool_name: name.into(),
            text: "ok".into(),
            ..Block::default()
        }],
        created_at: at(),
        ..Message::default()
    }
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages.iter().map(Message::text).collect()
}

async fn turn(session: &impl Session, record: TurnRecord) -> Result<(), Failure> {
    session
        .append_custom(TURN_CUSTOM_TYPE, &encode_turn_record(&record))
        .await
        .map_err(fail("session.turns", "append otto.turn"))
}

async fn session_order(session: &impl Session) -> Result<(), Failure> {
    const CHECK: &str = "session.order";
    for message in [user("one"), assistant("two"), user("three")] {
        session
            .append(message)
            .await
            .map_err(fail(CHECK, "append"))?;
    }
    let messages = session.messages();
    ensure(
        CHECK,
        texts(&messages) == ["one", "two", "three"],
        format!("messages out of order: {:?}", texts(&messages)),
    )?;
    let mut ids: Vec<&str> = messages.iter().map(|message| message.id.as_str()).collect();
    ensure(
        CHECK,
        ids.iter().all(|id| !id.is_empty()),
        "a stored message has no id",
    )?;
    ids.sort_unstable();
    ids.dedup();
    ensure(CHECK, ids.len() == messages.len(), "message ids repeat")
}

async fn session_tool_sequence(session: &impl Session) -> Result<(), Failure> {
    const CHECK: &str = "session.tool_sequence";
    ensure(
        CHECK,
        session.append(result("call-1", "read")).await.is_err(),
        "a tool result without a pending call was accepted",
    )?;
    session
        .append(call("call-1", "read"))
        .await
        .map_err(fail(CHECK, "append tool call"))?;
    ensure(
        CHECK,
        session.append(user("between")).await.is_err(),
        "a message between a tool call and its result was accepted",
    )?;
    ensure(
        CHECK,
        session.append(result("call-2", "read")).await.is_err(),
        "a result for another call was accepted",
    )?;
    session
        .append(result("call-1", "read"))
        .await
        .map_err(fail(CHECK, "append tool result"))?;
    ensure(
        CHECK,
        session.messages().len() == 2,
        format!(
            "rejected appends stored messages: {} stored",
            session.messages().len()
        ),
    )
}

async fn session_input_id(session: &impl Session) -> Result<(), Failure> {
    const CHECK: &str = "session.input_id";
    let first = session
        .append_input_if_absent("in_1", user("hello"))
        .await
        .map_err(fail(CHECK, "first delivery"))?;
    ensure(
        CHECK,
        first == AppendOutcome::Appended,
        "first delivery was not appended",
    )?;
    let again = session
        .append_input_if_absent("in_1", user("hello"))
        .await
        .map_err(fail(CHECK, "repeated delivery"))?;
    ensure(
        CHECK,
        again == AppendOutcome::AlreadyPresent,
        "a repeated input id was appended again",
    )?;
    ensure(
        CHECK,
        session.has_input("in_1") && !session.has_input("in_2"),
        "has_input disagrees with the transcript",
    )?;
    let stored: Vec<Message> = session
        .messages()
        .into_iter()
        .filter(|message| message.input_id == "in_1")
        .collect();
    ensure(
        CHECK,
        stored.len() == 1,
        format!("{} messages carry the input id", stored.len()),
    )?;
    let mut reply = assistant("hi");
    reply.input_id = "in_3".into();
    ensure(
        CHECK,
        session.append(reply).await.is_err(),
        "an assistant message with an input id was accepted",
    )
}

async fn session_turns(session: &impl Session) -> Result<(), Failure> {
    const CHECK: &str = "session.turns";
    turn(
        session,
        TurnRecord::started("t1", TurnTrigger::User, 1, None),
    )
    .await?;
    ensure(
        CHECK,
        session.interrupted_turn()
            == Some(InterruptedTurn {
                turn_id: "t1".into(),
                attempt: 1,
            }),
        format!(
            "an unended turn is not interrupted: {:?}",
            session.interrupted_turn()
        ),
    )?;
    turn(session, TurnRecord::ended("other", TurnOutcome::Failed)).await?;
    ensure(
        CHECK,
        session.interrupted_turn().map(|turn| turn.turn_id) == Some("t1".into()),
        "an end record for another turn closed the open turn",
    )?;
    turn(
        session,
        TurnRecord::started("t2", TurnTrigger::Recovery, 2, Some("t1".into())),
    )
    .await?;
    ensure(
        CHECK,
        session.interrupted_turn()
            == Some(InterruptedTurn {
                turn_id: "t2".into(),
                attempt: 2,
            }),
        "the newest start record is not the open turn",
    )?;
    turn(session, TurnRecord::ended("t2", TurnOutcome::Completed)).await?;
    ensure(
        CHECK,
        session.interrupted_turn().is_none(),
        "the matching end record did not close the turn",
    )
}

async fn session_capabilities(session: &impl Session) -> Result<(), Failure> {
    const CHECK: &str = "session.capabilities";
    for tools in [
        vec!["read".to_string()],
        vec!["read".into(), "write".into()],
    ] {
        let record = encode_capabilities(&Capabilities {
            tools,
            ..Capabilities::default()
        });
        session
            .append_custom(CAPABILITIES_CUSTOM_TYPE, &record)
            .await
            .map_err(fail(CHECK, "append otto.capabilities"))?;
    }
    let observed = session.observed_capabilities().map(|record| record.tools);
    ensure(
        CHECK,
        observed == Some(vec!["read".into(), "write".into()]),
        format!("the latest record does not win: {observed:?}"),
    )
}

fn plan_step(text: &str, status: PlanStatus) -> PlanStep {
    PlanStep {
        text: text.into(),
        status,
    }
}

/// The latest `otto.plan` record is the plan; an empty one clears it.
async fn session_plan(session: &impl Session) -> Result<(), Failure> {
    const CHECK: &str = "session.plan";
    ensure(
        CHECK,
        session.plan().is_empty(),
        "a fresh session has a plan",
    )?;
    let first = vec![plan_step("read", PlanStatus::InProgress)];
    let second = vec![
        plan_step("read", PlanStatus::Completed),
        plan_step("write", PlanStatus::InProgress),
    ];
    for steps in [&first, &second] {
        session
            .append_custom(PLAN_CUSTOM_TYPE, &encode_plan(steps))
            .await
            .map_err(fail(CHECK, "append otto.plan"))?;
    }
    let observed = session.plan();
    ensure(
        CHECK,
        observed == second,
        format!("the latest record does not win: {observed:?}"),
    )?;
    session
        .append_custom(PLAN_CUSTOM_TYPE, &encode_plan(&[]))
        .await
        .map_err(fail(CHECK, "append an empty otto.plan"))?;
    ensure(
        CHECK,
        session.plan().is_empty(),
        "an empty record does not clear the plan",
    )
}

fn operation(id: &str) -> OperationId {
    OperationId::new(id).expect("valid operation id")
}

fn outcome(disposition: OperationDisposition) -> OperationOutcome {
    OperationOutcome {
        disposition,
        effect_certainty: EffectCertainty::Completed,
        stop_reason: None,
    }
}

async fn session_operations(session: &impl Session) -> Result<(), Failure> {
    const CHECK: &str = "session.operations";
    session
        .append(call("call-1", "read"))
        .await
        .map_err(fail(CHECK, "append tool call"))?;
    session
        .append_operation_fact(
            OperationFact::attempt(operation("op_1"), 1, "call-1", "read")
                .with_retry_safety(RetrySafety::ReadOnly),
        )
        .await
        .map_err(fail(CHECK, "append attempt"))?;
    session
        .append_operation_fact(OperationFact::terminal(
            operation("op_1"),
            1,
            "call-1",
            "read",
            outcome(OperationDisposition::Succeeded),
        ))
        .await
        .map_err(fail(CHECK, "append terminal"))?;
    ensure(
        CHECK,
        session
            .append_operation_fact(OperationFact::terminal(
                operation("op_1"),
                1,
                "call-1",
                "read",
                outcome(OperationDisposition::Error),
            ))
            .await
            .is_err(),
        "a conflicting terminal fact was accepted",
    )?;
    let ledger = session.operation_ledger();
    let record = ledger.operation_for_tool_call("call-1");
    ensure(
        CHECK,
        record.is_some_and(|record| {
            record.attempts == 1
                && record.retry_safety == Some(RetrySafety::ReadOnly)
                && record.terminal == Some(outcome(OperationDisposition::Succeeded))
                && !record.corrupt
        }),
        format!("the ledger does not hold the folded operation: {record:?}"),
    )
}

async fn session_compaction(session: &impl Session) -> Result<(), Failure> {
    const CHECK: &str = "session.compaction";
    session
        .append_input_if_absent("in_old", user("old question"))
        .await
        .map_err(fail(CHECK, "append"))?;
    session
        .append(assistant("old answer"))
        .await
        .map_err(fail(CHECK, "append"))?;
    session
        .append(user("new question"))
        .await
        .map_err(fail(CHECK, "append"))?;
    session
        .append(assistant("new answer"))
        .await
        .map_err(fail(CHECK, "append"))?;
    let first_kept = session
        .messages()
        .iter()
        .find(|message| message.text() == "new question")
        .map(|message| message.id.clone())
        .ok_or_else(|| Failure {
            check: CHECK,
            detail: "the appended message is missing".into(),
        })?;
    let metadata = session
        .append_compaction(CompactionCheckpoint {
            summary: "the old exchange".into(),
            first_kept_entry_id: first_kept,
            tokens_before: 100,
            created_at: at(),
            ..CompactionCheckpoint::default()
        })
        .await
        .map_err(fail(CHECK, "append checkpoint"))?;
    ensure(
        CHECK,
        session
            .latest_compaction()
            .is_some_and(|latest| latest.id == metadata.id),
        "latest_compaction does not name the new checkpoint",
    )?;
    let projected = session.model_messages();
    ensure(
        CHECK,
        projected
            .first()
            .is_some_and(|summary| summary.context_type == COMPACTION_CONTEXT_TYPE)
            && texts(&projected[1..]) == ["new question", "new answer"],
        format!(
            "model context is not summary + retained: {:?}",
            texts(&projected)
        ),
    )?;
    ensure(
        CHECK,
        session.has_input("in_old"),
        "an input id before the checkpoint was forgotten",
    )
}

async fn session_reopen<S: Session>(
    session: S,
    reopen: &(dyn Fn(S) -> S + Sync),
) -> Result<(), Failure> {
    const CHECK: &str = "session.reopen";
    session
        .append_input_if_absent("in_1", user("question"))
        .await
        .map_err(fail(CHECK, "append"))?;
    session
        .append(assistant("answer"))
        .await
        .map_err(fail(CHECK, "append"))?;
    session
        .append_custom(
            CAPABILITIES_CUSTOM_TYPE,
            &encode_capabilities(&Capabilities {
                tools: vec!["read".into()],
                ..Capabilities::default()
            }),
        )
        .await
        .map_err(fail(CHECK, "append otto.capabilities"))?;
    let plan = vec![plan_step("ship", PlanStatus::Pending)];
    session
        .append_custom(PLAN_CUSTOM_TYPE, &encode_plan(&plan))
        .await
        .map_err(fail(CHECK, "append otto.plan"))?;
    turn(
        &session,
        TurnRecord::started("t1", TurnTrigger::User, 1, None),
    )
    .await?;
    let before = texts(&session.messages());

    let session = reopen(session);
    ensure(
        CHECK,
        texts(&session.messages()) == before,
        format!(
            "messages changed: {before:?} -> {:?}",
            texts(&session.messages())
        ),
    )?;
    ensure(CHECK, session.has_input("in_1"), "the input id was lost")?;
    ensure(
        CHECK,
        session.interrupted_turn().map(|turn| turn.turn_id) == Some("t1".into()),
        "the interrupted turn was lost",
    )?;
    ensure(
        CHECK,
        session.observed_capabilities().map(|record| record.tools) == Some(vec!["read".into()]),
        "the capabilities record was lost",
    )?;
    ensure(CHECK, session.plan() == plan, "the plan was lost")?;
    let again = session
        .append_input_if_absent("in_1", user("question"))
        .await
        .map_err(fail(CHECK, "repeated delivery"))?;
    ensure(
        CHECK,
        again == AppendOutcome::AlreadyPresent,
        "a delivered input was appended again after reopen",
    )
}

// ---------------------------------------------------------------------------
// mailbox checks
// ---------------------------------------------------------------------------

fn note(id: &str) -> Notification {
    Notification {
        text: format!("text {id}"),
        id: id.into(),
        ..Notification::default()
    }
}

fn ids(mailbox: &dyn Mailbox) -> Vec<String> {
    mailbox
        .queued()
        .into_iter()
        .map(|entry| entry.notification.id)
        .collect()
}

fn enqueue(
    check: &'static str,
    mailbox: &dyn Mailbox,
    notification: Notification,
) -> Result<u64, Failure> {
    mailbox
        .enqueue(notification)
        .map_err(fail(check, "enqueue"))
}

fn mailbox_fifo(mailbox: &dyn Mailbox) -> Result<(), Failure> {
    const CHECK: &str = "mailbox.fifo";
    for id in ["a", "b", "c"] {
        enqueue(CHECK, mailbox, note(id))?;
    }
    let snapshot = mailbox.queued();
    ensure(
        CHECK,
        ids(mailbox) == ["a", "b", "c"],
        format!("queued out of order: {:?}", ids(mailbox)),
    )?;
    mailbox.ack(snapshot[1].seq);
    enqueue(CHECK, mailbox, note("d"))?;
    ensure(
        CHECK,
        ids(mailbox) == ["a", "c", "d"],
        format!(
            "ack or a later enqueue misplaced entries: {:?}",
            ids(mailbox)
        ),
    )?;
    ensure(
        CHECK,
        mailbox.len() == 3 && !mailbox.is_empty(),
        "len disagrees with queued",
    )
}

fn mailbox_dedup(mailbox: &dyn Mailbox) -> Result<(), Failure> {
    const CHECK: &str = "mailbox.dedup";
    let first = enqueue(CHECK, mailbox, note("a"))?;
    let again = enqueue(
        CHECK,
        mailbox,
        Notification {
            text: "retry".into(),
            ..note("a")
        },
    )?;
    ensure(
        CHECK,
        first == again && mailbox.len() == 1,
        "an id already queued was queued again",
    )
}

fn mailbox_remove(mailbox: &dyn Mailbox) -> Result<(), Failure> {
    const CHECK: &str = "mailbox.remove";
    for (id, task) in [("a", "t1"), ("b", "t1"), ("c", "t2")] {
        enqueue(
            CHECK,
            mailbox,
            Notification {
                task_id: task.into(),
                kind: Some(NotificationKind::TaskFinished),
                ..note(id)
            },
        )?;
    }
    let removed = mailbox.remove("t1", NotificationKind::TaskFinished);
    ensure(
        CHECK,
        removed.map(|notification| notification.id) == Some("a".into())
            && ids(mailbox) == ["b", "c"],
        format!(
            "remove did not take only the first match: {:?}",
            ids(mailbox)
        ),
    )
}

fn mailbox_load(mailbox: &dyn Mailbox) -> Result<(), Failure> {
    const CHECK: &str = "mailbox.load";
    mailbox.load(vec![
        Entry {
            seq: 5,
            notification: note("a"),
        },
        Entry {
            seq: 9,
            notification: note("b"),
        },
    ]);
    ensure(
        CHECK,
        mailbox
            .queued()
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>()
            == [5, 9],
        "load did not restore the entries in order",
    )?;
    let next = enqueue(CHECK, mailbox, note("c"))?;
    ensure(
        CHECK,
        next > 9,
        format!("sequence numbering restarted at {next}"),
    )
}

fn mailbox_reopen(
    mailbox: Arc<dyn Mailbox>,
    reopen: &dyn Fn(Arc<dyn Mailbox>) -> Arc<dyn Mailbox>,
) -> Result<(), Failure> {
    const CHECK: &str = "mailbox.reopen";
    for id in ["a", "b", "c"] {
        enqueue(CHECK, mailbox.as_ref(), note(id))?;
    }
    let b = mailbox.queued()[1].seq;
    mailbox.ack(b);
    let mailbox = reopen(mailbox);
    ensure(
        CHECK,
        ids(mailbox.as_ref()) == ["a", "c"],
        format!("pending entries changed: {:?}", ids(mailbox.as_ref())),
    )?;
    let again = enqueue(CHECK, mailbox.as_ref(), note("a"))?;
    ensure(
        CHECK,
        mailbox.len() == 2 && again == mailbox.queued()[0].seq,
        "a reopened mailbox queued a known id again",
    )
}

#[cfg(test)]
mod tests;
