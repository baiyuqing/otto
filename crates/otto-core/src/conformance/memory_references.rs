//! The `session.memory_references` check: `otto.memory_recall` records are
//! anchored to their user message, listed in order, and compaction details
//! carry the union of the references.

use super::*;
use crate::session::MemoryReferenceCoverage;

fn memory_reference(id: &str, revision: Option<&str>) -> MemoryRecordRef {
    MemoryRecordRef {
        id: id.into(),
        revision: revision.map(str::to_owned),
    }
}

pub(super) async fn session_memory_references(session: &impl Session) -> Result<(), Failure> {
    const CHECK: &str = "session.memory_references";
    let turn_one = vec![
        memory_reference("rec-b", None),
        memory_reference("rec-a", Some("1")),
    ];
    let turn_two = vec![memory_reference("rec-c", None)];
    session
        .append_input_if_absent("in_mem_1", user("first question"))
        .await
        .map_err(fail(CHECK, "append first user message"))?;
    session
        .append_custom(MEMORY_RECALL_CUSTOM_TYPE, &encode_memory_recall(&turn_one))
        .await
        .map_err(fail(CHECK, "append first recall"))?;
    session
        .append(assistant("first answer"))
        .await
        .map_err(fail(CHECK, "append first answer"))?;
    session
        .append_input_if_absent("in_mem_2", user("second question"))
        .await
        .map_err(fail(CHECK, "append second user message"))?;
    session
        .append_custom(MEMORY_RECALL_CUSTOM_TYPE, &encode_memory_recall(&turn_two))
        .await
        .map_err(fail(CHECK, "append second recall"))?;
    session
        .append(assistant("second answer"))
        .await
        .map_err(fail(CHECK, "append second answer"))?;

    let user_ids: Vec<String> = session
        .messages()
        .iter()
        .filter(|message| message.role == Role::User)
        .map(|message| message.id.clone())
        .collect();
    ensure(
        CHECK,
        user_ids.len() == 2,
        format!("{} user messages stored", user_ids.len()),
    )?;
    let sorted_turn_one = vec![
        memory_reference("rec-a", Some("1")),
        memory_reference("rec-b", None),
    ];
    let expected = [(&user_ids[0], &sorted_turn_one), (&user_ids[1], &turn_two)];
    let check_listing = |stage: &str| -> Result<(), Failure> {
        let listed = session.memory_references();
        ensure(
            CHECK,
            listed.len() == 2
                && listed
                    .iter()
                    .zip(expected)
                    .all(|(turn, (user_id, records))| {
                        turn.user_message_id == *user_id && turn.records == *records
                    }),
            format!("{stage}: references are not the two anchored turns: {listed:?}"),
        )
    };
    check_listing("before compaction")?;

    let union = vec![
        memory_reference("rec-a", Some("1")),
        memory_reference("rec-b", None),
        memory_reference("rec-c", None),
    ];
    session
        .append_compaction(CompactionCheckpoint {
            summary: "the first exchange".into(),
            first_kept_entry_id: user_ids[1].clone(),
            tokens_before: 100,
            details: crate::session::CompactionDetails {
                memory_records: union.clone(),
                memory_coverage: MemoryReferenceCoverage::Complete,
                ..Default::default()
            },
            created_at: at(),
            ..CompactionCheckpoint::default()
        })
        .await
        .map_err(fail(CHECK, "append checkpoint"))?;
    let latest = session.latest_compaction().ok_or_else(|| Failure {
        check: CHECK,
        detail: "latest_compaction is missing after the checkpoint".into(),
    })?;
    ensure(
        CHECK,
        latest.details.memory_records == union
            && latest.details.memory_coverage == MemoryReferenceCoverage::Complete,
        format!(
            "checkpoint memory details changed: {:?} {:?}",
            latest.details.memory_records, latest.details.memory_coverage
        ),
    )?;
    check_listing("after compaction")
}
