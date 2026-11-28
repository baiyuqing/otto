//! The session messages that are not in the next request, against real turns:
//! a scripted provider, a session file written by the store, tool calls,
//! input provenance and a manual compaction. The report is compared with the
//! entries read back from the file.

use std::sync::Arc;

use chrono::Utc;
use otto::fixtures::scripted_provider::{
    Script, serve, summary_reply, text_reply, tool_call_reply,
};
use otto::provider::openaicompat;
use otto::runtime::builder::{SessionHandle, SharedSession};
use otto_core::agent::context_exclusion::{
    ExclusionReason, NotInRequest, not_in_request_for_entries,
};
use otto_core::agent::{Agent, CompactionSettings, Options};
use otto_core::config::endpoint::TrustedEndpoint;
use otto_core::model::{InputProvenance, InputSource, ToolDefinition};
use otto_core::operation::OperationControl;
use otto_core::session::Header;
use otto_core::tool::{ToolCall, ToolExecution, ToolExecutor, ToolResult};
use otto_session::Store;
use tokio_util::sync::CancellationToken;

struct EchoTool;

#[async_trait::async_trait]
impl ToolExecutor for EchoTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "echo".into(),
            description: "echo".into(),
            parameters: serde_json::value::RawValue::from_string(r#"{"type":"object"}"#.into())
                .ok(),
        }]
    }

    async fn execute(&self, _call: ToolCall<'_>, _: &dyn OperationControl) -> ToolExecution {
        ToolExecution::completed(ToolResult {
            content: "echoed output".into(),
            ..ToolResult::default()
        })
    }
}

struct Fixture {
    _workspace: tempfile::TempDir,
    store: Arc<Store>,
    endpoint: TrustedEndpoint,
}

fn fixture(root: &std::path::Path, replies: Vec<String>) -> Fixture {
    let workspace = tempfile::tempdir().expect("workspace");
    let (base_url, _requests) = serve(Script {
        replies,
        served: Arc::default(),
    });
    let origin = vec![base_url.clone()];
    let endpoint = TrustedEndpoint::authorize(&base_url, "openai-compatible", &origin, &origin)
        .expect("trust the loopback origin");
    let store = Arc::new(
        Store::create(
            root,
            Header {
                id: "not-in-request".into(),
                workspace: workspace.path().to_string_lossy().into_owned(),
                provider: "openai-compatible".into(),
                model: "test-model".into(),
                created_at: Utc::now(),
                ..Header::default()
            },
        )
        .expect("create session"),
    );
    Fixture {
        _workspace: workspace,
        store,
        endpoint,
    }
}

impl Fixture {
    fn agent(&self) -> Agent<openaicompat::Client, EchoTool, SharedSession> {
        let client = openaicompat::Client::new_trusted(&self.endpoint, "key");
        Agent::new(
            openaicompat::Client::new_trusted(&self.endpoint, "key"),
            EchoTool,
            SharedSession::new(Arc::clone(&self.store) as Arc<dyn SessionHandle>),
            Options {
                model: "test-model".into(),
                request_sizer: Some(Arc::new(client)),
                now: Box::new(Utc::now),
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

    async fn turn(&self, agent: &Agent<openaicompat::Client, EchoTool, SharedSession>, text: &str) {
        agent.set_next_input_provenance(Some(InputProvenance::local(InputSource::Tui)));
        agent
            .run(text, &mut |_| {}, &CancellationToken::new())
            .await
            .expect("turn");
    }

    fn not_in_request(
        &self,
        agent: &Agent<openaicompat::Client, EchoTool, SharedSession>,
    ) -> NotInRequest {
        let report = agent.context_report();
        let (entries, _) = Store::read_entries(self.store.path()).expect("read the session file");
        not_in_request_for_entries(&entries, &report.sections).expect("compare")
    }
}

fn answer(text: &str) -> String {
    text_reply(text)
}

#[tokio::test]
async fn turns_with_tool_calls_and_provenance_have_no_excluded_message() {
    let sessions = tempfile::tempdir().expect("sessions");
    let fixture = fixture(
        sessions.path(),
        vec![
            tool_call_reply("call_1", "echo", "{}"),
            answer("first answer"),
            answer("second answer"),
        ],
    );
    let agent = fixture.agent();
    fixture.turn(&agent, "first question").await;
    fixture.turn(&agent, "second question").await;

    let request_text: String = agent
        .context_report()
        .sections
        .iter()
        .flat_map(|section| &section.items)
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        request_text.contains("Runtime origin for the following user input"),
        "the request carries provenance labels that are not session messages"
    );
    assert!(request_text.contains("echoed output"), "{request_text}");
    assert_eq!(
        fixture.not_in_request(&agent),
        NotInRequest {
            items: Vec::new(),
            carried_user_messages: 0
        }
    );
}

#[tokio::test]
async fn a_manual_compaction_lists_the_messages_the_summary_replaced() {
    let sessions = tempfile::tempdir().expect("sessions");
    let fixture = fixture(
        sessions.path(),
        vec![
            tool_call_reply("call_1", "echo", "{}"),
            answer("first answer"),
            answer("second answer"),
            summary_reply("the summary"),
            answer("third answer"),
        ],
    );
    let agent = fixture.agent();
    fixture.turn(&agent, "first question").await;
    fixture.turn(&agent, "second question").await;
    assert!(fixture.not_in_request(&agent).items.is_empty());

    agent
        .compact("", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("compact");
    let after = fixture.not_in_request(&agent);
    let listed: Vec<(&str, ExclusionReason)> = after
        .items
        .iter()
        .map(|item| (item.label.as_str(), item.reason))
        .collect();
    assert!(
        listed.contains(&("session #3 tool echo", ExclusionReason::Summarized)),
        "{listed:?}"
    );
    assert!(
        after
            .items
            .iter()
            .any(|item| item.text.as_deref() == Some("first answer")),
        "{listed:?}"
    );
    assert!(
        after
            .items
            .iter()
            .all(|item| item.reason == ExclusionReason::Summarized),
        "{listed:?}"
    );
    assert_eq!(
        after.carried_user_messages, 1,
        "the first question is in the summary's user-messages block: {listed:?}"
    );

    fixture.turn(&agent, "third question").await;
    let later = fixture.not_in_request(&agent);
    assert_eq!(
        later.items, after.items,
        "new turns add no excluded message"
    );
}
