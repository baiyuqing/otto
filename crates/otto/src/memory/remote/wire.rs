//! JSON bodies of remote memory protocol 1.x. Field names and rules follow
//! `docs/specs/2026-10-17-remote-memory-protocol.md`. Unknown response fields
//! are ignored; unknown enum values fail to decode.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use otto_core::memory::Scope;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub scopes: Vec<Scope>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Human,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallBody {
    pub context: Context,
    pub query: String,
    pub limit: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallResponse {
    pub coverage: Coverage,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetBody {
    pub context: Context,
    pub scope: Scope,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetResponse {
    pub item: Item,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberBody {
    pub context: Context,
    pub request_id: String,
    pub origin: Origin,
    pub scope: Scope,
    pub text: String,
    pub target: Option<Target>,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetBody {
    pub context: Context,
    pub request_id: String,
    pub origin: Origin,
    pub scope: Scope,
    pub id: String,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    Applied,
    NoChange,
    Skipped,
    Pending,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub state: ReceiptState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Success,
    Cancelled,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRole {
    User,
    Assistant,
    Tool,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnBlock {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        id: String,
        name: String,
        text: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMessage {
    pub id: String,
    pub role: TurnRole,
    pub created_at: DateTime<Utc>,
    pub blocks: Vec<TurnBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRequest {
    pub context: Context,
    pub turn_id: String,
    pub status: TurnStatus,
    pub messages: Vec<TurnMessage>,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// Decodes a golden file into `T` and checks that re-encoding gives the
    /// same JSON value.
    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(fixture: &str) {
        let expected: Value = serde_json::from_str(fixture).expect("fixture is JSON");
        let decoded: T = serde_json::from_str(fixture).expect("fixture decodes");
        assert_eq!(serde_json::to_value(&decoded).expect("encodes"), expected);
    }

    #[test]
    fn fixtures_decode_and_re_encode_to_equal_values() {
        round_trip::<RecallBody>(include_str!("fixtures/v1/recall_request.json"));
        round_trip::<RecallResponse>(include_str!("fixtures/v1/recall_response.json"));
        round_trip::<GetBody>(include_str!("fixtures/v1/get_request.json"));
        round_trip::<GetResponse>(include_str!("fixtures/v1/get_response.json"));
        round_trip::<RememberBody>(include_str!("fixtures/v1/remember_request.json"));
        round_trip::<ForgetBody>(include_str!("fixtures/v1/forget_request.json"));
        round_trip::<Receipt>(include_str!("fixtures/v1/receipt.json"));
        round_trip::<ErrorBody>(include_str!("fixtures/v1/error.json"));
        round_trip::<TurnRequest>(include_str!("fixtures/v1/turns_request.json"));
    }

    #[test]
    fn unknown_response_fields_are_ignored() {
        let response: RecallResponse = serde_json::from_value(json!({
            "coverage": "complete",
            "future": 1,
            "items": [{"id": "a", "scope": {"namespace": "user", "id": "u", "x": 1},
                       "text": "t", "future": [1]}],
        }))
        .expect("decodes");
        assert_eq!(response.items.len(), 1);
        assert!(!response.items[0].pinned);
    }

    #[test]
    fn unknown_enum_values_are_rejected() {
        assert!(
            serde_json::from_value::<RecallResponse>(json!({"coverage": "most", "items": []}))
                .is_err()
        );
        assert!(serde_json::from_value::<Receipt>(json!({"state": "done"})).is_err());
    }
}
