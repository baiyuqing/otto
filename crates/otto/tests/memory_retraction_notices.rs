//! Retraction notices through the native memory binding and the agent loop
//! (issue #591 step 4). One session runs four turns against a scripted
//! provider; between turns the records change state, and the session is
//! reopened from its file. Each record carries a marker, and the assertions
//! look for the marker and for the notice lines in the provider request of the
//! turn.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use otto::fixtures::scripted_provider::{Script, serve, text_reply};
use otto::memory::{
    BindOptions, ForgetRequest, Record, RecordRef, RecordState, RememberRequest, Scope, Service,
    SetStateRequest,
};
use otto::provider::openaicompat;
use otto::runtime::builder::{SessionHandle, SharedSession};
use otto_core::agent::{Agent, Options};
use otto_core::config::endpoint::TrustedEndpoint;
use otto_core::config::memory::MemoryRuntime;
use otto_core::model::ToolDefinition;
use otto_core::operation::OperationControl;
use otto_core::session::Header;
use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};
use otto_session::Store;
use tokio_util::sync::CancellationToken;

const MARKER_A: &str = "MARKER-notice-a-4e21";
const MARKER_B: &str = "MARKER-notice-b-97c0";

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

struct Fixture {
    _home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    service: Arc<Service>,
    user_scope: Scope,
    provider_requests: Arc<Mutex<Vec<String>>>,
    endpoint: TrustedEndpoint,
}

fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let runtime = MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: home
            .path()
            .join(".otto")
            .join("memory")
            .to_string_lossy()
            .into_owned(),
        ..MemoryRuntime::default()
    };
    let (service, user_scope, usable) =
        otto::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
            .expect("open memory service");
    assert!(usable, "the store must be usable");
    let (base_url, provider_requests) = serve(Script {
        replies: (0..8)
            .map(|_| {
                text_reply("an answer that is long enough to be stored as the assistant reply")
            })
            .collect(),
        served: Arc::default(),
    });
    let origin = vec![base_url.clone()];
    let endpoint = TrustedEndpoint::authorize(&base_url, "openai-compatible", &origin, &origin)
        .expect("trust the loopback origin");
    Fixture {
        _home: home,
        workspace,
        service,
        user_scope,
        provider_requests,
        endpoint,
    }
}

impl Fixture {
    fn remember(&self, key: &str, marker: &str) -> Record {
        self.service
            .remember(&RememberRequest {
                scope: self.user_scope.clone(),
                kind: "note".into(),
                key: key.into(),
                text: format!("{marker} noticefact {key}"),
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

    fn supersede(&self, record: &Record, replacement: &Record) {
        let current = self.service.get(&self.reference(record)).expect("get");
        self.service
            .set_state(&SetStateRequest {
                reference: self.reference(record),
                expected_revision: current.revision,
                state: RecordState::Superseded,
                superseded_by: replacement.id.clone(),
                reason: String::new(),
            })
            .expect("set_state");
    }

    fn forget(&self, record: &Record) {
        let current = self.service.get(&self.reference(record)).expect("get");
        self.service
            .forget(&ForgetRequest {
                reference: self.reference(record),
                expected_revision: current.revision,
                purge_backups: false,
                confirm_purge: false,
            })
            .expect("forget");
    }

    fn create_session(&self, root: &std::path::Path) -> Arc<Store> {
        Arc::new(
            Store::create(
                root,
                Header {
                    id: "retraction-notices".into(),
                    workspace: self.workspace.path().to_string_lossy().into_owned(),
                    provider: "openai-compatible".into(),
                    model: "test-model".into(),
                    created_at: Utc::now(),
                    ..Header::default()
                },
            )
            .expect("create session"),
        )
    }

    /// Runs one turn of a new agent over `session` with a new native binding
    /// and returns the text of the provider request of that turn.
    async fn turn(&self, session: &Arc<Store>, user_text: &str) -> String {
        let before = self.provider_requests.lock().unwrap().len();
        let binding = self
            .service
            .bind(BindOptions {
                scopes: vec![self.user_scope.clone()],
                default_write_scope: self.user_scope.clone(),
                ..BindOptions::default()
            })
            .expect("bind");
        let agent = Agent::new(
            openaicompat::Client::new_trusted(&self.endpoint, "key"),
            NoTools,
            SharedSession::new(Arc::clone(session) as Arc<dyn SessionHandle>),
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
            .run(user_text, &mut |_| {}, &CancellationToken::new())
            .await
            .expect("turn");
        let requests = self.provider_requests.lock().unwrap().clone();
        assert_eq!(requests.len(), before + 1, "one provider request per turn");
        message_text(&requests[before])
    }
}

/// Every string in the `messages` of a provider request body, one per line,
/// so a JSON-escaped quote in a notice reads as a quote.
fn message_text(body: &str) -> String {
    fn collect(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => out.push(text.clone()),
            serde_json::Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
            serde_json::Value::Object(fields) => {
                fields.values().for_each(|item| collect(item, out))
            }
            _ => {}
        }
    }
    let request: serde_json::Value = serde_json::from_str(body).expect("provider request JSON");
    let mut strings = Vec::new();
    collect(&request["messages"], &mut strings);
    strings.join("\n")
}

fn notice_line_for(text: &str, id: &str) -> Option<String> {
    let needle = format!("<memory_retraction id=\"{id}\"");
    text.lines()
        .find(|line| line.contains(&needle))
        .map(str::to_owned)
}

fn reopen(session: Arc<Store>) -> (Arc<Store>, PathBuf) {
    let path = PathBuf::from(session.path());
    session.close().expect("close");
    drop(session);
    (Arc::new(Store::open(&path).expect("reopen").0), path)
}

#[tokio::test]
async fn a_superseded_and_a_forgotten_record_get_notices_across_turns_and_a_reopen() {
    let fixture = fixture();
    let sessions = tempfile::tempdir().expect("sessions");
    let session = fixture.create_session(sessions.path());
    let record_a = fixture.remember("a", MARKER_A);

    let turn_one = fixture.turn(&session, "first question").await;
    assert!(turn_one.contains(MARKER_A), "turn 1 must recall A");
    assert!(
        !turn_one.contains("memory_retraction"),
        "turn 1 has nothing to retract"
    );

    let record_b = fixture.remember("b", MARKER_B);
    fixture.supersede(&record_a, &record_b);
    let notice_a = format!(
        "<memory_retraction id=\"{}\" revision=\"1\" current_revision=\"2\" reason=\"superseded\" replacement_id=\"{}\"/>",
        record_a.id, record_b.id
    );
    let turn_two = fixture.turn(&session, "second question").await;
    assert!(
        turn_two.contains(&notice_a),
        "turn 2 lacks the notice for A:\n{turn_two}"
    );
    assert!(
        turn_two.contains(MARKER_B),
        "turn 2 must recall the replacement B"
    );
    assert!(!turn_two.contains(MARKER_A), "turn 2 must not recall A");

    let (session, path) = reopen(session);
    let turn_three = fixture.turn(&session, "third question").await;
    assert!(
        turn_three.contains(&notice_a),
        "the reopened session lacks the notice for A:\n{turn_three}"
    );
    assert!(turn_three.contains(MARKER_B));
    assert!(!turn_three.contains(MARKER_A));

    fixture.forget(&record_b);
    let turn_four = fixture.turn(&session, "fourth question").await;
    let forgotten = notice_line_for(&turn_four, &record_b.id).expect("a notice for B");
    assert!(forgotten.contains("reason=\"forgotten\""), "{forgotten}");
    assert!(!forgotten.contains("replacement_id"), "{forgotten}");
    assert!(
        turn_four.contains(&notice_a),
        "the notice for A stays:\n{turn_four}"
    );
    assert!(
        !turn_four.contains(MARKER_B),
        "turn 4 must not recall the forgotten B"
    );
    assert!(!turn_four.contains(MARKER_A));

    let session_file =
        String::from_utf8_lossy(&std::fs::read(&path).expect("read session")).into_owned();
    assert!(
        !session_file.contains("memory_retraction"),
        "notices must not be persisted"
    );
    for marker in [MARKER_A, MARKER_B] {
        assert!(
            !session_file.contains(marker),
            "session file holds {marker}"
        );
    }
    assert!(
        session_file.contains(&record_a.id),
        "the references stay in the session file"
    );
}
