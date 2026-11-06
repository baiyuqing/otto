//! Rendering of the record lifecycle fields and of the state and verify replies.

use super::*;
use crate::app::management::Reply;
use crate::memory::{Record, RecordState, Scope};
use chrono::{DateTime, Utc};

fn timestamp(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("timestamp")
}

fn record_in_state(state: RecordState) -> Record {
    Record {
        id: "r1".into(),
        scope: Scope::new("user", "u1"),
        kind: "note".into(),
        key: "editor".into(),
        text: "vim".into(),
        confidence: 1.0,
        revision: 3,
        created_at: timestamp(1_700_000_000),
        updated_at: timestamp(1_700_000_000),
        state,
        ..Record::default()
    }
}

#[test]
fn state_changed_reply_names_state_revision_replacement_and_reason() {
    let mut record = record_in_state(RecordState::NeedsReview);
    assert_eq!(
        format_management_reply(&Reply::MemoryStateChanged(record.clone())),
        "state of r1 is now needs_review (revision 3)"
    );
    record.state = RecordState::Superseded;
    record.superseded_by = "r2".into();
    record.state_reason = "newer".into();
    assert_eq!(
        format_management_reply(&Reply::MemoryStateChanged(record)),
        "state of r1 is now superseded (revision 3) superseded_by=r2 reason=newer"
    );
}

#[test]
fn verified_reply_shows_verified_at_and_review_at_or_none() {
    let mut record = record_in_state(RecordState::Active);
    record.verified_at = Some(timestamp(1_700_000_100));
    let without_deadline = format_management_reply(&Reply::MemoryVerified(record.clone()));
    assert_eq!(
        without_deadline,
        format!(
            "verified r1 (revision 3) verified_at={} review_at=none",
            timestamp(1_700_000_100)
        )
    );
    record.review_at = Some(timestamp(1_800_000_000));
    let with_deadline = format_management_reply(&Reply::MemoryVerified(record));
    assert!(
        with_deadline.ends_with(&format!("review_at={}", timestamp(1_800_000_000))),
        "{with_deadline}"
    );
}

#[test]
fn list_rendering_prints_state_and_time_fields() {
    let mut record = record_in_state(RecordState::Superseded);
    record.superseded_by = "r2".into();
    record.state_reason = "newer".into();
    record.verified_at = Some(timestamp(1_700_000_100));
    record.review_at = Some(timestamp(1_800_000_000));
    let page = crate::memory::RecordPage {
        records: vec![record],
        next_cursor: String::new(),
    };
    let text = format_management_reply(&Reply::MemoryList(page));
    for field in [
        "state=superseded superseded_by=r2 state_reason=newer ".to_string(),
        format!("verified_at={} ", timestamp(1_700_000_100)),
        format!("review_at={} ", timestamp(1_800_000_000)),
    ] {
        assert!(text.contains(&field), "lacks {field:?}: {text}");
    }
}

#[test]
fn list_rendering_of_an_active_record_omits_replacement_and_reason() {
    let page = crate::memory::RecordPage {
        records: vec![record_in_state(RecordState::Active)],
        next_cursor: String::new(),
    };
    let text = format_management_reply(&Reply::MemoryList(page));
    assert!(text.contains(" state=active created_at="), "{text}");
    assert!(
        !text.contains("superseded_by=") && !text.contains("state_reason="),
        "{text}"
    );
    assert!(text.contains("verified_at= review_at= text=vim"), "{text}");
}

#[test]
fn the_memory_usage_names_state_verify_and_the_list_filters() {
    for form in [
        "/memory state <id> active|needs-review|retracted [--reason TEXT]",
        "/memory state <id> superseded <replacement-id> [--reason TEXT]",
        "/memory verify <id> [--review-in AGE]",
        "[--state active|needs-review|superseded|retracted|all]",
        "[--unverified] [--review-due]",
    ] {
        assert!(
            MEMORY_USAGE.contains(form),
            "usage lacks {form}: {MEMORY_USAGE}"
        );
    }
    assert!(
        REMEMBER_USAGE.contains("[--review-in AGE]"),
        "{REMEMBER_USAGE}"
    );
}

// Needs the store implementation of `set_state` and `verify`.
#[tokio::test]
async fn the_repl_runs_state_and_verify_against_the_store() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let store = tempfile::tempdir().expect("store");
    let controller = controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;
    let (stdout, stderr) = session(
        "/remember --scope user --kind note --key editor vim\n/exit\n",
        &controller,
    )
    .await;
    assert!(stderr.is_empty(), "{stderr}");
    let id = stdout
        .split_whitespace()
        .skip_while(|word| *word != "remembered")
        .nth(1)
        .expect("record id")
        .to_owned();
    let (stdout, stderr) = session(
        &format!("/memory state {id} needs-review\n/memory list --scope user\n/memory list --scope user --state needs-review\n/memory verify {id}\n/exit\n"),
        &controller,
    )
    .await;
    assert!(stderr.is_empty(), "{stderr}");
    assert!(
        stdout.contains(&format!("state of {id} is now needs_review (revision 2)")),
        "{stdout}"
    );
    assert!(stdout.contains("no records"), "{stdout}");
    assert!(stdout.contains("state=needs_review"), "{stdout}");
    assert!(
        stdout.contains(&format!("verified {id} (revision 3)")),
        "{stdout}"
    );
}

fn proposed_candidate() -> crate::memory::Candidate {
    crate::memory::Candidate {
        id: "c1".into(),
        proposed: Record::default(),
        action: crate::memory::CandidateAction::Retract,
        target_id: "r1".into(),
        base_revision: 3,
        reason: "the source was withdrawn".into(),
        state: crate::memory::CandidateState::Pending,
        created_at: timestamp(1_700_000_000),
        decided_at: None,
        decision_source: None,
        result_record_id: String::new(),
        result_revision: 0,
    }
}

#[test]
fn proposed_reply_renders_each_outcome_variant() {
    use crate::memory::proposals::Outcome;
    let render = |outcome| format_management_reply(&Reply::MemoryProposed(outcome));
    assert_eq!(
        render(Outcome::NoChange {
            explanation: "evidence does not apply".into()
        }),
        "no change proposed: evidence does not apply"
    );
    assert_eq!(
        render(Outcome::Proposed(Box::new(proposed_candidate()))),
        "proposed retract of r1 as candidate c1 (review with /memory review c1 accept|reject): the source was withdrawn"
    );
    assert_eq!(
        render(Outcome::Rejected("no supporting evidence".into())),
        "proposal rejected: no supporting evidence"
    );
    assert_eq!(
        render(Outcome::Unavailable("timed out".into())),
        "proposal unavailable: timed out"
    );
}
