//! Frontend-neutral human approval contracts. Native callers supply identity,
//! time, storage and domain application; models never supply decision authority.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Authorization,
    Confirmation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Approved,
    Rejected,
    Withdrawn,
    Expired,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationState {
    NotRequired,
    NotStarted,
    Applying,
    Applied,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub workspace: String,
    pub session_id: String,
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    #[serde(default)]
    pub tool_call_id: String,
    pub source: String,
    pub id: String,
    pub version: String,
    /// Canonical domain reference or sanitized review content. No credentials.
    pub details: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub option_id: String,
    pub feedback: String,
    pub actor: String,
    pub transport: String,
    pub decided_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)]
    pub options: Vec<Choice>,
    pub schema_version: u32,
    pub id: String,
    pub revision: u64,
    pub kind: Kind,
    pub scope: Scope,
    pub subject: Subject,
    pub title: String,
    pub reason: String,
    pub persistent: bool,
    pub blocking: bool,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub state: State,
    pub decision: Option<Decision>,
    pub application: ApplicationState,
    pub application_error: String,
}

impl Request {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1
            || self.revision == 0
            || self.id.is_empty()
            || self.scope.workspace.is_empty()
            || self.subject.source.is_empty()
            || self.subject.id.is_empty()
            || self.subject.version.is_empty()
            || self.title.trim().is_empty()
            || self.title.len() > 4096
            || self.reason.len() > 8192
            || self.subject.details.len() > 65536
        {
            return Err("invalid approval request");
        }
        if self.expires_at.is_some_and(|time| time <= self.created_at) {
            return Err("approval expiry must follow creation");
        }
        if self.kind == Kind::Confirmation && self.persistent {
            return Err("confirmation cannot establish persistent authority");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub sequence: i64,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub requests: Vec<Request>,
    pub cursor: i64,
}

/// A confirmation requires a new provider step before later actions can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatchControl {
    #[default]
    Continue,
    Replan,
}
