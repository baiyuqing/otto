use std::sync::Mutex;

use otto_core::model::{Block, BlockType, FinishReason, Message, Role};
use otto_core::operation::OperationControl;
use otto_core::provider::{
    Provider, ProviderError, ProviderSettlement, Request, Response, StreamSink,
};
use otto_core::session::Session;
use serde_json::value::RawValue;

use super::*;
use crate::cli::testutil::{builder, initial_runtime};
use crate::memory::decide_default_policy;
use crate::subagent::tasks::Tasks;

enum Reply {
    Text(String),
    Fail(String),
    Hang,
}

/// Answers each request from a queue and keeps every request it saw.
struct Scripted {
    replies: Mutex<Vec<Reply>>,
    requests: Mutex<Vec<Request>>,
    started: tokio::sync::Notify,
}

impl Scripted {
    fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies),
            requests: Mutex::new(Vec::new()),
            started: tokio::sync::Notify::new(),
        })
    }

    fn calls(&self) -> Vec<Request> {
        self.requests.lock().expect("lock").clone()
    }
}

#[async_trait::async_trait]
impl Provider for Scripted {
    async fn complete(
        &self,
        request: &Request,
        _emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.requests.lock().expect("lock").push(request.clone());
        let reply = {
            let mut replies = self.replies.lock().expect("lock");
            assert!(!replies.is_empty(), "unexpected provider call");
            replies.remove(0)
        };
        match reply {
            Reply::Text(text) => ProviderSettlement::succeeded(
                Response {
                    message: Message {
                        role: Role::Assistant,
                        blocks: vec![Block::text(text)],
                        finish_reason: Some(FinishReason::Stop),
                        ..Message::default()
                    },
                    cached_input_tokens_present: false,
                    remote_compaction: None,
                },
                1,
            ),
            Reply::Fail(message) => ProviderSettlement::failed(
                ProviderError::Other(message),
                1,
                otto_core::model::EffectCertainty::Completed,
            ),
            Reply::Hang => {
                self.started.notify_one();
                control.cancellation_token().cancelled().await;
                ProviderSettlement::stopped(
                    ProviderError::Cancelled,
                    1,
                    otto_core::model::EffectCertainty::Completed,
                    otto_core::model::OperationStopReason::UserCancellation,
                )
            }
        }
    }
}

struct Fixture {
    _workspace: tempfile::TempDir,
    _sessions: tempfile::TempDir,
    _memory_dir: tempfile::TempDir,
    home: tempfile::TempDir,
    roots: skillwrite::Roots,
    runner: Runner,
    session: crate::runtime::builder::SharedSession,
    provider: Arc<Scripted>,
    service: Arc<Service>,
    user_scope: Scope,
    workspace_scope: Scope,
    store: Arc<Store>,
}

impl Fixture {
    async fn new(replies: Vec<Reply>) -> Self {
        let workspace = tempfile::tempdir().expect("workspace");
        let sessions = tempfile::tempdir().expect("sessions");
        let builder = builder(workspace.path(), sessions.path());
        let runtime = initial_runtime(&builder);
        let session = builder.create_session(&runtime).expect("session");
        let provider = Scripted::new(replies);
        let runner = Runner::scripted(
            session.clone(),
            provider.clone() as Arc<dyn Provider + Send + Sync>,
            Arc::new(Tasks::new()),
        );
        let (memory_dir, memory_store) = crate::memory::files::testsupport::open_temp();
        let identity = memory_store.identity().expect("identity");
        let user_scope = identity.user_scope.clone();
        let workspace_scope = Scope::new("workspace", "ws-1");
        let home = tempfile::tempdir().expect("home");
        let write_root = home.path().join(".otto/skills");
        let roots = skillwrite::Roots {
            history_root: home.path().join(".otto/skill-history"),
            lookup_roots: vec![write_root.clone()],
            write_root,
        };
        Self {
            _workspace: workspace,
            _sessions: sessions,
            _memory_dir: memory_dir,
            home,
            roots,
            runner,
            session,
            provider,
            service: Arc::new(Service::new(memory_store, decide_default_policy)),
            user_scope,
            workspace_scope,
            store: Arc::new(Store::open_in_memory().expect("store")),
        }
    }

    async fn say(&self, role: Role, text: &str) -> String {
        let finish_reason = (role == Role::Assistant).then_some(FinishReason::Stop);
        self.append(Message {
            role,
            blocks: vec![Block::text(text)],
            finish_reason,
            created_at: chrono::Utc::now(),
            ..Message::default()
        })
        .await
    }

    async fn append(&self, message: Message) -> String {
        self.session.append(message).await.expect("append");
        self.session.messages().last().expect("message").id.clone()
    }

    async fn tool_exchange(&self, name: &str, arguments: &str, result: &str) {
        let call_id = format!("call-{name}-{}", self.session.messages().len());
        self.append(Message {
            role: Role::Assistant,
            blocks: vec![Block {
                block_type: BlockType::ToolCall,
                tool_call_id: call_id.clone(),
                tool_name: name.into(),
                arguments: Some(RawValue::from_string(arguments.into()).expect("raw")),
                ..Block::default()
            }],
            finish_reason: Some(FinishReason::ToolCalls),
            created_at: chrono::Utc::now(),
            ..Message::default()
        })
        .await;
        self.append(Message {
            role: Role::Tool,
            blocks: vec![Block {
                block_type: BlockType::ToolResult,
                tool_call_id: call_id,
                tool_name: name.into(),
                text: result.into(),
                ..Block::default()
            }],
            created_at: chrono::Utc::now(),
            ..Message::default()
        })
        .await;
    }

    fn reflector(&self, config: ReflectionRuntime) -> Reflector {
        Reflector::new(config, Some(Arc::clone(&self.store)))
    }

    async fn run(
        &self,
        reflector: &Reflector,
        cancel: &CancellationToken,
    ) -> Result<Report, Error> {
        let path = self.session.path();
        let header = self.session.header();
        let approvals = Arc::new(crate::approval::Service::in_memory().expect("approvals"));
        let mut report = reflector
            .run(
                &Context {
                    approvals: Some(&approvals),
                    runner: &self.runner,
                    session_id: &header.id,
                    presentation_origin: None,
                    session_path: &path,
                    service: &self.service,
                    user_scope: &self.user_scope,
                    workspace_scope: &self.workspace_scope,
                    skill_roots: Some(&self.roots),
                },
                Trigger::Manual,
                "",
                cancel,
            )
            .await?;
        for id in report.skill_approvals.clone() {
            let request = approvals
                .get(&id, &header.workspace, None)
                .expect("skill request");
            let name = request
                .subject
                .id
                .rsplit(':')
                .next()
                .expect("skill name")
                .to_string();
            let action = if self.store.generated(&name).expect("generated").is_some() {
                skillwrite::Written::Revised
            } else {
                skillwrite::Written::Created
            };
            // A vetted model proposal must not reach the filesystem before
            // this explicit simulated human decision.
            if action == skillwrite::Written::Created {
                assert!(!self.roots.write_root.join(&name).exists());
            }
            approvals
                .decide(
                    &id,
                    &request.scope,
                    request.revision,
                    true,
                    "",
                    &crate::approval::Actor::local("test-human"),
                )
                .expect("decide");
            approvals.apply_available(&header.workspace).expect("apply");
            assert_eq!(
                approvals
                    .get(&id, &header.workspace, None)
                    .expect("applied")
                    .application,
                otto_core::approval::ApplicationState::Applied
            );
            report.skills.push((name, action));
        }
        Ok(report)
    }

    fn pending(&self) -> Vec<crate::memory::Candidate> {
        self.service
            .search(&SearchRequest {
                scopes: vec![self.user_scope.clone(), self.workspace_scope.clone()],
                include_candidates: true,
                candidate_states: vec![CandidateState::Pending],
                limit: 50,
                token_budget: 1,
                now: chrono::Utc::now(),
                ..SearchRequest::default()
            })
            .expect("search")
            .candidates
    }

    fn session_id(&self) -> String {
        self.session.header().id
    }
}

fn answer(entry: &str, quote: &str, key: &str) -> String {
    format!(
        r#"{{"memories":[{{"action":"create","scope":"user","kind":"preference","key":"{key}",
            "text":"Answer in Chinese","confidence":0.9,"reason":"the user said so",
            "evidence":[{{"entry":"{entry}","quote":"{quote}"}}]}}]}}"#
    )
}

fn enabled() -> ReflectionRuntime {
    ReflectionRuntime::default()
}

fn request_text(request: &Request) -> String {
    request.messages[0]
        .blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect()
}

// -- skills ---------------------------------------------------------------

const SKILL_BODY: &str = "1. Run `cargo fmt --all`.\n2. Run `cargo clippy --workspace -- -D warnings`.\n3. Fix every warning before committing.";

/// A session where the user asked for a procedure and it ran: returns the
/// user entry id and the tool-result entry id.
async fn lint_session(fixture: &Fixture) -> (String, String) {
    let user = fixture
        .say(
            Role::User,
            "Before every commit always run the lint gate: fmt then clippy.",
        )
        .await;
    fixture
        .tool_exchange(
            "bash",
            r#"{"command":"cargo fmt --all && cargo clippy --workspace"}"#,
            "formatted; clippy found nothing",
        )
        .await;
    let tool = fixture.session.messages().last().expect("tool").id.clone();
    (user, tool)
}

fn skill_answer(action: &str, name: &str, body: &str, user: &str, tool: &str) -> String {
    serde_json::json!({
        "memories": [],
        "skills": [{
            "action": action,
            "name": name,
            "description": "Run the lint gate before committing",
            "body": body,
            "reason": "the user asked for it and it ran cleanly",
            "evidence": [
                {"entry": user, "quote": "always run the lint gate"},
                {"entry": tool, "quote": "clippy found nothing"},
            ],
        }],
    })
    .to_string()
}

fn push(fixture: &Fixture, reply: Reply) {
    fixture.provider.replies.lock().expect("lock").push(reply);
}

mod automatic;
mod memory;
mod runlog_records;
mod skills;
