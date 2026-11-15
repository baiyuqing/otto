//! Tests of the plain-text rendering of management replies.

use super::*;
use crate::app::management::Reply;
use crate::memory::{
    CopyStatus, ForgetCleanupEntry, Origin, Provenance, Record, RecordHistory, RecordState, Scope,
    Tombstone,
};
use crate::skill;
use chrono::{DateTime, Utc};
use std::path::Path;

fn standalone_service() -> (tempfile::TempDir, crate::memory::Service) {
    let (directory, store) = crate::memory::files::testsupport::open_temp();
    (
        directory,
        crate::memory::Service::from_store(
            std::sync::Arc::new(store),
            crate::memory::decide_default_policy,
        ),
    )
}

#[test]
fn pending_candidates_show_the_advice_line_under_the_candidate() {
    let (_directory, service) = standalone_service();
    let scope = crate::memory::Scope::new(crate::memory::NAMESPACE_USER, "user-1");
    let advised = crate::memory::advice::testing::candidate(&service, scope.clone(), true);

    let text = render_pending_candidates(std::slice::from_ref(&advised));

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(
        lines[1].starts_with(&format!("id={}", advised.id)),
        "{text}"
    );
    assert_eq!(
        lines[2],
        format!("  {}", crate::memory::advice::testing::LINE)
    );
}

#[test]
fn pending_candidates_without_advice_render_exactly_one_line_each() {
    let (_directory, service) = standalone_service();
    let scope = crate::memory::Scope::new(crate::memory::NAMESPACE_USER, "user-1");
    let plain = crate::memory::advice::testing::candidate(&service, scope, false);

    let text = render_pending_candidates(std::slice::from_ref(&plain));

    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(!text.contains("Advice"), "{text}");
}

#[test]
fn search_results_render_as_plain_text() {
    assert_eq!(
        render_search_result(&SearchResult::default()),
        "no matching records"
    );
    let record = crate::memory::Record {
        id: "rec-1".into(),
        scope: Scope {
            namespace: "workspace".into(),
            id: "w1".into(),
        },
        kind: "preference".into(),
        key: "editor".into(),
        text: "uses vim".into(),
        revision: 1,
        ..crate::memory::Record::default()
    };
    assert_eq!(
        render_search_result(&SearchResult {
            records: vec![record],
            ..SearchResult::default()
        }),
        "1 records:\nid=rec-1 scope=workspace/w1 kind=preference key=editor revision=1 text=uses vim"
    );
}

fn fixed_timestamp() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp")
}

fn record_at(revision: u64, text: &str) -> Record {
    Record {
        id: "r1".into(),
        scope: Scope::new("user", "u1"),
        kind: "note".into(),
        key: "editor".into(),
        text: text.into(),
        source: Provenance {
            origin: Some(Origin::Human),
            ..Provenance::default()
        },
        confidence: 1.0,
        revision,
        created_at: fixed_timestamp(),
        updated_at: fixed_timestamp(),
        ..Record::default()
    }
}

#[test]
fn a_forgotten_reply_keeps_its_first_line_and_adds_one_line_per_cleanup_entry() {
    let reply = Reply::MemoryForgotten {
        tombstone: Tombstone {
            id: "r1".into(),
            scope: Scope::new("user", "u1"),
            revision: 3,
            created_at: fixed_timestamp(),
            updated_at: fixed_timestamp(),
            forgotten_at: fixed_timestamp(),
        },
        previous_revision: 3,
        cleanup: vec![
            ForgetCleanupEntry {
                copy: "history/r1".into(),
                status: CopyStatus::Removed,
                detail: "2 revision files deleted".into(),
            },
            ForgetCleanupEntry {
                copy: "session files".into(),
                status: CopyStatus::Present,
                detail: "not deleted".into(),
            },
        ],
    };

    let text = format_management_reply(&reply);
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(
        lines,
        [
            "forgot r1 (revision 3)",
            "  removed: history/r1 (2 revision files deleted)",
            "  present: session files (not deleted)",
        ]
    );
}

#[test]
fn a_forgotten_reply_without_cleanup_entries_is_one_line() {
    let reply = Reply::MemoryForgotten {
        tombstone: Tombstone {
            id: "r1".into(),
            scope: Scope::new("user", "u1"),
            revision: 1,
            created_at: fixed_timestamp(),
            updated_at: fixed_timestamp(),
            forgotten_at: fixed_timestamp(),
        },
        previous_revision: 1,
        cleanup: Vec::new(),
    };
    assert_eq!(format_management_reply(&reply), "forgot r1 (revision 1)");
}

#[test]
fn a_restored_reply_names_the_new_and_the_source_revision() {
    let mut restored = record_at(4, "vim");
    restored.metadata.insert(
        crate::memory::RESTORED_FROM_REVISION_METADATA.into(),
        "2".into(),
    );
    assert_eq!(
        format_management_reply(&Reply::MemoryRestored(restored)),
        "restored r1 as revision 4 (from revision 2)"
    );
}

#[test]
fn history_and_diff_replies_use_the_history_renderers() {
    let history = RecordHistory {
        revisions: vec![record_at(1, "vim"), record_at(2, "emacs")],
        skipped: Vec::new(),
    };
    assert_eq!(
        format_management_reply(&Reply::MemoryHistory(history.clone())),
        crate::memory::history::render_history(&history)
    );
    let diff = Reply::MemoryDiff {
        from: Box::new(record_at(1, "vim")),
        to: Box::new(record_at(2, "emacs")),
    };
    let text = format_management_reply(&diff);
    assert_eq!(
        text,
        crate::memory::history::render_comparison(&record_at(1, "vim"), &record_at(2, "emacs"))
    );
    assert!(text.contains("- vim") && text.contains("+ emacs"), "{text}");
}

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

fn contracted_skill(directory: &Path) -> skill::Skill {
    skill::Skill {
        name: "reviewer".to_string(),
        description: "reviews things".to_string(),
        contract: Some(skill::Contract {
            input: "a path".to_string(),
            output: "the result".to_string(),
        }),
        directory: directory.to_path_buf(),
        path: directory.join("SKILL.md"),
    }
}

#[test]
fn skill_list_line_keeps_the_name_and_contract_but_omits_the_description() {
    let dir = tempfile::tempdir().expect("tempdir");
    let skill = contracted_skill(dir.path());

    assert_eq!(skill_list_line(&skill), "\n- reviewer [contract]");
    assert!(!skill_list_line(&skill).contains(&skill.description));
}

#[test]
fn an_empty_server_list_reports_none_configured() {
    assert_eq!(format_mcp_report(&[]), "No MCP servers configured.");
}

#[test]
fn each_server_state_and_era_renders_its_own_line() {
    use crate::mcp::ServerStatus;

    let servers = vec![
        ServerStatus {
            name: "docs".to_string(),
            transport: "http",
            era: Some(Era::Modern),
            state: ServerState::Connected { tools: 3 },
        },
        ServerStatus {
            name: "legacy-tool".to_string(),
            transport: "http",
            era: Some(Era::Legacy("2025-11-25".to_string())),
            state: ServerState::NeedsLogin,
        },
        ServerStatus {
            name: "shell".to_string(),
            transport: "stdio",
            era: None,
            state: ServerState::Disabled,
        },
        ServerStatus {
            name: "broken".to_string(),
            transport: "stdio",
            era: None,
            state: ServerState::Failed("spawn failed: not found".to_string()),
        },
    ];

    assert_eq!(
        format_mcp_report(&servers),
        "MCP servers:\n\
             - docs: connected (3 tools) (http, modern)\n\
             - legacy-tool: needs login (http, legacy 2025-11-25)\n\
             - shell: disabled (stdio, -)\n\
             - broken: failed: spawn failed: not found (stdio, -)"
    );
}
