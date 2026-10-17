//! Remote memory protocol 1.0, replaced in place by the approved scope design.
use chrono::{DateTime, Utc};
use otto_core::memory::ScopeNamespace;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub namespace: ScopeNamespace,
    pub id: String,
}
impl From<&otto_core::memory::Scope> for Scope {
    fn from(scope: &otto_core::memory::Scope) -> Self {
        Self {
            namespace: match scope.namespace.as_str() {
                "organization" => ScopeNamespace::Organization,
                "workspace" => ScopeNamespace::Workspace,
                "user" => ScopeNamespace::User,
                _ => unreachable!("validated remote scope"),
            },
            id: scope.id.clone(),
        }
    }
}
impl From<Scope> for otto_core::memory::Scope {
    fn from(scope: Scope) -> Self {
        Self {
            namespace: scope.namespace.as_str().into(),
            id: scope.id,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub organization_id: String,
    pub user_id: String,
    pub workspace_id: Option<String>,
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
    pub scope_filter: Option<Vec<ScopeNamespace>>,
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
    pub revision: String,
    #[serde(deserialize_with = "required_nullable")]
    pub key: Option<String>,
    pub text: String,
    #[serde(default)]
    pub pinned: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecalledItem {
    #[serde(flatten)]
    pub item: Item,
    pub mandatory: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    #[serde(rename = "workspace_organization_user")]
    WorkspaceOrganizationUser,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallResponse {
    pub context: Context,
    pub resolution: Resolution,
    pub coverage: Coverage,
    pub items: Vec<RecalledItem>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetBody {
    pub context: Context,
    pub scope: Scope,
    pub id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RawView {
    #[serde(rename = "raw")]
    Raw,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetResponse {
    pub view: RawView,
    pub organization_id: String,
    pub item: Item,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub revision: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberBody {
    pub context: Context,
    pub request_id: String,
    pub origin: Origin,
    pub scope: Scope,
    pub target: Option<Target>,
    pub key: Option<String>,
    pub text: String,
    pub metadata: BTreeMap<String, String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetBody {
    pub context: Context,
    pub request_id: String,
    pub origin: Origin,
    pub scope: Scope,
    pub id: String,
    pub revision: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    Applied,
    NoChange,
    Skipped,
    Pending,
    Failed,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRef {
    pub organization_id: String,
    pub scope: Scope,
    pub id: String,
    pub revision: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub request_id: String,
    pub state: ReceiptState,
    #[serde(deserialize_with = "required_nullable")]
    pub item: Option<ItemRef>,
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

/// Reject duplicate object members recursively, including unknown fields, before DTO decoding.
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    use serde::de::{self, MapAccess, SeqAccess, Visitor};
    struct Unique;
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Unique;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("JSON without duplicate object members")
                }
                fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> Result<Unique, M::Error> {
                    let mut keys = std::collections::HashSet::new();
                    while let Some(key) = m.next_key::<String>()? {
                        if !keys.insert(key) {
                            return Err(de::Error::custom("duplicate JSON member"));
                        }
                        m.next_value::<Unique>()?;
                    }
                    Ok(Unique)
                }
                fn visit_seq<S: SeqAccess<'de>>(self, mut s: S) -> Result<Unique, S::Error> {
                    while s.next_element::<Unique>()?.is_some() {}
                    Ok(Unique)
                }
                fn visit_bool<E: de::Error>(self, _: bool) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_i64<E: de::Error>(self, _: i64) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_u64<E: de::Error>(self, _: u64) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_f64<E: de::Error>(self, _: f64) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_str<E: de::Error>(self, _: &str) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                    Ok(Unique)
                }
            }
            d.deserialize_any(V)
        }
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    Unique::deserialize(&mut decoder)?;
    decoder.end()?;
    serde_json::from_slice(bytes)
}

fn required_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(deserializer)
}
