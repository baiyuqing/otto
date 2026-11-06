use super::*;

fn record(id: &str, revision: &str) -> MemoryRecordRef {
    MemoryRecordRef {
        id: id.into(),
        revision: Some(revision.into()),
    }
}

fn message(id: &str, role: Role) -> Message {
    Message {
        id: id.into(),
        role,
        ..Message::default()
    }
}

fn turn(user_message_id: &str, records: Vec<MemoryRecordRef>) -> TurnMemoryReferences {
    TurnMemoryReferences {
        user_message_id: user_message_id.into(),
        records,
    }
}

fn details(records: Vec<MemoryRecordRef>, coverage: MemoryReferenceCoverage) -> CompactionDetails {
    CompactionDetails {
        memory_records: records,
        memory_coverage: coverage,
        ..CompactionDetails::default()
    }
}

#[test]
fn records_of_summarized_turns_are_unioned_sorted_and_deduplicated() {
    let messages = [message("u1", Role::User), message("u2", Role::User)];
    let references = [
        turn("u1", vec![record("b", "2"), record("a", "1")]),
        turn("u2", vec![record("a", "1"), record("c", "3")]),
    ];
    let (records, coverage) = summarized_memory_references(None, &messages, &references);
    assert_eq!(
        records,
        vec![record("a", "1"), record("b", "2"), record("c", "3")]
    );
    assert_eq!(coverage, MemoryReferenceCoverage::Complete);
}

#[test]
fn references_of_turns_outside_the_summarized_messages_are_ignored() {
    let messages = [message("u1", Role::User)];
    let references = [
        turn("u1", vec![record("a", "1")]),
        turn("u9", vec![record("z", "9")]),
    ];
    let (records, _) = summarized_memory_references(None, &messages, &references);
    assert_eq!(records, vec![record("a", "1")]);
}

#[test]
fn the_previous_checkpoint_records_are_carried_forward() {
    let previous = details(vec![record("a", "1")], MemoryReferenceCoverage::Complete);
    let messages = [message("u2", Role::User)];
    let references = [turn("u2", vec![record("c", "3")])];
    let (records, coverage) = summarized_memory_references(Some(&previous), &messages, &references);
    assert_eq!(records, vec![record("a", "1"), record("c", "3")]);
    assert_eq!(coverage, MemoryReferenceCoverage::Complete);
}

#[test]
fn unknown_coverage_of_the_previous_checkpoint_propagates() {
    let previous = details(vec![record("a", "1")], MemoryReferenceCoverage::Unknown);
    let messages = [message("u2", Role::User)];
    let references = [turn("u2", vec![])];
    let (_, coverage) = summarized_memory_references(Some(&previous), &messages, &references);
    assert_eq!(coverage, MemoryReferenceCoverage::Unknown);
}

#[test]
fn a_user_message_without_a_record_makes_coverage_unknown_and_keeps_other_records() {
    let messages = [message("u1", Role::User), message("u2", Role::User)];
    let references = [turn("u2", vec![record("b", "2")])];
    let (records, coverage) = summarized_memory_references(None, &messages, &references);
    assert_eq!(records, vec![record("b", "2")]);
    assert_eq!(coverage, MemoryReferenceCoverage::Unknown);
}

#[test]
fn a_message_that_is_not_a_user_message_needs_no_record() {
    let messages = [
        message("u1", Role::User),
        message("a1", Role::Assistant),
        message("c1", Role::Context),
    ];
    let references = [turn("u1", vec![])];
    let (records, coverage) = summarized_memory_references(None, &messages, &references);
    assert!(records.is_empty());
    assert_eq!(coverage, MemoryReferenceCoverage::Complete);
}
