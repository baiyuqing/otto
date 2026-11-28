//! `Controller::context_report` and the session messages that are not in the
//! next request.

use otto_core::agent::context_exclusion::ExclusionReason;
use otto_core::model::{Block, FinishReason, Message, Role};
use otto_core::session::{CompactionCheckpoint, CompactionDetails, Session};

use super::*;
use crate::runtime::builder::SharedSession;

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block::text(text)],
        finish_reason: Some(FinishReason::Stop),
        created_at: chrono::Utc::now(),
        ..Message::default()
    }
}

#[tokio::test]
async fn a_session_without_a_checkpoint_reports_nothing_excluded() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let session = controller.current_session();
    assert_eq!(
        controller.context_report().expect("report").not_in_request,
        None,
        "a session without a file has nothing to compare"
    );

    session.append(user("first")).await.expect("append");
    session.append(assistant("reply")).await.expect("append");
    let excluded = controller
        .context_report()
        .expect("report")
        .not_in_request
        .expect("computed from the session file");
    assert!(excluded.items.is_empty(), "{:?}", excluded.items);
    assert_eq!(excluded.carried_user_messages, 0);
}

#[tokio::test]
async fn a_checkpoint_in_the_session_file_lists_the_summarized_messages() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = controller(workspace.path(), sessions.path()).await;
    let session = controller.current_session();
    let older = [user("old question"), assistant("old answer")];
    for message in older.clone() {
        session.append(message).await.expect("append");
    }
    session.append(user("kept question")).await.expect("append");
    let kept = session.messages()[2].id.clone();
    let lines = otto_core::agent::summary_details::carry_user_messages(&[], older.iter());
    let summary = otto_core::agent::summary_details::append_compaction_blocks(
        "summary text",
        &lines,
        &[],
        &[],
        &CompactionDetails::default(),
    )
    .expect("summary");
    session
        .append_compaction(CompactionCheckpoint {
            summary,
            first_kept_entry_id: kept,
            tokens_before: 10,
            created_at: chrono::Utc::now(),
            ..CompactionCheckpoint::default()
        })
        .await
        .expect("checkpoint");

    let excluded = controller
        .context_report()
        .expect("report")
        .not_in_request
        .expect("computed from the session file");
    let listed: Vec<_> = excluded
        .items
        .iter()
        .map(|item| (item.label.as_str(), item.reason, item.text.as_deref()))
        .collect();
    assert_eq!(
        listed,
        [(
            "session #2 assistant",
            ExclusionReason::Summarized,
            Some("old answer")
        )]
    );
    assert_eq!(excluded.carried_user_messages, 1);
}

#[tokio::test]
async fn a_session_without_a_file_has_no_comparison() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let builder = builder(workspace.path(), sessions.path());
    let runtime = initial_runtime(&builder);
    let session = SharedSession::memory(otto_core::session::Header::default());
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    let controller = Controller::new(builder, true, session.clone(), runner, info);
    session.append(user("in memory")).await.expect("append");
    let report = controller.context_report().expect("report");
    assert!(!report.sections.is_empty());
    assert_eq!(report.not_in_request, None, "no session file to read");
}
