//! Delivery of scheduled items to an ACP client.
//!
//! A client that sets `clientCapabilities._meta.otto.scheduleDelivery` in
//! `initialize` (an array that may contain `telegram`) receives due items of
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
use otto::app::scheduled::run_new_session;
use otto::schedule::worker::{DeliverRole, PromptRunner, RunRole, Sink, Worker};
use otto::schedule::{Origin, RunOutcome, Scope};
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
        if platform == "telegram" && !platforms.iter().any(|p| p == platform) {
            platforms.push(platform.to_string());
        }
    }
    platforms
}

/// A present declaration is authoritative, including an empty list.
pub(super) fn delivery_chats(
    params: &Value,
    platforms: &[String],
) -> Result<Option<Vec<Origin>>, String> {
    let Some(value) = params.pointer("/clientCapabilities/_meta/otto/scheduleDeliveryChats") else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or("scheduleDeliveryChats must be an array")?;
    let mut chats = Vec::new();
    for value in values {
        let value = value
            .as_str()
            .ok_or("scheduleDeliveryChats entries must be strings")?;
        let (platform, id) = value
            .split_once(':')
            .ok_or("delivery chat must be <platform>:<chat id>")?;
        if !platforms.iter().any(|offered| offered == platform) {
            return Err("delivery chat platform was not advertised".into());
        }
        let origin = Origin::chat(platform, id)?;
        if !chats.contains(&origin) {
            chats.push(origin);
        }
    }
    Ok(Some(chats))
}

/// The worker of one connection, or `None` without platforms or a store.
pub(super) fn worker(connection: &Arc<Connection>, mut platforms: Vec<String>) -> Option<Worker> {
    let destinations = delivery_chats(
        &connection
            .client_capabilities
            .lock()
            .expect("ACP capabilities"),
        &platforms,
    )
    .ok()?;
    if let Some(chats) = &destinations {
        platforms.retain(|p| {
            chats
                .iter()
                .any(|chat| chat.chat_platform() == Some(p.as_str()))
        });
    }
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
                scope: destinations
                    .as_ref()
                    .map(|origins| Scope::Origins(origins.clone()))
                    .unwrap_or_else(|| Scope::Chats(platforms.clone())),
                workspace: Some(local.config.workspace.to_string_lossy().into_owned()),
                admit: None,
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
    let client_id = otto::runtime::builder::random_id().ok()?;
    if store
        .register_delivery_client(
            &client_id,
            &platforms,
            destinations.as_deref(),
            chrono::Utc::now(),
        )
        .is_err()
    {
        logger.warn("schedule_delivery_registration_failed", &[]);
        return None;
    }
    let sink = AcpSink {
        connection: Arc::clone(connection),
        timeout: DELIVERY_TIMEOUT,
    };
    Some(Worker {
        store,
        run,
        deliver: Some(DeliverRole {
            scope: Scope::DeliveryClient(client_id),
            sink: Arc::new(sink),
        }),
        logger,
        metrics,
    })
}

struct LocalRunner {
    builder: Arc<otto::runtime::builder::Builder>,
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
mod tests;
