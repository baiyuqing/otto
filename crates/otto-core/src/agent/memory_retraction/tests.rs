//! Unit tests for retraction notice selection and rendering.

use super::*;

fn reference(id: &str, revision: Option<&str>) -> MemoryRecordRef {
    MemoryRecordRef {
        id: id.into(),
        revision: revision.map(str::to_owned),
    }
}

fn lifecycle(id: &str, revision: Option<&str>, status: RecordStatus) -> RecordLifecycle {
    RecordLifecycle {
        id: id.into(),
        revision: revision.map(str::to_owned),
        status,
    }
}

fn notice(id: &str, reason: RetractionReason) -> RetractionNotice {
    RetractionNotice {
        id: id.into(),
        revision: None,
        current_revision: None,
        reason,
        replacement_id: None,
    }
}

// -- render ------------------------------------------------------------------

// Fails at the assert_eq if an attribute is dropped, renamed or reordered.
#[test]
fn a_superseded_notice_renders_every_attribute_in_order() {
    let rendered = render_retraction_notices(&[RetractionNotice {
        id: "A".into(),
        revision: Some("3".into()),
        current_revision: Some("4".into()),
        reason: RetractionReason::Superseded,
        replacement_id: Some("B".into()),
    }]);
    assert_eq!(
        rendered,
        format!(
            "{RETRACTION_CONTEXT_PREAMBLE}<memory_retraction id=\"A\" revision=\"3\" \
             current_revision=\"4\" reason=\"superseded\" replacement_id=\"B\"/>\n"
        )
    );
}

// Fails at the assert_eq if a replacement_id or revision attribute is written
// for a notice that has none.
#[test]
fn a_forgotten_notice_renders_id_and_reason_only() {
    let rendered = render_retraction_notices(&[notice("A", RetractionReason::Forgotten)]);
    assert_eq!(
        rendered,
        format!(
            "{RETRACTION_CONTEXT_PREAMBLE}<memory_retraction id=\"A\" reason=\"forgotten\"/>\n"
        )
    );
    assert!(!rendered.contains("replacement_id"));
}

// A notice has no text field, so the output is the preamble plus the quoted
// attribute values. Fails if rendering adds any other content.
#[test]
fn a_rendered_notice_holds_only_the_preamble_and_attribute_values() {
    let rendered = render_retraction_notices(&[notice("MARKER", RetractionReason::Retracted)]);
    let body = rendered
        .strip_prefix(RETRACTION_CONTEXT_PREAMBLE)
        .expect("the block starts with the preamble");
    assert_eq!(
        body,
        "<memory_retraction id=\"MARKER\" reason=\"retracted\"/>\n"
    );
}

// Fails at the assert_eq if an id is written without JSON quoting: the id
// would close the quote and the tag and add an attribute.
#[test]
fn an_id_with_a_quote_or_tag_end_is_json_quoted() {
    let rendered = render_retraction_notices(&[notice("a\"/><x y=\"", RetractionReason::Expired)]);
    assert_eq!(
        rendered,
        format!(
            "{RETRACTION_CONTEXT_PREAMBLE}<memory_retraction id=\"a\\\"/><x y=\\\"\" \
             reason=\"expired\"/>\n"
        )
    );
}

// Fails if a line break in an id splits the notice into two lines.
#[test]
fn an_id_with_a_line_break_stays_on_one_line() {
    let rendered = render_retraction_notices(&[notice("a\nb", RetractionReason::Expired)]);
    let body = rendered
        .strip_prefix(RETRACTION_CONTEXT_PREAMBLE)
        .expect("preamble");
    assert_eq!(body.lines().count(), 1, "{body:?}");
    assert!(body.contains("a\\nb"));
}

#[test]
fn no_notices_render_the_empty_string() {
    assert_eq!(render_retraction_notices(&[]), "");
}

#[test]
fn every_reason_has_its_own_attribute_value() {
    let reasons = [
        (RetractionReason::NeedsReview, "needs_review"),
        (RetractionReason::Superseded, "superseded"),
        (RetractionReason::Retracted, "retracted"),
        (RetractionReason::Expired, "expired"),
        (RetractionReason::Forgotten, "forgotten"),
        (RetractionReason::Updated, "updated"),
    ];
    for (reason, expected) in reasons {
        assert_eq!(reason.as_str(), expected);
    }
}

// Fails at the first assert_eq if the join drops or doubles the newline, at
// the others if a single part is altered.
#[test]
fn the_request_text_joins_both_parts_with_one_newline() {
    assert_eq!(
        memory_request_text("records\n", "notices\n"),
        "records\n\nnotices\n"
    );
    assert_eq!(memory_request_text("records\n", ""), "records\n");
    assert_eq!(memory_request_text("", "notices\n"), "notices\n");
    assert_eq!(memory_request_text("", ""), "");
}

// -- select ------------------------------------------------------------------

// Fails if an unchanged active record is reported.
#[test]
fn an_active_record_at_the_referenced_revision_selects_nothing() {
    let selected = select_retraction_notices(
        &[reference("A", Some("3"))],
        &[lifecycle("A", Some("3"), RecordStatus::Active)],
    );
    assert_eq!(selected, Vec::new());
}

// Fails if the updated reason, the seen revision or the current revision is
// not carried.
#[test]
fn an_active_record_at_a_higher_revision_selects_updated() {
    let selected = select_retraction_notices(
        &[reference("A", Some("3"))],
        &[lifecycle("A", Some("5"), RecordStatus::Active)],
    );
    assert_eq!(
        selected,
        vec![RetractionNotice {
            id: "A".into(),
            revision: Some("3".into()),
            current_revision: Some("5".into()),
            reason: RetractionReason::Updated,
            replacement_id: None,
        }]
    );
}

// Fails if a lexicographic or missing revision is compared as a number or
// treated as newer.
#[test]
fn revisions_that_are_not_numbers_never_select_updated() {
    let cases = [
        (Some("v1"), Some("v2")),
        (Some("3"), Some("v4")),
        (Some("v3"), Some("4")),
        (None, Some("4")),
        (Some("3"), None),
        (Some("4"), Some("3")),
        (Some("10"), Some("9")),
    ];
    for (seen, current) in cases {
        let selected = select_retraction_notices(
            &[reference("A", seen)],
            &[lifecycle("A", current, RecordStatus::Active)],
        );
        assert_eq!(selected, Vec::new(), "seen {seen:?}, current {current:?}");
    }
}

// Fails if a status maps to the wrong reason or loses the replacement id.
#[test]
fn each_retired_status_selects_its_reason() {
    let cases = [
        (
            RecordStatus::NeedsReview,
            RetractionReason::NeedsReview,
            None,
        ),
        (
            RecordStatus::Superseded {
                replacement_id: "B".into(),
            },
            RetractionReason::Superseded,
            Some("B"),
        ),
        (RecordStatus::Retracted, RetractionReason::Retracted, None),
        (RecordStatus::Expired, RetractionReason::Expired, None),
        (RecordStatus::Forgotten, RetractionReason::Forgotten, None),
    ];
    for (status, reason, replacement) in cases {
        let selected = select_retraction_notices(
            &[reference("A", Some("3"))],
            &[lifecycle("A", Some("4"), status.clone())],
        );
        assert_eq!(
            selected,
            vec![RetractionNotice {
                id: "A".into(),
                revision: Some("3".into()),
                current_revision: Some("4".into()),
                reason,
                replacement_id: replacement.map(str::to_owned),
            }],
            "{status:?}"
        );
    }
}

// Fails if an unknown or unlisted record produces a notice.
#[test]
fn unknown_and_unlisted_records_select_nothing() {
    let selected = select_retraction_notices(
        &[reference("A", Some("3")), reference("B", Some("1"))],
        &[lifecycle("A", None, RecordStatus::Unknown)],
    );
    assert_eq!(selected, Vec::new());
}

// Fails if a lifecycle entry for an id nobody referenced produces a notice.
#[test]
fn a_retired_record_that_was_never_referenced_selects_nothing() {
    let selected = select_retraction_notices(
        &[reference("A", Some("3"))],
        &[lifecycle("C", Some("1"), RecordStatus::Retracted)],
    );
    assert_eq!(selected, Vec::new());
}

// Fails if two notices are produced or the lower revision is kept.
#[test]
fn two_references_to_one_id_give_one_notice_at_the_highest_revision() {
    let selected = select_retraction_notices(
        &[
            reference("A", Some("2")),
            reference("A", Some("10")),
            reference("A", Some("9")),
            reference("A", None),
        ],
        &[lifecycle("A", Some("11"), RecordStatus::Retracted)],
    );
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].revision.as_deref(), Some("10"));
}

// Fails if the highest revision is judged as a string ("9" > "10").
#[test]
fn the_highest_revision_is_compared_as_a_number() {
    let selected = select_retraction_notices(
        &[reference("A", Some("9")), reference("A", Some("10"))],
        &[lifecycle("A", Some("10"), RecordStatus::Active)],
    );
    assert_eq!(
        selected,
        Vec::new(),
        "revision 10 was seen, so 10 is not newer"
    );
}

// Fails if the output follows the reference or lifecycle order.
#[test]
fn notices_are_ordered_by_id() {
    let selected = select_retraction_notices(
        &[
            reference("c", Some("1")),
            reference("a", Some("1")),
            reference("b", Some("1")),
        ],
        &[
            lifecycle("b", None, RecordStatus::Retracted),
            lifecycle("c", None, RecordStatus::Retracted),
            lifecycle("a", None, RecordStatus::Retracted),
        ],
    );
    let ids: Vec<&str> = selected.iter().map(|notice| notice.id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c"]);
}

// -- summary notices ---------------------------------------------------------

fn latest_checkpoint(
    records: Vec<MemoryRecordRef>,
    retracted: Vec<MemoryRecordRef>,
    coverage: crate::session::MemoryReferenceCoverage,
) -> crate::session::CompactionMetadata {
    crate::session::CompactionMetadata {
        id: "compaction-1".into(),
        details: crate::session::CompactionDetails {
            memory_records: records,
            memory_retracted: retracted,
            memory_coverage: coverage,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn summary_line(id: &str, record: &str, reason: RetractionReason) -> SummaryNotice {
    SummaryNotice {
        checkpoint_id: id.into(),
        record: Some((record.into(), reason)),
    }
}

// Fails at the assert_eq if an Active record at the same revision or an
// Unknown record selects a line, or an updated record selects none.
#[test]
fn a_summary_notice_is_selected_per_retired_record_of_the_checkpoint() {
    let latest = latest_checkpoint(
        vec![
            reference("rec-a", Some("3")),
            reference("rec-b", Some("1")),
            reference("rec-c", Some("1")),
            reference("rec-d", Some("1")),
        ],
        Vec::new(),
        crate::session::MemoryReferenceCoverage::Complete,
    );
    let notices = select_summary_notices(
        Some(&latest),
        false,
        &[
            lifecycle("rec-a", Some("3"), RecordStatus::Retracted),
            lifecycle("rec-b", Some("1"), RecordStatus::Active),
            lifecycle("rec-c", Some("1"), RecordStatus::Unknown),
            lifecycle("rec-d", Some("2"), RecordStatus::Active),
        ],
        &[],
    );
    assert_eq!(
        notices,
        [
            summary_line("compaction-1", "rec-a", RetractionReason::Retracted),
            summary_line("compaction-1", "rec-d", RetractionReason::Updated),
        ]
    );
}

// Fails at the assert_eq if a record already listed in memory_retracted is
// selected again.
#[test]
fn records_in_memory_retracted_are_skipped() {
    let latest = latest_checkpoint(
        vec![reference("rec-a", Some("3")), reference("rec-b", Some("1"))],
        vec![reference("rec-a", Some("3"))],
        crate::session::MemoryReferenceCoverage::Complete,
    );
    let notices = select_summary_notices(
        Some(&latest),
        false,
        &[
            lifecycle("rec-a", Some("3"), RecordStatus::Retracted),
            lifecycle("rec-b", Some("1"), RecordStatus::Expired),
        ],
        &[],
    );
    assert_eq!(
        notices,
        [summary_line(
            "compaction-1",
            "rec-b",
            RetractionReason::Expired
        )]
    );
}

// Fails at the first assert_eq if the unknown-coverage line needs no record
// notice, and at the second if it is missing with one.
#[test]
fn unknown_coverage_gives_a_line_only_with_a_record_notice() {
    let latest = latest_checkpoint(
        Vec::new(),
        Vec::new(),
        crate::session::MemoryReferenceCoverage::Unknown,
    );
    assert_eq!(select_summary_notices(Some(&latest), false, &[], &[]), []);
    assert_eq!(
        select_summary_notices(
            Some(&latest),
            false,
            &[],
            &[notice("rec-x", RetractionReason::Retracted)]
        ),
        [SummaryNotice {
            checkpoint_id: "compaction-1".into(),
            record: None
        }]
    );
}

// Fails at the assert_eq if a remote compaction in force gives no `remote`
// line with a record notice, or gives one without.
#[test]
fn a_remote_compaction_in_force_gives_a_remote_line_with_a_record_notice() {
    let record_notices = [notice("rec-x", RetractionReason::Retracted)];
    assert_eq!(
        select_summary_notices(None, true, &[], &record_notices),
        [SummaryNotice {
            checkpoint_id: "remote".into(),
            record: None
        }]
    );
    assert_eq!(select_summary_notices(None, true, &[], &[]), []);
    assert_eq!(
        select_summary_notices(None, false, &[], &record_notices),
        []
    );
}

// Fails at the assert_eq if a line, its attribute order or the preamble changes.
#[test]
fn summary_notices_render_exact_lines_after_the_preamble() {
    let rendered = render_summary_notices(&[
        summary_line("compaction-1", "rec-a", RetractionReason::Retracted),
        SummaryNotice {
            checkpoint_id: "compaction-1".into(),
            record: None,
        },
    ]);
    assert_eq!(
        rendered,
        format!(
            "{SUMMARY_AFFECTED_PREAMBLE}\
             <memory_summary_affected checkpoint=\"compaction-1\" record=\"rec-a\" reason=\"retracted\"/>\n\
             <memory_summary_affected checkpoint=\"compaction-1\" coverage=\"unknown\"/>\n"
        )
    );
    assert_eq!(render_summary_notices(&[]), "");
}

// Fails at the assert_eq if the order is not records then summaries, or if an
// empty side leaves a stray separator.
#[test]
fn the_retraction_block_concatenates_records_then_summaries() {
    let records = [notice("rec-a", RetractionReason::Forgotten)];
    let summaries = [summary_line(
        "compaction-1",
        "rec-a",
        RetractionReason::Forgotten,
    )];
    assert_eq!(
        render_retraction_block(&records, &summaries),
        format!(
            "{}{}",
            render_retraction_notices(&records),
            render_summary_notices(&summaries)
        )
    );
    assert_eq!(
        render_retraction_block(&records, &[]),
        render_retraction_notices(&records)
    );
    assert_eq!(
        render_retraction_block(&[], &summaries),
        render_summary_notices(&summaries)
    );
    assert_eq!(render_retraction_block(&[], &[]), "");
}

// Fails at the assert_eq if the heading, a line or the rule changes, or if
// current_revision leaks into the summary instructions.
#[test]
fn summary_instructions_render_heading_lines_and_rule() {
    let rendered = render_summary_instructions(&[
        RetractionNotice {
            id: "rec-a".into(),
            revision: Some("3".into()),
            current_revision: Some("4".into()),
            reason: RetractionReason::Superseded,
            replacement_id: Some("rec-b".into()),
        },
        notice("rec-c", RetractionReason::Retracted),
    ]);
    assert_eq!(
        rendered,
        format!(
            "{SUMMARY_INSTRUCTIONS_HEADING}\n\
             <memory_retraction id=\"rec-a\" revision=\"3\" reason=\"superseded\" replacement_id=\"rec-b\"/>\n\
             <memory_retraction id=\"rec-c\" reason=\"retracted\"/>\n\
             {SUMMARY_INSTRUCTIONS_RULE}"
        )
    );
    assert_eq!(render_summary_instructions(&[]), "");
}

// Fails at the assert_eq if the union drops the previous references, keeps a
// duplicate or leaves the result unsorted.
#[test]
fn retracted_union_merges_sorts_and_deduplicates() {
    let previous = crate::session::CompactionDetails {
        memory_retracted: vec![reference("rec-b", Some("1")), reference("rec-a", Some("3"))],
        ..Default::default()
    };
    let notices = [
        RetractionNotice {
            revision: Some("3".into()),
            ..notice("rec-a", RetractionReason::Retracted)
        },
        RetractionNotice {
            revision: Some("2".into()),
            ..notice("rec-c", RetractionReason::Expired)
        },
    ];
    assert_eq!(
        retracted_union(Some(&previous), &notices),
        [
            reference("rec-a", Some("3")),
            reference("rec-b", Some("1")),
            reference("rec-c", Some("2")),
        ]
    );
    assert_eq!(retracted_union(None, &[]), []);
}
