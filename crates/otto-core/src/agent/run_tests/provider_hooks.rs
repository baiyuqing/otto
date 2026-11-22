//! Run-loop tests: the `before_provider_call` and `after_provider_call` hooks.

use super::*;
use crate::agent::hooks::{Decision, Edit, Hooks, Response as HookResponse};
use serde_json::{Value, json};

type BeforeResponder = Box<dyn Fn(&Value) -> Result<HookResponse, String> + Send + Sync>;

/// Records every hook call. `before_provider_call` answers from `before`,
/// which sees the event data; `after_provider_call` answers with `after`.
#[derive(Default)]
struct ProviderHooks {
    calls: Mutex<Vec<(String, Value)>>,
    before: Option<BeforeResponder>,
    after: Option<Result<HookResponse, String>>,
    /// Events `handles` reports; `None` reports every event.
    handled: Option<&'static [&'static str]>,
}

impl ProviderHooks {
    fn answering(
        before: impl Fn(&Value) -> Result<HookResponse, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            before: Some(Box::new(before)),
            ..Self::default()
        }
    }

    fn data(&self, event: &str) -> Vec<Value> {
        let calls = self.calls.lock().expect("calls");
        calls
            .iter()
            .filter(|(name, _)| name == event)
            .map(|(_, data)| data.clone())
            .collect()
    }

    fn events(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl Hooks for ProviderHooks {
    fn handles(&self, event: &str) -> bool {
        self.handled.is_none_or(|handled| handled.contains(&event))
    }

    async fn run(
        &self,
        event: &str,
        data: Value,
        _: &dyn OperationControl,
    ) -> Result<HookResponse, String> {
        self.calls
            .lock()
            .expect("calls")
            .push((event.into(), data.clone()));
        match (event, &self.before, &self.after) {
            ("before_provider_call", Some(before), _) => before(&data),
            ("after_provider_call", _, Some(after)) => after.clone(),
            _ => Ok(HookResponse::default()),
        }
    }
}

fn hooked_agent(
    hooks: &Arc<ProviderHooks>,
    turns: Vec<Turn>,
    session: TurnRecordingSession,
) -> Agent<FakeProvider, EchoExecutor, TurnRecordingSession> {
    Agent::new(
        FakeProvider::new(turns),
        EchoExecutor::default(),
        session,
        Options {
            hooks: Some(hooks.clone()),
            ..options()
        },
    )
}

fn provider_request_records(session: &TurnRecordingSession) -> Vec<Value> {
    session
        .customs
        .lock()
        .expect("customs")
        .iter()
        .filter(|(custom_type, _)| custom_type == "otto.hook.provider_request")
        .map(|(_, data)| serde_json::from_str(data).expect("record JSON"))
        .collect()
}

fn notifications(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Notification { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A session with a tool call, its result and a context message before the next prompt.
async fn mixed_session() -> TurnRecordingSession {
    let session = TurnRecordingSession::default();
    let messages = [
        Message {
            id: "u0".into(),
            role: Role::User,
            blocks: vec![Block::text("first")],
            ..Message::default()
        },
        Message {
            id: "a0".into(),
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: "call-0".into(),
                tool_name: "echo".into(),
                arguments: Some(raw(r#"{"value":1}"#)),
                ..Block::default()
            }],
            ..Message::default()
        },
        Message {
            id: "t0".into(),
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_call_id: "call-0".into(),
                tool_name: "echo".into(),
                text: "echoed".into(),
                ..Block::default()
            }],
            ..Message::default()
        },
        Message {
            id: "a1".into(),
            role: Role::Assistant,
            blocks: vec![Block::text("answer")],
            ..Message::default()
        },
        Message {
            id: "c0".into(),
            role: Role::Context,
            context_type: "task_notification".into(),
            blocks: vec![Block::text("task done")],
            ..Message::default()
        },
    ];
    for message in messages {
        session.append(message).await.expect("append");
    }
    session
}

#[tokio::test]
async fn the_payload_lists_every_block_and_the_changes_reach_only_that_request() {
    let hooks = Arc::new(ProviderHooks::answering(|data| {
        let messages = data["messages"].as_array().expect("messages");
        let last = messages.len() - 1;
        Ok(HookResponse {
            system_prompt: Some("replaced prompt".into()),
            context: Some("plugin note".into()),
            edits: Some(vec![Edit {
                message: last,
                block: 0,
                text: "edited prompt".into(),
            }]),
            notice: Some("request inspected".into()),
            ..HookResponse::default()
        })
    }));
    let agent = hooked_agent(&hooks, vec![Turn::text("ok")], mixed_session().await);
    let mut events = Vec::new();
    agent
        .run(
            "hello",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("run");

    let before = hooks.data("before_provider_call");
    assert_eq!(before.len(), 1);
    let data = &before[0];
    assert_eq!(data["model"], "test-model");
    assert_eq!(data["system_prompt"], "be brief");
    assert_eq!(data["tools"], json!(["echo"]));
    let messages = data["messages"].as_array().expect("messages");
    let request = &agent.provider().requests()[0];
    // One payload entry per request message and per block, in order.
    assert_eq!(messages.len(), request.messages.len());
    for (entry, message) in messages.iter().zip(&request.messages) {
        assert_eq!(
            entry["blocks"].as_array().expect("blocks").len(),
            message.blocks.len()
        );
    }
    assert_eq!(
        messages[0],
        json!({"role": "user", "blocks": [{"type": "text", "text": "first"}]})
    );
    assert_eq!(
        messages[1]["blocks"][0],
        json!({"type": "tool_call", "id": "call-0", "name": "echo", "arguments": {"value": 1}})
    );
    assert_eq!(
        messages[2]["blocks"][0],
        json!({"type": "tool_result", "id": "call-0", "name": "echo", "text": "echoed", "is_error": false})
    );
    assert!(
        messages
            .iter()
            .any(|entry| entry["context_type"] == "task_notification" && entry["role"] == "context")
    );
    assert!(
        messages
            .iter()
            .filter(|entry| entry["role"] == "user")
            .all(|entry| entry.get("context_type").is_none())
    );

    assert_eq!(
        request.system_prompt,
        "replaced prompt\n<plugin_context>Untrusted plugin context:\nplugin note\n</plugin_context>"
    );
    assert_eq!(
        request.messages.last().expect("prompt").text(),
        "edited prompt"
    );
    // The session keeps the original prompt: the changes are request-local.
    assert!(
        agent
            .session()
            .messages()
            .iter()
            .any(|message| message.text() == "hello")
    );
    assert!(
        agent
            .session()
            .messages()
            .iter()
            .all(|message| message.text() != "edited prompt")
    );
    assert!(notifications(&events).contains(&"[hook] request inspected".to_owned()));

    let records = provider_request_records(agent.session());
    assert_eq!(
        records,
        vec![json!({
            "version": 1,
            "operation_id": data["operation_id"],
            "edits": [{"message": messages.len() - 1, "block": 0, "text": "edited prompt"}],
            "system_prompt": "replaced prompt",
            "context": "plugin note",
        })]
    );
    // The provider call record and the after hook name the same operation.
    assert_eq!(
        agent.session().calls()[0].operation_id,
        data["operation_id"].as_str().expect("id")
    );
    assert_eq!(
        hooks.data("after_provider_call")[0]["operation_id"],
        data["operation_id"]
    );
}

#[tokio::test]
async fn the_audit_record_holds_only_changed_values_and_is_absent_without_changes() {
    // An identical system prompt and an identical edit change nothing.
    let unchanged = Arc::new(ProviderHooks::answering(|data| {
        let last = data["messages"].as_array().expect("messages").len() - 1;
        Ok(HookResponse {
            system_prompt: Some("be brief".into()),
            edits: Some(vec![Edit {
                message: last,
                block: 0,
                text: "hello".into(),
            }]),
            ..HookResponse::default()
        })
    }));
    let agent = hooked_agent(
        &unchanged,
        vec![Turn::text("ok")],
        TurnRecordingSession::default(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    assert!(provider_request_records(agent.session()).is_empty());
    assert_eq!(agent.provider().requests()[0].system_prompt, "be brief");

    let context_only = Arc::new(ProviderHooks::answering(|_| {
        Ok(HookResponse {
            context: Some("note".into()),
            ..HookResponse::default()
        })
    }));
    let agent = hooked_agent(
        &context_only,
        vec![Turn::text("ok")],
        TurnRecordingSession::default(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let records = provider_request_records(agent.session());
    assert_eq!(records.len(), 1);
    let keys: Vec<&String> = records[0].as_object().expect("object").keys().collect();
    assert_eq!(keys, ["context", "operation_id", "version"]);
}

#[tokio::test]
async fn a_denial_or_a_failing_hook_stops_the_turn_before_the_provider() {
    let cases: [(BeforeResponder, &str); 2] = [
        (
            Box::new(|_| {
                Ok(HookResponse {
                    decision: Some(Decision::Deny),
                    reason: Some("blocked by policy".into()),
                    ..HookResponse::default()
                })
            }),
            "blocked by policy",
        ),
        (Box::new(|_| Err("hook crashed".into())), "hook crashed"),
    ];
    for (before, expected) in cases {
        let hooks = Arc::new(ProviderHooks {
            before: Some(before),
            ..ProviderHooks::default()
        });
        let agent = hooked_agent(
            &hooks,
            vec![Turn::text("ok")],
            TurnRecordingSession::default(),
        );
        let error = agent
            .run("hello", &mut |_| {}, &CancellationToken::new())
            .await
            .expect_err("turn fails");
        assert!(error.to_string().contains(expected), "{error}");
        assert!(agent.provider().requests().is_empty());
        assert!(agent.session().calls().is_empty());
        assert!(!hooks.events().contains(&"after_provider_call".to_owned()));
    }
}

#[tokio::test]
async fn invalid_edits_fail_the_turn_without_applying_any() {
    // Each picks edits from the payload; the first edit of each case is valid.
    type Picker = fn(&[Value]) -> Vec<Edit>;
    let cases: [(&str, Picker); 5] = [
        ("message", |m| {
            vec![
                Edit {
                    message: m.len() - 1,
                    block: 0,
                    text: "x".into(),
                },
                Edit {
                    message: m.len(),
                    block: 0,
                    text: "x".into(),
                },
            ]
        }),
        ("block", |m| {
            vec![Edit {
                message: m.len() - 1,
                block: 1,
                text: "x".into(),
            }]
        }),
        ("not a text block", |m| {
            let tool_call = m
                .iter()
                .position(|entry| entry["blocks"][0]["type"] == "tool_call")
                .expect("tool call");
            vec![Edit {
                message: tool_call,
                block: 0,
                text: "x".into(),
            }]
        }),
        ("context_type", |m| {
            let context = m
                .iter()
                .position(|entry| entry.get("context_type").is_some())
                .expect("context message");
            vec![Edit {
                message: context,
                block: 0,
                text: "x".into(),
            }]
        }),
        ("twice", |m| {
            vec![
                Edit {
                    message: m.len() - 1,
                    block: 0,
                    text: "x".into(),
                },
                Edit {
                    message: m.len() - 1,
                    block: 0,
                    text: "y".into(),
                },
            ]
        }),
    ];
    for (expected, picker) in cases {
        let hooks = Arc::new(ProviderHooks::answering(move |data| {
            let messages = data["messages"].as_array().expect("messages");
            Ok(HookResponse {
                system_prompt: Some("replaced".into()),
                edits: Some(picker(messages)),
                ..HookResponse::default()
            })
        }));
        let agent = hooked_agent(&hooks, vec![Turn::text("ok")], mixed_session().await);
        let error = agent
            .run("hello", &mut |_| {}, &CancellationToken::new())
            .await
            .expect_err(expected);
        assert!(error.to_string().contains(expected), "{expected}: {error}");
        assert!(agent.provider().requests().is_empty(), "{expected}");
        assert!(
            provider_request_records(agent.session()).is_empty(),
            "{expected}"
        );
    }
}

#[tokio::test]
async fn hook_text_passes_the_secret_guard_and_secrets_never_reach_the_hook_or_audit() {
    const SECRET: &str = "sk-live-abcdef";
    let hooks = Arc::new(ProviderHooks::answering(|data| {
        let last = data["messages"].as_array().expect("messages").len() - 1;
        Ok(HookResponse {
            system_prompt: Some(format!("prompt {SECRET}")),
            context: Some(format!("context {SECRET}")),
            edits: Some(vec![Edit {
                message: last,
                block: 0,
                text: format!("edit {SECRET}"),
            }]),
            ..HookResponse::default()
        })
    }));
    let agent = Agent::with_redactor(
        FakeProvider::new(vec![Turn::text("ok")]),
        EchoExecutor::default(),
        TurnRecordingSession::default(),
        Options {
            hooks: Some(hooks.clone()),
            ..options()
        },
        Redactor::new(&[SECRET.to_owned()]),
    );
    agent
        .run(
            &format!("use {SECRET}"),
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("the guard redacts the known secret");
    let before = hooks.data("before_provider_call");
    assert_eq!(before.len(), 1);
    assert!(
        !before[0].to_string().contains(SECRET),
        "the hook input held the secret"
    );
    for request in agent.provider().requests() {
        assert!(
            !format!("{request:?}").contains(SECRET),
            "the secret reached the provider"
        );
    }
    for (custom_type, data) in agent.session().customs.lock().expect("customs").iter() {
        assert!(!data.contains(SECRET), "{custom_type} held the secret");
    }
    // The hook text did reach the request, with the secret removed.
    let requests = agent.provider().requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].system_prompt.starts_with("prompt "),
        "{}",
        requests[0].system_prompt
    );
    assert!(
        requests[0]
            .system_prompt
            .contains("Untrusted plugin context:\ncontext ")
    );
    assert!(
        requests[0]
            .messages
            .last()
            .expect("prompt")
            .text()
            .starts_with("edit ")
    );
}

#[tokio::test]
async fn an_oversized_request_omits_messages_and_rejects_edits() {
    async fn large_session() -> TurnRecordingSession {
        let session = TurnRecordingSession::default();
        session
            .append(Message {
                id: "big".into(),
                role: Role::User,
                blocks: vec![Block::text("z".repeat(1 << 20))],
                ..Message::default()
            })
            .await
            .expect("append");
        session
            .append(Message {
                id: "big-reply".into(),
                role: Role::Assistant,
                blocks: vec![Block::text("ok")],
                ..Message::default()
            })
            .await
            .expect("append");
        session
    }
    let prompt_only = Arc::new(ProviderHooks::answering(|_| {
        Ok(HookResponse {
            system_prompt: Some("replaced".into()),
            ..HookResponse::default()
        })
    }));
    let agent = hooked_agent(&prompt_only, vec![Turn::text("ok")], large_session().await);
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let data = &prompt_only.data("before_provider_call")[0];
    assert_eq!(data["request_omitted"], "size");
    assert!(data.get("messages").is_none());
    assert_eq!(data["system_prompt"], "be brief");
    assert_eq!(agent.provider().requests()[0].system_prompt, "replaced");

    let with_edits = Arc::new(ProviderHooks::answering(|_| {
        Ok(HookResponse {
            edits: Some(vec![Edit {
                message: 0,
                block: 0,
                text: "x".into(),
            }]),
            ..HookResponse::default()
        })
    }));
    let agent = hooked_agent(&with_edits, vec![Turn::text("ok")], large_session().await);
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("edits rejected");
    assert!(error.to_string().contains("omitted"), "{error}");
    assert!(agent.provider().requests().is_empty());
}

#[tokio::test]
async fn after_provider_call_reports_success_and_error_and_a_failure_only_warns() {
    let reply = Turn {
        events: Vec::new(),
        outcome: Ok(Response {
            message: Message {
                role: Role::Assistant,
                finish_reason: Some(FinishReason::Stop),
                usage: Some(Usage {
                    input_tokens: 11,
                    output_tokens: 4,
                    ..Usage::default()
                }),
                blocks: vec![
                    Block {
                        block_type: BlockType::Reasoning,
                        text: "private reasoning".into(),
                        ..Block::default()
                    },
                    Block::text("ok"),
                ],
                ..Message::default()
            },
            ..Response::default()
        }),
    };
    let hooks = Arc::new(ProviderHooks {
        after: Some(Err("after hook crashed".into())),
        ..ProviderHooks::default()
    });
    let agent = hooked_agent(&hooks, vec![reply], TurnRecordingSession::default());
    let mut events = Vec::new();
    agent
        .run(
            "hello",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect("a failing after hook keeps the turn");
    let data = &hooks.data("after_provider_call")[0];
    assert_eq!(data["outcome"], "success");
    assert_eq!(data["attempts"], 1);
    assert_eq!(data["usage"]["input_tokens"], 11);
    assert_eq!(data["usage"]["output_tokens"], 4);
    assert!(data["duration_ms"].is_u64());
    assert!(data.get("error").is_none());
    assert_eq!(
        data["message"],
        json!({"role": "assistant", "blocks": [{"type": "reasoning"}, {"type": "text", "text": "ok"}]})
    );
    assert!(
        notifications(&events)
            .iter()
            .any(|text| text.contains("after hook crashed"))
    );
    assert!(
        agent
            .session()
            .messages()
            .iter()
            .any(|message| message.text() == "ok")
    );

    let hooks = Arc::new(ProviderHooks {
        after: Some(Ok(HookResponse {
            notice: Some("call failed".into()),
            ..HookResponse::default()
        })),
        ..ProviderHooks::default()
    });
    let agent = hooked_agent(
        &hooks,
        vec![Turn::failure("upstream boom")],
        TurnRecordingSession::default(),
    );
    let mut events = Vec::new();
    agent
        .run(
            "hello",
            &mut |event| events.push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("provider error");
    let data = &hooks.data("after_provider_call")[0];
    assert_eq!(data["outcome"], "error");
    assert!(
        data["error"]
            .as_str()
            .expect("error")
            .contains("upstream boom")
    );
    assert!(data["usage"].is_null());
    assert!(data.get("message").is_none());
    assert!(notifications(&events).contains(&"[hook] call failed".to_owned()));
}

#[tokio::test]
async fn the_overflow_retry_runs_the_hooks_again_and_summary_requests_do_not() {
    let hooks = Arc::new(ProviderHooks::default());
    let agent = Agent::new(
        FakeProvider::new(vec![
            Turn::overflow(),
            Turn::summary(&structured_summary()),
            Turn::text("ok"),
        ]),
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            hooks: Some(hooks.clone()),
            compaction: CompactionSettings {
                auto: true,
                ..CompactionSettings::default()
            },
            ..options()
        },
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("the retry succeeds");
    assert_eq!(agent.provider().summary_requests().len(), 1);
    assert_eq!(agent.provider().normal_requests().len(), 2);
    let before = hooks.data("before_provider_call");
    assert_eq!(before.len(), 2);
    assert_ne!(before[0]["operation_id"], before[1]["operation_id"]);
    let after = hooks.data("after_provider_call");
    assert_eq!(
        after
            .iter()
            .map(|data| data["outcome"].clone())
            .collect::<Vec<_>>(),
        [json!("error"), json!("success")]
    );
}

#[tokio::test]
async fn events_without_a_hook_are_not_built_or_run() {
    let hooks = Arc::new(ProviderHooks {
        before: Some(Box::new(|_| panic!("before_provider_call must not run"))),
        handled: Some(&["before_prompt"]),
        ..ProviderHooks::default()
    });
    let agent = hooked_agent(
        &hooks,
        vec![Turn::text("ok")],
        TurnRecordingSession::default(),
    );
    agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("run");
    let events = hooks.events();
    assert!(events.contains(&"before_prompt".to_owned()));
    assert!(
        events.iter().all(|event| !event.ends_with("provider_call")),
        "{events:?}"
    );
    assert_eq!(agent.provider().requests().len(), 1);
}
