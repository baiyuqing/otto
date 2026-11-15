//! Compaction summaries affected by retired memory records (issue #591 step
//! 5). One session runs three turns and two manual compactions against a
//! scripted provider; record A is retracted between them, and the session is
//! reopened from its file. Assertions read the provider request bodies.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use otto::fixtures::scripted_provider::{Script, serve, summary_reply, text_reply};
use otto::memory::{
    BindOptions, Record, RecordRef, RecordState, RememberRequest, Scope, Service, SetStateRequest,
};
use otto::provider::openaicompat;
use otto::runtime::builder::{SessionHandle, SharedSession};
use otto_core::agent::memory_retraction::SUMMARY_INSTRUCTIONS_HEADING;
use otto_core::agent::{Agent, CompactionSettings, Event, Options};
use otto_core::config::endpoint::TrustedEndpoint;
use otto_core::config::memory::MemoryRuntime;
use otto_core::model::ToolDefinition;
use otto_core::operation::OperationControl;
use otto_core::session::Header;
use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};
use otto_session::Store;
use tokio_util::sync::CancellationToken;

const MARKER_A: &str = "MARKER-summary-a-5d13";

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

fn answer() -> String {
    text_reply("an answer that is long enough to be stored as the assistant reply")
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
        // Requests in order: turn 1, turn 1b (kept verbatim by the compaction), summary 1, turn 2, summary 2, turn 3, turn 4.
        replies: vec![
            answer(),
            answer(),
            summary_reply("first summary"),
            answer(),
            summary_reply("second summary"),
            answer(),
            answer(),
        ],
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

    fn retract(&self, record: &Record) {
        let current = self.service.get(&self.reference(record)).expect("get");
        self.service
            .set_state(&SetStateRequest {
                reference: self.reference(record),
                expected_revision: current.revision,
                state: RecordState::Retracted,
                superseded_by: String::new(),
                reason: "found to be wrong".into(),
            })
            .expect("set_state");
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

    fn agent(&self, session: &Arc<Store>) -> Agent<openaicompat::Client, NoTools, SharedSession> {
        let binding = self
            .service
            .bind(BindOptions {
                scopes: vec![self.user_scope.clone()],
                default_write_scope: self.user_scope.clone(),
                ..BindOptions::default()
            })
            .expect("bind");
        let client = openaicompat::Client::new_trusted(&self.endpoint, "key");
        Agent::new(
            openaicompat::Client::new_trusted(&self.endpoint, "key"),
            NoTools,
            SharedSession::new(Arc::clone(session) as Arc<dyn SessionHandle>),
            Options {
                model: "test-model".into(),
                request_sizer: Some(Arc::new(client)),
                now: Box::new(Utc::now),
                memory: Some(Arc::new(binding)),
                memory_recall_limit: 12,
                memory_recall_token_budget: 2000,
                compaction: CompactionSettings {
                    auto: false,
                    hard_input_window: 1_000_000,
                    working_window: 100_000,
                    reserve_tokens: 16_384,
                    keep_recent_tokens: 0,
                },
                ..Default::default()
            },
        )
    }

    fn request_count(&self) -> usize {
        self.provider_requests.lock().unwrap().len()
    }

    /// The message text of the one provider request made since `before`.
    fn request_since(&self, before: usize) -> String {
        let requests = self.provider_requests.lock().unwrap().clone();
        assert_eq!(requests.len(), before + 1, "one provider request expected");
        message_text(&requests[before])
    }

    /// Runs one turn of a new agent; returns the request text and the
    /// `MemoryWarning` messages of the turn.
    async fn turn(&self, session: &Arc<Store>, user_text: &str) -> (String, Vec<String>) {
        let before = self.request_count();
        let mut warnings = Vec::new();
        self.agent(session)
            .run(
                user_text,
                &mut |event| {
                    if let Event::MemoryWarning { message } = event {
                        warnings.push(message);
                    }
                },
                &CancellationToken::new(),
            )
            .await
            .expect("turn");
        (self.request_since(before), warnings)
    }

    /// Runs a manual compaction of a new agent; returns the summary request
    /// text.
    async fn compact(&self, session: &Arc<Store>) -> String {
        let before = self.request_count();
        self.agent(session)
            .compact("", &mut |_| {}, &CancellationToken::new())
            .await
            .expect("compact");
        self.request_since(before)
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

fn reopen(session: Arc<Store>) -> (Arc<Store>, PathBuf) {
    let path = PathBuf::from(session.path());
    session.close().expect("close");
    drop(session);
    (Arc::new(Store::open(&path).expect("reopen").0), path)
}

fn affected_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.contains("<memory_summary_affected"))
        .collect()
}

#[tokio::test]
async fn a_summary_from_a_retracted_record_is_marked_then_rebuilt_without_it() {
    let fixture = fixture();
    let sessions = tempfile::tempdir().expect("sessions");
    let session = fixture.create_session(sessions.path());
    let record_a = fixture.remember("a", MARKER_A);

    let (turn_one, _) = fixture.turn(&session, "first question").await;
    assert!(turn_one.contains(MARKER_A), "turn 1 must recall A");
    let session_file = |session: &Arc<Store>| {
        String::from_utf8_lossy(&std::fs::read(session.path()).expect("read session")).into_owned()
    };
    let file_one = session_file(&session);
    assert!(
        file_one.contains("otto.memory_recall"),
        "turn 1 stores its references"
    );
    assert!(file_one.contains(&record_a.id), "the reference names A");

    // A compaction summarizes every turn but the last, so a second turn precedes it.
    fixture.turn(&session, "first follow-up").await;
    let first_summary_request = fixture.compact(&session).await;
    assert!(
        !first_summary_request.contains(SUMMARY_INSTRUCTIONS_HEADING),
        "nothing is retracted at the first compaction:\n{first_summary_request}"
    );
    let first = session.latest_compaction().expect("first checkpoint");
    assert!(
        first
            .details
            .memory_records
            .iter()
            .any(|reference| reference.id == record_a.id),
        "the checkpoint lists A: {:?}",
        first.details.memory_records
    );
    assert!(first.details.memory_retracted.is_empty());

    fixture.retract(&record_a);
    let (turn_two, warnings) = fixture.turn(&session, "second question").await;
    let line_one = format!(
        "<memory_summary_affected checkpoint=\"{}\" record=\"{}\" reason=\"retracted\"/>",
        first.id, record_a.id
    );
    assert!(
        turn_two.contains(&line_one),
        "turn 2 lacks the summary line:\n{turn_two}"
    );
    assert_eq!(affected_lines(&turn_two).len(), 1, "{turn_two}");
    assert!(
        turn_two.contains(&format!("<memory_retraction id=\"{}\"", record_a.id)),
        "turn 2 lacks the record notice for A:\n{turn_two}"
    );
    let summary_warnings: Vec<&String> = warnings
        .iter()
        .filter(|message| message.contains(&first.id) && message.contains("/compact"))
        .collect();
    assert_eq!(summary_warnings.len(), 1, "warnings: {warnings:?}");
    assert!(
        !session_file(&session).contains("memory_summary_affected"),
        "summary lines must not be persisted"
    );

    let second_summary_request = fixture.compact(&session).await;
    assert!(
        second_summary_request.contains(SUMMARY_INSTRUCTIONS_HEADING),
        "the second summary request lacks the retraction block:\n{second_summary_request}"
    );
    let instruction_line = format!("<memory_retraction id=\"{}\"", record_a.id);
    let instruction = second_summary_request
        .lines()
        .skip_while(|line| !line.contains(SUMMARY_INSTRUCTIONS_HEADING))
        .find(|line| line.contains(&instruction_line))
        .expect("a retraction line after the heading");
    assert!(
        instruction.contains("reason=\"retracted\""),
        "{instruction}"
    );
    let second = session.latest_compaction().expect("second checkpoint");
    assert_ne!(second.id, first.id);
    let retracted_ids: Vec<&str> = second
        .details
        .memory_retracted
        .iter()
        .map(|reference| reference.id.as_str())
        .collect();
    assert_eq!(retracted_ids, [record_a.id.as_str()]);
    assert!(second.details.memory_retracted[0].revision.is_some());

    let (turn_three, warnings_three) = fixture.turn(&session, "third question").await;
    assert!(
        affected_lines(&turn_three).is_empty(),
        "turn 3:\n{turn_three}"
    );
    assert!(
        warnings_three
            .iter()
            .all(|message| !message.contains("/compact")),
        "{warnings_three:?}"
    );

    let (session, _) = reopen(session);
    let reopened = session
        .latest_compaction()
        .expect("checkpoint after reopen");
    assert_eq!(reopened.id, second.id);
    assert_eq!(
        reopened.details.memory_retracted,
        second.details.memory_retracted
    );
    let (turn_four, _) = fixture.turn(&session, "fourth question").await;
    assert!(
        affected_lines(&turn_four).is_empty(),
        "turn 4:\n{turn_four}"
    );
}
