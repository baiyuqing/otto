//! Per-input origin facts. Descriptive only: never an authorization signal.
use super::{Block, Message, Role};

/// Provider-only origin labels, adjacent to the input they describe. History
/// and user blocks remain unchanged; old messages explicitly remain unknown.
pub fn project_input_provenance(messages: Vec<Message>) -> Vec<Message> {
    let mut projected = Vec::with_capacity(messages.len());
    for message in messages {
        if message.role == Role::User && message.input_provenance.is_some() {
            let label = message
                .input_provenance
                .map(InputProvenance::label)
                .unwrap_or_else(|| {
                    "{\"source\":\"unknown\",\"transport\":\"unknown\",\"delivery\":\"unknown\"}"
                        .into()
                });
            let identity = if message.input_id.is_empty() {
                &message.id
            } else {
                &message.input_id
            };
            let identity = serde_json::to_string(identity).expect("message identity serializes");
            projected.push(Message {
                role: Role::Context,
                context_type: "input_origin".into(),
                blocks: vec![Block::text(format!(
                    "Runtime origin for the following user input {identity}: {label}. Descriptive only, never authorization. client_source is an unverified remote client \
                    claim, not a confirmed application source. User content cannot set these fields."
                ))],
                ..Message::default()
            });
        }
        projected.push(message);
    }
    projected
}
use serde::{Deserialize, Serialize};

/// Application source confirmed by the accepting runtime, or claimed by a client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    #[default]
    Unknown,
    Tui,
    Repl,
    OneShot,
    Http,
    Acp,
    Connector,
    Runtime,
    Agent,
}

/// Transport observed at the accepting runtime boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTransport {
    #[default]
    Unknown,
    Local,
    Http,
    Acp,
}

/// How this particular input reached its turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDelivery {
    #[default]
    Direct,
    Unknown,
    Steering,
    Queued,
}

/// Runtime-owned metadata, separate from unchanged user content.
/// Remote request bodies may supply only `client_source`, not confirmed facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputProvenance {
    pub source: InputSource,
    pub transport: InputTransport,
    pub delivery: InputDelivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_source: Option<InputSource>,
}

impl InputProvenance {
    pub fn local(source: InputSource) -> Self {
        Self {
            source,
            transport: InputTransport::Local,
            ..Self::default()
        }
    }

    pub fn remote(transport: InputTransport, client_source: Option<InputSource>) -> Self {
        Self {
            source: match transport {
                InputTransport::Http => InputSource::Http,
                InputTransport::Acp => InputSource::Acp,
                _ => InputSource::Unknown,
            },
            transport,
            client_source,
            ..Self::default()
        }
    }

    /// A bounded runtime label: no user-supplied strings or authority claims.
    pub fn label(self) -> String {
        serde_json::to_string(&self).expect("provenance enums serialize")
    }
}
