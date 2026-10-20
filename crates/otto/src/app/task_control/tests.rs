
use super::*;
use crate::{
    app::Controller,
    cli::testutil::{builder, initial_runtime},
    runtime::builder::Runner,
    subagent::tasks::Tasks,
};
use otto_core::{
    model::{Block, BlockType, FinishReason, Message, Role},
    operation::OperationControl,
    provider::{Provider, ProviderSettlement, Request, Response, StreamSink},
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::oneshot;

#[derive(Clone, Copy)]
enum Choice {
    Steer,
    SteerFinalFailure,
    Queue,
    Reply,
}

struct ControlProvider {
    choice: Choice,
    fail_final: bool,
    main_calls: AtomicUsize,
    control_calls: AtomicUsize,
    started: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait::async_trait]
impl Provider for ControlProvider {
    async fn complete(
        &self,
        request: &Request,
        _: StreamSink<'_>,
        _: &dyn OperationControl,
    ) -> ProviderSettlement {
        let message = if request.system_prompt.starts_with("You are Otto routing") {
            let mut names: Vec<_> = request
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect();
            names.sort();
            assert_eq!(
                names,
                ["otto_help", "task_queue", "task_reply", "task_steer"]
            );
            let input = request
                .messages
                .iter()
                .map(|message| message.text())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(input.contains("original task"), "{input}");
            assert!(input.contains("progress?"), "{input}");
            let control_call = self.control_calls.fetch_add(1, Ordering::SeqCst);
            if control_call > 0 && self.fail_final {
                return ProviderSettlement::failed(
                    otto_core::provider::ProviderError::Other("fixture failure".into()),
                    1,
                    otto_core::model::EffectCertainty::NotStarted,
                );
            }
            if control_call == 0 {
                let (name, args) = match self.choice {
                    Choice::Steer | Choice::SteerFinalFailure => ("task_steer", "{}"),
                    Choice::Queue => ("task_queue", "{}"),
                    Choice::Reply => ("task_reply", "{}"),
                };
                Message {
                    role: Role::Assistant,
                    blocks: vec![Block {
                        block_type: BlockType::ToolCall,
                        tool_call_id: "route-1".into(),
                        tool_name: name.into(),
                        arguments: Some(
                            serde_json::value::RawValue::from_string(args.into()).unwrap(),
                        ),
                        ..Block::default()
                    }],
                    finish_reason: Some(FinishReason::ToolCalls),
                    ..Message::default()
                }
            } else {
                Message {
                    role: Role::Assistant,
                    blocks: vec![Block::text("read-only answer")],
                    finish_reason: Some(FinishReason::Stop),
                    ..Message::default()
                }
            }
        } else {
            let call = self.main_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                self.started
                    .lock()
                    .unwrap()
                    .take()
                    .expect("main request start signal")
                    .send(())
                    .unwrap();
                let release = self
                    .release
                    .lock()
                    .unwrap()
                    .take()
                    .expect("main request gate");
                release.await.unwrap();
            }
            if call > 0 && matches!(self.choice, Choice::Steer | Choice::SteerFinalFailure) {
                assert!(
                    request.messages.iter().any(|message| {
                        message.role == Role::User && message.text() == "progress?"
                    }),
                    "steering was not appended as the original user message"
                );
            }
            Message {
                role: Role::Assistant,
                blocks: vec![Block::text("task result")],
                finish_reason: Some(FinishReason::Stop),
                ..Message::default()
            }
        };
        ProviderSettlement::succeeded(
            Response {
                message,
                ..Response::default()
            },
            1,
        )
    }
}

#[test]
fn followups_are_bounded_fifo_and_scoped_to_the_logical_run() {
    let control = Arc::new(TaskControl::default());
    let run = control.begin("session-1", "original task").unwrap();
    assert!(control.begin("session-1", "second task").is_err());
    control
        .queue_followup("survives refused second prompt")
        .unwrap();
    for i in 0..MAX_FOLLOWUPS - 1 {
        control.queue_followup(&format!("task {i}")).unwrap();
    }
    assert!(control.queue_followup("overflow").is_err());
    assert_eq!(
        run.next_followup().map(|followup| followup.text).as_deref(),
        Some("survives refused second prompt")
    );
    drop(run);
    assert!(control.queue_followup("stale").is_err());
}

#[test]
fn followup_input_is_validated_after_trimming() {
    assert_eq!(checked_text("  task  ").unwrap(), "task");
    assert!(checked_text(" \n").is_err());
    assert!(checked_text(&"x".repeat(MAX_TEXT_BYTES + 1)).is_err());
}

#[test]
fn current_phase_preserves_the_original_objective_and_session_boundary() {
    let control = Arc::new(TaskControl::default());
    let run = control.begin("session-1", "research the topic").unwrap();
    run.set_current_task("write the final report");
    assert_eq!(
        control.original_task("session-1").as_deref(),
        Some("research the topic")
    );
    assert_eq!(
        control.current_task("session-1").as_deref(),
        Some("write the final report")
    );
    assert!(control.current_task("another-session").is_none());
    drop(run);
    let _next = control.begin("session-1", "a new task").unwrap();
    assert_eq!(
        control.current_task("session-1").as_deref(),
        Some("a new task")
    );
}

#[tokio::test]
async fn natural_control_uses_only_routing_tools_and_preserves_original_user_text() {
    for choice in [
        Choice::Steer,
        Choice::SteerFinalFailure,
        Choice::Queue,
        Choice::Reply,
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let builder = builder(workspace.path(), sessions.path());
        let runtime = initial_runtime(&builder);
        let session = builder.create_session(&runtime).unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let provider = Arc::new(ControlProvider {
            choice,
            fail_final: matches!(choice, Choice::SteerFinalFailure),
            main_calls: AtomicUsize::new(0),
            control_calls: AtomicUsize::new(0),
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(Some(release_rx)),
        });
        let runner = Runner::scripted(
            session.clone(),
            Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
            Arc::new(Tasks::new()),
        );
        let info = builder.runtime_info(&runtime);
        let controller = Arc::new(Controller::new(
            builder,
            true,
            session.clone(),
            runner,
            info,
        ));
        let active = Arc::clone(&controller);
        let turn = tokio::spawn(async move {
            let mut discard = |_| {};
            active
                .prompt("original task", &mut discard, &CancellationToken::new())
                .await
        });
        started_rx.await.unwrap();
        let reply = controller
            .control_message_with_channel("progress?", None, &CancellationToken::new())
            .await
            .unwrap()
            .expect("active control dialogue");
        match choice {
            Choice::Steer | Choice::SteerFinalFailure => {
                assert!(!reply.queued);
                assert!(reply.text.contains("Guidance accepted"));
            }
            Choice::Queue => {
                assert!(reply.queued);
                assert!(reply.text.contains("original message"));
            }
            Choice::Reply => {
                assert!(!reply.queued);
                assert_eq!(reply.text, "read-only answer");
            }
        }
        release_tx.send(()).unwrap();
        turn.await.unwrap().unwrap();
        let users: Vec<_> = session
            .messages()
            .into_iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.text())
            .collect();
        assert_eq!(users.first().map(String::as_str), Some("original task"));
        assert_eq!(
            users.iter().any(|message| message == "progress?"),
            matches!(choice, Choice::Steer | Choice::SteerFinalFailure),
            "{users:?}"
        );
    }
}

fn journaled() -> (Arc<TaskControl>, Arc<otto_core::mailbox::InMemoryMailbox>) {
    let journal = Arc::new(otto_core::mailbox::InMemoryMailbox::default());
    let control = Arc::new(TaskControl::default());
    control.set_followup_journal(journal.clone());
    (control, journal)
}

#[test]
fn followups_are_journaled_until_their_step_runs_or_the_task_ends() {
    let (control, journal) = journaled();
    let run = control.begin("s1", "task").unwrap();
    control.queue_followup("first").unwrap();
    control.queue_followup("second").unwrap();
    assert_eq!(journal.len(), 2);
    let first = run.next_followup().unwrap();
    assert!(first.input_id.starts_with("followup:") && first.input_id.ends_with(":1"));
    run.followup_done(&first);
    assert_eq!(journal.len(), 1);
    drop(run);
    assert!(journal.is_empty(), "the task's end discards the rest");
}

#[test]
fn a_resuming_run_adopts_the_interrupted_tasks_followups_in_order() {
    let (control, journal) = journaled();
    let run = control.begin("s1", "task").unwrap();
    control.queue_followup("first").unwrap();
    control.queue_followup("second").unwrap();
    // Process loss: the run never ends.
    std::mem::forget(run);

    let restarted = Arc::new(TaskControl::default());
    restarted.set_followup_journal(journal.clone());
    let run = restarted.begin_resuming("s1", "", true).unwrap();
    assert_eq!(run.discarded(), 0);
    let texts: Vec<String> = std::iter::from_fn(|| run.next_followup())
        .map(|followup| followup.text)
        .collect();
    assert_eq!(texts, ["first", "second"]);
    assert_eq!(journal.len(), 2, "entries stay until their steps run");
}

#[test]
fn a_new_task_discards_and_counts_an_unresumed_tasks_followups() {
    let (control, journal) = journaled();
    let run = control.begin("s1", "task").unwrap();
    control.queue_followup("first").unwrap();
    std::mem::forget(run);

    let restarted = Arc::new(TaskControl::default());
    restarted.set_followup_journal(journal.clone());
    let run = restarted.begin("s1", "new task").unwrap();
    assert_eq!(run.discarded(), 1);
    assert!(run.next_followup().is_none());
    assert!(journal.is_empty());
}

#[test]
fn a_followup_that_cannot_be_saved_is_not_queued() {
    let journal = Arc::new(otto_core::mailbox::InMemoryMailbox::default());
    journal.set_persist(Box::new(|_| Err("disk full".into())));
    let control = Arc::new(TaskControl::default());
    control.set_followup_journal(journal.clone());
    let run = control.begin("s1", "task").unwrap();
    assert!(control.queue_followup("first").is_err());
    assert!(run.next_followup().is_none());
    assert!(journal.is_empty());
}
