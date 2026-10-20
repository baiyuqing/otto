//! Wake-turn reply queueing tests.

use super::*;

#[tokio::test]
async fn a_wake_turn_queues_its_text_with_a_blank_line_after_a_tool_call() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = wake_controller(
        workspace.path(),
        sessions.path(),
        vec![("checking  ", true), ("all done", false)],
    )
    .await;
    let mut seen = Vec::new();
    let wake = controller.prepare_wake().unwrap().expect("wake claimed");
    controller
        .run_with_channel(
            Step::Wake(wake),
            None,
            None,
            &mut |event| seen.push(event),
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap();
    assert!(
        seen.iter()
            .any(|event| matches!(event, Event::TextDelta { text } if text == "all done")),
        "events still reach the caller's sink"
    );
    assert_eq!(
        controller.wake_replies_since(0),
        (vec![(1, "checking\n\nall done".to_string())], 1)
    );
}

#[tokio::test]
async fn a_wake_turn_without_text_queues_nothing() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = wake_controller(workspace.path(), sessions.path(), vec![("  ", false)]).await;
    assert_eq!(
        run_wake(&controller, &CancellationToken::new()).await,
        Stop::EndTurn
    );
    assert_eq!(controller.wake_replies_since(0), (Vec::new(), 0));
}

#[tokio::test]
async fn a_cancelled_wake_turn_queues_nothing() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller =
        wake_controller(workspace.path(), sessions.path(), vec![("partial", false)]).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(run_wake(&controller, &cancel).await, Stop::Cancelled);
    assert_eq!(controller.wake_replies_since(0), (Vec::new(), 0));
}

#[tokio::test]
async fn a_prompt_turn_queues_no_wake_reply() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = wake_controller(
        workspace.path(),
        sessions.path(),
        vec![("an answer", false)],
    )
    .await;
    controller
        .run_with_approvals(
            Step::Text("hello"),
            &mut |_| {},
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap();
    assert_eq!(controller.wake_replies_since(0), (Vec::new(), 0));
}
