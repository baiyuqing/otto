//! Rendering of the `/memory` history, diff, restore and forget replies.

use super::*;
use crate::app::management::Reply;
use crate::memory::{
    CopyStatus, ForgetCleanupEntry, Origin, Provenance, Record, RecordHistory, Scope, Tombstone,
};
use chrono::{DateTime, Utc};

fn timestamp() -> DateTime<Utc> {
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
        created_at: timestamp(),
        updated_at: timestamp(),
        ..Record::default()
    }
}

#[test]
fn the_memory_usage_names_history_diff_and_restore() {
    for form in [
        "/memory history <id>",
        "/memory diff <id> <from> [<to>]",
        "/memory restore <id> <revision>",
    ] {
        assert!(
            MEMORY_USAGE.contains(form),
            "usage lacks {form}: {MEMORY_USAGE}"
        );
    }
}

#[test]
fn a_forgotten_reply_keeps_its_first_line_and_adds_one_line_per_cleanup_entry() {
    let reply = Reply::MemoryForgotten {
        tombstone: Tombstone {
            id: "r1".into(),
            scope: Scope::new("user", "u1"),
            revision: 3,
            created_at: timestamp(),
            updated_at: timestamp(),
            forgotten_at: timestamp(),
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
            created_at: timestamp(),
            updated_at: timestamp(),
            forgotten_at: timestamp(),
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

#[tokio::test]
async fn the_repl_runs_history_diff_and_restore_against_the_store() {
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
        "/remember --scope user --kind note --key editor vim\n\
         /remember --scope user --kind note --key editor emacs\n/exit\n",
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
        &format!("/memory history {id}\n/memory diff {id} 1 2\n/memory restore {id} 1\n/exit\n"),
        &controller,
    )
    .await;

    assert!(stderr.is_empty(), "{stderr}");
    assert!(stdout.contains("revision=1 "), "{stdout}");
    assert!(stdout.contains("revision=2 "), "{stdout}");
    assert!(stdout.contains("current=yes"), "{stdout}");
    assert!(
        stdout.contains("- vim") && stdout.contains("+ emacs"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("restored {id} as revision 3 (from revision 1)")),
        "{stdout}"
    );
}
