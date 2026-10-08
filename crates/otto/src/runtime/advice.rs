//! The session-model advice backend.
//!
//! It owns a tool-less agent built from the same provider client, model,
//! thinking setting and redaction boundary as the session's runner, and a
//! throwaway in-memory session. It does not hold the `Runner`: the runner
//! holds the advisor that holds this backend, so a reference back would be a
//! cycle. Requests go through the one-shot completion path, so the redaction
//! check and `redact_string` apply to the prompt exactly as for a turn.

use std::collections::HashMap;
use std::time::Duration;

use otto_core::agent::Agent;
use otto_core::agent::oneshot::TextRequest;

use crate::deadline::{Control, Deadline};
use crate::memory::advice::{Answer, Backend, Exchange, Failure, Question, session};
use crate::tool::registry::Registry;

use super::builder::{ProviderClient, SharedSession};

/// The usage task id of advice requests.
pub const TASK_ID: &str = "memory-advice";

/// The largest accepted answer, in bytes. Five short JSON keys fit well below.
const MAXIMUM_BYTES: usize = 4096;

pub struct SessionBackend {
    agent: Agent<ProviderClient, Registry, SharedSession>,
    model: String,
    usage: Option<crate::usage::Collector>,
}

impl SessionBackend {
    pub fn new(
        agent: Agent<ProviderClient, Registry, SharedSession>,
        model: String,
        usage: Option<crate::usage::Collector>,
    ) -> Self {
        Self {
            agent,
            model,
            usage,
        }
    }
}

#[async_trait::async_trait]
impl Backend for SessionBackend {
    fn model(&self) -> String {
        self.model.clone()
    }

    fn auxiliary(&self) -> bool {
        false
    }

    async fn ask(&self, state: &str, questions: &[Question], limit: Duration) -> Exchange {
        let user_text = session::user_text(state, questions);
        let request = TextRequest {
            system_prompt: session::SYSTEM_PROMPT,
            user_text: &user_text,
            maximum_bytes: MAXIMUM_BYTES,
        };
        let sent = serde_json::json!({
            "system": session::SYSTEM_PROMPT,
            "user": self.agent.redact_text(&user_text),
        })
        .to_string();
        let control = Control::new(Deadline::after(limit));
        let collector = self.usage.as_ref().map(|usage| usage.for_task(TASK_ID));
        let mut emit = |event: otto_core::agent::Event| {
            if let Some(collector) = &collector {
                let _ = collector.record(&event);
            }
        };
        let call = self.agent.complete_text(&request, &mut emit, &control);
        let (response, result) = match tokio::time::timeout(limit, call).await {
            Err(_) => (None, Err(Failure::Timeout)),
            Ok(Err(error)) => (None, Err(Failure::Error(error.to_string()))),
            Ok(Ok(response)) => {
                if let Some(collector) = &collector {
                    let _ = collector.record(&otto_core::agent::Event::ProviderUsage {
                        usage: response.usage,
                        present: response.usage_present,
                        cached_input_tokens_present: response.cached_input_tokens_present,
                    });
                }
                let result: Result<HashMap<String, Answer>, Failure> =
                    session::parse(&response.text, questions);
                (Some(response.text), result)
            }
        };
        Exchange {
            request: sent,
            response,
            result,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use chrono::Utc;
    use otto_core::agent::Options;
    use otto_core::agent::redactor::Redactor;
    use otto_core::model::{Block, FinishReason, Message, Role, Usage};
    use otto_core::operation::OperationControl;
    use otto_core::provider::{Provider, ProviderSettlement, Request, Response, StreamSink};
    use otto_core::session::Header;

    use super::*;
    use crate::memory::advice::{Kind, YES};
    use crate::usage::{ApiOperationRecord, Collector, Context, Sink, UsageRecord};

    struct Scripted {
        reply: String,
        requests: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        async fn complete(
            &self,
            request: &Request,
            _emit: StreamSink<'_>,
            _control: &dyn OperationControl,
        ) -> ProviderSettlement {
            self.requests
                .lock()
                .expect("lock")
                .push(format!("{request:?}"));
            ProviderSettlement::succeeded(
                Response {
                    message: Message {
                        role: Role::Assistant,
                        blocks: vec![Block::text(self.reply.clone())],
                        finish_reason: Some(FinishReason::Stop),
                        usage: Some(Usage {
                            input_tokens: 7,
                            output_tokens: 3,
                            cached_input_tokens: 0,
                        }),
                        ..Message::default()
                    },
                    cached_input_tokens_present: true,
                    remote_compaction: None,
                },
                1,
            )
        }
    }

    #[derive(Default)]
    struct Records(Mutex<Vec<UsageRecord>>, Mutex<Vec<ApiOperationRecord>>);

    impl Sink for Records {
        fn append(&self, record: &UsageRecord) -> crate::usage::Result<()> {
            self.0.lock().expect("lock").push(record.clone());
            Ok(())
        }

        fn append_api_operation(&self, record: &ApiOperationRecord) -> crate::usage::Result<()> {
            self.1.lock().expect("lock").push(record.clone());
            Ok(())
        }
    }

    fn backend(reply: &str, secrets: &[String]) -> (SessionBackend, Arc<Scripted>, Arc<Records>) {
        let provider = Arc::new(Scripted {
            reply: reply.to_owned(),
            requests: Mutex::new(Vec::new()),
        });
        let agent = Agent::with_redactor(
            ProviderClient::Scripted(provider.clone()),
            Registry::new(Vec::new()).expect("registry"),
            SharedSession::memory(Header::default()),
            Options {
                model: "test-model".into(),
                provider_name: "openai-compatible".into(),
                now: Box::new(Utc::now),
                ..Options::default()
            },
            Redactor::new(secrets),
        );
        let records = Arc::new(Records::default());
        let collector = Collector::new(
            Arc::clone(&records),
            Context {
                workspace: "w".into(),
                session_id: "s".into(),
                task_id: String::new(),
                provider: "openai-compatible".into(),
                profile: "p".into(),
                model: "test-model".into(),
            },
        );
        (
            SessionBackend::new(agent, "test-model".into(), Some(collector)),
            provider,
            records,
        )
    }

    fn durable_question() -> Vec<Question> {
        vec![Question {
            id: "durable",
            instructions: "Is it durable?".into(),
            kind: Kind::YesNo {
                yes: "durable".into(),
                no: "one-off".into(),
            },
        }]
    }

    #[tokio::test]
    async fn a_valid_json_reply_is_parsed_and_its_usage_is_recorded_under_the_advice_task() {
        let (backend, provider, records) = backend(r#"{"durable":true}"#, &[]);

        let exchange = backend
            .ask("state text", &durable_question(), Duration::from_secs(5))
            .await;

        let answers = exchange.result.expect("valid");
        assert_eq!(answers["durable"].choice, YES);
        assert_eq!(answers["durable"].probability, None);
        assert_eq!(exchange.response.as_deref(), Some(r#"{"durable":true}"#));
        assert!(exchange.request.contains("state text"));
        assert_eq!(provider.requests.lock().expect("lock").len(), 1);
        let operations = records.1.lock().expect("lock");
        let records = records.0.lock().expect("lock");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].context.task_id, TASK_ID);
        assert_eq!(records[0].usage.input_tokens, 7);
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].context.task_id, TASK_ID);
        assert_eq!(operations[0].attempts, 1);
        assert!(!backend.auxiliary());
        assert_eq!(backend.model(), "test-model");
    }

    #[tokio::test]
    async fn a_reply_that_is_not_the_requested_json_is_invalid() {
        let (backend, _, _) = backend("Sure, it is durable.", &[]);

        let exchange = backend
            .ask("state", &durable_question(), Duration::from_secs(5))
            .await;

        assert!(matches!(exchange.result, Err(Failure::Invalid(_))));
        assert_eq!(exchange.response.as_deref(), Some("Sure, it is durable."));
    }

    #[tokio::test]
    async fn the_prompt_is_redacted_before_it_reaches_the_provider_and_the_audit_request() {
        let (backend, provider, _) =
            backend(r#"{"durable":false}"#, &["hunter2-secret".to_owned()]);

        let exchange = backend
            .ask(
                "the password is hunter2-secret",
                &durable_question(),
                Duration::from_secs(5),
            )
            .await;

        assert!(exchange.result.is_ok());
        let sent = provider.requests.lock().expect("lock");
        assert!(!sent[0].contains("hunter2-secret"), "{}", sent[0]);
        assert!(!exchange.request.contains("hunter2-secret"));
    }

    fn selection_builder(
        key: Option<&str>,
        memory: bool,
    ) -> (tempfile::TempDir, crate::runtime::builder::Builder) {
        let directory = tempfile::tempdir().expect("dir");
        let workspace = directory.path().join("ws");
        let sessions = directory.path().join("sessions");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let mut builder = crate::cli::testutil::builder(&workspace, &sessions);
        if let Some(key) = key {
            builder
                .shared_mut()
                .environment
                .insert("TYPESAFE_API_KEY".into(), key.into());
        }
        if memory {
            let runtime = otto_core::config::MemoryRuntime {
                enabled: true,
                backend: "turso".into(),
                turso_path: directory
                    .path()
                    .join("memory.db")
                    .to_string_lossy()
                    .into_owned(),
                ..otto_core::config::MemoryRuntime::default()
            };
            let (service, user_scope, usable) =
                crate::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
                    .expect("memory");
            let shared = builder.shared_mut();
            shared.memory.service = service;
            shared.memory.user_scope = user_scope;
            shared.memory.usable = usable;
            builder.workspace_scope = crate::runtime::wiring::workspace_memory_scope(
                &runtime,
                &workspace.to_string_lossy(),
            )
            .expect("workspace scope");
        }
        (directory, builder)
    }

    fn advice_agent() -> (crate::runtime::builder::AdviceAgent, Arc<Scripted>) {
        let (backend, provider, _) = backend("{}", &[]);
        (backend.agent, provider)
    }

    #[test]
    fn the_auxiliary_model_is_used_only_when_typesafe_api_key_is_non_empty() {
        for (key, auxiliary) in [(Some("tsk-key"), true), (Some(""), false), (None, false)] {
            let (_directory, builder) = selection_builder(key, true);
            let (agent, _) = advice_agent();

            let advisor = builder
                .advisor(agent, "test-model", None)
                .expect("memory is usable");

            assert_eq!(advisor.is_auxiliary(), auxiliary, "key {key:?}");
        }
    }

    #[test]
    fn there_is_no_advisor_when_memory_is_unusable() {
        let (_directory, builder) = selection_builder(Some("tsk-key"), false);
        let (agent, _) = advice_agent();

        assert!(builder.advisor(agent, "test-model", None).is_none());
    }

    #[tokio::test]
    async fn a_built_runner_carries_the_advisor_when_memory_is_usable() {
        let (_directory, builder) = selection_builder(None, true);
        let runtime = crate::cli::testutil::initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");

        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");

        assert!(runner.advisor.is_some());
        let (_directory, builder) = selection_builder(None, false);
        let runtime = crate::cli::testutil::initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let runner = builder
            .build_runner(&session, &runtime)
            .await
            .expect("runner");
        assert!(runner.advisor.is_none());
    }
}
