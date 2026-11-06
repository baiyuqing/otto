use super::*;
use crate::model::Usage;
use crate::session::memory_recall::MemoryRecordRef;
use serde_json::value::RawValue;

fn valid_checkpoint() -> CompactionCheckpoint {
    CompactionCheckpoint {
        summary: "summary".into(),
        first_kept_entry_id: "0000000a".into(),
        tokens_before: 12,
        usage: None,
        details: CompactionDetails::default(),
        created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
    }
}

fn fixed_width_path(prefix: &str, index: usize) -> String {
    format!("{prefix}{index:04}-{}", "x".repeat(58))
}

#[test]
fn validate_compaction_checkpoint_enforces_bounds() {
    type Case = (
        &'static str,
        Box<dyn Fn(&mut CompactionCheckpoint)>,
        PiErrorKind,
    );
    let cases: Vec<Case> = vec![
        (
            "summary above byte bound",
            Box::new(|checkpoint| {
                checkpoint.summary = "s".repeat(COMPACTION_SUMMARY_MAXIMUM_BYTES + 1);
            }),
            PiErrorKind::EntryTooLarge,
        ),
        (
            "blank summary",
            Box::new(|checkpoint| checkpoint.summary = "   ".into()),
            PiErrorKind::Invalid,
        ),
        (
            "blank first-kept entry id",
            Box::new(|checkpoint| checkpoint.first_kept_entry_id = " ".into()),
            PiErrorKind::Invalid,
        ),
        (
            "negative tokens before",
            Box::new(|checkpoint| checkpoint.tokens_before = -1),
            PiErrorKind::Invalid,
        ),
        (
            "negative omitted read files",
            Box::new(|checkpoint| checkpoint.details.omitted_read_files = -1),
            PiErrorKind::Invalid,
        ),
        (
            "negative omitted modified files",
            Box::new(|checkpoint| checkpoint.details.omitted_modified_files = -1),
            PiErrorKind::Invalid,
        ),
        (
            "path count above bound",
            Box::new(|checkpoint| {
                checkpoint.details.read_files = (0..=COMPACTION_DETAILS_MAXIMUM_PATHS)
                    .map(|index| format!("path-{index}.go"))
                    .collect();
            }),
            PiErrorKind::EntryTooLarge,
        ),
        (
            "path text above bound",
            Box::new(|checkpoint| {
                checkpoint.details.read_files =
                    vec!["x".repeat(COMPACTION_DETAILS_MAXIMUM_BYTES + 1)];
            }),
            PiErrorKind::EntryTooLarge,
        ),
        (
            "empty path",
            Box::new(|checkpoint| checkpoint.details.read_files = vec![String::new()]),
            PiErrorKind::Invalid,
        ),
        (
            "C0 control path",
            Box::new(|checkpoint| checkpoint.details.read_files = vec!["a\u{0}.go".into()]),
            PiErrorKind::Invalid,
        ),
        (
            "DEL control path",
            Box::new(|checkpoint| checkpoint.details.read_files = vec!["a\u{7f}.go".into()]),
            PiErrorKind::Invalid,
        ),
        (
            "C1 control path",
            Box::new(|checkpoint| checkpoint.details.read_files = vec!["a\u{85}.go".into()]),
            PiErrorKind::Invalid,
        ),
        (
            "unclean path",
            Box::new(|checkpoint| checkpoint.details.read_files = vec!["dir/../file.go".into()]),
            PiErrorKind::Invalid,
        ),
        (
            "duplicate read path",
            Box::new(|checkpoint| {
                checkpoint.details.read_files = vec!["dup.go".into(), "dup.go".into()];
            }),
            PiErrorKind::Invalid,
        ),
        (
            "duplicate modified path",
            Box::new(|checkpoint| {
                checkpoint.details.modified_files = vec!["dup.go".into(), "dup.go".into()];
            }),
            PiErrorKind::Invalid,
        ),
        (
            "read and modified overlap",
            Box::new(|checkpoint| {
                checkpoint.details.read_files = vec!["both.go".into()];
                checkpoint.details.modified_files = vec!["both.go".into()];
            }),
            PiErrorKind::Invalid,
        ),
    ];

    for (name, mutate, kind) in cases {
        let mut checkpoint = valid_checkpoint();
        mutate(&mut checkpoint);
        let error = validate_compaction_checkpoint(&checkpoint)
            .expect_err(&format!("{name}: expected rejection"));
        assert_eq!(error.kind(), kind, "{name}: kind");
    }
}

#[test]
fn validate_compaction_checkpoint_accepts_exact_bounds() {
    let mut checkpoint = valid_checkpoint();
    checkpoint.summary = "s".repeat(COMPACTION_SUMMARY_MAXIMUM_BYTES);
    let mut read_files: Vec<String> = (0..COMPACTION_DETAILS_MAXIMUM_PATHS)
        .map(|index| format!("path-{index:04}"))
        .collect();
    let total: usize = read_files.iter().map(String::len).sum();
    let last = read_files.len() - 1;
    read_files[last].push_str(&"x".repeat(COMPACTION_DETAILS_MAXIMUM_BYTES - total));
    checkpoint.details.read_files = read_files;
    checkpoint.usage = Some(Usage::default());
    validate_compaction_checkpoint(&checkpoint).expect("exact bounds must be accepted");
}

#[test]
fn decode_compaction_details_is_lenient() {
    let cases = [
        r#"{"readFiles":["first.go"],"readFiles":["second.go"]}"#,
        "[]",
        r#"{"readFiles":"README.md"}"#,
        r#"{"readFiles":["README.md",1]}"#,
        r#"{"modifiedFiles":{}}"#,
        r#"{"omittedReadFiles":"1"}"#,
        r#"{"omittedModifiedFiles":9223372036854775808}"#,
    ];
    for case in cases {
        let raw = RawValue::from_string(case.to_string()).expect("fixture must be JSON");
        assert_eq!(
            decode_compaction_details(Some(&raw)),
            CompactionDetails::default(),
            "{case}",
        );
    }
    assert_eq!(
        decode_compaction_details(None),
        CompactionDetails::default(),
    );
}

#[test]
fn decode_compaction_details_sanitizes_paths() {
    let raw = RawValue::from_string(
        serde_json::json!({
            "readFiles": ["z.go", "a.go", "a.go", "dup.go", "", "bad\u{0}.go",
                          "del\u{7f}.go", "c1\u{85}.go", "dir/../unclean.go",
                          "./unclean.go", "both.go"],
            "modifiedFiles": ["m.go", "both.go", "m.go"],
            "omittedReadFiles": 2,
            "omittedModifiedFiles": 3,
            "unknown": {"nested": true},
        })
        .to_string(),
    )
    .expect("fixture must be JSON");
    assert_eq!(
        decode_compaction_details(Some(&raw)),
        CompactionDetails {
            read_files: vec!["a.go".into(), "dup.go".into(), "z.go".into()],
            modified_files: vec!["both.go".into(), "m.go".into()],
            omitted_read_files: 2,
            omitted_modified_files: 3,
            ..CompactionDetails::default()
        },
    );
}

#[test]
fn decode_compaction_details_bounds_oversized_metadata() {
    let modified: Vec<String> = (0..1_100)
        .map(|index| fixed_width_path("m", index))
        .collect();
    let read: Vec<String> = (0..20).map(|index| fixed_width_path("r", index)).collect();
    let raw = RawValue::from_string(
        serde_json::json!({
            "readFiles": read,
            "modifiedFiles": modified,
            "omittedReadFiles": i64::MAX - 10,
            "omittedModifiedFiles": i64::MAX - 50,
        })
        .to_string(),
    )
    .expect("fixture must be JSON");
    let details = decode_compaction_details(Some(&raw));
    assert_eq!(
        details.modified_files.len(),
        COMPACTION_DETAILS_MAXIMUM_PATHS
    );
    assert!(details.read_files.is_empty());
    let bytes: usize = details
        .modified_files
        .iter()
        .chain(&details.read_files)
        .map(String::len)
        .sum();
    assert_eq!(bytes, COMPACTION_DETAILS_MAXIMUM_BYTES);
    assert_eq!(details.omitted_read_files, i64::MAX);
    assert_eq!(details.omitted_modified_files, i64::MAX);
    let mut sorted = details.modified_files.clone();
    sorted.sort();
    assert_eq!(details.modified_files, sorted);
}

#[test]
fn clean_path_matches_go_filepath_clean() {
    for (input, want) in [
        ("", "."),
        (".", "."),
        ("a/b", "a/b"),
        ("a//b", "a/b"),
        ("a/./b", "a/b"),
        ("a/../b", "b"),
        ("../a", "../a"),
        ("/../a", "/a"),
        ("a/b/../..", "."),
        ("./unclean.go", "unclean.go"),
        ("/a/b/", "/a/b"),
    ] {
        assert_eq!(clean_path(input), want, "{input}");
    }
}

#[test]
fn safe_context_token_count_floors_at_zero() {
    assert_eq!(safe_context_token_count(-5), 0);
    assert_eq!(safe_context_token_count(7), 7);
}

#[test]
fn compaction_usage_to_pi_keeps_absent_usage_absent() {
    assert_eq!(compaction_usage_to_pi(None).unwrap(), None);
    let usage = Usage::default();
    assert_eq!(
        compaction_usage_to_pi(Some(&usage)).unwrap(),
        Some(PiUsage::default()),
    );
}

fn memory_reference(id: &str, revision: Option<&str>) -> MemoryRecordRef {
    MemoryRecordRef {
        id: id.into(),
        revision: revision.map(str::to_owned),
    }
}

fn raw_json(text: &str) -> Box<RawValue> {
    RawValue::from_string(text.to_string()).expect("fixture must be JSON")
}

#[test]
fn compaction_details_with_memory_fields_round_trip_with_wire_names() {
    let details = CompactionDetails {
        memory_records: vec![
            memory_reference("rec-a", Some("3")),
            memory_reference("rec-b", None),
        ],
        memory_coverage: MemoryReferenceCoverage::Complete,
        ..CompactionDetails::default()
    };
    let encoded = serde_json::to_string(&details).expect("encode details");
    let value: serde_json::Value = serde_json::from_str(&encoded).expect("details JSON");
    assert_eq!(
        value,
        serde_json::json!({
            "memoryRecords": [{"id": "rec-a", "revision": "3"}, {"id": "rec-b"}],
            "memoryCoverage": "complete",
        })
    );
    assert_eq!(
        decode_compaction_details(Some(&raw_json(&encoded))),
        details
    );

    let empty = serde_json::to_value(CompactionDetails::default()).expect("encode empty details");
    assert_eq!(
        empty,
        serde_json::json!({}),
        "empty memory fields stay absent"
    );
}

#[test]
fn legacy_compaction_details_decode_with_unknown_memory_coverage() {
    let details = decode_compaction_details(Some(&raw_json(r#"{"readFiles":["README.md"]}"#)));
    assert_eq!(details.read_files, ["README.md"]);
    assert!(details.memory_records.is_empty());
    assert_eq!(details.memory_coverage, MemoryReferenceCoverage::Unknown);
}

#[test]
fn malformed_memory_fields_yield_default_details() {
    let cases = [
        r#"{"readFiles":["a.go"],"memoryRecords":{"id":"rec-a"}}"#,
        r#"{"readFiles":["a.go"],"memoryRecords":"rec-a"}"#,
        r#"{"readFiles":["a.go"],"memoryRecords":[{"revision":"1"}]}"#,
        r#"{"readFiles":["a.go"],"memoryRecords":["rec-a"]}"#,
        r#"{"readFiles":["a.go"],"memoryRecords":null}"#,
        r#"{"readFiles":["a.go"],"memoryCoverage":"partial"}"#,
        r#"{"readFiles":["a.go"],"memoryCoverage":true}"#,
        r#"{"readFiles":["a.go"],"memoryCoverage":null}"#,
    ];
    for case in cases {
        assert_eq!(
            decode_compaction_details(Some(&raw_json(case))),
            CompactionDetails::default(),
            "{case}",
        );
    }
}

#[test]
fn sanitize_compaction_details_drops_duplicate_and_invalid_references_and_resets_coverage() {
    let kept = CompactionDetails {
        memory_records: vec![
            memory_reference("rec-b", None),
            memory_reference("rec-a", Some("1")),
        ],
        memory_coverage: MemoryReferenceCoverage::Complete,
        ..CompactionDetails::default()
    };
    let sanitized = sanitize_compaction_details(&kept);
    assert_eq!(
        sanitized.memory_records,
        [
            memory_reference("rec-a", Some("1")),
            memory_reference("rec-b", None)
        ]
    );
    assert_eq!(sanitized.memory_coverage, MemoryReferenceCoverage::Complete);

    let duplicate = CompactionDetails {
        memory_records: vec![
            memory_reference("rec-a", None),
            memory_reference("rec-a", None),
        ],
        memory_coverage: MemoryReferenceCoverage::Complete,
        ..CompactionDetails::default()
    };
    let sanitized = sanitize_compaction_details(&duplicate);
    assert_eq!(sanitized.memory_records, [memory_reference("rec-a", None)]);
    assert_eq!(sanitized.memory_coverage, MemoryReferenceCoverage::Unknown);

    let invalid = CompactionDetails {
        memory_records: vec![
            memory_reference("rec-a", None),
            memory_reference(" bad", None),
        ],
        memory_coverage: MemoryReferenceCoverage::Complete,
        ..CompactionDetails::default()
    };
    let sanitized = sanitize_compaction_details(&invalid);
    assert_eq!(sanitized.memory_records, [memory_reference("rec-a", None)]);
    assert_eq!(sanitized.memory_coverage, MemoryReferenceCoverage::Unknown);
}

#[test]
fn sanitize_compaction_details_bounds_memory_references() {
    let records: Vec<MemoryRecordRef> = (0..=MEMORY_RECALL_MAXIMUM_RECORDS)
        .map(|index| memory_reference(&format!("rec-{index:05}"), None))
        .collect();
    let sanitized = sanitize_compaction_details(&CompactionDetails {
        memory_records: records,
        memory_coverage: MemoryReferenceCoverage::Complete,
        ..CompactionDetails::default()
    });
    assert_eq!(
        sanitized.memory_records.len(),
        MEMORY_RECALL_MAXIMUM_RECORDS
    );
    assert_eq!(sanitized.memory_coverage, MemoryReferenceCoverage::Unknown);
}

#[test]
fn validate_compaction_checkpoint_rejects_bad_memory_references() {
    type Case = (&'static str, Vec<MemoryRecordRef>, PiErrorKind);
    let cases: Vec<Case> = vec![
        (
            "empty id",
            vec![memory_reference("", None)],
            PiErrorKind::Invalid,
        ),
        (
            "padded id",
            vec![memory_reference(" rec-a", None)],
            PiErrorKind::Invalid,
        ),
        (
            "empty revision",
            vec![memory_reference("rec-a", Some(""))],
            PiErrorKind::Invalid,
        ),
        (
            "duplicate reference",
            vec![
                memory_reference("rec-a", Some("1")),
                memory_reference("rec-a", Some("1")),
            ],
            PiErrorKind::Invalid,
        ),
        (
            "reference count above bound",
            (0..=MEMORY_RECALL_MAXIMUM_RECORDS)
                .map(|index| memory_reference(&format!("rec-{index:05}"), None))
                .collect(),
            PiErrorKind::EntryTooLarge,
        ),
    ];
    for (name, records, kind) in cases {
        let mut checkpoint = valid_checkpoint();
        checkpoint.details.memory_records = records;
        let error = validate_compaction_checkpoint(&checkpoint)
            .expect_err(&format!("{name}: expected rejection"));
        assert_eq!(error.kind(), kind, "{name}: kind");
    }

    let mut accepted = valid_checkpoint();
    accepted.details.memory_records = vec![
        memory_reference("rec-a", Some("1")),
        memory_reference("rec-a", Some("2")),
        memory_reference("rec-b", None),
    ];
    accepted.details.memory_coverage = MemoryReferenceCoverage::Complete;
    validate_compaction_checkpoint(&accepted).expect("distinct revisions are distinct references");
}

#[test]
fn compaction_details_present_is_true_for_memory_only_details() {
    assert!(!compaction_details_present(&CompactionDetails::default()));
    assert!(compaction_details_present(&CompactionDetails {
        memory_records: vec![memory_reference("rec-a", None)],
        ..CompactionDetails::default()
    }));
    assert!(compaction_details_present(&CompactionDetails {
        memory_coverage: MemoryReferenceCoverage::Complete,
        ..CompactionDetails::default()
    }));
}

#[test]
fn memory_retracted_round_trips_with_wire_name_and_stays_absent_when_empty() {
    let details = CompactionDetails {
        memory_records: vec![memory_reference("rec-a", Some("3"))],
        memory_retracted: vec![memory_reference("rec-a", Some("3"))],
        memory_coverage: MemoryReferenceCoverage::Complete,
        ..CompactionDetails::default()
    };
    let encoded = serde_json::to_string(&details).expect("encode details");
    let value: serde_json::Value = serde_json::from_str(&encoded).expect("details JSON");
    assert_eq!(
        value["memoryRetracted"],
        serde_json::json!([{"id": "rec-a", "revision": "3"}])
    );
    assert_eq!(
        decode_compaction_details(Some(&raw_json(&encoded))),
        details
    );

    let without = CompactionDetails {
        memory_records: vec![memory_reference("rec-a", Some("3"))],
        ..CompactionDetails::default()
    };
    let value = serde_json::to_value(&without).expect("encode details");
    assert!(
        value.get("memoryRetracted").is_none(),
        "empty memoryRetracted stays absent"
    );
    let legacy =
        decode_compaction_details(Some(&raw_json(r#"{"memoryRecords":[{"id":"rec-a"}]}"#)));
    assert!(legacy.memory_retracted.is_empty());
}

#[test]
fn malformed_memory_retracted_yields_default_details() {
    for text in [
        r#"{"readFiles":["a.go"],"memoryRetracted":{"id":"rec-a"}}"#,
        r#"{"readFiles":["a.go"],"memoryRetracted":"rec-a"}"#,
        r#"{"readFiles":["a.go"],"memoryRetracted":[{"revision":"1"}]}"#,
        r#"{"readFiles":["a.go"],"memoryRetracted":null}"#,
    ] {
        assert_eq!(
            decode_compaction_details(Some(&raw_json(text))),
            CompactionDetails::default(),
            "{text}"
        );
    }
}

#[test]
fn sanitize_deduplicates_memory_retracted_without_changing_coverage() {
    let sanitized = sanitize_compaction_details(&CompactionDetails {
        memory_records: vec![memory_reference("rec-a", None)],
        memory_retracted: vec![
            memory_reference("rec-b", None),
            memory_reference("rec-a", None),
            memory_reference("rec-a", None),
            memory_reference("", None),
        ],
        memory_coverage: MemoryReferenceCoverage::Complete,
        ..CompactionDetails::default()
    });
    assert_eq!(
        sanitized.memory_retracted,
        [
            memory_reference("rec-a", None),
            memory_reference("rec-b", None)
        ]
    );
    assert_eq!(sanitized.memory_coverage, MemoryReferenceCoverage::Complete);
}

#[test]
fn validate_compaction_checkpoint_rejects_invalid_memory_retracted() {
    for records in [
        vec![memory_reference("", None)],
        vec![
            memory_reference("rec-a", None),
            memory_reference("rec-a", None),
        ],
    ] {
        let mut checkpoint = valid_checkpoint();
        checkpoint.details.memory_retracted = records;
        let error = validate_compaction_checkpoint(&checkpoint).expect_err("rejected");
        assert_eq!(error.kind(), PiErrorKind::Invalid);
    }
    let mut checkpoint = valid_checkpoint();
    checkpoint.details.memory_retracted = (0..=MEMORY_RECALL_MAXIMUM_RECORDS)
        .map(|index| memory_reference(&format!("rec-{index:05}"), None))
        .collect();
    let error = validate_compaction_checkpoint(&checkpoint).expect_err("count above bound");
    assert_eq!(error.kind(), PiErrorKind::EntryTooLarge);
}

#[test]
fn compaction_details_present_is_true_for_retracted_only_details() {
    assert!(compaction_details_present(&CompactionDetails {
        memory_retracted: vec![memory_reference("rec-a", None)],
        ..CompactionDetails::default()
    }));
}
