//! The memory tools.
//!
//! `memory_search` reads; `remember` and `forget` only ever queue a candidate
//! for human review, because the model's origin never carries write authority
//! through [`crate::memory::Service::propose`]. A direct human `/remember`
//! command is a separate trusted entry point and applies immediately; a model
//! must never claim that authority by creating a generic confirmation.
//!
//! Ownership: each tool holds an `Arc` of the shared service, which stays open
//! for the life of the session. Concurrency and cancellation follow the tool
//! contract: `execute` takes `&self`, and a cancelled token yields the `context
//! canceled` text before any store call.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::Utc;
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::result::{capped_text_result, decode_strict_json};
use super::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};
use crate::memory::advice::Advisor;
use crate::memory::{
    CandidateAction, ErrorKind, Origin, ProposeRequest, Provenance, RecordRef, Scope,
    SearchRequest, Service,
};

const MEMORY_SEARCH_LIMIT: usize = 20;
const MEMORY_SEARCH_TOKEN_BUDGET: usize = 4000;

/// Reads the records this session is bound to.
pub(crate) struct MemorySearchTool {
    service: Arc<Service>,
    scopes: Vec<Scope>,
    max_output_bytes: usize,
}

impl MemorySearchTool {
    pub fn new(service: Arc<Service>, scopes: Vec<Scope>, max_output_bytes: usize) -> Self {
        Self {
            service,
            scopes,
            max_output_bytes,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemorySearchArgs {
    #[serde(default)]
    query: String,
    id: Option<String>,
}

/// The schema advertised for `memory_search`.
pub fn memory_search_definition() -> ToolDefinition {
    definition(
        "memory_search",
        "Search remembered facts and preferences for the current user and \
            workspace. Query terms match as case-insensitive substrings of record \
            text, kind, key and labels; an empty query lists recently updated \
            records. A search returns active records only. An id lookup returns the \
            record in any state and shows state, and superseded_by when set. \
            Previous turns retain only record IDs; use id to read the \
            current scope and revision before proposing an update or forget.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Space-separated terms matched as case-insensitive substrings of text, kind, key and labels; empty lists recently updated active records"
                },
                "id": {
                    "type": "string",
                    "description": "Exact record ID to look up in this session's scopes in any state; the output shows the record state; omit query when using id"
                }
            }
        }),
    )
}

pub(crate) const MEMORY_SEARCH_RETRY: (&str, otto_core::model::RetrySafety) =
    ("memory_search", otto_core::model::RetrySafety::ReadOnly);

#[async_trait::async_trait]
impl Tool for MemorySearchTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        MEMORY_SEARCH_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        memory_search_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: MemorySearchArgs = match decode_strict_json(arguments.get(), &[]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let records = if let Some(id) = args.id {
            if !args.query.is_empty() {
                return error_result("use either id or query, not both");
            }
            let mut records = Vec::new();
            for scope in &self.scopes {
                match self.service.get(&RecordRef {
                    scope: scope.clone(),
                    id: id.clone(),
                }) {
                    Ok(record) => {
                        records.push(record);
                        break;
                    }
                    Err(error) if error.is(ErrorKind::NotFound) => {}
                    Err(error) => return error_result(error),
                }
            }
            records
        } else {
            match self.service.search(&SearchRequest {
                query: args.query,
                scopes: self.scopes.clone(),
                limit: MEMORY_SEARCH_LIMIT,
                token_budget: MEMORY_SEARCH_TOKEN_BUDGET,
                now: Utc::now(),
                ..SearchRequest::default()
            }) {
                Ok(result) => result.records,
                Err(error) => return error_result(error),
            }
        };
        if records.is_empty() {
            return ToolResult {
                content: "no matching records".into(),
                persisted_content: Some("0 records".into()),
                is_error: false,
                outcome_override: None,
            };
        }

        let mut content = String::new();
        let mut ids = Vec::with_capacity(records.len());
        for record in &records {
            ids.push(record.id.as_str());
            let superseded_by = if record.superseded_by.is_empty() {
                String::new()
            } else {
                format!(" superseded_by={}", record.superseded_by)
            };
            let _ = writeln!(
                content,
                "id={} scope={}/{} kind={} key={} revision={} state={}{} text={}",
                record.id,
                record.scope.namespace,
                record.scope.id,
                record.kind,
                record.key,
                record.revision,
                record.state.as_str(),
                superseded_by,
                record.text
            );
        }
        let mut rendered = capped_text_result(&content, self.max_output_bytes);
        rendered.persisted_content = Some(format!("{} records: {}", records.len(), ids.join(", ")));
        rendered
    }
}

/// Proposes a record for human review.
pub(crate) struct RememberTool {
    service: Arc<Service>,
    user_scope: Scope,
    workspace_scope: Scope,
    advisor: Option<Arc<Advisor>>,
    session_id: String,
    turn_origin: Option<Arc<otto_session::origin::TurnOrigin>>,
}

impl RememberTool {
    pub fn new(service: Arc<Service>, user_scope: Scope, workspace_scope: Scope) -> Self {
        Self {
            service,
            user_scope,
            workspace_scope,
            advisor: None,
            session_id: String::new(),
            turn_origin: None,
        }
    }

    pub(crate) fn with_turn_context(
        mut self,
        session_id: impl Into<String>,
        origin: Arc<otto_session::origin::TurnOrigin>,
    ) -> Self {
        self.session_id = session_id.into();
        self.turn_origin = Some(origin);
        self
    }

    /// Adds advisory review hints to the candidates this tool queues.
    pub(crate) fn with_advisor(mut self, advisor: Option<Arc<Advisor>>) -> Self {
        self.advisor = advisor;
        self
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberArgs {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    confidence: f64,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    target_id: String,
    #[serde(default)]
    base_revision: u64,
}

/// The schema advertised for `remember`.
pub fn remember_definition() -> ToolDefinition {
    definition(
        "remember",
        "Propose a fact or preference to remember. Returns a version-bound \
            candidate and its state. Otto's approval authority reviews its content \
            and scope before applying it; AUTO uses original user evidence and \
            MANUAL requires a person. Pending is not a completed memory change.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "kind": {"type": "string", "description": "Record category, e.g. preference, instruction, fact"},
                "key": {"type": "string", "description": "Stable key for updating this record later"},
                "text": {"type": "string", "description": "Text to remember"},
                "scope": {"type": "string", "enum": ["user", "workspace"], "description": "user applies in every workspace and is for how the user wants the agent \
                    to work (working habits, communication preferences); workspace \
                    (default) is for this project only. For an update, use the scope of the \
                    target record as returned by memory_search. A user-scope preference or \
                    instruction needs a key"},
                "labels": {"type": "array", "items": {"type": "string"}},
                "confidence": {"type": "number", "description": "Confidence from 0 to 1"},
                "reason": {"type": "string", "description": "Why this should be remembered"},
                "target_id": {"type": "string", "description": "Existing record ID, from memory_search, when proposing an update"},
                "base_revision": {"type": "integer", "description": "Revision of the record being updated, from memory_search"}
            },
            "required": ["kind", "text"]
        }),
    )
}

pub(crate) const REMEMBER_RETRY: (&str, otto_core::model::RetrySafety) =
    ("remember", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for RememberTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        REMEMBER_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        remember_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: RememberArgs = match decode_strict_json(arguments.get(), &["kind", "text"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let scope = match args.scope.as_str() {
            "" | "workspace" => self.workspace_scope.clone(),
            "user" => self.user_scope.clone(),
            other => {
                return error_result(format!(
                    "invalid scope {other:?}: use \"user\" or \"workspace\""
                ));
            }
        };
        let action = if args.target_id.is_empty() {
            CandidateAction::Create
        } else {
            CandidateAction::Update
        };
        let mut metadata = std::collections::BTreeMap::new();
        if let Some(origin) = self.turn_origin.as_ref().and_then(|origin| origin.get()) {
            metadata.insert(
                crate::memory::PRESENTATION_ORIGIN_METADATA.into(),
                origin.as_str().to_owned(),
            );
        }
        let request = ProposeRequest {
            action,
            scope,
            kind: args.kind,
            key: args.key,
            text: args.text,
            labels: args.labels,
            metadata,
            confidence: args.confidence,
            target_id: args.target_id,
            base_revision: args.base_revision,
            reason: args.reason,
            source: Provenance {
                origin: Some(Origin::Model),
                session_id: self.session_id.clone(),
                ..Provenance::default()
            },
            ..ProposeRequest::default()
        };
        let Some(advisor) = &self.advisor else {
            return proposal_result(self.service.propose(&request));
        };
        match advisor
            .propose(&self.service, &self.user_scope, vec![request], cancel)
            .await
            .and_then(|mut results| results.pop())
        {
            Some(result) => proposal_result(result),
            None => error_result(CONTEXT_CANCELED),
        }
    }
}

/// Proposes forgetting a record for human review.
pub(crate) struct ForgetTool {
    service: Arc<Service>,
    scopes: Vec<Scope>,
    session_id: String,
    turn_origin: Option<Arc<otto_session::origin::TurnOrigin>>,
}

impl ForgetTool {
    pub fn new(service: Arc<Service>, scopes: Vec<Scope>) -> Self {
        Self {
            service,
            scopes,
            session_id: String::new(),
            turn_origin: None,
        }
    }

    pub(crate) fn with_turn_context(
        mut self,
        session_id: impl Into<String>,
        origin: Arc<otto_session::origin::TurnOrigin>,
    ) -> Self {
        self.session_id = session_id.into();
        self.turn_origin = Some(origin);
        self
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetArgs {
    #[serde(default)]
    scope_namespace: String,
    #[serde(default)]
    scope_id: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    reason: String,
}

/// The schema advertised for `forget`.
pub fn forget_definition() -> ToolDefinition {
    definition(
        "forget",
        "Propose forgetting a remembered record found via memory_search. Returns \
            a version-bound candidate and its state; Otto's approval authority \
            reviews scope before applying it. If only the record ID is available, \
            first call memory_search with id to read its current scope and \
            revision. Pending is not a completed deletion.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "scope_namespace": {"type": "string", "description": "Scope namespace of the record, from memory_search"},
                "scope_id": {"type": "string", "description": "Scope ID of the record, from memory_search"},
                "id": {"type": "string", "description": "Record ID to forget"},
                "revision": {"type": "integer", "description": "Record revision, from memory_search"},
                "reason": {"type": "string", "description": "Why this should be forgotten"}
            },
            "required": ["scope_namespace", "scope_id", "id", "revision"]
        }),
    )
}

pub(crate) const FORGET_RETRY: (&str, otto_core::model::RetrySafety) =
    ("forget", otto_core::model::RetrySafety::NonIdempotent);

#[async_trait::async_trait]
impl Tool for ForgetTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        FORGET_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        forget_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: ForgetArgs = match decode_strict_json(
            arguments.get(),
            &["scope_namespace", "scope_id", "id", "revision"],
        ) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let scope = Scope::new(args.scope_namespace, args.scope_id);
        if !self.scopes.contains(&scope) {
            return error_result("scope is not one of the scopes this session is bound to");
        }
        let mut metadata = std::collections::BTreeMap::new();
        if let Some(origin) = self.turn_origin.as_ref().and_then(|origin| origin.get()) {
            metadata.insert(
                crate::memory::PRESENTATION_ORIGIN_METADATA.into(),
                origin.as_str().to_owned(),
            );
        }
        proposal_result(self.service.propose(&ProposeRequest {
            action: CandidateAction::Forget,
            scope,
            metadata,
            target_id: args.id,
            base_revision: args.revision,
            reason: args.reason,
            source: Provenance {
                origin: Some(Origin::Model),
                session_id: self.session_id.clone(),
                ..Provenance::default()
            },
            ..ProposeRequest::default()
        }))
    }
}

/// The shared reply of `remember` and `forget`.
/// Tells the model how the person decides a queued candidate, so it can point
/// them there instead of claiming nobody can. It grants the model nothing:
/// every path named here is a human command.
const REVIEW_HINT: &str = "You cannot approve it yourself. Otto's approval authority reviews this \
    exact candidate against original user evidence and live policy. If a \
    human decision is needed, use /memory in chat or /memory review in the \
    terminal. Report its actual state; queued does not mean applied.";

fn proposal_result(outcome: crate::memory::Result<Vec<crate::memory::Candidate>>) -> ToolResult {
    let candidates = match outcome {
        Ok(candidates) => candidates,
        Err(error) => return error_result(error),
    };
    match candidates.first() {
        None => text_result("proposal was not queued for review"),
        Some(candidate) => text_result(format!(
            "candidate {} queued for approval review (state={}). {REVIEW_HINT}",
            candidate.id,
            candidate.state.as_str()
        )),
    }
}

#[cfg(test)]
mod tests;
