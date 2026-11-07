//! Tests for the record lifecycle commands: list filters, `/memory state`,
//! `/memory verify` and `/remember --review-in`.

use super::{ListFilters, Reply, Request, ScopeSelector, parse_time_or_age, review_deadline};
use crate::memory::{Record, RecordState};
use chrono::{Duration, TimeZone, Utc};

const SECONDS_TOLERANCE: i64 = 30;

fn parse_memory(args: &str) -> Result<Request, String> {
    Request::parse("memory", args)
}

fn list_filters(args: &str) -> ListFilters {
    match parse_memory(&format!("list {args}")) {
        Ok(Request::MemoryList { filters, .. }) => filters,
        other => panic!("`list {args}` parsed as {other:?}"),
    }
}

fn assert_usage(args: &str) {
    match parse_memory(args) {
        Err(usage) => assert!(usage.starts_with("usage: /memory list"), "{args}: {usage}"),
        Ok(request) => panic!("`{args}` parsed as {request:?}"),
    }
}

#[test]
fn parse_time_or_age_reads_rfc3339_and_counts_ages_back_from_now() {
    let now = Utc.with_ymd_and_hms(2026, 3, 10, 12, 0, 0).unwrap();
    assert_eq!(
        parse_time_or_age("2026-01-01T00:00:00Z", now),
        Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
    );
    assert_eq!(
        parse_time_or_age("2026-01-01T02:00:00+02:00", now),
        Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
    );
    assert_eq!(
        parse_time_or_age("30d", now),
        Some(now - Duration::days(30))
    );
    assert_eq!(
        parse_time_or_age("12h", now),
        Some(now - Duration::hours(12))
    );
    assert_eq!(
        parse_time_or_age("45m", now),
        Some(now - Duration::minutes(45))
    );
    for invalid in [
        "",
        "d",
        "0d",
        "-1d",
        "5w",
        "1.5d",
        "abc",
        "30",
        "2026-01-01",
    ] {
        assert_eq!(parse_time_or_age(invalid, now), None, "{invalid:?}");
    }
}

#[test]
fn review_deadline_counts_forward_and_rejects_absolute_times() {
    let before = Utc::now();
    let deadline = review_deadline("7d").expect("an age is a deadline");
    let after = Utc::now();
    assert!(deadline >= before + Duration::days(7), "{deadline}");
    assert!(deadline <= after + Duration::days(7), "{deadline}");
    assert!(review_deadline("2026-01-01T00:00:00Z").is_none());
    assert!(review_deadline("2999-01-01T00:00:00Z").is_none());
    assert!(review_deadline("soon").is_none());
    assert!(review_deadline("0h").is_none());
}

#[test]
fn list_without_filters_has_empty_filters() {
    assert_eq!(list_filters(""), ListFilters::default());
}

#[test]
fn list_state_flag_accepts_one_repeated_and_all_states() {
    assert_eq!(
        list_filters("--state needs-review").states,
        vec![RecordState::NeedsReview]
    );
    assert_eq!(
        list_filters("--state needs_review").states,
        vec![RecordState::NeedsReview]
    );
    assert_eq!(
        list_filters("--state active --state retracted").states,
        vec![RecordState::Active, RecordState::Retracted]
    );
    assert_eq!(
        list_filters("--state all").states,
        RecordState::ALL.to_vec()
    );
    assert_usage("list --state bogus");
}

#[test]
fn list_kind_label_and_boolean_flags_fill_the_filters() {
    let filters =
        list_filters("--kind fact --kind note --label a --label b --unverified --review-due");
    assert_eq!(filters.kinds, vec!["fact", "note"]);
    assert_eq!(filters.labels, vec!["a", "b"]);
    assert!(filters.unverified_only);
    assert!(filters.review_due_only);
    assert!(filters.updated_after.is_none() && filters.verified_after.is_none());
}

#[test]
fn list_time_flags_accept_absolute_times_and_ages() {
    let filters = list_filters("--updated-after 2026-01-01T00:00:00Z");
    assert_eq!(
        filters.updated_after,
        Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
    );
    let before = Utc::now();
    let filters = list_filters("--updated-after 30d --verified-after 12h");
    let updated = filters.updated_after.unwrap();
    let verified = filters.verified_after.unwrap();
    assert!((before - Duration::days(30) - updated).num_seconds().abs() < SECONDS_TOLERANCE);
    assert!(
        (before - Duration::hours(12) - verified)
            .num_seconds()
            .abs()
            < SECONDS_TOLERANCE
    );
    assert_usage("list --updated-after yesterday");
    assert_usage("list --verified-after 5w");
}

#[test]
fn list_flags_without_a_value_are_usage_errors() {
    for flag in [
        "--state",
        "--kind",
        "--label",
        "--updated-after",
        "--verified-after",
        "--limit",
        "--scope",
    ] {
        assert_usage(&format!("list {flag}"));
    }
    assert_usage("list --unknown");
}

#[test]
fn list_keeps_scope_limit_and_cursor_with_the_new_filters() {
    match parse_memory("list --scope user --limit 5 --cursor abc --state all") {
        Ok(Request::MemoryList {
            scope,
            limit,
            cursor,
            filters,
        }) => {
            assert!(matches!(scope, ScopeSelector::User));
            assert_eq!((limit, cursor.as_str()), (5, "abc"));
            assert_eq!(filters.states.len(), 4);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn state_command_parses_each_target_state() {
    assert!(matches!(
        parse_memory("state r1 needs-review"),
        Ok(Request::MemoryState { id, state: RecordState::NeedsReview, superseded_by, reason, expected_revision: None })
            if id == "r1" && superseded_by.is_empty() && reason.is_empty()
    ));
    assert!(matches!(
        parse_memory("state r1 active"),
        Ok(Request::MemoryState {
            state: RecordState::Active,
            ..
        })
    ));
    assert!(matches!(
        parse_memory("state r1 retracted --reason two words"),
        Ok(Request::MemoryState { state: RecordState::Retracted, reason, .. }) if reason == "two words"
    ));
    assert!(matches!(
        parse_memory("state r1 superseded r2"),
        Ok(Request::MemoryState { state: RecordState::Superseded, superseded_by, .. }) if superseded_by == "r2"
    ));
    assert!(matches!(
        parse_memory("state r1 superseded r2 --reason newer"),
        Ok(Request::MemoryState { superseded_by, reason, .. }) if superseded_by == "r2" && reason == "newer"
    ));
}

#[test]
fn state_command_rejects_missing_replacement_and_bad_arguments() {
    for args in [
        "state r1 superseded",
        "state r1 superseded --reason x",
        "state r1 bogus",
        "state r1 retracted --reason",
        "state r1 retracted extra",
        "state r1",
    ] {
        assert_usage(args);
    }
}

#[test]
fn verify_command_parses_the_review_deadline() {
    assert!(matches!(
        parse_memory("verify r1"),
        Ok(Request::MemoryVerify { id, review_at: None, expected_revision: None }) if id == "r1"
    ));
    let before = Utc::now();
    match parse_memory("verify r1 --review-in 30d") {
        Ok(Request::MemoryVerify {
            review_at: Some(deadline),
            ..
        }) => {
            assert!(deadline >= before + Duration::days(30), "{deadline}");
        }
        other => panic!("{other:?}"),
    }
    assert_usage("verify r1 --review-in 2026-01-01T00:00:00Z");
    assert_usage("verify r1 --review-in");
    assert_usage("verify r1 --review-in 0d");
}

#[test]
fn remember_review_in_sets_a_deadline_about_seven_days_ahead() {
    let before = Utc::now();
    match Request::parse("remember", "--review-in 7d some text") {
        Ok(Request::Remember {
            review_at: Some(deadline),
            text,
            ..
        }) => {
            assert_eq!(text, "some text");
            assert!(
                (deadline - before - Duration::days(7)).num_seconds().abs() < SECONDS_TOLERANCE
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(Request::parse("remember", "--review-in 2026-01-01T00:00:00Z text").is_err());
    assert!(Request::parse("remember", "--review-in 7d").is_err());
}

struct MemoryFixture {
    controller: crate::app::Controller,
    _directories: [tempfile::TempDir; 3],
}

async fn memory_fixture() -> MemoryFixture {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let controller = crate::fixtures::controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;
    MemoryFixture {
        controller,
        _directories: [workspace, sessions, store],
    }
}

fn remember_text(controller: &crate::app::Controller, key: &str, text: &str) -> Record {
    let request = Request::parse(
        "remember",
        &format!("--scope user --kind note --key {key} {text}"),
    )
    .expect("parse remember");
    match controller.management(request) {
        Ok(Reply::Remembered(record)) => record,
        other => panic!("expected a remembered record, got {:?}", other.map(|_| ())),
    }
}

fn run_memory(controller: &crate::app::Controller, args: &str) -> Result<Reply, String> {
    controller.management(parse_memory(args).expect("parse memory"))
}

fn listed_ids(controller: &crate::app::Controller, args: &str) -> Vec<String> {
    match run_memory(controller, args) {
        Ok(Reply::MemoryList(page)) => page.records.into_iter().map(|record| record.id).collect(),
        other => panic!("expected a list, got {:?}", other.map(|_| ())),
    }
}

// Needs the store implementation of `set_state` and of the list state filter.
#[tokio::test(flavor = "multi_thread")]
async fn needs_review_record_leaves_the_default_list_and_appears_under_its_state() {
    let fixture = memory_fixture().await;
    let controller = &fixture.controller;
    let record = remember_text(controller, "editor", "prefers vim");
    let changed = match run_memory(controller, &format!("state {} needs-review", record.id)) {
        Ok(Reply::MemoryStateChanged(changed)) => changed,
        other => panic!("{:?}", other.map(|_| ())),
    };
    assert_eq!(changed.state, RecordState::NeedsReview);
    assert_eq!(changed.revision, record.revision + 1);
    assert_eq!(changed.updated_at, record.updated_at);
    assert!(!listed_ids(controller, "list --scope user").contains(&record.id));
    assert!(listed_ids(controller, "list --scope user --state needs-review").contains(&record.id));
    assert!(listed_ids(controller, "list --scope user --state all").contains(&record.id));
}

// Needs the store implementation of `verify`.
#[tokio::test(flavor = "multi_thread")]
async fn verify_sets_verified_at_and_the_review_deadline() {
    let fixture = memory_fixture().await;
    let controller = &fixture.controller;
    let record = remember_text(controller, "editor", "prefers vim");
    assert!(record.verified_at.is_none());
    let verified = match run_memory(controller, &format!("verify {} --review-in 30d", record.id)) {
        Ok(Reply::MemoryVerified(verified)) => verified,
        other => panic!("{:?}", other.map(|_| ())),
    };
    assert!(verified.verified_at.is_some());
    assert!(verified.review_at.unwrap() > Utc::now() + Duration::days(29));
    assert_eq!(verified.updated_at, record.updated_at);
    assert!(listed_ids(controller, "list --scope user --unverified").is_empty());
}

// Needs the store implementation of `verify`.
#[tokio::test(flavor = "multi_thread")]
async fn verify_returns_a_needs_review_record_to_active() {
    let fixture = memory_fixture().await;
    let controller = &fixture.controller;
    let record = remember_text(controller, "editor", "prefers vim");
    run_memory(controller, &format!("state {} needs-review", record.id)).unwrap();
    match run_memory(controller, &format!("verify {}", record.id)) {
        Ok(Reply::MemoryVerified(verified)) => assert_eq!(verified.state, RecordState::Active),
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert!(listed_ids(controller, "list --scope user").contains(&record.id));
}

// Needs the store implementation of `set_state`.
#[tokio::test(flavor = "multi_thread")]
async fn superseded_requires_an_active_replacement_record() {
    let fixture = memory_fixture().await;
    let controller = &fixture.controller;
    let old = remember_text(controller, "editor", "prefers vim");
    let replacement = remember_text(controller, "editor-new", "prefers helix");
    run_memory(
        controller,
        &format!("state {} needs-review", replacement.id),
    )
    .unwrap();
    assert!(
        run_memory(
            controller,
            &format!("state {} superseded {}", old.id, replacement.id)
        )
        .is_err()
    );
    assert!(
        run_memory(
            controller,
            &format!("state {} superseded missing-id", old.id)
        )
        .is_err()
    );
    run_memory(controller, &format!("verify {}", replacement.id)).unwrap();
    match run_memory(
        controller,
        &format!("state {} superseded {}", old.id, replacement.id),
    ) {
        Ok(Reply::MemoryStateChanged(changed)) => {
            assert_eq!(changed.state, RecordState::Superseded);
            assert_eq!(changed.superseded_by, replacement.id);
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn state_and_verify_of_an_unknown_record_report_not_found() {
    let fixture = memory_fixture().await;
    for args in ["state nope retracted", "verify nope"] {
        let error = run_memory(&fixture.controller, args)
            .map(|_| ())
            .expect_err("an error");
        assert!(error.contains("not found"), "{args}: {error}");
    }
}

// Needs the store implementation of `review_at` on write.
#[tokio::test(flavor = "multi_thread")]
async fn remember_review_in_stores_the_review_deadline() {
    let fixture = memory_fixture().await;
    let request = Request::parse(
        "remember",
        "--scope user --kind note --review-in 7d keep this",
    )
    .unwrap();
    match fixture.controller.management(request) {
        Ok(Reply::Remembered(record)) => {
            let deadline = record.review_at.expect("a deadline");
            assert!(
                (deadline - Utc::now() - Duration::days(7))
                    .num_seconds()
                    .abs()
                    < SECONDS_TOLERANCE
            );
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}
