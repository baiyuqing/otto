//! Copy inventory of a forgotten record (issue #591 section 11, row 2).
//!
//! One record text exists in four places: the record file, a history file and
//! a pending update candidate in the store, an advice audit row, and (as the
//! recalled text only in the provider request, never in the session) a session
//! file. After forget, the cleanup report must account for every remaining
//! copy: this test scans the bytes of each place for the marker and compares
//! the hits with the report.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto::fixtures::scripted_provider::{Script, serve, text_reply};
use otto::memory::advice::audit::{AuditStore, NewRow};
use otto::memory::advice::sqlite;
use otto::memory::{
    BindOptions, CandidateAction, CandidateListRequest, CandidateState, CopyStatus,
    ForgetCleanupEntry, ForgetRequest, Origin, ProposeRequest, Provenance, RememberRequest,
    Service,
};
use otto::provider::openaicompat;
use otto::runtime::builder::{SessionHandle, SharedSession};
use otto_core::agent::{Agent, Options};
use otto_core::config::endpoint::TrustedEndpoint;
use otto_core::config::memory::MemoryRuntime;
use otto_core::session::Header;
use otto_session::Store;
use tokio_util::sync::CancellationToken;

const TEXT_MARKER: &str = "MARKER-inventory-4e9c1b";
const RECORD_KEY: &str = "inventorykey";

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root).expect("readable directory") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// The files under `root` whose bytes contain the marker.
fn files_holding_marker(root: &Path) -> Vec<PathBuf> {
    files_under(root)
        .into_iter()
        .filter(|path| {
            let bytes = std::fs::read(path).expect("read file");
            bytes
                .windows(TEXT_MARKER.len())
                .any(|window| window == TEXT_MARKER.as_bytes())
        })
        .collect()
}

fn entry<'a>(cleanup: &'a [ForgetCleanupEntry], copy: &str) -> &'a ForgetCleanupEntry {
    cleanup
        .iter()
        .find(|entry| entry.copy == copy)
        .unwrap_or_else(|| panic!("no cleanup entry for {copy}: {cleanup:?}"))
}

#[tokio::test]
async fn forget_reports_every_copy_of_the_text_that_remains_on_disk() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let store_root = home.path().join(".otto").join("memory");
    let runtime = MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: store_root.to_string_lossy().into_owned(),
        ..MemoryRuntime::default()
    };
    let (service, user_scope, usable): (Arc<Service>, _, bool) =
        otto::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
            .expect("open memory service");
    assert!(usable, "the store must be usable");

    // A record whose current revision carries the marker, updated once so a
    // history file with the first revision exists.
    let remember = |text: &str| RememberRequest {
        scope: user_scope.clone(),
        kind: "preference".into(),
        key: RECORD_KEY.into(),
        text: text.into(),
        ..RememberRequest::default()
    };
    let first = service
        .remember(&remember(&format!("first {TEXT_MARKER}")))
        .expect("remember");
    let current = service
        .remember(&remember(&format!("second {TEXT_MARKER}")))
        .expect("update");
    assert_eq!(
        (current.id.as_str(), current.revision),
        (first.id.as_str(), 2)
    );
    assert!(
        store_root.join("history").join(&first.id).is_dir(),
        "an update must leave a history directory"
    );

    // A pending update candidate that targets the record and carries the text.
    let proposed = service
        .propose(&ProposeRequest {
            action: CandidateAction::Update,
            scope: user_scope.clone(),
            kind: "preference".into(),
            key: RECORD_KEY.into(),
            text: format!("third {TEXT_MARKER}"),
            target_id: first.id.clone(),
            base_revision: current.revision,
            reason: "said so".into(),
            confidence: 0.5,
            source: Provenance {
                origin: Some(Origin::Model),
                ..Provenance::default()
            },
            ..ProposeRequest::default()
        })
        .expect("propose");
    let candidate_id = proposed[0].id.clone();
    assert_eq!(proposed[0].state, CandidateState::Pending, "{proposed:?}");

    // An advice audit row keyed by that candidate whose request holds the text.
    let advice_path = home.path().join(".otto").join("memory-advice.db");
    {
        let advice = sqlite::Store::open(&advice_path).expect("open advice log");
        AuditStore::append(
            &advice,
            &NewRow {
                model: "advice-model".into(),
                auxiliary: true,
                scope: user_scope.namespace.clone(),
                request: format!("candidate text: {TEXT_MARKER}"),
                response: None,
                outcome: "ok",
                error: None,
                latency_ms: 1,
            },
            Some(&candidate_id),
        )
        .expect("append advice row");
    }

    // One agent turn that recalls the record; the provider sees the text, the
    // session does not store it.
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
                id: "forget-inventory".into(),
                workspace: workspace.path().to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: chrono::Utc::now(),
                ..Header::default()
            },
        )
        .expect("create session"),
    );
    let session_path = session.path();
    let binding = service
        .bind(BindOptions {
            scopes: vec![user_scope.clone()],
            default_write_scope: user_scope.clone(),
            ..BindOptions::default()
        })
        .expect("bind");
    let agent = Agent::new(
        openaicompat::Client::new_trusted(&endpoint, "key"),
        NoTools,
        SharedSession::new(Arc::clone(&session) as Arc<dyn SessionHandle>),
        Options {
            model: "test-model".into(),
            now: Box::new(chrono::Utc::now),
            memory: Some(Arc::new(binding)),
            memory_recall_limit: 12,
            memory_recall_token_budget: 2000,
            ..Default::default()
        },
    );
    agent
        .run(RECORD_KEY, &mut |_| {}, &CancellationToken::new())
        .await
        .expect("turn");
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|body| body.contains(TEXT_MARKER)),
        "the recall must have reached the provider, or the session proves nothing"
    );
    session.close().expect("close session");
    assert!(
        !String::from_utf8_lossy(&std::fs::read(&session_path).expect("session file"))
            .contains(TEXT_MARKER),
        "the session must not hold the recalled text"
    );

    // Before forget the store holds the marker in the record, history and candidate.
    let before: Vec<PathBuf> = files_holding_marker(&store_root);
    assert!(
        before.len() >= 3,
        "store files holding the marker: {before:?}"
    );

    let result = service
        .forget(&ForgetRequest {
            reference: otto::memory::RecordRef {
                scope: user_scope.clone(),
                id: first.id.clone(),
            },
            expected_revision: current.revision,
            purge_backups: false,
            confirm_purge: false,
        })
        .expect("forget");

    // Reconcile: every place that still holds the marker is reported as a copy
    // that is not removed.
    let store_hits = files_holding_marker(&store_root);
    let session_hits = files_holding_marker(sessions.path());
    let advice_hits = files_holding_marker(advice_path.parent().expect("advice directory"))
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("memory-advice.db"))
        })
        .collect::<Vec<_>>();
    assert!(
        store_hits.is_empty(),
        "store files still hold the text: {store_hits:?}"
    );
    assert!(
        session_hits.is_empty(),
        "session files hold the text: {session_hits:?}"
    );
    assert!(
        !advice_hits.is_empty(),
        "the advice database must still hold the row"
    );

    let cleanup = &result.cleanup;
    for copy in [
        format!("records/{}.md", first.id),
        format!("history/{}/", first.id),
        format!("candidates/{candidate_id}.json"),
    ] {
        assert_eq!(
            entry(cleanup, &copy).status,
            CopyStatus::Removed,
            "{copy}: {cleanup:?}"
        );
    }
    let advice = entry(
        cleanup,
        "memory advice audit log (~/.otto/memory-advice.db)",
    );
    assert_eq!(advice.status, CopyStatus::Unverified, "{cleanup:?}");
    assert_eq!(
        entry(cleanup, "session files").status,
        CopyStatus::Present,
        "{cleanup:?}"
    );
    // Each remaining hit location maps to a non-removed entry naming it.
    assert!(
        cleanup
            .iter()
            .filter(|entry| entry.status != CopyStatus::Removed)
            .any(|entry| entry.copy.contains("memory-advice.db")),
        "the advice database hit has no report entry: {cleanup:?}"
    );

    // The candidate stays as a rejected receipt without the text.
    let candidates = service
        .list_candidates(&CandidateListRequest {
            scopes: vec![user_scope],
            states: Vec::new(),
            limit: 10,
            cursor: String::new(),
        })
        .expect("list candidates");
    let receipt = candidates
        .candidates
        .iter()
        .find(|candidate| candidate.id == candidate_id)
        .expect("the receipt is archived");
    assert_eq!(receipt.state, CandidateState::Rejected);
    assert!(!receipt.proposed.text.contains(TEXT_MARKER));
    service.close().expect("close");
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
