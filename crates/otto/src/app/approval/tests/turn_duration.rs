//! The active-time limit across approvals, wake turns and the notice that a
//! wake is held, with an injected clock.

use super::*;
use crate::fixtures::subagent_doubles::{
    FakeProvider, RouteStep, assistant_text, assistant_tool_call, match_any,
};
use otto_core::agent::budget::{BudgetKind, BudgetPolicy, Limit, ManualClock};
use otto_core::model::Usage;
use std::time::Duration;

const APPROVAL_CALL: &str = r#"{"command":"git push","sandbox_permissions":"require_escalated"}"#;
const LIMIT_SECONDS: u64 = 2_400;

fn policy() -> BudgetPolicy {
    BudgetPolicy {
        max_duration: Limit::from_count(LIMIT_SECONDS),
        ..BudgetPolicy::unlimited()
    }
}

fn usage() -> Usage {
    Usage {
        input_tokens: 1,
        ..Usage::default()
    }
}

/// A tool that takes `seconds` on the injected clock.
struct SlowTool {
    clock: ManualClock,
    seconds: u64,
}

#[async_trait::async_trait]
impl Tool for SlowTool {
    fn definition(&self) -> otto_core::model::ToolDefinition {
        definition("slow", "fixture", json!({"type":"object"}))
    }

    async fn execute(&self, _: &RawValue, _: &CancellationToken) -> ToolResult {
        self.clock.advance(Duration::from_secs(self.seconds));
        crate::tool::text_result("ok")
    }
}

struct Fixture {
    controller: Controller,
    provider: Arc<FakeProvider>,
    tasks: Arc<Tasks>,
    clock: ManualClock,
    _dirs: [tempfile::TempDir; 2],
}

/// A controller whose roots measure active time with a manual clock. Its
/// tools are `slow` (`slow_seconds` on that clock), `echo`, and `bash`, which
/// always asks for an approval.
async fn fixture(steps: Vec<RouteStep>, slow_seconds: u64) -> Fixture {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, _) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let session_id = controller.info().session_id;
    let request_id = approvals.request(&session_id, "git push");
    let tasks = controller.subagent_tasks().expect("session registry");
    let provider = FakeProvider::new();
    provider.add_route(match_any, steps);
    let clock = ManualClock::new();
    let runner = Runner::scripted_with_clock(
        controller.current_session(),
        provider.clone(),
        Arc::clone(&tasks),
        vec![
            Box::new(PendingApprovalTool { request_id }),
            Box::new(EchoTool),
            Box::new(SlowTool {
                clock: clock.clone(),
                seconds: slow_seconds,
            }),
        ],
        policy(),
        None,
        clock.shared(),
    );
    let runtime = initial_runtime(controller.builder());
    let info = controller.builder().runtime_info(&runtime);
    assert!(
        controller
            .replace_runner_if_current(&session_id, runner, info)
            .unwrap()
    );
    Fixture {
        controller,
        provider,
        tasks,
        clock,
        _dirs: [workspace, sessions],
    }
}

fn notify(tasks: &Tasks, text: &str) {
    tasks
        .notifications()
        .push(otto_core::agent::inbox::Notification {
            text: text.to_string(),
            ..otto_core::agent::inbox::Notification::default()
        });
}

async fn run_text(
    controller: &Controller,
    text: &str,
    decision: ApprovalDecision,
) -> Result<Stop, AgentError> {
    controller
        .run_with_approvals(
            Step::Text(text),
            &mut |_| {},
            &CancellationToken::new(),
            |_| async move { decision },
        )
        .await
}

#[tokio::test]
async fn a_turn_stops_with_the_duration_error_once_its_active_time_passes_the_limit() {
    let fixture = fixture(
        vec![
            assistant_tool_call("call-1", "slow", "{}", usage()),
            assistant_text("must not be requested", usage()),
        ],
        LIMIT_SECONDS + 1,
    )
    .await;
    let error = run_text(&fixture.controller, "go", ApprovalDecision::Deny)
        .await
        .expect_err("the active time is used up after the slow tool");
    match error {
        AgentError::BudgetExceeded(exceeded) => {
            assert_eq!(exceeded.kind, BudgetKind::Duration);
            assert_eq!(exceeded.limit.get(), LIMIT_SECONDS);
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(fixture.provider.requests().len(), 1);
}

#[tokio::test]
async fn time_spent_waiting_for_an_approval_decision_does_not_count() {
    let fixture = fixture(
        vec![
            assistant_tool_call("call-1", "bash", APPROVAL_CALL, usage()),
            assistant_text("approval needed", usage()),
            assistant_tool_call("call-2", "slow", "{}", usage()),
            assistant_text("done", usage()),
        ],
        10,
    )
    .await;
    let clock = fixture.clock.clone();
    let stop = fixture
        .controller
        .run_with_approvals(
            Step::Text("go"),
            &mut |_| {},
            &CancellationToken::new(),
            |_| {
                // The user takes three hours to answer.
                clock.advance(Duration::from_secs(3 * 3600));
                async { ApprovalDecision::Allow }
            },
        )
        .await
        .expect("three hours in the prompt are not active time");
    assert_eq!(stop, Stop::EndTurn);
    assert_eq!(fixture.provider.requests().len(), 4);
}

#[tokio::test]
async fn a_wake_is_withheld_when_the_active_time_is_used_up_and_announced_once() {
    let fixture = fixture(
        vec![
            assistant_tool_call("call-1", "slow", "{}", usage()),
            assistant_text("second root", usage()),
        ],
        LIMIT_SECONDS + 1,
    )
    .await;
    run_text(&fixture.controller, "hello", ApprovalDecision::Deny)
        .await
        .expect_err("the root turn uses up its active time");
    notify(&fixture.tasks, "[task] secret sub-agent text");
    notify(&fixture.tasks, "[task] another");

    for _ in 0..3 {
        assert!(fixture.controller.prepare_wake().unwrap().is_none());
        assert!(fixture.controller.withheld_wake().is_some());
    }
    assert_eq!(fixture.provider.requests().len(), 1, "no wake turn ran");

    let expected = concat!(
        "2 notifications are held: the active-time budget of the last root turn is used up, ",
        "2401 of 2400 seconds of active time used by this root turn ([agent.budget].max_duration). ",
        "They are delivered with the next input."
    );
    // The terminal UI and the REPL take it as one notice.
    assert_eq!(fixture.controller.take_notices(), [expected]);
    assert!(fixture.controller.take_notices().is_empty());
    // The HTTP server and the ACP frontends read it as one wake reply, and the
    // routed notice queue does not repeat it.
    let (replies, _) = fixture.controller.wake_replies_since(0);
    assert_eq!(
        replies
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        [expected]
    );
    assert!(fixture.controller.routed_notices_since(0).0.is_empty());
    assert!(!expected.contains("secret sub-agent text"));
    // `otto --prompt` has no next input and says so.
    assert_eq!(
        fixture
            .controller
            .withheld_wake()
            .unwrap()
            .one_shot_message(),
        concat!(
            "2 notifications were not delivered: the active-time budget of this run is used up, ",
            "2401 of 2400 seconds of active time used by this root turn ([agent.budget].max_duration). ",
            "A one-shot run has no next input."
        )
    );
}

#[tokio::test]
async fn a_timer_wake_after_a_long_idle_gap_is_delivered_without_a_notice() {
    let fixture = fixture(
        vec![
            assistant_tool_call("call-1", "slow", "{}", usage()),
            assistant_text("answer", usage()),
            assistant_text("woke", usage()),
        ],
        60,
    )
    .await;
    run_text(&fixture.controller, "hello", ApprovalDecision::Deny)
        .await
        .unwrap();
    // The session is idle for three hours; then a timer notification arrives.
    fixture.clock.advance(Duration::from_secs(3 * 3600));
    notify(&fixture.tasks, "[timer] ping");
    let wake = fixture
        .controller
        .prepare_wake()
        .unwrap()
        .expect("idle time does not withhold a wake");
    fixture
        .controller
        .run_with_channel(
            Step::Wake(wake),
            None,
            None,
            &mut |_| {},
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap();
    assert_eq!(fixture.provider.requests().len(), 3);
    assert!(fixture.controller.take_notices().is_empty());
}
