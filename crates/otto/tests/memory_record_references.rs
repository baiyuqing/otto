//! Memory record references in session files: the ids and revisions of the
//! records a turn rendered are persisted without their text (issue #591).
//! Every fixture record carries `RECORD_TEXT_MARKER`; the assertions search
//! the session file bytes for it.

use std::path::Path;
use std::sync::Arc;

use otto::fixtures::scripted_provider::{Script, serve, text_reply};
use otto::provider::openaicompat;
use otto::runtime::builder::{SessionHandle, SharedSession};
use otto_core::agent::memory::{
    MemoryError, MemoryRecall, RecallRequest, RecallResult, Record, Scope,
};
use otto_core::agent::{Agent, Options};
use otto_core::config::endpoint::TrustedEndpoint;
use otto_core::model::{Block, FinishReason, Message, Role, ToolDefinition};
use otto_core::operation::OperationControl;
use otto_core::session::{
    CompactionCheckpoint, CompactionDetails, Header, MemoryRecordRef, MemoryReferenceCoverage,
    Session, encode_memory_recall,
};
use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};
use otto_session::Store;
use tokio_util::sync::CancellationToken;

const RECORD_TEXT_MARKER: &str = "MARKER-7f3a-record-text";
const RECORD_KIND_MARKER: &str = "kind-marker-5c1d";
const RECORD_KEY_MARKER: &str = "key-marker-9b42";

fn header(workspace: &Path) -> Header {
    Header {
        id: "memory-record-references".into(),
        workspace: workspace.to_string_lossy().into_owned(),
        provider: "openai-compatible".into(),
        model: "test-model".into(),
        created_at: chrono::Utc::now(),
        ..Header::default()
    }
}

fn message(id: &str, role: Role, text: &str) -> Message {
    let finish_reason = (role == Role::Assistant).then_some(FinishReason::Stop);
    Message {
        id: id.into(),
        role,
        blocks: vec![Block::text(text)],
        created_at: chrono::Utc::now(),
        finish_reason,
        ..Message::default()
    }
}

fn reference(id: &str, revision: Option<&str>) -> MemoryRecordRef {
    MemoryRecordRef {
        id: id.into(),
        revision: revision.map(str::to_owned),
    }
}

fn session_file_text(path: &str) -> String {
    String::from_utf8_lossy(&std::fs::read(path).expect("read session file")).into_owned()
}

#[tokio::test]
async fn store_persists_recall_entries_and_checkpoint_references_without_text_across_a_reopen() {
    let root = tempfile::tempdir().expect("root");
    let workspace = tempfile::tempdir().expect("workspace");
    let store = Store::create(root.path(), header(workspace.path())).expect("create");
    let first_turn = [reference("rec-a", Some("1")), reference("rec-b", None)];
    let second_turn = [reference("rec-c", None)];

    store
        .append_message(&message("user-1", Role::User, "first question"))
        .expect("user 1");
    store
        .append_custom_entry("otto.memory_recall", &encode_memory_recall(&first_turn))
        .expect("recall 1");
    store
        .append_message(&message("assistant-1", Role::Assistant, "first answer"))
        .expect("assistant 1");
    store
        .append_message(&message("user-2", Role::User, "second question"))
        .expect("user 2");
    store
        .append_custom_entry("otto.memory_recall", &encode_memory_recall(&second_turn))
        .expect("recall 2");
    store
        .append_message(&message("assistant-2", Role::Assistant, "second answer"))
        .expect("assistant 2");
    let persisted_ids: Vec<String> = store
        .messages()
        .iter()
        .map(|persisted| persisted.id.clone())
        .collect();
    let (first_user_id, second_user_id) = (persisted_ids[0].clone(), persisted_ids[2].clone());
    let checkpoint_records = vec![
        reference("rec-a", Some("1")),
        reference("rec-b", None),
        reference("rec-c", None),
    ];
    store
        .append_compaction(&CompactionCheckpoint {
            summary: "summary of the first turn".into(),
            first_kept_entry_id: second_user_id.clone(),
            tokens_before: 100,
            details: CompactionDetails {
                memory_records: checkpoint_records.clone(),
                memory_coverage: MemoryReferenceCoverage::Complete,
                ..CompactionDetails::default()
            },
            created_at: chrono::Utc::now(),
            ..CompactionCheckpoint::default()
        })
        .expect("compaction");
    let path = store.path();
    store.close().expect("close");
    drop(store);

    let reopened = Store::open(&path).expect("reopen").0;
    let shared = SharedSession::new(Arc::new(reopened));
    let references = Session::memory_references(&shared);
    let anchored: Vec<(&str, &[MemoryRecordRef])> = references
        .iter()
        .map(|turn| (turn.user_message_id.as_str(), turn.records.as_slice()))
        .collect();
    assert_eq!(
        anchored,
        [
            (first_user_id.as_str(), &first_turn[..]),
            (second_user_id.as_str(), &second_turn[..])
        ]
    );
    let details = Session::latest_compaction(&shared)
        .expect("checkpoint in force")
        .details;
    assert_eq!(details.memory_records, checkpoint_records);
    assert_eq!(details.memory_coverage, MemoryReferenceCoverage::Complete);

    let file_text = session_file_text(&path);
    for expected in [
        "\"otto.memory_recall\"",
        "\"memoryRecords\"",
        "\"memoryCoverage\":\"complete\"",
    ] {
        assert!(
            file_text.contains(expected),
            "session file lacks {expected}"
        );
    }
    for id in ["rec-a", "rec-b", "rec-c"] {
        assert!(file_text.contains(id), "session file lacks {id}");
    }
    for forbidden in [RECORD_TEXT_MARKER, RECORD_KIND_MARKER, RECORD_KEY_MARKER] {
        assert!(
            !file_text.contains(forbidden),
            "session file holds {forbidden}"
        );
    }
}

struct FixedRecall;

#[async_trait::async_trait]
impl MemoryRecall for FixedRecall {
    async fn recall(
        &self,
        _: &RecallRequest,
        _: &CancellationToken,
    ) -> Result<RecallResult, MemoryError> {
        Ok(RecallResult {
            records: vec![Record {
                id: "rec-1".into(),
                revision: Some("3".into()),
                scope: Scope {
                    namespace: "user".into(),
                    id: "user-1".into(),
                },
                kind: RECORD_KIND_MARKER.into(),
                key: RECORD_KEY_MARKER.into(),
                text: RECORD_TEXT_MARKER.into(),
                standing: false,
            }],
            used_tokens: 10,
        })
    }
}

struct NoTools;

#[async_trait::async_trait]
impl ToolExecutor for NoTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    async fn execute(&self, call: ToolCall<'_>, _: &dyn OperationControl) -> ToolExecution {
        ToolExecution::completed(ToolResult::unknown_tool(call.name))
    }
}

#[tokio::test]
async fn agent_turn_persists_recalled_record_id_and_revision_but_sends_the_text_to_the_provider() {
    let (base_url, requests) = serve(Script {
        replies: vec![text_reply(
            "an answer that is long enough to be stored as the assistant reply",
        )],
        served: Arc::default(),
    });
    let origin = vec![base_url.clone()];
    let endpoint = TrustedEndpoint::authorize(&base_url, "openai-compatible", &origin, &origin)
        .expect("trust the loopback origin");
    let root = tempfile::tempdir().expect("root");
    let workspace = tempfile::tempdir().expect("workspace");
    let store = Arc::new(Store::create(root.path(), header(workspace.path())).expect("create"));
    let path = store.path();
    let agent = Agent::new(
        openaicompat::Client::new_trusted(&endpoint, "key"),
        NoTools,
        SharedSession::new(Arc::clone(&store) as Arc<dyn SessionHandle>),
        Options {
            model: "test-model".into(),
            now: Box::new(chrono::Utc::now),
            memory: Some(Arc::new(FixedRecall)),
            memory_recall_limit: 12,
            memory_recall_token_budget: 2000,
            ..Default::default()
        },
    );
    agent
        .run("a question", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("turn");

    let provider_requests = requests.lock().unwrap().clone();
    assert!(
        provider_requests
            .iter()
            .any(|body| body.contains(RECORD_TEXT_MARKER)),
        "the provider request must carry the recalled text"
    );
    let file_text = session_file_text(&path);
    for expected in ["\"otto.memory_recall\"", "\"rec-1\"", "\"revision\":\"3\""] {
        assert!(
            file_text.contains(expected),
            "session file lacks {expected}"
        );
    }
    for forbidden in [RECORD_TEXT_MARKER, RECORD_KIND_MARKER, RECORD_KEY_MARKER] {
        assert!(
            !file_text.contains(forbidden),
            "session file holds {forbidden}"
        );
    }
    let references = Session::memory_references(store.as_ref());
    assert_eq!(references.len(), 1);
    assert_eq!(references[0].records, [reference("rec-1", Some("3"))]);
}
