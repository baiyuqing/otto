//! Memory record lifecycle states and time fields through the service and the
//! agent loop (issue #591 step 3).
//!
//! Every turn runs a fresh agent and session against a scripted provider, so
//! the provider request holds only the text recalled for that turn. The
//! assertions look for a per-record marker in that request.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Duration, Utc};
use otto::fixtures::scripted_provider::{Script, serve, text_reply};
use otto::memory::{
    BindOptions, ListRequest, Record, RecordRef, RecordState, RememberRequest, Scope, Service,
    SetStateRequest, VerifyRequest,
};
use otto::provider::openaicompat;
use otto::runtime::builder::{SessionHandle, SharedSession};
use otto::session::Store;
use otto_core::agent::{Agent, Options};
use otto_core::config::endpoint::TrustedEndpoint;
use otto_core::config::memory::MemoryRuntime;
use otto_core::session::Header;
use tokio_util::sync::CancellationToken;

/// A word every fixture record contains, so the turn query recalls all of them.
const COMMON_QUERY_WORD: &str = "lifecyclefact";
const MARKER_A: &str = "MARKER-state-a-31c7";
const MARKER_B: &str = "MARKER-state-b-82e0";
const MARKER_UNTOUCHED: &str = "MARKER-state-untouched-5d19";

struct Fixture {
    _home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    store_root: PathBuf,
    service: Arc<Service>,
    user_scope: Scope,
}

fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let store_root = home.path().join(".otto").join("memory");
    let runtime = MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: store_root.to_string_lossy().into_owned(),
        ..MemoryRuntime::default()
    };
    let (service, user_scope, usable) =
        otto::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
            .expect("open memory service");
    assert!(usable, "the store must be usable");
    Fixture {
        _home: home,
        workspace,
        store_root,
        service,
        user_scope,
    }
}

impl Fixture {
    fn remember(
        &self,
        key: &str,
        marker: &str,
        review_at: Option<chrono::DateTime<Utc>>,
    ) -> Record {
        self.service
            .remember(&RememberRequest {
                scope: self.user_scope.clone(),
                kind: "note".into(),
                key: key.into(),
                text: format!("{marker} {COMMON_QUERY_WORD} {key}"),
                review_at,
                ..RememberRequest::default()
            })
            .expect("remember")
    }

    fn reference(&self, record: &Record) -> RecordRef {
        RecordRef {
            scope: self.user_scope.clone(),
            id: record.id.clone(),
        }
    }

    fn set_state(&self, record: &Record, state: RecordState, superseded_by: &str) -> Record {
        let current = self.service.get(&self.reference(record)).expect("get");
        self.service
            .set_state(&SetStateRequest {
                reference: self.reference(record),
                expected_revision: current.revision,
                state,
                superseded_by: superseded_by.into(),
                reason: String::new(),
            })
            .expect("set_state")
    }

    fn verify(&self, record: &Record) -> Record {
        let current = self.service.get(&self.reference(record)).expect("get");
        self.service
            .verify(&VerifyRequest {
                reference: self.reference(record),
                expected_revision: current.revision,
                review_at: None,
            })
            .expect("verify")
    }

    /// Runs one agent turn with a memory binding and returns the provider
    /// request bodies of that turn joined into one string.
    async fn provider_request(&self) -> String {
        let (base_url, requests) = serve(Script {
            replies: vec![text_reply(
                "an answer that is long enough to be stored as the assistant reply",
            )],
            served: Arc::default(),
        });
        let origin = vec![base_url.clone()];
        let endpoint = TrustedEndpoint::authorize(&base_url, "openai-compatible", &origin, &origin)
            .expect("trust the loopback origin");
        let sessions = tempfile::tempdir().expect("sessions");
        let session = Arc::new(
            Store::create(
                sessions.path(),
                Header {
                    id: "record-state".into(),
                    workspace: self.workspace.path().to_string_lossy().into_owned(),
                    provider: "openai-compatible".into(),
                    model: "test-model".into(),
                    created_at: Utc::now(),
                    ..Header::default()
                },
            )
            .expect("create session"),
        );
        let binding = self
            .service
            .bind(BindOptions {
                scopes: vec![self.user_scope.clone()],
                default_write_scope: self.user_scope.clone(),
                ..BindOptions::default()
            })
            .expect("bind");
        let agent = Agent::new(
            openaicompat::Client::new_trusted(&endpoint, "key"),
            NoTools,
            SharedSession::new(Arc::clone(&session) as Arc<dyn SessionHandle>),
            Options {
                model: "test-model".into(),
                now: Box::new(Utc::now),
                memory: Some(Arc::new(binding)),
                memory_recall_limit: 12,
                memory_recall_token_budget: 2000,
                ..Default::default()
            },
        );
        agent
            .run(COMMON_QUERY_WORD, &mut |_| {}, &CancellationToken::new())
            .await
            .expect("turn");
        requests.lock().unwrap().join("\n")
    }
}

fn files_holding(root: &Path, marker: &str) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root).expect("readable directory") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            found.extend(files_holding(&path, marker));
        } else {
            let bytes = std::fs::read(&path).expect("read file");
            if bytes
                .windows(marker.len())
                .any(|window| window == marker.as_bytes())
            {
                found.push((path, bytes));
            }
        }
    }
    found.sort();
    found
}

struct NoTools;

#[async_trait::async_trait]
impl otto_core::tool::ToolExecutor for NoTools {
    fn definitions(&self) -> Vec<otto_core::model::ToolDefinition> {
        Vec::new()
    }

    async fn execute(
        &self,
        call: otto_core::tool::ToolCall<'_>,
        _: &dyn otto_core::operation::OperationControl,
    ) -> otto_core::tool::ToolExecution {
        otto_core::tool::ToolExecution::completed(otto_core::tool::ToolResult::unknown_tool(
            call.name,
        ))
    }
}

#[tokio::test]
async fn needs_review_and_retracted_records_are_not_recalled_and_active_records_are() {
    let fixture = fixture();
    let record = fixture.remember("alpha", MARKER_A, None);
    assert!(
        fixture.provider_request().await.contains(MARKER_A),
        "an active record must be recalled, or the other turns prove nothing"
    );

    fixture.set_state(&record, RecordState::NeedsReview, "");
    assert!(
        !fixture.provider_request().await.contains(MARKER_A),
        "a needs_review record must not be injected"
    );

    fixture.set_state(&record, RecordState::Active, "");
    assert!(
        fixture.provider_request().await.contains(MARKER_A),
        "returning to active must recall the record again"
    );

    fixture.set_state(&record, RecordState::Retracted, "");
    assert!(
        !fixture.provider_request().await.contains(MARKER_A),
        "a retracted record must not be injected"
    );
}

#[tokio::test]
async fn a_passed_review_deadline_removes_the_record_from_recall_until_it_is_verified() {
    let fixture = fixture();
    let record = fixture.remember("beta", MARKER_A, Some(Utc::now() - Duration::hours(1)));
    assert!(
        !fixture.provider_request().await.contains(MARKER_A),
        "a record whose review_at has passed must not be injected"
    );

    let due = fixture
        .service
        .list(&ListRequest {
            all_scopes: false,
            scopes: vec![fixture.user_scope.clone()],
            kinds: Vec::new(),
            labels: Vec::new(),
            limit: 20,
            cursor: String::new(),
            now: Utc::now(),
            include_expired: false,
            states: Vec::new(),
            updated_after: None,
            verified_after: None,
            unverified_only: false,
            review_due_only: true,
        })
        .expect("list review-due records");
    let due_ids: Vec<&str> = due.records.iter().map(|due| due.id.as_str()).collect();
    assert_eq!(due_ids, [record.id.as_str()]);

    let verified = fixture.verify(&record);
    assert!(
        verified.verified_at.is_some(),
        "verify must set verified_at"
    );
    assert_eq!(
        verified.review_at, None,
        "verify without a deadline clears review_at"
    );
    assert!(
        fixture.provider_request().await.contains(MARKER_A),
        "a verified record must be recalled again"
    );
}

#[tokio::test]
async fn a_superseded_record_is_replaced_by_its_replacement_in_recall() {
    let fixture = fixture();
    let old = fixture.remember("old", MARKER_A, None);
    let new = fixture.remember("new", MARKER_B, None);
    let request = fixture.provider_request().await;
    assert!(
        request.contains(MARKER_A) && request.contains(MARKER_B),
        "both active records must be recalled before the change"
    );

    let superseded = fixture.set_state(&old, RecordState::Superseded, &new.id);
    assert_eq!(superseded.state, RecordState::Superseded);
    assert_eq!(superseded.superseded_by, new.id);
    let request = fixture.provider_request().await;
    assert!(
        request.contains(MARKER_B),
        "the replacement must be recalled"
    );
    assert!(
        !request.contains(MARKER_A),
        "the superseded record must not be injected"
    );
}

#[tokio::test]
async fn recall_and_listing_never_write_the_record_file_or_verified_at() {
    let fixture = fixture();
    let untouched = fixture.remember("untouched", MARKER_UNTOUCHED, None);
    let before = files_holding(&fixture.store_root, MARKER_UNTOUCHED);
    assert!(!before.is_empty(), "the record file must hold its marker");

    for _ in 0..2 {
        assert!(
            fixture.provider_request().await.contains(MARKER_UNTOUCHED),
            "the record must be recalled in each turn"
        );
    }
    fixture
        .service
        .get(&fixture.reference(&untouched))
        .expect("get");

    assert_eq!(
        files_holding(&fixture.store_root, MARKER_UNTOUCHED),
        before,
        "recall, get and listing must not change the record file bytes"
    );
    let current = fixture
        .service
        .get(&fixture.reference(&untouched))
        .expect("get");
    assert_eq!(current.verified_at, None);
    assert_eq!(current.revision, untouched.revision);
}
