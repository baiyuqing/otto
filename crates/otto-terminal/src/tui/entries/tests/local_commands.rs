use super::*;
use otto_core::session::local_command::{LocalCommandReceipt, LocalCommandStatus};

#[test]
fn local_command_receipts_are_distinct_foldable_rows_with_collapsed_details() {
    let history = vec![
        Message {
            role: Role::User,
            blocks: vec![Block::text("before")],
            ..Default::default()
        },
        Message {
            role: Role::Assistant,
            blocks: vec![Block::text("after")],
            ..Default::default()
        },
    ];
    let (mut entries, _) = entries_from_history(&history);
    let receipt = LocalCommandReceipt {
        id: "stable-invocation".into(),
        command: "/memory show".into(),
        parameters: "id=full-memory-id".into(),
        status: LocalCommandStatus::Success,
        summary: "full-memory-id revision=7".into(),
        output: "expand-only-output".into(),
        created_at: "2026-11-07T00:00:00Z".into(),
        after_messages: 1,
    };
    project_local_commands(&mut entries, std::slice::from_ref(&receipt));
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[1].kind, Some(EntryKind::LocalCommand));
    assert_eq!(entries[1].id, "local-command-stable-invocation");
    let folded = crate::tui::transcript::entry_lines(&entries[1], false, 120);
    let folded = format!("{folded:?}");
    assert!(folded.contains("›_"));
    assert!(folded.contains("full-memory-id"));
    assert!(!folded.contains("expand-only-output"));
    let expanded = format!(
        "{:?}",
        crate::tui::transcript::entry_lines(&entries[1], true, 120)
    );
    assert!(expanded.contains("expand-only-output"));
    assert!(expanded.contains("Parameters:"));
    project_local_commands(&mut entries, &[receipt]);
    assert_eq!(
        entries.len(),
        3,
        "replay must replace, not duplicate the receipt"
    );
}

#[test]
fn no_session_receipts_remain_in_memory_and_outside_messages() {
    use otto_core::session::Session;
    use otto_session::SessionHandle;
    let session = otto::runtime::builder::MemoryHandle::new(otto_core::session::Header::default());
    let receipt = LocalCommandReceipt {
        id: "memory-only".into(),
        command: "/memory list".into(),
        parameters: String::new(),
        status: LocalCommandStatus::Pending,
        summary: "interrupted outcome pending".into(),
        output: String::new(),
        created_at: "2026-11-07T00:00:00Z".into(),
        after_messages: 0,
    };
    session
        .append_local_command(&serde_json::to_string(&receipt).unwrap())
        .unwrap();
    assert_eq!(session.local_commands(), vec![receipt]);
    assert!(session.path().is_empty());
    assert!(session.messages().is_empty());
    assert!(session.model_messages().is_empty());
}
