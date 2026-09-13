//! The memory tools. Port of `internal/tool/{memory_search,remember,forget}.go`.
//!
//! `memory_search` reads; `remember` and `forget` only ever queue a candidate
//! for human review, because the model's origin never carries write authority
//! through [`crate::memory::Service::propose`].
//!
//! Ownership: each tool holds an `Arc` of the shared service, which stays open
//! for the life of the session. Concurrency and cancellation follow the tool
//! contract: `execute` takes `&self`, and a cancelled token yields the Go
//! `context canceled` text before any store call.

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
use crate::memory::{
    CandidateAction, Origin, ProposeRequest, Provenance, Scope, SearchRequest, Service,
};

const MEMORY_SEARCH_LIMIT: usize = 20;
const MEMORY_SEARCH_TOKEN_BUDGET: usize = 4000;

/// Reads the records this session is bound to.
pub struct MemorySearchTool {
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
}

/// The schema advertised for `memory_search`.
pub fn memory_search_definition() -> ToolDefinition {
    definition(
        "memory_search",
        "Search remembered facts and preferences for the current user and workspace",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search terms; empty lists recently active records"
                }
            }
        }),
    )
}

#[async_trait::async_trait]
impl Tool for MemorySearchTool {
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
        let result = match self.service.search(&SearchRequest {
            query: args.query,
            scopes: self.scopes.clone(),
            limit: MEMORY_SEARCH_LIMIT,
            token_budget: MEMORY_SEARCH_TOKEN_BUDGET,
            now: Utc::now(),
            ..SearchRequest::default()
        }) {
            Ok(result) => result,
            Err(error) => return error_result(error),
        };
        if result.records.is_empty() {
            return ToolResult {
                content: "no matching records".into(),
                persisted_content: Some("0 records".into()),
                is_error: false,
            };
        }

        let mut content = String::new();
        let mut ids = Vec::with_capacity(result.records.len());
        for record in &result.records {
            ids.push(record.id.as_str());
            let _ = writeln!(
                content,
                "id={} scope={}/{} kind={} key={} revision={} text={}",
                record.id,
                record.scope.namespace,
                record.scope.id,
                record.kind,
                record.key,
                record.revision,
                record.text
            );
        }
        let mut rendered = capped_text_result(&content, self.max_output_bytes);
        rendered.persisted_content = Some(format!(
            "{} records: {}",
            result.records.len(),
            ids.join(", ")
        ));
        rendered
    }
}

/// Proposes a record for human review.
pub struct RememberTool {
    service: Arc<Service>,
    default_scope: Scope,
}

impl RememberTool {
    pub fn new(service: Arc<Service>, default_scope: Scope) -> Self {
        Self {
            service,
            default_scope,
        }
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
        "Propose a fact or preference to remember; a human reviews it before it takes effect",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "kind": {"type": "string", "description": "Record category, e.g. preference, instruction, fact"},
                "key": {"type": "string", "description": "Stable key for updating this record later"},
                "text": {"type": "string", "description": "Text to remember"},
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

#[async_trait::async_trait]
impl Tool for RememberTool {
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
        let action = if args.target_id.is_empty() {
            CandidateAction::Create
        } else {
            CandidateAction::Update
        };
        proposal_result(self.service.propose(&ProposeRequest {
            action,
            scope: self.default_scope.clone(),
            kind: args.kind,
            key: args.key,
            text: args.text,
            labels: args.labels,
            confidence: args.confidence,
            target_id: args.target_id,
            base_revision: args.base_revision,
            reason: args.reason,
            source: Provenance {
                origin: Some(Origin::Model),
                ..Provenance::default()
            },
            ..ProposeRequest::default()
        }))
    }
}

/// Proposes forgetting a record for human review.
pub struct ForgetTool {
    service: Arc<Service>,
    scopes: Vec<Scope>,
}

impl ForgetTool {
    pub fn new(service: Arc<Service>, scopes: Vec<Scope>) -> Self {
        Self { service, scopes }
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
        "Propose forgetting a remembered record found via memory_search; a human reviews it before it takes effect",
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

#[async_trait::async_trait]
impl Tool for ForgetTool {
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
        proposal_result(self.service.propose(&ProposeRequest {
            action: CandidateAction::Forget,
            scope,
            target_id: args.id,
            base_revision: args.revision,
            reason: args.reason,
            source: Provenance {
                origin: Some(Origin::Model),
                ..Provenance::default()
            },
            ..ProposeRequest::default()
        }))
    }
}

/// The shared reply of `remember` and `forget`.
fn proposal_result(outcome: crate::memory::Result<Vec<crate::memory::Candidate>>) -> ToolResult {
    let candidates = match outcome {
        Ok(candidates) => candidates,
        Err(error) => return error_result(error),
    };
    match candidates.first() {
        None => text_result("proposal was not queued for review"),
        Some(candidate) => text_result(format!(
            "candidate {} queued for human review (state={})",
            candidate.id,
            candidate.state.as_str()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::sqlite::testsupport::open_temp;
    use crate::memory::{
        CandidateRef, CandidateState, NAMESPACE_USER, RememberRequest, decide_default_policy,
    };
    use crate::tool::testutil::{MAX_OUTPUT_BYTES, run};

    fn service() -> (tempfile::TempDir, Arc<Service>) {
        let (directory, store) = open_temp();
        (
            directory,
            Arc::new(Service::new(store, decide_default_policy)),
        )
    }

    fn scope() -> Scope {
        Scope::new(NAMESPACE_USER, "user-1")
    }

    fn remember_record(service: &Service, key: &str, text: &str) -> String {
        service
            .remember(&RememberRequest {
                scope: scope(),
                kind: "preference".into(),
                key: key.into(),
                text: text.into(),
                ..RememberRequest::default()
            })
            .expect("the record is writable")
            .id
    }

    /// The candidate ID out of `candidate %s queued for human review (...)`.
    fn candidate_id(content: &str) -> String {
        content
            .strip_prefix("candidate ")
            .and_then(|rest| rest.split_once(' '))
            .map(|(id, _)| id.to_owned())
            .unwrap_or_else(|| panic!("unexpected content {content:?}"))
    }

    /// Go's `TestMemorySearchToolRendersRecordsAndPlaceholder`. Go injects a
    /// fake reader to fix the record IDs; here the real store mints them, so
    /// the placeholder is checked by count and membership instead of by a
    /// literal string.
    #[tokio::test]
    async fn search_renders_one_line_per_record_and_a_bounded_placeholder() {
        let (_directory, service) = service();
        let first = remember_record(&service, "editor", "prefers vim");
        let second = remember_record(&service, "commits", "always run tests before commit");
        let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

        let result = run(&tool, r#"{"query":""}"#).await;
        assert!(!result.is_error, "{result:?}");
        for (id, text, revision) in [
            (&first, "prefers vim", "revision=1"),
            (&second, "always run tests before commit", "revision=1"),
        ] {
            assert!(result.content.contains(id.as_str()), "{result:?}");
            assert!(result.content.contains(text), "{result:?}");
            assert!(result.content.contains(revision), "{result:?}");
        }
        assert!(
            result
                .content
                .contains("scope=user/user-1 kind=preference key=editor"),
            "{result:?}"
        );

        let persisted = result.persisted_content.expect("a bounded placeholder");
        assert!(persisted.starts_with("2 records: "), "{persisted:?}");
        assert!(
            persisted.contains(&first) && persisted.contains(&second),
            "{persisted:?}"
        );
    }

    /// Go's `TestMemorySearchToolHandlesNoMatches`.
    #[tokio::test]
    async fn search_with_no_matches_reports_zero_records() {
        let (_directory, service) = service();
        let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

        let result = run(&tool, r#"{"query":"nothing"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "no matching records");
        assert_eq!(result.persisted_content.as_deref(), Some("0 records"));
    }

    /// Go's `TestMemorySearchToolSurfacesReaderError`, with a closed service
    /// standing in for the fake reader that returns an error.
    #[tokio::test]
    async fn search_surfaces_a_service_error() {
        let (_directory, service) = service();
        service.close().expect("close");
        let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

        let result = run(&tool, r#"{"query":"x"}"#).await;
        assert!(result.is_error, "{result:?}");
        assert!(result.content.contains("closed"), "{result:?}");
    }

    /// Go's `TestRememberToolProposesCreateCandidate`.
    #[tokio::test]
    async fn remember_queues_a_create_candidate_for_human_review() {
        let (_directory, service) = service();
        let tool = RememberTool::new(Arc::clone(&service), scope());

        let result = run(&tool, r#"{"kind":"preference","text":"likes dark mode"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert!(
            result
                .content
                .ends_with(" queued for human review (state=pending)"),
            "{result:?}"
        );

        let candidate = service
            .get_candidate(&CandidateRef {
                scope: scope(),
                id: candidate_id(&result.content),
            })
            .expect("the candidate is stored");
        assert_eq!(candidate.action, CandidateAction::Create);
        assert_eq!(candidate.state, CandidateState::Pending);
        assert_eq!(candidate.proposed.scope, scope());
        assert_eq!(candidate.proposed.text, "likes dark mode");
        assert_eq!(candidate.proposed.source.origin, Some(Origin::Model));
    }

    /// Go's `TestRememberToolProposesUpdateCandidateWhenTargetIDSet`.
    #[tokio::test]
    async fn remember_queues_an_update_candidate_when_a_target_id_is_given() {
        let (_directory, service) = service();
        let target = remember_record(&service, "editor", "prefers vim");
        let tool = RememberTool::new(Arc::clone(&service), scope());

        let arguments = format!(
            r#"{{"kind":"preference","text":"prefers neovim","target_id":"{target}","base_revision":1}}"#
        );
        let result = run(&tool, &arguments).await;
        assert!(!result.is_error, "{result:?}");

        let candidate = service
            .get_candidate(&CandidateRef {
                scope: scope(),
                id: candidate_id(&result.content),
            })
            .expect("the candidate is stored");
        assert_eq!(candidate.action, CandidateAction::Update);
        assert_eq!(candidate.target_id, target);
        assert_eq!(candidate.base_revision, 1);
    }

    /// Go's `TestRememberToolRequiresKindAndText`.
    #[tokio::test]
    async fn remember_requires_a_kind_and_a_text() {
        let (_directory, service) = service();
        let tool = RememberTool::new(service, scope());

        let result = run(&tool, r#"{"kind":"preference"}"#).await;
        assert!(result.is_error, "{result:?}");
        assert!(result.content.contains("text"), "{result:?}");
    }

    /// Go's `TestForgetToolProposesForgetCandidate`.
    #[tokio::test]
    async fn forget_queues_a_forget_candidate_for_human_review() {
        let (_directory, service) = service();
        let target = remember_record(&service, "editor", "prefers vim");
        let tool = ForgetTool::new(Arc::clone(&service), vec![scope()]);

        let arguments = format!(
            r#"{{"scope_namespace":"user","scope_id":"user-1","id":"{target}","revision":1}}"#
        );
        let result = run(&tool, &arguments).await;
        assert!(!result.is_error, "{result:?}");

        let candidate = service
            .get_candidate(&CandidateRef {
                scope: scope(),
                id: candidate_id(&result.content),
            })
            .expect("the candidate is stored");
        assert_eq!(candidate.action, CandidateAction::Forget);
        assert_eq!(candidate.target_id, target);
        assert_eq!(candidate.base_revision, 1);
        assert_eq!(candidate.proposed.source.origin, Some(Origin::Model));
    }

    /// Go's `TestForgetToolRequiresIdentifyingFields`.
    #[tokio::test]
    async fn forget_requires_the_identifying_fields() {
        let (_directory, service) = service();
        let tool = ForgetTool::new(service, vec![scope()]);

        let result = run(&tool, r#"{"id":"rec-1"}"#).await;
        assert!(result.is_error, "{result:?}");
    }

    /// Go's `TestForgetToolRejectsScopeOutsideBoundScopes`. The rejection must
    /// happen before the service is asked, so an unbound scope leaves no
    /// candidate behind.
    #[tokio::test]
    async fn forget_rejects_a_scope_outside_the_bound_scopes() {
        let (_directory, service) = service();
        let tool = ForgetTool::new(Arc::clone(&service), vec![scope()]);

        let result = run(
            &tool,
            r#"{"scope_namespace":"workspace","scope_id":"someone-elses-workspace","id":"rec-1","revision":2}"#,
        )
        .await;
        assert!(result.is_error, "{result:?}");
        assert_eq!(
            result.content,
            "scope is not one of the scopes this session is bound to"
        );
    }
}
