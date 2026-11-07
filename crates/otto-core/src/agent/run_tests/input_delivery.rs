//! Run-loop tests: images, steering, follow-ups, the inbox and wake turns.

use super::*;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_options_cache_key_reaches_the_provider_request() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            cache_key: "sess-1".into(),
            ..options()
        },
    );
    agent
        .run("hi", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let requests = agent.provider().normal_requests();
    assert_eq!(requests[0].cache_key, "sess-1");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn image_is_persisted_and_sent_with_the_user_prompt() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run_with_image(
            "read it",
            Some(Block::image("iVBORw0KGgo=", "image/png")),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let messages = agent.session().messages();
    assert_eq!(messages[0].blocks[0], Block::text("read it"));
    assert_eq!(messages[0].blocks[1].block_type, BlockType::Image);
    let requests = agent.provider().normal_requests();
    assert_eq!(
        requests[0].messages[0].blocks[1].block_type,
        BlockType::Image
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn image_without_text_is_persisted_and_sent() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run_with_image(
            "",
            Some(Block::image("iVBORw0KGgo=", "image/png")),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let messages = agent.session().messages();
    assert_eq!(messages[0].blocks.len(), 1);
    assert_eq!(messages[0].blocks[0].block_type, BlockType::Image);
    assert_eq!(
        agent.provider().normal_requests()[0].messages[0].blocks[0].block_type,
        BlockType::Image
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn steering_is_delivered_to_the_active_run_and_closed_before_finish() {
    let steering = Arc::new(crate::agent::steering::Steering::default());
    let provider = FakeProvider::new(vec![Turn::text("first"), Turn::text("second")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            steering: Arc::clone(&steering),
            ..options()
        },
    );
    let submitted = Arc::new(AtomicUsize::new(0));
    let submit = Arc::clone(&submitted);
    let mut ticket = None;
    agent
        .run(
            "hello",
            &mut |event| match event {
                Event::ProviderApiCall { .. } if submit.fetch_add(1, Ordering::SeqCst) == 0 => {
                    let current = steering.ticket().expect("active run ticket");
                    ticket = Some(current);
                    steering
                        .submit_for(current, "change course")
                        .expect("steer active run");
                    steering
                        .submit_for(current, "and check the result")
                        .expect("second steer active run");
                }
                Event::AgentFinished => {
                    assert_eq!(steering.ticket(), None, "completion closes acceptance");
                    assert_eq!(
                        steering.submit_for(ticket.expect("captured ticket"), "too late"),
                        Err(crate::agent::steering::SubmitError::Inactive)
                    );
                }
                _ => {}
            },
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let messages = agent.session().messages();
    assert_eq!(messages[0].text(), "hello");
    let guidance: Vec<_> = messages
        .iter()
        .filter(|message| message.role == Role::User && message.text() != "hello")
        .map(|message| message.text())
        .collect();
    assert_eq!(guidance, ["change course", "and check the result"]);
    let requests = agent.provider().normal_requests();
    assert_eq!(requests.len(), 2);
    let request_guidance: Vec<_> = requests[1]
        .messages
        .iter()
        .filter(|message| message.role == Role::User && message.text() != "hello")
        .map(|message| message.text())
        .collect();
    assert_eq!(request_guidance, ["change course", "and check the result"]);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn steering_waits_until_the_full_tool_batch_finishes() {
    let steering = Arc::new(crate::agent::steering::Steering::default());
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls(), Turn::text("done")]),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            steering: Arc::clone(&steering),
            ..options()
        },
    );
    let submitted = AtomicUsize::new(0);
    agent
        .run(
            "hello",
            &mut |event| {
                if matches!(event, Event::ToolCallStarted { .. })
                    && submitted.fetch_add(1, Ordering::SeqCst) == 0
                {
                    steering
                        .submit("change course")
                        .expect("active run accepts guidance");
                }
            },
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let messages = agent.session().messages();
    let guidance = messages
        .iter()
        .position(|message| message.role == Role::User && message.text() == "change course")
        .expect("guidance persisted");
    let second_result = messages
        .iter()
        .position(|message| message.role == Role::Tool && message.blocks[0].tool_call_id == "c2")
        .expect("second tool result persisted");
    assert!(guidance > second_result, "guidance split the tool batch");
    let requests = agent.provider().normal_requests();
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| { message.role == Role::User && message.text() == "change course" })
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn cancellation_discards_undelivered_steering() {
    let steering = Arc::new(crate::agent::steering::Steering::default());
    let cancel = CancellationToken::new();
    let agent = Agent::new(
        FakeProvider::new(vec![two_tool_calls()]),
        CancellingExecutor {
            cancel: cancel.clone(),
        },
        MemorySession::new(),
        Options {
            steering: Arc::clone(&steering),
            ..options()
        },
    );
    let submitted = AtomicUsize::new(0);
    let error = agent
        .run(
            "hello",
            &mut |event| {
                if matches!(event, Event::ToolCallStarted { .. })
                    && submitted.fetch_add(1, Ordering::SeqCst) == 0
                {
                    steering.submit("do not leak").expect("active run");
                }
            },
            &cancel,
        )
        .await
        .expect_err("tool cancellation stops the run");
    assert!(error.is_cancelled());
    assert_eq!(steering.ticket(), None);
    assert!(
        !agent
            .session()
            .messages()
            .iter()
            .any(|message| message.text() == "do not leak")
    );
    // A cancelled turn never delivers the guidance it accepted, so the loss is
    // handed to the frontend instead of disappearing.
    let reported = steering.take_discarded();
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert_eq!(reported[0].text, "do not leak");
    assert_eq!(
        reported[0].status,
        crate::agent::steering::Status::Discarded
    );
    assert!(
        steering.take_discarded().is_empty(),
        "the loss is reported exactly once"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn followup_request_is_context_not_user_authority() {
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::text("done")]),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    agent
        .run_followup_with_system_context_control(
            "continue the analysis",
            "",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("follow-up");

    let messages = agent.session().messages();
    assert_eq!(messages[0].role, Role::Context);
    assert_eq!(messages[0].context_type, "agent_followup");
    assert!(messages[0].display);
    assert_eq!(
        messages[0].text(),
        "[Agent follow-up]\ncontinue the analysis"
    );
    let request = &agent.provider().normal_requests()[0];
    assert!(
        request
            .messages
            .iter()
            .any(|message| { message.text() == "[Agent follow-up]\ncontinue the analysis" })
    );
}

// -- inbox and wake turns --------------------------------------------------

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn pending_notifications_are_delivered_before_the_user_message() {
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        task_id: "t1".into(),
        kind: Some(NotificationKind::TaskFinished),
        text: "child finished".into(),
        usage: Some(Usage {
            input_tokens: 2,
            output_tokens: 1,
            cached_input_tokens: 0,
            ..Default::default()
        }),
        ..Default::default()
    });
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    agent
        .run(
            "hello",
            &mut |event| sink.lock().expect("events").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    // A notification queued before the turn started is delivered before the
    // user's own message is appended: the provider sees it as something the
    // user's prompt responds to, not the other way around.
    let messages = agent.session().messages();
    assert_eq!(messages[0].role, Role::Context);
    assert_eq!(messages[0].context_type, "task_notification");
    assert_eq!(messages[0].text(), "child finished");
    assert!(messages[0].display);
    assert_eq!(
        messages[0]
            .context_metadata
            .as_ref()
            .expect("metadata")
            .task_id,
        "t1"
    );
    assert_eq!(messages[1].role, Role::User);
    assert!(inbox.is_empty(), "the inbox was not drained");
    assert!(
        names(&collect(&events)).contains(&"notification"),
        "no notification event was emitted"
    );
    // The notification reaches the provider on the very first request.
    assert_eq!(
        agent.provider().normal_requests()[0].messages.len(),
        2,
        "the notification was not in the first request"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn the_inbox_is_drained_again_before_the_next_provider_request() {
    let inbox = Arc::new(Inbox::default());
    let provider = FakeProvider::new(vec![
        Turn::tool_call("c1", r#"{"value":1}"#),
        Turn::text("ok"),
    ]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    // Pushed while the turn is set up; it must be delivered between the tool
    // result and the second provider call.
    inbox.push(Notification {
        task_id: "t2".into(),
        kind: Some(NotificationKind::Message),
        text: "from the parent".into(),
        usage: None,
        ..Default::default()
    });
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");

    let messages = agent.session().messages();
    let contexts: Vec<&Message> = messages
        .iter()
        .filter(|message| message.role == Role::Context)
        .collect();
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].context_type, "parent_message");
    let requests = agent.provider().normal_requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.text() == "from the parent"),
        "the notification did not reach the second request"
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn user_input_queued_during_a_provider_call_continues_the_same_turn() {
    let inbox = Arc::new(Inbox::default());
    let provider = FakeProvider::new(vec![Turn::text("first"), Turn::text("second")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    let queued = AtomicUsize::new(0);
    agent
        .run(
            "start",
            &mut |event| {
                if matches!(event, Event::TextDelta { .. })
                    && queued.fetch_add(1, Ordering::SeqCst) == 0
                {
                    inbox.push(Notification {
                        kind: Some(NotificationKind::UserMessage),
                        text: "change course".into(),
                        ..Notification::default()
                    });
                }
            },
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let messages = agent.session().messages();
    assert_eq!(
        messages
            .iter()
            .map(|message| &message.role)
            .collect::<Vec<_>>(),
        [&Role::User, &Role::Assistant, &Role::User, &Role::Assistant]
    );
    assert_eq!(messages[2].text(), "change course");
    assert_eq!(agent.provider().normal_requests().len(), 2);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn empty_text_without_notifications_fails() {
    let provider = FakeProvider::new(Vec::new());
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .run(
            "  \t\n",
            &mut |event| sink.lock().expect("e").push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("empty text is rejected");
    assert!(error.is_empty_user_text(), "unexpected error: {error}");
    assert_eq!(names(&collect(&events)), ["agent_error"]);
    assert!(agent.provider().requests().is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn empty_text_with_a_notification_is_a_wake_turn() {
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        task_id: "t3".into(),
        kind: Some(NotificationKind::TaskReport),
        text: "report".into(),
        usage: None,
        ..Default::default()
    });
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("a wake turn runs");
    let messages = agent.session().messages();
    assert_eq!(messages[0].role, Role::Context, "no user message is stored");
    assert_eq!(messages[0].context_type, "task_notification");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
async fn a_notification_without_a_generated_task_id_still_persists() {
    let inbox = Arc::new(Inbox::default());
    inbox.push(Notification {
        task_id: String::new(),
        kind: Some(NotificationKind::Message),
        text: "[feishu] hello".into(),
        usage: None,
        ..Default::default()
    });
    inbox.push(Notification {
        task_id: "timer".into(),
        text: "[timer] due".into(),
        usage: None,
        kind: None,
        ..Default::default()
    });
    let provider = FakeProvider::new(vec![Turn::text("ok")]);
    let agent = Agent::new(
        provider,
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            mailbox: inbox.clone(),
            ..options()
        },
    );
    agent
        .run("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("a wake turn runs");
    let contexts: Vec<_> = agent
        .session()
        .messages()
        .into_iter()
        .filter(|message| message.role == Role::Context)
        .collect();
    assert_eq!(contexts.len(), 2);
    assert!(
        contexts
            .iter()
            .all(|message| message.context_metadata.is_none()),
        "ungenerated task ids must not become context metadata: {contexts:?}"
    );
    assert_eq!(contexts[0].context_type, "parent_message");
    assert_eq!(contexts[1].context_type, "task_notification");
}
