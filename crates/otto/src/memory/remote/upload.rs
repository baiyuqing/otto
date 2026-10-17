//! Turn capture for the remote memory service: converts the messages of one
//! prompt step, filters them, and sends them from one background worker.
//! See "Turn upload" in `docs/specs/2026-10-17-remote-memory-scope-redesign.md`.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use otto_core::{
    memory::MemoryContext,
    model::{Block, BlockType, Message, Role},
};
use serde_json::Value;
use tokio::{sync::mpsc, task::JoinHandle};

use super::{MAX_TURN_BYTES, RemoteService, wire};
use crate::{log::Logger, memory::guard::check_value};

const QUEUE_CAPACITY: usize = 8;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Uploader {
    sender: Mutex<Option<mpsc::Sender<wire::TurnRequest>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    logger: Arc<Logger>,
    drain_timeout: Duration,
}

impl Uploader {
    pub fn new(service: Arc<RemoteService>, logger: Arc<Logger>) -> Arc<Self> {
        Self::with_drain_timeout(service, logger, DRAIN_TIMEOUT)
    }

    fn with_drain_timeout(
        service: Arc<RemoteService>,
        logger: Arc<Logger>,
        drain_timeout: Duration,
    ) -> Arc<Self> {
        let (sender, mut receiver) = mpsc::channel::<wire::TurnRequest>(QUEUE_CAPACITY);
        let worker_logger = logger.clone();
        let worker = tokio::spawn(async move {
            while let Some(turn) = receiver.recv().await {
                if let Err(error) = service.upload_turn(&turn).await {
                    worker_logger.memory_turn(
                        "memory_turn_upload_failed",
                        &[
                            ("turn_id", turn.turn_id.clone()),
                            ("kind", format!("{:?}", error.kind)),
                        ],
                    );
                }
            }
        });
        Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
            logger,
            drain_timeout,
        })
    }

    /// Queues one finished turn. Never blocks; a turn that is filtered out or
    /// finds the queue full is logged and dropped.
    pub fn submit(
        &self,
        context: &MemoryContext,
        status: wire::TurnStatus,
        messages: Vec<Message>,
    ) {
        let messages = convert(&messages);
        if messages.is_empty() {
            return;
        }
        let turn_id = match crate::memory::new_id() {
            Ok(id) => id,
            Err(_) => return self.drop_turn("id generation failed", "", 0),
        };
        let Ok(context) = RemoteService::context(context) else {
            return self.drop_turn("invalid context", "", 0);
        };
        let request = wire::TurnRequest {
            context,
            turn_id,
            status,
            messages,
        };
        let Ok(body) = serde_json::to_vec(&request) else {
            return self.drop_turn("encode failed", &request.turn_id, 0);
        };
        if body.len() > MAX_TURN_BYTES {
            return self.drop_turn("over size limit", &request.turn_id, body.len());
        }
        if let Err(error) = guard(&request) {
            return self.drop_turn(&error.to_string(), &request.turn_id, body.len());
        }
        let size = body.len();
        let turn_id = request.turn_id.clone();
        let sent = match self
            .sender
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            Some(sender) => sender.try_send(request).map_err(|_| "queue full"),
            None => Err("uploader closed"),
        };
        if let Err(reason) = sent {
            self.drop_turn(reason, &turn_id, size);
        }
    }

    /// Stops accepting turns and waits up to the drain bound for the queue to
    /// empty; the remaining turns are discarded. Idempotent.
    pub async fn close(&self) {
        self.sender.lock().unwrap_or_else(|p| p.into_inner()).take();
        let worker = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(mut worker) = worker
            && tokio::time::timeout(self.drain_timeout, &mut worker)
                .await
                .is_err()
        {
            worker.abort();
            self.logger.memory_turn("memory_turn_drain_timeout", &[]);
        }
    }

    fn drop_turn(&self, reason: &str, turn_id: &str, bytes: usize) {
        self.logger.memory_turn(
            "memory_turn_dropped",
            &[
                ("reason", reason.to_string()),
                ("turn_id", turn_id.to_string()),
                ("bytes", bytes.to_string()),
            ],
        );
    }
}

/// Runs the `DefaultGuard` per-field rules on every string of the request.
fn guard(request: &wire::TurnRequest) -> crate::memory::Result<()> {
    for message in &request.messages {
        check_value(&message.id, false)?;
        for block in &message.blocks {
            match block {
                wire::TurnBlock::Text { text } => check_value(text, false)?,
                wire::TurnBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    check_value(id, false)?;
                    check_value(name, false)?;
                    check_json(arguments)?;
                }
                wire::TurnBlock::ToolResult { id, name, text, .. } => {
                    check_value(id, false)?;
                    check_value(name, false)?;
                    check_value(text, false)?;
                }
            }
        }
    }
    Ok(())
}

fn check_json(value: &Value) -> crate::memory::Result<()> {
    match value {
        Value::String(text) => check_value(text, false),
        Value::Array(items) => items.iter().try_for_each(check_json),
        Value::Object(map) => map.iter().try_for_each(|(key, item)| {
            check_value(key, false)?;
            check_json(item)
        }),
        _ => Ok(()),
    }
}

fn convert(messages: &[Message]) -> Vec<wire::TurnMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                Role::User => wire::TurnRole::User,
                Role::Assistant => wire::TurnRole::Assistant,
                Role::Tool => wire::TurnRole::Tool,
                Role::Context => wire::TurnRole::Context,
                Role::Other(_) => return None,
            };
            let blocks: Vec<_> = message.blocks.iter().filter_map(convert_block).collect();
            (!blocks.is_empty()).then(|| wire::TurnMessage {
                id: message.id.clone(),
                role,
                created_at: message.created_at,
                blocks,
            })
        })
        .collect()
}

fn convert_block(block: &Block) -> Option<wire::TurnBlock> {
    match block.block_type {
        BlockType::Text => Some(wire::TurnBlock::Text {
            text: block.text.clone(),
        }),
        BlockType::ToolCall => Some(wire::TurnBlock::ToolCall {
            id: block.tool_call_id.clone(),
            name: block.tool_name.clone(),
            // Absent or unparseable arguments are sent as an empty object,
            // so the field is always a JSON object.
            arguments: block
                .arguments
                .as_ref()
                .and_then(|raw| serde_json::from_str(raw.get()).ok())
                .filter(Value::is_object)
                .unwrap_or_else(|| Value::Object(Default::default())),
        }),
        BlockType::ToolResult => Some(wire::TurnBlock::ToolResult {
            id: block.tool_call_id.clone(),
            name: block.tool_name.clone(),
            text: block.text.clone(),
            is_error: block.is_error,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use axum::{Router, body::Bytes};
    use otto_core::memory::{Origin, Scope};
    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;
    use crate::memory::remote::Config;

    type Seen = Arc<Mutex<Vec<Value>>>;

    /// Loopback stub: records each body, answers with `status`, and holds
    /// every request until `gate` (when set) is notified.
    async fn serve(status: u16, gate: Option<Arc<Notify>>) -> (String, Seen) {
        let seen: Seen = Seen::default();
        let log = seen.clone();
        let app = Router::new().fallback(move |body: Bytes| {
            let (log, gate) = (log.clone(), gate.clone());
            async move {
                log.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap());
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                axum::response::Response::builder()
                    .status(status)
                    .header("Otto-Memory-Protocol", "1.0")
                    .body(axum::body::Body::from("{}"))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    fn uploader(url: &str, drain: Duration) -> Arc<Uploader> {
        logged_uploader(url, drain, Arc::new(Logger::discard()))
    }

    fn logged_uploader(url: &str, drain: Duration, logger: Arc<Logger>) -> Arc<Uploader> {
        let service = RemoteService::new(Config {
            url: url.into(),
            token: "test-token".into(),
            recall_timeout: Duration::from_secs(5),
            logger: logger.clone(),
        })
        .unwrap();
        Uploader::with_drain_timeout(Arc::new(service), logger, drain)
    }

    fn context() -> MemoryContext {
        MemoryContext {
            organization_id: Some("acme".into()),
            principal: "local".into(),
            scopes: vec![
                Scope {
                    namespace: "organization".into(),
                    id: "acme".into(),
                },
                Scope {
                    namespace: "user".into(),
                    id: "local".into(),
                },
            ],
            write_scope: None,
            workspace_id: None,
            session_id: Some("s-1".into()),
            task_id: None,
            origin: Origin::Model,
        }
    }

    fn text_message(role: Role, text: &str) -> Message {
        message(role, vec![Block::text(text)])
    }

    fn message(role: Role, blocks: Vec<Block>) -> Message {
        Message {
            id: "m-1".into(),
            role,
            blocks,
            ..Message::default()
        }
    }

    fn block(block_type: BlockType) -> Block {
        Block {
            block_type,
            text: String::new(),
            data: String::new(),
            mime_type: String::new(),
            tool_call_id: String::new(),
            tool_name: String::new(),
            arguments: None,
            is_error: false,
            operation_metadata: None,
        }
    }

    fn tool_result(text: &str) -> Block {
        Block {
            tool_call_id: "c-1".into(),
            tool_name: "bash".into(),
            text: text.into(),
            ..block(BlockType::ToolResult)
        }
    }

    async fn settle(seen: &Seen, count: usize) {
        for _ in 0..200 {
            if seen.lock().unwrap().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn sends_one_request_per_turn_with_roles_and_block_types() {
        let (url, seen) = serve(200, None).await;
        let up = uploader(&url, Duration::from_secs(5));
        let call = Block {
            tool_call_id: "c-1".into(),
            tool_name: "bash".into(),
            arguments: Some(serde_json::value::to_raw_value(&json!({"command": "ls"})).unwrap()),
            ..block(BlockType::ToolCall)
        };
        up.submit(
            &context(),
            wire::TurnStatus::Success,
            vec![
                text_message(Role::User, "hi"),
                message(Role::Assistant, vec![call]),
                message(Role::Tool, vec![tool_result("ok")]),
            ],
        );
        up.close().await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let body = &seen[0];
        assert_eq!(body["status"], "success");
        assert_eq!(body["context"]["session_id"], "s-1");
        let roles: Vec<_> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "tool"]);
        assert_eq!(body["messages"][0]["blocks"][0]["type"], "text");
        assert_eq!(
            body["messages"][1]["blocks"][0],
            json!({"type": "tool_call", "id": "c-1", "name": "bash", "arguments": {"command": "ls"}})
        );
        assert_eq!(body["messages"][2]["blocks"][0]["type"], "tool_result");
    }

    #[tokio::test]
    async fn failed_upload_is_not_retried_and_next_turn_is_sent() {
        let (url, seen) = serve(503, None).await;
        let up = uploader(&url, Duration::from_secs(5));
        up.submit(
            &context(),
            wire::TurnStatus::Error,
            vec![text_message(Role::User, "one")],
        );
        settle(&seen, 1).await;
        up.submit(
            &context(),
            wire::TurnStatus::Error,
            vec![text_message(Role::User, "two")],
        );
        up.close().await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["messages"][0]["blocks"][0]["text"], "one");
        assert_eq!(seen[1]["messages"][0]["blocks"][0]["text"], "two");
    }

    /// The skip is also recorded in the diagnostic file of a terminal frontend,
    /// whose logger discards ordinary lines, without the turn text.
    #[tokio::test]
    async fn turn_with_redaction_marker_is_not_sent() {
        let (url, seen) = serve(200, None).await;
        let home = tempfile::tempdir().unwrap();
        let logger = Arc::new(Logger::discard());
        logger.enable_diagnostics(home.path()).unwrap();
        let up = logged_uploader(&url, Duration::from_secs(5), logger);
        up.submit(
            &context(),
            wire::TurnStatus::Success,
            vec![
                text_message(Role::User, "hi"),
                message(Role::Tool, vec![tool_result("private-output [REDACTED]")]),
            ],
        );
        up.close().await;
        assert!(seen.lock().unwrap().is_empty());
        let mut text = String::new();
        for entry in std::fs::read_dir(home.path().join(".otto/logs")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                text.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        let dropped: Value = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|record| record["event"] == "memory_turn_dropped")
            .unwrap_or_else(|| panic!("no memory_turn_dropped record: {text}"));
        assert!(
            dropped["reason"]
                .as_str()
                .unwrap()
                .contains("redaction marker")
        );
        assert!(!dropped["turn_id"].as_str().unwrap().is_empty());
        assert!(!text.contains("private-output"), "{text}");
    }

    #[tokio::test]
    async fn turn_over_four_mebibytes_is_not_sent() {
        let (url, seen) = serve(200, None).await;
        let up = uploader(&url, Duration::from_secs(5));
        up.submit(
            &context(),
            wire::TurnStatus::Success,
            vec![text_message(Role::User, &"a".repeat(MAX_TURN_BYTES))],
        );
        up.close().await;
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn image_and_reasoning_blocks_are_omitted() {
        let (url, seen) = serve(200, None).await;
        let up = uploader(&url, Duration::from_secs(5));
        let mut image = block(BlockType::Image);
        image.data = "AAAA".into();
        let mut reasoning = block(BlockType::Reasoning);
        reasoning.text = "thinking".into();
        up.submit(
            &context(),
            wire::TurnStatus::Success,
            vec![
                message(Role::User, vec![Block::text("look"), image.clone()]),
                message(Role::User, vec![image.clone()]),
                message(Role::Assistant, vec![reasoning, Block::text("done")]),
                text_message(Role::Other("system".into()), "skipped"),
            ],
        );
        // A turn with no kept message sends nothing.
        up.submit(
            &context(),
            wire::TurnStatus::Success,
            vec![message(Role::User, vec![image])],
        );
        up.close().await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let messages = seen[0]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0]["blocks"],
            json!([{"type": "text", "text": "look"}])
        );
        assert_eq!(
            messages[1]["blocks"],
            json!([{"type": "text", "text": "done"}])
        );
    }

    #[tokio::test]
    async fn full_queue_discards_turns() {
        let gate = Arc::new(Notify::new());
        let (url, seen) = serve(200, Some(gate.clone())).await;
        let up = uploader(&url, Duration::from_secs(5));
        up.submit(
            &context(),
            wire::TurnStatus::Success,
            vec![text_message(Role::User, "0")],
        );
        // The first turn is in flight (held by the stub) and off the queue.
        settle(&seen, 1).await;
        for i in 1..10 {
            up.submit(
                &context(),
                wire::TurnStatus::Success,
                vec![text_message(Role::User, &i.to_string())],
            );
        }
        let releaser = tokio::spawn(async move {
            loop {
                gate.notify_one();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        up.close().await;
        releaser.abort();
        assert_eq!(seen.lock().unwrap().len(), 9);
    }

    #[tokio::test]
    async fn close_returns_within_the_bound_when_the_service_never_answers() {
        let gate = Arc::new(Notify::new());
        let (url, seen) = serve(200, Some(gate)).await;
        let up = uploader(&url, Duration::from_millis(200));
        up.submit(
            &context(),
            wire::TurnStatus::Success,
            vec![text_message(Role::User, "hi")],
        );
        settle(&seen, 1).await;
        let started = std::time::Instant::now();
        up.close().await;
        up.close().await;
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
