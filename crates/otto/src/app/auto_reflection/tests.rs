use super::*;

#[test]
fn notices_are_drained_in_order_and_bounded() {
    let notices = Notices::new();
    let changes = notices.signal.subscribe();
    notices.push("first".into());
    assert!(changes.has_changed().expect("open"));
    notices.push("second".into());
    assert_eq!(notices.take(), ["first", "second"]);
    assert!(notices.take().is_empty());
    for index in 0..MAXIMUM_NOTICES + 5 {
        notices.push(format!("n{index}"));
    }
    let kept = notices.take();
    assert_eq!(kept.len(), MAXIMUM_NOTICES);
    assert_eq!(kept[0], "n5", "the oldest are dropped first");
    let (_, last) = notices.since(0);
    assert_eq!(
        last as usize,
        MAXIMUM_NOTICES + 5 + 2,
        "ids keep counting past the cap"
    );
}

#[test]
fn notices_can_be_read_by_id_without_draining() {
    let notices = Notices::new();
    assert_eq!(notices.since(0), (Vec::new(), 0));
    notices.push("first".into());
    notices.push("second".into());
    let (all, last) = notices.since(0);
    assert_eq!(
        all,
        [
            Notice {
                id: 1,
                text: "first".to_owned(),
                presentation_origin: String::new(),
            },
            Notice {
                id: 2,
                text: "second".to_owned(),
                presentation_origin: String::new(),
            },
        ]
    );
    assert_eq!(last, 2);
    // A second reader sees the same lines; a reader that is caught up sees none.
    assert_eq!(notices.since(0).0.len(), 2);
    assert_eq!(notices.since(2), (Vec::new(), 2));
    assert_eq!(notices.since(1).0[0].text, "second");
    // Draining is for the single-reader frontends and does not reuse ids.
    assert_eq!(notices.take(), ["first", "second"]);
    assert_eq!(notices.since(0), (Vec::new(), 2));
    notices.push("third".into());
    assert_eq!(notices.since(2).0[0].text, "third");
}

#[test]
fn a_completed_compaction_is_noticed_and_a_noop_is_not() {
    use otto_core::agent::CompactionResult;
    let compacted = AtomicBool::new(false);
    let mut seen = 0;
    {
        let mut sink = |_event: Event| seen += 1;
        let mut observed = observing(&mut sink, &compacted);
        observed(Event::AgentStarted);
        observed(Event::CompactionCompleted {
            compaction: CompactionResult {
                noop: true,
                ..CompactionResult::default()
            },
        });
        assert!(
            !compacted.load(Ordering::SeqCst),
            "a noop is not a compaction"
        );
        observed(Event::CompactionCompleted {
            compaction: CompactionResult {
                checkpoint_id: "abc12345".into(),
                ..CompactionResult::default()
            },
        });
    }
    assert!(compacted.load(Ordering::SeqCst));
    assert_eq!(seen, 3, "every event still reaches the frontend");
}

fn finished_tool(id: &str, attempt: u32, outcome: OperationOutcome, result: ToolResult) -> Event {
    Event::ToolCallFinished {
        operation_id: OperationId::new(id).expect("operation id"),
        attempt,
        tool_name: "read".into(),
        tool_call_id: format!("call-{id}-{attempt}"),
        result,
        outcome,
    }
}

fn successful_tool(id: &str, attempt: u32) -> Event {
    finished_tool(
        id,
        attempt,
        OperationOutcome {
            disposition: OperationDisposition::Succeeded,
            effect_certainty: EffectCertainty::Completed,
            stop_reason: None,
        },
        ToolResult::default(),
    )
}

#[test]
fn task_end_requires_ten_distinct_successes_and_a_finished_run() {
    let observation = RunObservation::default();
    for index in 0..9 {
        observation.observe(&successful_tool(&format!("op_{index}"), 1));
    }
    // A successful retry of an already counted logical operation is still
    // one operation; failed and canceled calls never count.
    observation.observe(&successful_tool("op_0", 2));
    observation.observe(&finished_tool(
        "op_error",
        1,
        OperationOutcome {
            disposition: OperationDisposition::Error,
            effect_certainty: EffectCertainty::KnownNoEffect,
            stop_reason: None,
        },
        ToolResult::error("no"),
    ));
    observation.observe(&finished_tool(
        "op_cancelled",
        1,
        OperationOutcome {
            disposition: OperationDisposition::Cancelled,
            effect_certainty: EffectCertainty::NotStarted,
            stop_reason: Some(otto_core::model::OperationStopReason::UserCancellation),
        },
        ToolResult::error("cancelled"),
    ));
    assert!(!observation.eligible_work(true));

    observation.observe(&successful_tool("op_9", 1));
    assert!(!observation.eligible_work(true), "the run has not finished");
    observation.observe(&Event::AgentFinished);
    assert!(observation.eligible_work(true));
    assert!(
        !observation.eligible_work(false),
        "errors/cancellation suppress"
    );
}

#[test]
fn approval_required_suppresses_task_end_reflection() {
    let observation = RunObservation::default();
    for index in 0..10 {
        observation.observe(&successful_tool(&format!("op_{index}"), 1));
    }
    observation.observe(&Event::AgentFinished);
    observation.observe(&Event::ToolCallFinished {
            operation_id: OperationId::new("op_approval").expect("operation id"),
            attempt: 1,
            tool_name: "bash".into(),
            tool_call_id: "call-approval".into(),
            result: ToolResult {
                content: "approval required.\nApprove in Otto: /approve approval-1\nCommand: \"git push\"\nJustification: \"push\"".into(),
                is_error: true,
                ..ToolResult::default()
            },
            outcome: OperationOutcome {
                disposition: OperationDisposition::Error,
                effect_certainty: EffectCertainty::NotStarted,
                stop_reason: None,
            },
        });
    assert!(!observation.eligible_work(true));
}

#[test]
fn every_completed_run_and_every_failure_is_announced() {
    let dir = Path::new("/logs");
    let skipped = Report::skipped_for_test();
    assert_eq!(notice_for(Ok(skipped.clone()), Some(dir)), None);
    assert_eq!(notice_for(Err(Error::Cancelled), Some(dir)), None);
    assert_eq!(notice_for(Err(Error::Disabled), Some(dir)), None);
    let mut noop = skipped;
    noop.note.clear();
    noop.log = Some(Ok("/logs/run.md".into()));
    let line = notice_for(Ok(noop), Some(dir)).expect("a noop run is announced");
    assert_eq!(
        line,
        "reflection: nothing new to reflect on; log: /logs/run.md"
    );
    let failure =
        notice_for(Err(Error::Model("boom".into())), Some(dir)).expect("a failure is shown");
    assert_eq!(
        failure,
        "reflection: reflection model call failed: boom; run logs: /logs"
    );
    let failure = notice_for(Err(Error::Model("boom".into())), None).expect("shown");
    assert!(!failure.contains("run logs"), "{failure}");
}

// -- end to end: a compaction starts a background run ---------------------

use std::sync::atomic::AtomicUsize;

use otto_core::agent::summary::SUMMARIZATION_SYSTEM_PROMPT;
use otto_core::model::{
    EffectCertainty, FinishReason, Message, OperationDisposition, OperationId, OperationOutcome,
    Role,
};
use otto_core::operation::OperationControl;
use otto_core::provider::{Provider, ProviderSettlement, Request, Response, StreamSink};
use otto_core::session::Session;
use otto_core::tool::ToolResult;

use crate::cli::fixture_tests::{builder, initial_runtime};
use crate::runtime::builder::Runner;
use crate::subagent::tasks::Tasks;

const SUMMARY: &str = "## Goal\nx\n## Constraints & Preferences\nx\n## Observations\nx\n## \
    Progress\n### Done\nx\n### In Progress\nx\n### Blocked\nx\n## Key \
    Decisions\nx\n## Next Steps\nx\n## Critical Context\nx";

/// Answers a compaction request with a valid summary and a reflection
/// request with `reflection`, counting each.
struct Router {
    reflection: String,
    compactions: AtomicUsize,
    reflections: AtomicUsize,
    /// How long a reflection answer takes.
    delay: std::time::Duration,
}

#[async_trait::async_trait]
impl Provider for Router {
    async fn complete(
        &self,
        request: &Request,
        _emit: StreamSink<'_>,
        control: &dyn OperationControl,
    ) -> ProviderSettlement {
        let text = if request
            .system_prompt
            .starts_with(SUMMARIZATION_SYSTEM_PROMPT)
        {
            self.compactions.fetch_add(1, Ordering::SeqCst);
            SUMMARY.to_owned()
        } else {
            self.reflections.fetch_add(1, Ordering::SeqCst);
            tokio::select! {
                () = tokio::time::sleep(self.delay) => {}
                () = control.cancellation_token().cancelled() => {
                    return ProviderSettlement::failed(
                        otto_core::provider::ProviderError::Cancelled,
                        1,
                        EffectCertainty::KnownNoEffect,
                    );
                }
            }
            self.reflection.clone()
        };
        ProviderSettlement::succeeded(
            Response {
                message: Message {
                    role: Role::Assistant,
                    blocks: vec![otto_core::model::Block::text(text)],
                    finish_reason: Some(FinishReason::Stop),
                    ..Message::default()
                },
                cached_input_tokens_present: false,
                remote_compaction: None,
            },
            1,
        )
    }
}

struct Rig {
    controller: Controller,
    provider: Arc<Router>,
    service: Arc<crate::memory::Service>,
    user_scope: crate::memory::Scope,
    _dirs: Vec<tempfile::TempDir>,
}

/// A controller over a persisted session of `exchanges` user and assistant
/// pairs, with a real memory store and a reflector configured by `auto`.
async fn rig(auto: Auto, exchanges: usize) -> Rig {
    rig_with(auto, exchanges, std::time::Duration::ZERO).await
}

async fn rig_with(auto: Auto, exchanges: usize, delay: std::time::Duration) -> Rig {
    use crate::reflection::{Reflector, Store};
    use crate::runtime::wiring::{MemoryWiring, open_memory_service, workspace_memory_scope};
    use otto_core::config::ReflectionRuntime;
    use otto_core::config::memory::MemoryRuntime;

    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let home = tempfile::tempdir().expect("home");
    let memory = tempfile::tempdir().expect("memory");
    let runtime_config = MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: memory.path().join("memory").to_string_lossy().into_owned(),
        ..MemoryRuntime::default()
    };
    let (service, user_scope, usable) =
        open_memory_service(&runtime_config, &[], &mut Vec::new()).expect("memory");
    let mut builder = builder(workspace.path(), sessions.path());
    builder.shared_mut().home = home.path().to_string_lossy().into_owned();
    builder.shared_mut().memory = MemoryWiring {
        client: None,
        extension: None,
        plugin: None,
        service: Arc::clone(&service),
        usable,
        user_scope: user_scope.clone(),
        recall_limit: 8,
        recall_token_budget: 1000,
        advice_audit: None,
    };
    builder.workspace_scope =
        workspace_memory_scope(&runtime_config, &workspace.path().to_string_lossy())
            .expect("workspace scope");
    builder.shared_mut().reflector = Arc::new(
        Reflector::new(
            ReflectionRuntime {
                auto,
                skills: false,
                ..ReflectionRuntime::default()
            },
            Some(Arc::new(Store::open_in_memory().expect("store"))),
        )
        .with_log_dir(home.path().join(".otto/reflection/runs")),
    );
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let mut user_ids = Vec::new();
    for index in 0..exchanges {
        session
            .append(Message {
                role: Role::User,
                blocks: vec![otto_core::model::Block::text(format!(
                    "Message {index}: please always answer in Chinese from now on, thanks."
                ))],
                created_at: chrono::Utc::now(),
                ..Message::default()
            })
            .await
            .expect("append user");
        user_ids.push(session.messages().last().expect("user").id.clone());
        session
            .append(Message {
                role: Role::Assistant,
                blocks: vec![otto_core::model::Block::text(format!("Reply {index}."))],
                finish_reason: Some(FinishReason::Stop),
                created_at: chrono::Utc::now(),
                ..Message::default()
            })
            .await
            .expect("append assistant");
    }
    let reflection = serde_json::json!({"retrospective": "Asked for Chinese replies; nothing took a detour.", "memories": [{
            "action": "create", "scope": "user", "kind": "preference", "key": "language",
            "text": "Answer in Chinese", "confidence": 0.9, "reason": "the user said so",
            "evidence": [{"entry": user_ids.first().cloned().unwrap_or_default(), "quote": "always answer in Chinese"}],
        }]})
        .to_string();
    let provider = Arc::new(Router {
        reflection,
        compactions: AtomicUsize::new(0),
        reflections: AtomicUsize::new(0),
        delay,
    });
    let runner = Runner::scripted(
        session.clone(),
        Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
        Arc::new(Tasks::new()),
    );
    let info = builder.runtime_info(&runtime);
    Rig {
        controller: Controller::new(builder, true, session, runner, info),
        provider,
        service,
        user_scope,
        _dirs: vec![workspace, sessions, home, memory],
    }
}

async fn compact(rig: &Rig) {
    let _ = rig
        .controller
        .compact("", &mut |_event| {}, &CancellationToken::new())
        .await;
}

async fn first_notice(rig: &Rig) -> String {
    let mut changes = rig.controller.notices_changed();
    for _ in 0..200 {
        let notices = rig.controller.take_notices();
        if let Some(line) = notices
            .into_iter()
            .find(|line| !line.starts_with("reflection: running"))
        {
            return line;
        }
        let _ = tokio::time::timeout(std::time::Duration::from_millis(50), changes.changed()).await;
    }
    panic!("no notice arrived");
}

fn pending(rig: &Rig) -> usize {
    use crate::memory::{CandidateState, SearchRequest};
    rig.service
        .search(&SearchRequest {
            scopes: vec![rig.user_scope.clone()],
            include_candidates: true,
            candidate_states: vec![CandidateState::Pending],
            limit: 20,
            token_budget: 1,
            now: chrono::Utc::now(),
            ..SearchRequest::default()
        })
        .expect("search")
        .candidates
        .len()
}

fn successful_work() -> RunObservation {
    let observation = RunObservation::default();
    for index in 0..MINIMUM_SUCCESSFUL_TOOLS {
        observation.observe(&successful_tool(&format!("work_{index}"), 1));
    }
    observation.observe(&Event::AgentFinished);
    observation
}

#[tokio::test]
async fn on_work_reflects_only_after_qualified_work_and_also_keeps_compaction_trigger() {
    let rig = rig(Auto::OnWork, 6).await;
    let observation = successful_work();
    rig.controller.after_prompt(&observation, true);
    let line = first_notice(&rig).await;
    assert!(line.contains("1 candidate(s) queued for review"), "{line}");
    assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
    assert_eq!(pending(&rig), 1);

    // The task-end and compaction triggers share one automatic cooldown.
    compact(&rig).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
    assert!(rig.controller.take_notices().is_empty());
}

#[tokio::test]
async fn only_on_work_auto_mode_runs_the_task_end_trigger() {
    for auto in [Auto::Off, Auto::OnExit, Auto::OnCompaction] {
        let rig = rig(auto, 6).await;
        let observation = successful_work();
        rig.controller.after_prompt(&observation, true);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            rig.provider.reflections.load(Ordering::SeqCst),
            0,
            "{auto:?} must not run task-end reflection"
        );
        assert!(rig.controller.take_notices().is_empty());
    }
}

#[tokio::test]
async fn compaction_modes_start_one_background_reflection_and_queue_a_notice() {
    for auto in [Auto::OnCompaction, Auto::OnWork] {
        let rig = rig(auto, 6).await;
        compact(&rig).await;
        assert_eq!(
            rig.provider.compactions.load(Ordering::SeqCst),
            1,
            "{auto:?}: the compaction ran"
        );

        let line = first_notice(&rig).await;
        assert!(line.contains("1 candidate(s) queued for review"), "{line}");
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
        assert_eq!(pending(&rig), 1);

        // A second compaction inside the interval starts nothing.
        compact(&rig).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
        assert!(rig.controller.take_notices().is_empty());
    }
}

#[tokio::test]
async fn auto_off_and_on_exit_do_not_reflect_after_a_compaction() {
    for auto in [Auto::Off, Auto::OnExit] {
        let rig = rig(auto, 6).await;
        compact(&rig).await;
        assert_eq!(
            rig.provider.compactions.load(Ordering::SeqCst),
            1,
            "{auto:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            rig.provider.reflections.load(Ordering::SeqCst),
            0,
            "{auto:?}"
        );
        assert!(rig.controller.take_notices().is_empty());
    }
}

#[tokio::test]
async fn a_compaction_that_does_nothing_starts_no_reflection() {
    let rig = rig(Auto::OnCompaction, 0).await;
    compact(&rig).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn on_exit_reflects_once_for_a_long_enough_session() {
    let rig = rig(Auto::OnExit, 5).await;
    assert!(rig.controller.reflects_on_exit());
    let line = rig
        .controller
        .reflect_on_exit(&CancellationToken::new())
        .await
        .expect("something to show");
    assert!(line.contains("1 candidate(s) queued for review"), "{line}");
    assert_eq!(pending(&rig), 1);
    // Nothing new: the watermark covers it.
    assert_eq!(
        rig.controller
            .reflect_on_exit(&CancellationToken::new())
            .await,
        None
    );
    assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn on_exit_does_nothing_when_another_mode_is_selected_or_the_session_is_short() {
    let rig = rig(Auto::OnCompaction, 5).await;
    assert!(!rig.controller.reflects_on_exit());
    assert_eq!(
        rig.controller
            .reflect_on_exit(&CancellationToken::new())
            .await,
        None
    );
    let short = self::rig(Auto::OnExit, 2).await;
    assert_eq!(
        short
            .controller
            .reflect_on_exit(&CancellationToken::new())
            .await,
        None
    );
    assert_eq!(short.provider.reflections.load(Ordering::SeqCst), 0);
}

/// The `reflect` tool of the controller's current runner.
fn reflect_tool(rig: &Rig) -> crate::tool::reflect::ReflectTool {
    let runner = rig.controller.runner().expect("runner");
    crate::tool::reflect::ReflectTool::new(Arc::clone(&runner.reflection))
}

async fn call(tool: &crate::tool::reflect::ReflectTool, cancel: &CancellationToken) -> ToolResult {
    use crate::tool::Tool;
    tool.execute(
        &crate::tool::testutil::raw(r#"{"focus":"language"}"#),
        cancel,
    )
    .await
}

#[tokio::test]
async fn the_reflect_tool_runs_a_requested_reflection_whatever_the_auto_mode() {
    let rig = rig(Auto::Off, 3).await;
    let result = call(&reflect_tool(&rig), &CancellationToken::new()).await;
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("1 candidate(s) queued for review"),
        "{}",
        result.content
    );
    assert!(
        result
            .content
            .contains("\n\nRetrospective:\nAsked for Chinese replies; nothing took a detour.\n"),
        "{}",
        result.content
    );
    assert_eq!(pending(&rig), 1);
    assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);

    // Every frontend gets the same line as a notice, and the run log
    // opens with the retrospective.
    let notices = rig.controller.take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let log = notices[0].rsplit("; log: ").next().expect("log path");
    let log = std::fs::read_to_string(log).expect("run log");
    assert!(log.contains("- trigger: requested"), "{log}");
    assert!(
        log.contains("## Retrospective\n\n> Asked for Chinese replies"),
        "{log}"
    );
}

#[tokio::test]
async fn the_reflect_tool_is_rebound_when_the_runner_is_replaced() {
    let rig = rig(Auto::Off, 3).await;
    let session = rig.controller.current_session_opt().expect("session");
    let replaced = Runner::scripted(
        session.clone(),
        Arc::clone(&rig.provider) as Arc<dyn Provider + Send + Sync>,
        Arc::new(Tasks::new()),
    );
    let builder = &rig.controller.builder;
    let info = builder.runtime_info(&initial_runtime(builder));
    assert!(
        rig.controller
            .replace_runner_if_current(&session.header().id, replaced, info)
            .expect("replace")
    );
    let result = call(&reflect_tool(&rig), &CancellationToken::new()).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_reflect_tool_shares_the_single_flight_flag_and_ignores_the_cooldown() {
    let rig = rig(Auto::OnWork, 6).await;
    rig.controller.auto_running.store(true, Ordering::SeqCst);
    let result = call(&reflect_tool(&rig), &CancellationToken::new()).await;
    assert!(!result.is_error);
    assert_eq!(
        result.content,
        super::super::requested_reflection::ALREADY_RUNNING
    );
    assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 0);
    rig.controller.auto_running.store(false, Ordering::SeqCst);

    // An automatic run just started, and the cooldown would hold back
    // another automatic one; a requested one still runs.
    rig.controller.after_prompt(&successful_work(), true);
    let _ = first_notice(&rig).await;
    assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 1);
    let session = rig.controller.current_session_opt().expect("session");
    session
        .append(Message {
            role: Role::User,
            blocks: vec![otto_core::model::Block::text(
                "Please do a retrospective of this session.",
            )],
            created_at: chrono::Utc::now(),
            ..Message::default()
        })
        .await
        .expect("append the request");
    let result = call(&reflect_tool(&rig), &CancellationToken::new()).await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(rig.provider.reflections.load(Ordering::SeqCst), 2);
    assert!(!rig.controller.auto_running.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancelling_the_turn_cancels_a_requested_reflection() {
    let rig = rig_with(Auto::Off, 3, std::time::Duration::from_secs(30)).await;
    let cancel = CancellationToken::new();
    let tool = reflect_tool(&rig);
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        stop.cancel();
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), call(&tool, &cancel))
        .await
        .expect("the cancel ends the run");
    assert!(result.is_error);
    assert!(result.content.contains("cancel"), "{}", result.content);
    assert_eq!(pending(&rig), 0);
    assert!(!rig.controller.auto_running.load(Ordering::SeqCst));
}
