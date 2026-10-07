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
    id: Option<String>,
}

/// The schema advertised for `memory_search`.
pub fn memory_search_definition() -> ToolDefinition {
    definition(
        "memory_search",
        "Search remembered facts and preferences for the current user and workspace. Previous turns retain only record IDs; use id to read the current scope and revision before proposing an update or forget.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search terms; empty lists recently active records"
                },
                "id": {
                    "type": "string",
                    "description": "Exact record ID to look up in this session's scopes; omit query when using id"
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
        rendered.persisted_content = Some(format!("{} records: {}", records.len(), ids.join(", ")));
        rendered
    }
}

/// Proposes a record for human review.
pub struct RememberTool {
    service: Arc<Service>,
    user_scope: Scope,
    workspace_scope: Scope,
    advisor: Option<Arc<Advisor>>,
    session_id: String,
    turn_origin: Option<Arc<crate::schedule::tools::TurnOrigin>>,
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

    pub fn with_turn_context(
        mut self,
        session_id: impl Into<String>,
        origin: Arc<crate::schedule::tools::TurnOrigin>,
    ) -> Self {
        self.session_id = session_id.into();
        self.turn_origin = Some(origin);
        self
    }

    /// Adds advisory review hints to the candidates this tool queues.
    pub fn with_advisor(mut self, advisor: Option<Arc<Advisor>>) -> Self {
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
        "Propose a fact or preference to remember; a human reviews it before it takes effect",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "kind": {"type": "string", "description": "Record category, e.g. preference, instruction, fact"},
                "key": {"type": "string", "description": "Stable key for updating this record later"},
                "text": {"type": "string", "description": "Text to remember"},
                "scope": {"type": "string", "enum": ["user", "workspace"], "description": "user applies in every workspace and is for how the user wants the agent to work (working habits, communication preferences); workspace (default) is for this project only. For an update, use the scope of the target record as returned by memory_search. A user-scope preference or instruction needs a key"},
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
pub struct ForgetTool {
    service: Arc<Service>,
    scopes: Vec<Scope>,
    session_id: String,
    turn_origin: Option<Arc<crate::schedule::tools::TurnOrigin>>,
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

    pub fn with_turn_context(
        mut self,
        session_id: impl Into<String>,
        origin: Arc<crate::schedule::tools::TurnOrigin>,
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
        "Propose forgetting a remembered record found via memory_search; a human reviews it before it takes effect. If only the record ID is available, first call memory_search with id to read its current scope and revision.",
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
const REVIEW_HINT: &str = "You cannot approve it yourself. The user decides it: in a chat client they send /memory to list pending candidates, then /memory accept <id> or /memory reject <id>; in the terminal they use /memory review. If the user says approve, tell them this.";

fn proposal_result(outcome: crate::memory::Result<Vec<crate::memory::Candidate>>) -> ToolResult {
    let candidates = match outcome {
        Ok(candidates) => candidates,
        Err(error) => return error_result(error),
    };
    match candidates.first() {
        None => text_result("proposal was not queued for review"),
        Some(candidate) => text_result(format!(
            "candidate {} queued for human review (state={}). {REVIEW_HINT}",
            candidate.id,
            candidate.state.as_str()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::turso::testsupport::open_temp;
    use crate::memory::{
        CandidateRef, CandidateState, NAMESPACE_USER, NAMESPACE_WORKSPACE, RememberRequest,
        ReviewDecision, ReviewRequest, decide_default_policy,
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

    fn workspace_scope() -> Scope {
        Scope::new(NAMESPACE_WORKSPACE, "ws-1")
    }

    fn remember_tool(service: &Arc<Service>) -> RememberTool {
        RememberTool::new(Arc::clone(service), scope(), workspace_scope())
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

    #[tokio::test]
    async fn remember_records_the_session_and_exact_presentation_origin() {
        let (_directory, service) = service();
        let origin = Arc::new(crate::schedule::tools::TurnOrigin::default());
        origin.set(Some(
            crate::schedule::Origin::chat("telegram", "42").unwrap(),
        ));
        let tool = RememberTool::new(Arc::clone(&service), scope(), workspace_scope())
            .with_turn_context("session-1", origin);

        let result = run(&tool, r#"{"kind":"fact","text":"blue","reason":"said so"}"#).await;
        assert!(!result.is_error, "{result:?}");
        let candidate = service
            .get_candidate(&CandidateRef {
                scope: workspace_scope(),
                id: candidate_id(&result.content),
            })
            .unwrap();
        assert_eq!(candidate.proposed.source.session_id, "session-1");
        assert_eq!(
            candidate.proposed.metadata[crate::memory::PRESENTATION_ORIGIN_METADATA],
            "chat:telegram:42"
        );
    }

    /// The real store mints the record IDs, so the placeholder is checked by
    /// count and membership instead of by a literal string.
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

    #[tokio::test]
    async fn search_with_no_matches_reports_zero_records() {
        let (_directory, service) = service();
        let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

        let result = run(&tool, r#"{"query":"nothing"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.content, "no matching records");
        assert_eq!(result.persisted_content.as_deref(), Some("0 records"));
    }

    #[tokio::test]
    async fn search_by_id_refreshes_metadata_for_a_reviewed_forget() {
        let (_directory, service) = service();
        let workspace = Scope::new("workspace", "project-1");
        let original = service
            .remember(&RememberRequest {
                scope: workspace.clone(),
                kind: "fact".into(),
                text: "old text".into(),
                ..RememberRequest::default()
            })
            .unwrap();
        let updated = service
            .remember(&RememberRequest {
                id: original.id.clone(),
                scope: workspace.clone(),
                kind: "fact".into(),
                text: "current text".into(),
                expected_revision: Some(original.revision),
                ..RememberRequest::default()
            })
            .unwrap();
        let scopes = vec![scope(), workspace.clone()];
        let search = MemorySearchTool::new(Arc::clone(&service), scopes.clone(), MAX_OUTPUT_BYTES);
        let result = run(&search, &json!({"id": original.id}).to_string()).await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("current text"), "{result:?}");
        assert!(!result.content.contains("old text"), "{result:?}");
        assert_eq!(
            result.persisted_content,
            Some(format!("1 records: {}", updated.id))
        );

        let fields: Vec<_> = result.content.split_whitespace().collect();
        let (namespace, scope_id) = fields
            .iter()
            .find_map(|field| field.strip_prefix("scope="))
            .unwrap()
            .split_once('/')
            .unwrap();
        let revision: u64 = fields
            .iter()
            .find_map(|field| field.strip_prefix("revision="))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(revision, updated.revision);
        let forget = ForgetTool::new(Arc::clone(&service), scopes);
        let proposal = run(
            &forget,
            &json!({
                "id": updated.id,
                "scope_namespace": namespace,
                "scope_id": scope_id,
                "revision": revision,
            })
            .to_string(),
        )
        .await;
        assert!(!proposal.is_error, "{proposal:?}");
        let reference = RecordRef {
            scope: workspace.clone(),
            id: updated.id.clone(),
        };
        assert_eq!(service.get(&reference).unwrap().revision, revision);
        let candidate = CandidateRef {
            scope: workspace,
            id: candidate_id(&proposal.content),
        };
        assert_eq!(
            service.get_candidate(&candidate).unwrap().state,
            CandidateState::Pending
        );
        service
            .review(&ReviewRequest {
                reference: candidate,
                decision: ReviewDecision::Accept,
                edited: None,
                target_revision: None,
            })
            .unwrap();
        assert!(service.get(&reference).unwrap_err().is(ErrorKind::NotFound));
        let after = run(&search, &json!({"id": updated.id}).to_string()).await;
        assert_eq!(after.content, "no matching records");
    }

    #[tokio::test]
    async fn search_by_id_does_not_disclose_unbound_records() {
        let (_directory, service) = service();
        let foreign = remember_record(&service, "editor", "private preference");
        for scopes in [vec![Scope::new("user", "someone-else")], vec![]] {
            let search = MemorySearchTool::new(Arc::clone(&service), scopes, MAX_OUTPUT_BYTES);
            for id in [&foreign, "missing-record"] {
                let result = run(&search, &json!({"id": id}).to_string()).await;
                assert!(!result.is_error, "{result:?}");
                assert_eq!(result.content, "no matching records");
                assert_eq!(result.persisted_content.as_deref(), Some("0 records"));
            }
        }
    }

    #[tokio::test]
    async fn search_by_id_rejects_invalid_or_conflicting_arguments() {
        let (_directory, service) = service();
        let search = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);
        for args in [
            json!({"id": ""}),
            json!({"id": "bad/id"}),
            json!({"id": "rec-1", "query": "editor"}),
        ] {
            let result = run(&search, &args.to_string()).await;
            assert!(result.is_error, "{args}: {result:?}");
        }
    }

    /// A closed service stands in for a reader that returns an error.
    #[tokio::test]
    async fn search_surfaces_a_service_error() {
        let (_directory, service) = service();
        service.close().expect("close");
        let tool = MemorySearchTool::new(service, vec![scope()], MAX_OUTPUT_BYTES);

        for args in [r#"{"query":"x"}"#, r#"{"id":"rec-1"}"#] {
            let result = run(&tool, args).await;
            assert!(result.is_error, "{result:?}");
            assert!(result.content.contains("closed"), "{result:?}");
        }
    }

    #[tokio::test]
    async fn remember_queues_a_create_candidate_for_human_review() {
        let (_directory, service) = service();
        let tool = remember_tool(&service);

        let result = run(&tool, r#"{"kind":"preference","text":"likes dark mode"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert!(
            result.content.contains(
                " queued for human review (state=pending). You cannot approve it yourself."
            ),
            "{result:?}"
        );

        let candidate = service
            .get_candidate(&CandidateRef {
                scope: workspace_scope(),
                id: candidate_id(&result.content),
            })
            .expect("the candidate is stored");
        assert_eq!(candidate.action, CandidateAction::Create);
        assert_eq!(candidate.state, CandidateState::Pending);
        assert_eq!(candidate.proposed.scope, workspace_scope());
        assert_eq!(candidate.proposed.text, "likes dark mode");
        assert_eq!(candidate.proposed.source.origin, Some(Origin::Model));
    }

    #[tokio::test]
    async fn remember_queues_an_update_candidate_when_a_target_id_is_given() {
        let (_directory, service) = service();
        let target = remember_record(&service, "editor", "prefers vim");
        let tool = remember_tool(&service);

        let arguments = format!(
            r#"{{"kind":"preference","scope":"user","text":"prefers neovim","target_id":"{target}","base_revision":1}}"#
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

    #[tokio::test]
    async fn remember_scope_selects_the_user_or_workspace_scope() {
        let (_directory, service) = service();
        let tool = remember_tool(&service);

        let user = run(
            &tool,
            r#"{"kind":"preference","key":"tests","text":"run tests first","scope":"user"}"#,
        )
        .await;
        assert!(!user.is_error, "{user:?}");
        let candidate = service
            .get_candidate(&CandidateRef {
                scope: scope(),
                id: candidate_id(&user.content),
            })
            .expect("the candidate is in the user scope");
        assert_eq!(candidate.proposed.scope, scope());

        let omitted = run(&tool, r#"{"kind":"fact","text":"uses cargo"}"#).await;
        assert!(!omitted.is_error, "{omitted:?}");
        let candidate = service
            .get_candidate(&CandidateRef {
                scope: workspace_scope(),
                id: candidate_id(&omitted.content),
            })
            .expect("the candidate is in the workspace scope");
        assert_eq!(candidate.proposed.scope, workspace_scope());
    }

    #[tokio::test]
    async fn remember_rejects_an_unknown_scope_and_queues_nothing() {
        let (_directory, service) = service();
        let tool = remember_tool(&service);

        let result = run(&tool, r#"{"kind":"fact","text":"x","scope":"bogus"}"#).await;
        assert!(result.is_error, "{result:?}");
        assert!(result.content.contains("bogus"), "{result:?}");
        let listed = service
            .search(&SearchRequest {
                scopes: vec![scope(), workspace_scope()],
                include_candidates: true,
                limit: 10,
                token_budget: 1000,
                now: Utc::now(),
                ..SearchRequest::default()
            })
            .expect("search");
        assert!(listed.candidates.is_empty(), "{listed:?}");
    }

    #[tokio::test]
    async fn remember_surfaces_the_key_rule_for_a_user_preference() {
        let (_directory, service) = service();
        let tool = remember_tool(&service);

        let result = run(
            &tool,
            r#"{"kind":"preference","text":"run tests first","scope":"user"}"#,
        )
        .await;
        assert!(result.is_error, "{result:?}");
        assert!(result.content.contains("needs a key"), "{result:?}");
    }

    #[tokio::test]
    async fn remember_requires_a_kind_and_a_text() {
        let (_directory, service) = service();
        let tool = remember_tool(&service);

        let result = run(&tool, r#"{"kind":"preference"}"#).await;
        assert!(result.is_error, "{result:?}");
        assert!(result.content.contains("text"), "{result:?}");
    }

    #[tokio::test]
    async fn forget_queues_a_forget_candidate_for_human_review() {
        let (_directory, service) = service();
        let target = remember_record(&service, "editor", "prefers vim");
        let origin = Arc::new(crate::schedule::tools::TurnOrigin::default());
        origin.set(Some(
            crate::schedule::Origin::chat("feishu", "oc_42").unwrap(),
        ));
        let tool = ForgetTool::new(Arc::clone(&service), vec![scope()])
            .with_turn_context("session-1", origin);

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
        assert_eq!(candidate.proposed.source.session_id, "session-1");
        assert_eq!(
            candidate.proposed.metadata[crate::memory::PRESENTATION_ORIGIN_METADATA],
            "chat:feishu:oc_42"
        );
    }

    #[tokio::test]
    async fn forget_requires_the_identifying_fields() {
        let (_directory, service) = service();
        let tool = ForgetTool::new(service, vec![scope()]);

        let result = run(&tool, r#"{"id":"rec-1"}"#).await;
        assert!(result.is_error, "{result:?}");
    }

    /// The rejection must happen before the service is asked, so an unbound
    /// scope leaves no candidate behind.
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

    use crate::memory::advice::{self, Advisor, Answer, Backend, Exchange, Question};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Answers every question the same way and counts its calls.
    struct Counting(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Backend for Counting {
        fn model(&self) -> String {
            "gpt-test".into()
        }

        fn auxiliary(&self) -> bool {
            false
        }

        async fn ask(&self, _state: &str, _questions: &[Question], _limit: Duration) -> Exchange {
            self.0.fetch_add(1, Ordering::SeqCst);
            let answer = |choice: &str| Answer {
                choice: choice.to_owned(),
                probability: None,
            };
            Exchange {
                request: "request".into(),
                response: None,
                result: Ok(HashMap::from([
                    ("scope".to_owned(), answer("all_workspaces")),
                    ("durable".to_owned(), answer("yes")),
                ])),
            }
        }
    }

    fn advised_tool(service: &Arc<Service>) -> (RememberTool, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let advisor = Advisor::new(Box::new(Counting(Arc::clone(&calls))), None);
        (
            remember_tool(service).with_advisor(Some(Arc::new(advisor))),
            calls,
        )
    }

    #[tokio::test]
    async fn remember_with_an_advisor_queues_the_candidate_with_an_advice_line() {
        let (_directory, service) = service();
        let (tool, calls) = advised_tool(&service);

        let result = run(&tool, r#"{"kind":"preference","text":"likes dark mode"}"#).await;
        assert!(!result.is_error, "{result:?}");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let candidate = service
            .get_candidate(&CandidateRef {
                scope: workspace_scope(),
                id: candidate_id(&result.content),
            })
            .expect("the candidate is stored");
        assert_eq!(candidate.state, CandidateState::Pending);
        assert_eq!(candidate.proposed.scope, workspace_scope());
        assert_eq!(
            advice::advice_line(&candidate.proposed).as_deref(),
            Some(
                "Advice (session model gpt-test): all workspaces (proposed: workspace) · new · durable"
            )
        );
    }

    #[tokio::test]
    async fn remember_with_an_advisor_asks_no_model_for_a_proposal_the_service_refuses() {
        let (_directory, service) = service();
        let (tool, calls) = advised_tool(&service);

        let result = run(&tool, r#"{"kind":"preference","text":""}"#).await;

        assert!(result.is_error, "{result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn remember_with_an_advisor_returns_context_canceled_when_canceled_during_advice() {
        let (_directory, service) = service();
        let (tool, _) = advised_tool(&service);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = tool
            .execute(
                &RawValue::from_string(r#"{"kind":"preference","text":"likes dark mode"}"#.into())
                    .expect("raw"),
                &cancel,
            )
            .await;

        assert!(result.is_error);
        assert_eq!(result.content, CONTEXT_CANCELED);
    }
}
