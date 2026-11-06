use super::*;
use crate::memory::{Origin, Provenance, Scope};
use chrono::{DateTime, Utc};

fn timestamp(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("timestamp")
}

fn revision_record(revision: u64, text: &str) -> Record {
    Record {
        id: "r1".into(),
        scope: Scope::new("user", "u1"),
        kind: "preference".into(),
        key: "editor".into(),
        text: text.into(),
        source: Provenance {
            origin: Some(Origin::Human),
            ..Provenance::default()
        },
        confidence: 1.0,
        revision,
        created_at: timestamp(1_700_000_000),
        updated_at: timestamp(1_700_000_000 + revision as i64),
        ..Record::default()
    }
}

fn kept(line: &str) -> LineChange {
    LineChange::Kept(line.into())
}
fn removed(line: &str) -> LineChange {
    LineChange::Removed(line.into())
}
fn added(line: &str) -> LineChange {
    LineChange::Added(line.into())
}

#[test]
fn identical_texts_give_only_kept_lines() {
    assert_eq!(
        line_changes("a\nb\nc", "a\nb\nc"),
        vec![kept("a"), kept("b"), kept("c")]
    );
}

#[test]
fn an_inserted_line_is_added_between_kept_lines() {
    assert_eq!(
        line_changes("a\nc", "a\nb\nc"),
        vec![kept("a"), added("b"), kept("c")]
    );
}

#[test]
fn a_removed_line_is_removed_between_kept_lines() {
    assert_eq!(
        line_changes("a\nb\nc", "a\nc"),
        vec![kept("a"), removed("b"), kept("c")]
    );
}

#[test]
fn a_replaced_line_is_removed_before_the_added_line() {
    assert_eq!(
        line_changes("a\nold\nc", "a\nnew\nc"),
        vec![kept("a"), removed("old"), added("new"), kept("c")]
    );
}

#[test]
fn an_empty_side_makes_every_line_added_or_removed() {
    assert_eq!(line_changes("", "a\nb"), vec![added("a"), added("b")]);
    assert_eq!(line_changes("a\nb", ""), vec![removed("a"), removed("b")]);
    assert_eq!(line_changes("", ""), Vec::<LineChange>::new());
}

#[test]
fn unchanged_records_have_no_field_changes() {
    let record = revision_record(1, "vim");
    assert_eq!(field_changes(&record, &record.clone()), Vec::new());
}

#[test]
fn a_text_only_change_has_no_field_changes() {
    assert_eq!(
        field_changes(&revision_record(1, "vim"), &revision_record(2, "emacs")),
        Vec::new()
    );
}

#[test]
fn field_changes_are_listed_in_a_fixed_order() {
    let from = revision_record(1, "vim");
    let mut to = revision_record(2, "vim");
    // Set in the reverse of the expected order to show the order is not the mutation order.
    to.metadata.insert("note".into(), "x".into());
    to.expires_at = Some(timestamp(1_800_000_000));
    to.confidence = 0.5;
    to.labels = vec!["editor".into()];
    to.key = "ide".into();
    to.kind = "fact".into();

    let fields: Vec<&str> = field_changes(&from, &to)
        .iter()
        .map(|change| change.field)
        .collect();

    assert_eq!(
        fields,
        [
            "kind",
            "key",
            "labels",
            "confidence",
            "expires_at",
            "metadata"
        ]
    );
}

#[test]
fn field_changes_carry_the_old_and_new_values() {
    let from = revision_record(1, "vim");
    let mut to = revision_record(2, "vim");
    to.kind = "fact".into();
    to.confidence = 0.5;

    assert_eq!(
        field_changes(&from, &to),
        vec![
            FieldChange {
                field: "kind",
                from: "preference".into(),
                to: "fact".into()
            },
            FieldChange {
                field: "confidence",
                from: "1".into(),
                to: "0.5".into()
            },
        ]
    );
}

#[test]
fn a_label_with_a_comma_differs_from_two_labels() {
    let mut from = revision_record(1, "vim");
    let mut to = revision_record(2, "vim");
    from.labels = vec!["a,b".into()];
    to.labels = vec!["a".into(), "b".into()];

    let changes = field_changes(&from, &to);

    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].field, "labels");
    assert_ne!(changes[0].from, changes[0].to);
}

#[test]
fn history_renders_one_line_per_revision_with_the_current_marker_last() {
    let mut restored = revision_record(3, "vim");
    restored
        .metadata
        .insert(RESTORED_FROM_REVISION_METADATA.into(), "1".into());
    restored.source.origin = Some(Origin::Human);
    let history = RecordHistory {
        revisions: vec![
            revision_record(1, "vim"),
            revision_record(2, "emacs"),
            restored,
        ],
        skipped: Vec::new(),
    };

    let text = render_history(&history);
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].starts_with("revision=1 updated_at="), "{text}");
    assert!(lines[1].starts_with("revision=2 updated_at="), "{text}");
    assert!(lines[2].starts_with("revision=3 updated_at="), "{text}");
    assert!(
        lines.iter().all(|line| line.contains("origin=human")),
        "{text}"
    );
    assert!(!lines[0].contains("restored_from="), "{text}");
    assert!(!lines[1].contains("restored_from="), "{text}");
    assert!(lines[2].contains("restored_from=1"), "{text}");
    assert!(!lines[0].contains("current=yes"), "{text}");
    assert!(!lines[1].contains("current=yes"), "{text}");
    assert!(lines[2].contains("current=yes"), "{text}");
    assert!(lines[1].ends_with("text=emacs"), "{text}");
}

#[test]
fn history_appends_one_line_per_skipped_file() {
    let history = RecordHistory {
        revisions: vec![revision_record(1, "vim")],
        skipped: vec![
            crate::memory::SkippedRevision {
                file: "history/r1/2.md".into(),
                reason: "unreadable front matter".into(),
            },
            crate::memory::SkippedRevision {
                file: "history/r1/3.md".into(),
                reason: "revision mismatch".into(),
            },
        ],
    };

    let text = render_history(&history);
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines.len(), 3, "{text}");
    assert_eq!(lines[1], "skipped history/r1/2.md: unreadable front matter");
    assert_eq!(lines[2], "skipped history/r1/3.md: revision mismatch");
}

#[test]
fn comparison_has_a_header_field_lines_and_a_prefixed_text_block() {
    let from = revision_record(1, "keep\nold");
    let mut to = revision_record(2, "keep\nnew");
    to.kind = "fact".into();

    let text = render_comparison(&from, &to);
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(
        lines[0],
        format!(
            "revision 1 ({}) -> revision 2 ({})",
            from.updated_at, to.updated_at
        )
    );
    assert_eq!(lines[1], "kind: preference -> fact");
    assert_eq!(lines[2..], ["text:", "  keep", "- old", "+ new"], "{text}");
}

#[test]
fn comparison_omits_the_text_block_when_the_texts_are_equal() {
    let from = revision_record(1, "vim");
    let mut to = revision_record(2, "vim");
    to.confidence = 0.5;

    let text = render_comparison(&from, &to);

    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(!text.contains("text:"), "{text}");
    assert!(
        text.lines().nth(1).unwrap().starts_with("confidence: "),
        "{text}"
    );
}
