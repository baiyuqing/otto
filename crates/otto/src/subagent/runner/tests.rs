use super::*;
use crate::faults::files::{FaultyFileDriver, FileOp};
use crate::faults::{Fault, FaultError, FaultPlan, Filter, Rule, Trigger};
use crate::fixtures::subagent_doubles::{
    FakeProvider, RouteStep, StubTool, assistant_text, assistant_tool_call, last_user_text,
    match_any, match_prompt, raw, redaction, redactor, stub, test_config, test_prompt_for,
    tool_names, wait_status,
};
use otto_core::agent::inbox::NotificationKind;
use otto_core::model::{Block, BlockType, FinishReason, Message, Role, Usage};
use otto_core::provider::Response;
use otto_platform::fsops::PosixDriver;
use otto_platform::fsops::files::FileDriver;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// There is no publish window to race: `MemorySession` carries no header,
/// so the transcript is created before `Tasks::add` publishes the history
/// closure.
const _: () = ();

fn runner(config: Config) -> (Arc<Runner>, Vec<String>) {
    let (runner, warnings) = Runner::new(config).expect("the test config is valid");
    (Arc::new(runner), warnings)
}

/// Waits for `id` to finish, failing the test after five seconds.
async fn wait_final(tasks: &Tasks, id: &str) -> Task {
    let cancel = CancellationToken::new();
    tokio::select! {
        result = tasks.wait(id, &cancel) => result.expect("the task exists"),
        () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
            panic!("task {id} did not finish in time")
        }
    }
}

fn definition(name: &str) -> Definition {
    Definition {
        name: name.to_string(),
        ..Definition::default()
    }
}

fn catalog(definitions: Vec<Definition>) -> Catalog {
    Catalog::from_definitions(definitions)
}

fn child_tools(
    registry: Arc<Registry>,
    write_policy: WritePolicy,
    write_paths: Vec<String>,
    parent_inbox: Arc<dyn Mailbox>,
) -> ChildTools {
    child_tools_with_budget(
        registry,
        write_policy,
        write_paths,
        parent_inbox,
        REPORT_BUDGET,
    )
}

fn child_tools_with_budget(
    registry: Arc<Registry>,
    write_policy: WritePolicy,
    write_paths: Vec<String>,
    parent_inbox: Arc<dyn Mailbox>,
    budget: usize,
) -> ChildTools {
    ChildTools {
        registry,
        allowed: None,
        write_policy,
        write_paths,
        parent_inbox,
        task_id: "t1".to_string(),
        session_id: "test-child-session".to_string(),
        max_output_bytes: 16384,
        reports_left: Arc::new(AtomicUsize::new(budget)),
    }
}

async fn execute_child(tools: &ChildTools, name: &str, arguments: &str) -> ToolExecution {
    let operation_id = otto_core::model::OperationId::new("op_test").expect("operation id");
    let arguments = raw(arguments);
    tools
        .execute(
            ToolCall {
                tool_call_id: "",
                operation_id: &operation_id,
                name,
                arguments: &arguments,
                attempt: 1,
            },
            &CancellationToken::new(),
        )
        .await
}

/// A transcript wrapper that asserts, at the moment `otto.task_result` is
/// appended, that the task's completion notification already sits in the
/// parent inbox. This checks the order `finish`
/// (crates/otto/src/subagent/runner.rs) is required to follow: the
/// notification is pushed before the transcript entry is appended.
struct OrderCheckingTranscript {
    inner: Transcript,
    tasks: Arc<Tasks>,
    task_id: String,
}

#[async_trait::async_trait]
impl Session for OrderCheckingTranscript {
    fn memory_references(&self) -> Vec<otto_core::session::TurnMemoryReferences> {
        self.inner.memory_references()
    }

    fn messages(&self) -> Vec<Message> {
        self.inner.messages()
    }

    async fn append(&self, message: Message) -> Result<(), otto_core::session::SessionError> {
        self.inner.append(message).await
    }

    fn latest_compaction(&self) -> Option<otto_core::session::CompactionMetadata> {
        self.inner.latest_compaction()
    }

    async fn append_compaction(
        &self,
        checkpoint: otto_core::session::CompactionCheckpoint,
    ) -> Result<otto_core::session::CompactionMetadata, otto_core::session::SessionError> {
        self.inner.append_compaction(checkpoint).await
    }

    async fn append_operation_fact(
        &self,
        fact: otto_core::session::OperationFact,
    ) -> Result<(), otto_core::session::SessionError> {
        self.inner.append_operation_fact(fact).await
    }

    fn observed_capabilities(&self) -> Option<otto_core::session::Capabilities> {
        self.inner.observed_capabilities()
    }

    fn interrupted_turn(&self) -> Option<otto_core::session::InterruptedTurn> {
        self.inner.interrupted_turn()
    }

    fn plan(&self) -> Vec<otto_core::session::PlanStep> {
        self.inner.plan()
    }

    async fn append_input_if_absent(
        &self,
        input_id: &str,
        message: otto_core::model::Message,
    ) -> Result<otto_core::session::AppendOutcome, otto_core::session::SessionError> {
        self.inner.append_input_if_absent(input_id, message).await
    }

    fn has_input(&self, input_id: &str) -> bool {
        self.inner.has_input(input_id)
    }

    async fn append_custom(
        &self,
        custom_type: &str,
        data: &str,
    ) -> Result<(), otto_core::session::SessionError> {
        if custom_type == TASK_RESULT_CUSTOM_TYPE {
            let pushed = self
                .tasks
                .notifications()
                .snapshot()
                .iter()
                .any(|notification| {
                    notification.task_id == self.task_id
                        && notification.kind == Some(NotificationKind::TaskFinished)
                });
            assert!(
                !pushed,
                "otto.task_result for {} must be durable before its completion notification is pushed",
                self.task_id
            );
        }
        self.inner.append_custom(custom_type, data).await
    }
}

fn user_message(id: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::User,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

fn assistant_message(id: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::Assistant,
        blocks: vec![Block::text(text)],
        ..Message::default()
    }
}

fn tool_call_message(id: &str, tool_name: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_name: tool_name.into(),
            tool_call_id: "call-1".into(),
            ..Block::default()
        }],
        ..Message::default()
    }
}

/// A child store for task `task_id` beside `parent`. With a `plan`, the
/// store's journal runs over a `FaultyFileDriver` for it.
fn child_store(
    parent: &std::path::Path,
    task_id: &str,
    plan: Option<Arc<FaultPlan<FileOp>>>,
) -> (Transcript, String) {
    let name = format!("{task_id}-child");
    let driver: Arc<dyn FileDriver> = match plan {
        Some(plan) => Arc::new(FaultyFileDriver::new(Arc::new(PosixDriver), plan)),
        None => Arc::new(PosixDriver),
    };
    let store = crate::session::Store::create_child_lazy_with_driver(
        parent,
        &name,
        otto_core::session::Header {
            id: "child".into(),
            workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: Utc::now(),
            ..otto_core::session::Header::default()
        },
        driver,
    )
    .expect("child store");
    let path = crate::session::Store::child_path(parent, &name);
    (Arc::new(store), path.to_string_lossy().into_owned())
}

/// The `custom` entries in a child transcript file, as `(customType,
/// data)` pairs, in file order.
fn read_custom_entries(path: &std::path::Path) -> Vec<(String, serde_json::Value)> {
    std::fs::read_to_string(path)
        .expect("read child transcript")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry.get("type").and_then(|value| value.as_str()) == Some("custom"))
        .map(|entry| {
            let custom_type = entry
                .get("customType")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let data = entry
                .get("data")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            (custom_type, data)
        })
        .collect()
}

fn base() -> DateTime<Utc> {
    "2026-01-01T00:00:00Z"
        .parse::<DateTime<Utc>>()
        .expect("the base timestamp parses")
}

fn finished(seconds: i64) -> Option<DateTime<Utc>> {
    Some(base() + chrono::Duration::seconds(seconds))
}

/// An assistant message with one dangling tool call, for building fixture
/// transcripts directly.
fn dangling_call() -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            arguments: Some(raw("{}")),
            ..Block::default()
        }],
        finish_reason: Some(FinishReason::ToolCalls),
        created_at: Utc::now(),
        ..Message::default()
    }
}

/// Writes a child transcript directly, bypassing the runner: an
/// `otto.task_spec` entry, the prompt as a user message when `started`,
/// a dangling tool call when `dangling`, and an `otto.task_result` entry
/// with `status`. Returns the file path. Sets up the fixtures the resume
/// tests below exercise through [`Runner::resume`] and the `agent` tool.
#[allow(clippy::too_many_arguments)]
fn interrupted_fixture(
    parent: &std::path::Path,
    task_id: &str,
    prompt: &str,
    definition: Option<TaskSpecDefinition>,
    started: bool,
    dangling: bool,
    status: &str,
) -> PathBuf {
    interrupted_fixture_with_capabilities(
        parent, task_id, prompt, definition, started, dangling, status, None,
    )
}

/// [`interrupted_fixture`] that also records `capabilities` as the child's
/// `otto.capabilities` entry when set.
#[allow(clippy::too_many_arguments)]
fn interrupted_fixture_with_capabilities(
    parent: &std::path::Path,
    task_id: &str,
    prompt: &str,
    definition: Option<TaskSpecDefinition>,
    started: bool,
    dangling: bool,
    status: &str,
    capabilities: Option<&[String]>,
) -> PathBuf {
    let name = format!("{task_id}-child");
    let store = crate::session::Store::create_child_lazy(
        parent,
        &name,
        otto_core::session::Header {
            id: "child".into(),
            workspace: parent.parent().expect("dir").to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: Utc::now(),
            ..otto_core::session::Header::default()
        },
    )
    .expect("child store");

    let spec = serde_json::to_string(&TaskSpecData {
        agent: String::new(),
        id: task_id.to_string(),
        name: String::new(),
        description: String::new(),
        model: "test-model".to_string(),
        model_source: super::super::tasks::ModelSource::Unknown,
        context: "fresh".to_string(),
        prompt: prompt.to_string(),
        definition,
        parent_tool_call_id: String::new(),
    })
    .expect("serialize task_spec");
    store
        .append_custom_entry(TASK_SPEC_CUSTOM_TYPE, &spec)
        .expect("append task_spec");

    if started {
        let message = Message {
            role: Role::User,
            blocks: vec![Block::text(prompt)],
            created_at: Utc::now(),
            ..Message::default()
        };
        store.append_message(&message).expect("append prompt");
    }
    if dangling {
        store
            .append_message(&dangling_call())
            .expect("append dangling call");
    }
    if let Some(names) = capabilities {
        store
            .append_custom_entry(
                otto_core::session::CAPABILITIES_CUSTOM_TYPE,
                &otto_core::session::encode_capabilities(&otto_core::session::Capabilities {
                    tools: names.to_vec(),
                    ..Default::default()
                }),
            )
            .expect("append capabilities");
    }

    let result = serde_json::to_string(&TaskResultData { status, error: "" })
        .expect("serialize task_result");
    store
        .append_custom_entry(TASK_RESULT_CUSTOM_TYPE, &result)
        .expect("append task_result");

    let path = crate::session::Store::child_path(parent, &name);
    store.close().expect("close");
    path
}

/// A [`Config`] whose `children_dir` resolves to `dir`, for resume tests.
fn config_with_children_dir(
    provider: &Arc<FakeProvider>,
    tasks: &Arc<Tasks>,
    dir: PathBuf,
) -> Config {
    let mut config = test_config(provider, tasks, Vec::new());
    config.children_dir = Some(Arc::new(move || Some(dir.clone())));
    config
}

/// Resumes a started, dangling child whose recorded tool set is
/// `recorded`, with a runner that offers `stub("read")`, `stub("grep")` and
/// the child-only `agent_report`. Returns the first provider request.
async fn resume_with_recorded_tools(recorded: &[&str]) -> otto_core::provider::Request {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    let children_dir = parent_path.with_extension("");
    let recorded: Vec<String> = recorded.iter().map(|name| (*name).to_string()).collect();
    interrupted_fixture_with_capabilities(
        &parent_path,
        "t1",
        "look at this",
        None,
        true,
        true,
        interrupted::INTERRUPTED_STATUS,
        Some(&recorded),
    );

    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = config_with_children_dir(&provider, &tasks, children_dir);
    config.tools = vec![stub("read"), stub("grep")];
    let (runner, _) = runner(config);

    runner
        .resume("t1", "continue please".to_string())
        .await
        .expect("resume succeeds");
    wait_final(&tasks, "t1").await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    requests[0].clone()
}

mod attempt_retry;
mod cancel_cases;
mod external;
mod notifications;
mod records;
mod resume;
mod resume_tool;
mod scheduling;
mod start;
mod tools;

mod session_identity;
