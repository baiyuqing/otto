//! Context tests: `otto.memory_recall` entries and the memory fields of compaction details.

use super::*;
use crate::session::compaction::latest_compaction_metadata;
use crate::session::memory_recall::{
    MemoryRecordRef, MemoryReferenceCoverage, TurnMemoryReferences,
};

fn memory_reference(id: &str, revision: Option<&str>) -> MemoryRecordRef {
    MemoryRecordRef {
        id: id.into(),
        revision: revision.map(str::to_owned),
    }
}

fn recall_entry(id: &str, parent_id: Option<&str>, data: &str) -> PiEntry {
    let mut built = entry("custom", id, parent_id);
    built.custom = Some(PiCustom {
        custom_type: "otto.memory_recall".into(),
        data: Some(RawValue::from_string(data.into()).expect("valid JSON")),
    });
    built
}

test!(build_context_lists_memory_references_per_user_turn {
    let context = context_from_fixture("memory-recall.jsonl");
    assert_eq!(
        context.memory_references,
        [
            TurnMemoryReferences {
                user_message_id: "a0000001".into(),
                records: vec![memory_reference("rec-a", Some("3")), memory_reference("rec-b", None)],
            },
            TurnMemoryReferences {
                user_message_id: "a0000004".into(),
                records: vec![memory_reference("rec-c", Some("1"))],
            },
        ]
    );
});

test!(latest_compaction_metadata_carries_memory_records_and_coverage {
    let file = read_pi_fixture("memory-recall.jsonl");
    let leaf = file.entries.last().expect("entries").id.clone();
    let metadata = latest_compaction_metadata(&file.entries, &leaf)
        .expect("metadata")
        .expect("compaction on the active path");
    assert_eq!(
        metadata.details.memory_records,
        [
            memory_reference("rec-a", Some("3")),
            memory_reference("rec-b", None),
            memory_reference("rec-c", Some("1")),
        ]
    );
    assert_eq!(metadata.details.memory_coverage, MemoryReferenceCoverage::Complete);
});

test!(a_session_without_recall_entries_has_no_memory_references {
    assert!(context_from_fixture("linear.jsonl").memory_references.is_empty());
});

test!(a_checkpoint_written_before_memory_references_has_unknown_coverage {
    let file = read_pi_fixture("compacted.jsonl");
    let leaf = file.entries.last().expect("entries").id.clone();
    let metadata = latest_compaction_metadata(&file.entries, &leaf)
        .expect("metadata")
        .expect("compaction on the active path");
    assert!(metadata.details.memory_records.is_empty());
    assert_eq!(metadata.details.memory_coverage, MemoryReferenceCoverage::Unknown);
});

test!(a_malformed_recall_entry_is_ignored_with_a_warning {
    let entries = vec![
        user_entry("aaaaaaa1", None, "question"),
        recall_entry("aaaaaaa2", Some("aaaaaaa1"), r#"{"schemaVersion":1,"records":[{"id":""}]}"#),
        recall_entry("aaaaaaa3", Some("aaaaaaa2"), r#"{"schemaVersion":1,"records":[{"id":"rec-a"}]}"#),
    ];
    let (context, warnings) = build_context(&entries, "aaaaaaa3").expect("build context");
    assert_warning_contains(&warnings, "otto.memory_recall");
    assert_warning_contains(&warnings, "aaaaaaa2");
    assert_eq!(
        context.memory_references,
        [TurnMemoryReferences {
            user_message_id: "aaaaaaa1".into(),
            records: vec![memory_reference("rec-a", None)],
        }]
    );
});

test!(a_recall_entry_before_any_user_message_is_ignored_with_a_warning {
    let entries = vec![
        recall_entry("bbbbbbb1", None, r#"{"schemaVersion":1,"records":[{"id":"rec-a"}]}"#),
        user_entry("bbbbbbb2", Some("bbbbbbb1"), "question"),
    ];
    let (context, warnings) = build_context(&entries, "bbbbbbb2").expect("build context");
    assert_warning_contains(&warnings, "otto.memory_recall");
    assert_warning_contains(&warnings, "before any user message");
    assert!(context.memory_references.is_empty());
});
