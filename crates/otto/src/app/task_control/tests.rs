use super::*;
use crate::{
    app::Controller,
    fixtures::{builder, initial_runtime},
    runtime::builder::Runner,
    subagent::tasks::Tasks,
};
use otto_core::{
    model::{Block, BlockType, FinishReason, Message, Role},
    operation::OperationControl,
    provider::{Provider, ProviderSettlement, Request, Response, StreamSink},
};
use std::sync::Mutex;
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
