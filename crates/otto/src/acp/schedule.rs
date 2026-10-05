//! Delivery of scheduled items to an ACP client.
//!
//! A client that sets `clientCapabilities._meta.otto.scheduleDelivery` in
//! `initialize` (an array of `telegram` / `feishu`) receives due items of
//! those platforms as the agent-to-client request `_otto/schedule/deliver`
//! with params `{"chat": "<platform>:<chat id>", "text": ...}`. A JSON-RPC
//! `result` acknowledges the delivery; an `error` or no answer within
//! [`DELIVERY_TIMEOUT`] makes the worker retry later.
//!
//! The local backend (`otto acp`) also runs scheduled prompts of its own
//! workspace; the attach backend only delivers, because `otto serve` runs
//! prompts.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::{Backend, Connection};
use crate::app::scheduled::run_new_session;
use crate::schedule::worker::{DeliverRole, PromptRunner, RunRole, Sink, Worker};
use crate::schedule::{Origin, RunOutcome, Scope};
use otto_core::config::resolve::Runtime;

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(60);

/// The platforms in `params.clientCapabilities._meta.otto.scheduleDelivery`
/// that otto-connect supports, without duplicates.
pub(super) fn delivery_platforms(params: &Value) -> Vec<String> {
    let mut platforms: Vec<String> = Vec::new();
    let listed = params
        .pointer("/clientCapabilities/_meta/otto/scheduleDelivery")
        .and_then(Value::as_array);
    for platform in listed.into_iter().flatten().filter_map(Value::as_str) {
        if matches!(platform, "telegram" | "feishu") && !platforms.iter().any(|p| p == platform) {
            platforms.push(platform.to_string());
        }
    }
    platforms
}

/// The worker of one connection, or `None` without platforms or a store.
pub(super) fn worker(connection: &Arc<Connection>, platforms: Vec<String>) -> Option<Worker> {
    if platforms.is_empty() {
        return None;
    }
    let (store, run, logger, metrics) = match &connection.backend {
        Backend::Local(local) => {
            let runner = LocalRunner {
                builder: Arc::clone(&local.config.builder),
                runtime: local.config.runtime.clone(),
            };
            let run = RunRole {
                scope: Scope::Chats(platforms.clone()),
                workspace: Some(local.config.workspace.to_string_lossy().into_owned()),
                runner: Arc::new(runner),
            };
            (
                local.config.builder.schedule.clone()?,
                Some(run),
                Arc::clone(&local.config.builder.logger),
                None,
            )
        }
        // In-process ACP (`otto serve --acp-listen`) counts into serve's
        // `/metrics`; `otto acp --attach` has no registry of its own.
        Backend::Attach(relay) => (
            relay.schedule.clone()?,
            None,
            Arc::clone(&relay.logger),
            relay
                .server
                .as_ref()
                .map(|server| Arc::clone(server.metrics())),
        ),
    };
    let sink = AcpSink {
        connection: Arc::clone(connection),
        timeout: DELIVERY_TIMEOUT,
    };
    Some(Worker {
        store,
        run,
        deliver: Some(DeliverRole {
            scope: Scope::Chats(platforms),
            sink: Arc::new(sink),
        }),
        logger,
        metrics,
    })
}

struct LocalRunner {
    builder: Arc<crate::runtime::builder::Builder>,
    runtime: Runtime,
}

#[async_trait::async_trait]
impl PromptRunner for LocalRunner {
    /// `workspace` is ignored: the run role only claims items of this
    /// connection's workspace.
    async fn run(
        &self,
        _workspace: &str,
        text: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> RunOutcome {
        run_new_session(Arc::clone(&self.builder), &self.runtime, text, cancel).await
    }
}

pub(super) struct AcpSink {
    pub(super) connection: Arc<Connection>,
    pub(super) timeout: Duration,
}

#[async_trait::async_trait]
impl Sink for AcpSink {
    async fn deliver(&self, origin: &Origin, text: &str) -> Result<(), String> {
        let chat = origin
            .as_str()
            .strip_prefix("chat:")
            .ok_or("origin is not a chat")?;
        let (id, answer) = self.connection.send_request(
            "_otto/schedule/deliver",
            json!({"chat": chat, "text": text}),
        );
        let frame = match tokio::time::timeout(self.timeout, answer).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(_)) => return Err("connection closed".into()),
            Err(_) => {
                self.connection.forget_request(id);
                return Err("delivery timed out".into());
            }
        };
        match frame.get("error") {
            None => Ok(()),
            Some(error) => Err(error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("client rejected the delivery")
                .to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::io::Write;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    use chrono::{Duration as Chrono, Utc};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::super::Local;
    use super::*;
    use crate::schedule::{Kind, NewItem, Store};
    use otto_core::session::Session as _;

    fn connection(
        schedule: Option<Arc<Store>>,
    ) -> (Arc<Connection>, mpsc::UnboundedReceiver<Value>) {
        let (out, frames) = mpsc::unbounded_channel();
        let connection = Arc::new(Connection {
            workspace: "/work".into(),
            backend: Backend::Attach(super::super::attach::Relay::unreachable(schedule)),
            out,
            pending: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(1),
            client_capabilities: Mutex::new(Value::Null),
            task_stop: CancellationToken::new(),
            notice_stop: CancellationToken::new(),
            watching_notices: Mutex::new(HashSet::new()),
            notice_ready: Mutex::new(HashMap::new()),
            wake_loops: Mutex::new(HashMap::new()),
        });
        (connection, frames)
    }

    fn sink(connection: &Arc<Connection>, timeout: Duration) -> Arc<AcpSink> {
        Arc::new(AcpSink {
            connection: Arc::clone(connection),
            timeout,
        })
    }

    fn origin() -> Origin {
        Origin::chat("telegram", "123").unwrap()
    }

    /// Delivers once and answers the request with `answer` (a frame body
    /// merged with the request id), or leaves it unanswered.
    async fn deliver_with(answer: Option<Value>, timeout: Duration) -> Result<(), String> {
        let (connection, mut frames) = connection(None);
        let sink = sink(&connection, timeout);
        let delivery = tokio::spawn(async move { sink.deliver(&origin(), "drink water").await });
        let request = frames.recv().await.unwrap();
        assert_eq!(request["method"], "_otto/schedule/deliver");
        assert_eq!(
            request["params"],
            json!({"chat": "telegram:123", "text": "drink water"})
        );
        if let Some(mut answer) = answer {
            answer["jsonrpc"] = json!("2.0");
            answer["id"] = request["id"].clone();
            connection.client_response(answer);
        }
        delivery.await.unwrap()
    }

    #[tokio::test]
    async fn a_result_answer_is_a_successful_delivery() {
        let outcome = deliver_with(Some(json!({"result": {}})), Duration::from_secs(5)).await;
        assert_eq!(outcome, Ok(()));
    }

    #[tokio::test]
    async fn an_error_answer_fails_the_delivery_with_its_message() {
        let answer = json!({"error": {"code": -32000, "message": "chat not found"}});
        let outcome = deliver_with(Some(answer), Duration::from_secs(5)).await;
        assert_eq!(outcome, Err("chat not found".to_string()));
    }

    #[tokio::test]
    async fn no_answer_times_out_and_forgets_the_request() {
        let outcome = deliver_with(None, Duration::from_millis(50)).await;
        assert_eq!(outcome, Err("delivery timed out".to_string()));
    }

    #[tokio::test]
    async fn a_timed_out_delivery_leaves_no_pending_request() {
        let (connection, _frames) = connection(None);
        let sink = sink(&connection, Duration::from_millis(20));
        assert!(sink.deliver(&origin(), "x").await.is_err());
        assert!(connection.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn delivery_platforms_keep_known_platforms_once() {
        let params = |list: Value| json!({"clientCapabilities": {"_meta": {"otto": {"scheduleDelivery": list}}}});
        assert_eq!(
            delivery_platforms(&params(json!([
                "telegram", "slack", "feishu", "telegram", 7
            ]))),
            ["telegram", "feishu"]
        );
        assert!(delivery_platforms(&params(json!([]))).is_empty());
        assert!(delivery_platforms(&params(json!("telegram"))).is_empty());
        assert!(delivery_platforms(&json!({"clientCapabilities": {}})).is_empty());
        assert!(delivery_platforms(&Value::Null).is_empty());
    }

    #[test]
    fn no_worker_without_platforms_or_store() {
        let (without_store, _frames) = connection(None);
        assert!(worker(&without_store, vec!["telegram".into()]).is_none());
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (with_store, _frames) = connection(Some(store));
        assert!(worker(&with_store, Vec::new()).is_none());
        let attach = worker(&with_store, vec!["telegram".into()]).unwrap();
        assert!(attach.run.is_none() && attach.deliver.is_some());
    }

    /// A client of `drive`: frames go in through a pipe, out through a
    /// channel.
    struct Wire {
        input: Option<std::io::PipeWriter>,
        output: mpsc::UnboundedReceiver<Value>,
    }

    struct LineWriter {
        pending: Vec<u8>,
        frames: mpsc::UnboundedSender<Value>,
    }

    impl Write for LineWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.pending.extend_from_slice(bytes);
            while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                if let Ok(frame) = serde_json::from_slice(&line) {
                    let _ = self.frames.send(frame);
                }
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Wire {
        fn send(&mut self, frame: Value) {
            let input = self.input.as_mut().unwrap();
            writeln!(input, "{frame}").unwrap();
        }

        async fn recv(&mut self) -> Value {
            tokio::time::timeout(Duration::from_secs(10), self.output.recv())
                .await
                .expect("a frame within 10 s")
                .expect("output open")
        }

        /// The next frame that is not a notification.
        async fn recv_message(&mut self) -> Value {
            loop {
                let frame = self.recv().await;
                if frame.get("method") != Some(&json!("session/update")) {
                    return frame;
                }
            }
        }

        fn close(&mut self) {
            self.input = None;
        }
    }

    fn start<T: Send + 'static>(
        backend: Backend,
        workspace: &str,
        collect: impl FnOnce(&Connection) -> T + Send + 'static,
    ) -> (Wire, tokio::task::JoinHandle<T>) {
        let (reader, writer) = std::io::pipe().unwrap();
        let (frames, output) = mpsc::unbounded_channel();
        let workspace = std::path::PathBuf::from(workspace);
        let handle = tokio::spawn(async move {
            let mut out = LineWriter {
                pending: Vec::new(),
                frames,
            };
            super::super::drive(
                workspace,
                backend,
                Box::new(std::io::BufReader::new(reader)),
                &mut out,
                &CancellationToken::new(),
                &CancellationToken::new(),
                collect,
            )
            .await
        });
        let wire = Wire {
            input: Some(writer),
            output,
        };
        (wire, handle)
    }

    fn due_text_item(store: &Store, origin: Origin) {
        let now = Utc::now();
        store
            .create(
                NewItem {
                    origin,
                    session_id: "s1".into(),
                    workspace: "/work".into(),
                    kind: Kind::Text,
                    body: "drink water".into(),
                    repeat: None,
                    local_time: None,
                    due_at: now - Chrono::minutes(1),
                },
                now - Chrono::minutes(2),
            )
            .unwrap();
    }

    fn initialize(platforms: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": 1,
            "clientCapabilities": {"_meta": {"otto": {"scheduleDelivery": platforms}}},
        }})
    }

    #[tokio::test]
    async fn a_due_chat_item_is_delivered_after_initialize_declares_its_platform() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let chat = origin();
        due_text_item(&store, chat.clone());
        let backend = Backend::Attach(super::super::attach::Relay::unreachable(Some(Arc::clone(
            &store,
        ))));
        let (mut wire, handle) = start(backend, "/work", |_| ());
        wire.send(initialize(json!(["telegram"])));
        let reply = wire.recv_message().await;
        assert_eq!(reply["id"], 1);
        let request = wire.recv_message().await;
        assert_eq!(request["method"], "_otto/schedule/deliver");
        assert_eq!(request["params"]["chat"], "telegram:123");
        assert!(
            request["params"]["text"]
                .as_str()
                .unwrap()
                .contains("drink water")
        );
        assert!(
            store
                .pending_writebacks(&chat, "other", 10)
                .unwrap()
                .is_empty()
        );
        wire.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": {}}));
        let mut remaining = 200;
        while store
            .pending_writebacks(&chat, "other", 10)
            .unwrap()
            .is_empty()
        {
            remaining -= 1;
            assert!(
                remaining > 0,
                "item still pending after the client acknowledged it"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        wire.close();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn without_the_capability_nothing_is_delivered() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let chat = origin();
        due_text_item(&store, chat.clone());
        let backend = Backend::Attach(super::super::attach::Relay::unreachable(Some(Arc::clone(
            &store,
        ))));
        let (mut wire, handle) = start(backend, "/work", |_| ());
        wire.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}));
        wire.send(initialize(json!(["slack"])));
        assert_eq!(wire.recv_message().await["id"], 1);
        assert_eq!(wire.recv_message().await["id"], 1);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(wire.output.try_recv().is_err());
        assert!(
            store
                .pending_writebacks(&chat, "other", 10)
                .unwrap()
                .is_empty()
        );
        wire.close();
        handle.await.unwrap();
    }

    /// Provider that answers every request with "ok".
    struct OkProvider;

    #[async_trait::async_trait]
    impl otto_core::provider::Provider for OkProvider {
        async fn complete(
            &self,
            _: &otto_core::provider::Request,
            _: otto_core::provider::StreamSink<'_>,
            _: &dyn otto_core::operation::OperationControl,
        ) -> otto_core::provider::ProviderSettlement {
            let message = otto_core::model::Message {
                role: otto_core::model::Role::Assistant,
                blocks: vec![otto_core::model::Block::text("ok")],
                finish_reason: Some(otto_core::model::FinishReason::Stop),
                ..otto_core::model::Message::default()
            };
            otto_core::provider::ProviderSettlement::succeeded(
                otto_core::provider::Response { message },
                1,
            )
        }
    }

    /// Runs one prompt with `meta` on a local session while the store holds
    /// a delivered item of `chat:telegram:123`, and returns whether the turn
    /// appended that delivery to the session. The append happens only for a
    /// user turn whose origin is that chat.
    async fn writes_back_after_prompt(meta: Value) -> bool {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let now = Utc::now();
        let chat = origin();
        let item = store
            .create(
                NewItem {
                    origin: chat.clone(),
                    session_id: "creator".into(),
                    workspace: "/work".into(),
                    kind: Kind::Text,
                    body: "drink water".into(),
                    repeat: None,
                    local_time: None,
                    due_at: now,
                },
                now,
            )
            .unwrap();
        store
            .claim_deliveries(&Scope::Any, Duration::from_secs(60), 10, now)
            .unwrap();
        assert!(store.delivered(item.id, item.due_at, now).unwrap());
        let mut builder = crate::cli::testutil::builder(workspace.path(), sessions.path());
        builder.shared_mut().schedule = Some(store);
        let builder = Arc::new(builder);
        let runtime = crate::cli::testutil::initial_runtime(&builder);
        let controller = crate::app::Controller::create(Arc::clone(&builder), &runtime)
            .await
            .unwrap();
        let runner = crate::runtime::builder::Runner::scripted(
            controller.current_session(),
            Arc::new(OkProvider),
            Arc::new(crate::subagent::tasks::Tasks::new()),
        );
        let info = builder.runtime_info(&runtime);
        let id = controller.info().session_id;
        assert!(
            controller
                .replace_runner_if_current(&id, runner, info)
                .unwrap()
        );
        let local = Local {
            config: super::super::Config {
                builder,
                runtime,
                workspace: workspace.path().to_path_buf(),
                sandbox: None,
            },
            sessions: Mutex::new(HashMap::new()),
        };
        let (_, session) = local.register(controller);
        let (mut wire, handle) = start(Backend::Local(Box::new(local)), "/work", |_| ());
        wire.send(
            json!({"jsonrpc": "2.0", "id": 7, "method": "session/prompt", "params": {
                "sessionId": id,
                "prompt": [{"type": "text", "text": "hello"}],
                "_meta": meta,
            }}),
        );
        let reply = wire.recv_message().await;
        assert_eq!(reply["result"]["stopReason"], "end_turn", "{reply}");
        wire.close();
        handle.await.unwrap();
        session
            .controller
            .current_session()
            .messages()
            .iter()
            .any(|message| message.context_type == "scheduled_delivery")
    }

    #[tokio::test]
    async fn a_prompt_with_chat_metadata_runs_its_turn_with_that_origin() {
        assert!(writes_back_after_prompt(json!({"otto": {"chat": "telegram:123"}})).await);
    }

    #[tokio::test]
    async fn a_prompt_without_valid_chat_metadata_has_no_origin() {
        assert!(!writes_back_after_prompt(json!({})).await);
        let mismatch = json!({"otto": {"chat": "telegram:123", "channel": "feishu"}});
        assert!(!writes_back_after_prompt(mismatch).await);
    }

    /// Sends `prompt_text` with `meta` through a relay whose server records
    /// the start-turn bodies, and returns the body it received.
    async fn relayed_turn_body(prompt_text: &str, meta: Value) -> Value {
        relayed_turn(prompt_text, meta).await.0
    }

    /// Like [`relayed_turn_body`], plus the `X-Request-Id` header.
    async fn relayed_turn(prompt_text: &str, meta: Value) -> (Value, Option<String>) {
        let bodies = Arc::new(Mutex::new(Vec::<(Value, Option<String>)>::new()));
        let recorded = Arc::clone(&bodies);
        let router =
            axum::Router::new().fallback(move |headers: axum::http::HeaderMap, body: String| {
                let recorded = Arc::clone(&recorded);
                async move {
                    let id = headers
                        .get("x-request-id")
                        .map(|value| value.to_str().unwrap().to_string());
                    recorded
                        .lock()
                        .unwrap()
                        .push((serde_json::from_str(&body).unwrap(), id));
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                }
            });
        let relay = super::super::attach::Relay::with_router(router);
        relay.register("s1");
        let (mut wire, handle) = start(Backend::Attach(relay), "/work", |_| ());
        wire.send(
            json!({"jsonrpc": "2.0", "id": 7, "method": "session/prompt", "params": {
                "sessionId": "s1",
                "prompt": [{"type": "text", "text": prompt_text}],
                "_meta": meta,
            }}),
        );
        // The fake server answers HTTP 500, so the prompt ends with an error
        // after the request was recorded.
        let reply = wire.recv_message().await;
        assert_eq!(reply["id"], 7, "{reply}");
        wire.close();
        handle.await.unwrap();
        let mut bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1, "{bodies:?}");
        bodies.remove(0)
    }

    #[tokio::test]
    async fn the_attach_relay_sends_the_chat_origin_for_prompts_and_queued_follow_ups() {
        let chat = json!({"otto": {"chat": "telegram:123", "channel": "telegram"}});
        for text in ["hello", "/queue later"] {
            let body = relayed_turn_body(text, chat.clone()).await;
            assert_eq!(body["origin"], "chat:telegram:123", "{text}: {body}");
            assert_eq!(body["channel"], "telegram", "{text}: {body}");
            let body = relayed_turn_body(text, json!({"otto": {"channel": "telegram"}})).await;
            assert!(body.get("origin").is_none(), "{text}: {body}");
        }
    }

    #[tokio::test]
    async fn the_attach_relay_sends_a_valid_request_id_header_and_drops_an_invalid_one() {
        for text in ["hello", "/queue later"] {
            let meta = json!({"otto": {"requestId": "0123abcd"}});
            assert_eq!(
                relayed_turn(text, meta).await.1.as_deref(),
                Some("0123abcd"),
                "{text}"
            );
            for invalid in [json!("\u{1}\u{2}"), json!(""), json!(7)] {
                let meta = json!({"otto": {"requestId": invalid}});
                assert_eq!(relayed_turn(text, meta).await.1, None, "{text}");
            }
            assert_eq!(relayed_turn(text, json!({})).await.1, None, "{text}");
        }
    }
}
