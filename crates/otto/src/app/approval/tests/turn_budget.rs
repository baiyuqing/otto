//! The turn budget across approval continuations, wake turns and follow-ups.

use super::*;
use crate::fixtures::subagent_doubles::{
    FakeProvider, assistant_text, assistant_tool_call, match_any,
};
use otto_core::agent::budget::{BudgetKind, BudgetPolicy, Limit};
use otto_core::model::Usage;

fn policy(max_tokens: u64, max_steps: u64) -> BudgetPolicy {
    BudgetPolicy {
        max_tokens: Limit::from_count(max_tokens),
        max_steps: Limit::from_count(max_steps),
    }
}

fn usage(total: i64) -> Usage {
    Usage {
        input_tokens: total,
        ..Usage::default()
    }
}

const APPROVAL_CALL: &str = r#"{"command":"git push","sandbox_permissions":"require_escalated"}"#;

/// A controller whose bash tool always asks for approval and whose provider
/// plays `steps`.
async fn budget_controller(
    steps: Vec<crate::fixtures::subagent_doubles::RouteStep>,
    policy: BudgetPolicy,
) -> (Controller, Arc<FakeProvider>, [tempfile::TempDir; 2]) {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, _) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let session_id = controller.info().session_id;
    let request_id = approvals.request(&session_id, "git push");
    let provider = FakeProvider::new();
    provider.add_route(match_any, steps);
    let runner = Runner::scripted_with_budget(
        controller.current_session(),
        provider.clone(),
        Arc::new(Tasks::new()),
        vec![
            Box::new(PendingApprovalTool { request_id }),
            Box::new(EchoTool),
        ],
        policy,
        None,
    );
    let runtime = initial_runtime(controller.builder());
    let info = controller.builder().runtime_info(&runtime);
    assert!(
        controller
            .replace_runner_if_current(&session_id, runner, info)
            .unwrap()
    );
    (controller, provider, [workspace, sessions])
}

async fn run_allowing(controller: &Controller) -> Result<Stop, AgentError> {
    controller
        .run_with_approvals(
            Step::Text("go"),
            &mut |_| {},
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Allow },
        )
        .await
}

#[tokio::test]
async fn an_approval_continuation_keeps_the_step_count_of_its_turn() {
    // Two provider calls before the approval (a bash call that needs it, then
    // the answer that ends the run) and two after it (an echo call and the
    // final answer). With three steps allowed, the fourth call is refused.
    // Before the budget was shared, the continuation started from zero and the
    // turn completed after four calls.
    let (controller, provider, _dirs) = budget_controller(
        vec![
            assistant_tool_call("call-1", "bash", APPROVAL_CALL, usage(1)),
            assistant_text("approval needed", usage(1)),
            assistant_tool_call("call-2", "echo", "{}", usage(1)),
            assistant_text("done", usage(1)),
        ],
        policy(0, 3),
    )
    .await;

    let error = run_allowing(&controller)
        .await
        .expect_err("the step limit counts across the approval");

    match error {
        AgentError::BudgetExceeded(exceeded) => {
            assert_eq!(exceeded.kind, BudgetKind::Steps);
            assert_eq!((exceeded.used, exceeded.limit.get()), (3, 3));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn an_approval_continuation_keeps_the_token_total_of_its_turn() {
    // 600 tokens before the approval and 600 more in the continuation's first
    // response cross the 1000 limit; the continuation's second call is refused.
    let (controller, provider, _dirs) = budget_controller(
        vec![
            assistant_tool_call("call-1", "bash", APPROVAL_CALL, usage(300)),
            assistant_text("approval needed", usage(300)),
            assistant_tool_call("call-2", "echo", "{}", usage(400)),
            assistant_text("done", usage(1)),
        ],
        policy(1000, 0),
    )
    .await;

    let error = run_allowing(&controller)
        .await
        .expect_err("the token limit counts across the approval");

    assert!(
        matches!(&error, AgentError::BudgetExceeded(e) if e.kind == BudgetKind::Tokens && e.used == 1000),
        "{error:?}"
    );
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn a_turn_with_one_approval_inside_its_limits_completes() {
    let (controller, provider, _dirs) = budget_controller(
        vec![
            assistant_tool_call("call-1", "bash", APPROVAL_CALL, usage(1)),
            assistant_text("approval needed", usage(1)),
            assistant_text("done", usage(1)),
        ],
        policy(0, 3),
    )
    .await;
    assert_eq!(run_allowing(&controller).await.unwrap(), Stop::EndTurn);
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn the_next_user_input_starts_a_new_root_with_fresh_counts() {
    let (controller, provider, _dirs) = budget_controller(
        vec![
            assistant_tool_call("call-1", "echo", "{}", usage(1)),
            assistant_text("first", usage(1)),
            assistant_tool_call("call-2", "echo", "{}", usage(1)),
            assistant_text("second", usage(1)),
        ],
        policy(0, 2),
    )
    .await;
    // Each turn uses its two steps exactly; the second turn would be refused
    // at once if it continued the first turn's count.
    for text in ["one", "two"] {
        controller
            .run_with_approvals(
                Step::Text(text),
                &mut |_| {},
                &CancellationToken::new(),
                |_| async { ApprovalDecision::Deny },
            )
            .await
            .unwrap();
    }
    assert_eq!(provider.requests().len(), 4);
}

/// Records the wake-skip reports of a runner.
#[derive(Default)]
struct SkipLog {
    skipped: Mutex<Vec<(String, usize)>>,
}

impl otto_core::diagnostics::Diagnostics for SkipLog {
    fn start(
        &self,
        _: otto_core::diagnostics::Kind,
        _: &str,
        _: Option<&str>,
        _: &str,
        _: &str,
    ) -> Arc<dyn otto_core::diagnostics::Observer> {
        Arc::new(otto_core::diagnostics::Noop)
    }

    fn turn_budget_wake_skipped(&self, budget_id: &str, pending: usize) {
        self.skipped
            .lock()
            .unwrap()
            .push((budget_id.to_string(), pending));
    }
}

/// A controller whose root turn uses 150 of 100 tokens, and a task registry
/// whose inbox the test fills.
async fn exhausted_controller(
    steps: Vec<crate::fixtures::subagent_doubles::RouteStep>,
) -> (
    Controller,
    Arc<FakeProvider>,
    Arc<SkipLog>,
    Arc<Tasks>,
    [tempfile::TempDir; 2],
) {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, _, _) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let tasks = controller.subagent_tasks().expect("session registry");
    let provider = FakeProvider::new();
    provider.add_route(match_any, steps);
    let log = Arc::new(SkipLog::default());
    let runner = Runner::scripted_with_budget(
        controller.current_session(),
        provider.clone(),
        Arc::clone(&tasks),
        Vec::new(),
        policy(100, 0),
        Some(log.clone()),
    );
    let runtime = initial_runtime(controller.builder());
    let info = controller.builder().runtime_info(&runtime);
    let session_id = controller.info().session_id;
    assert!(
        controller
            .replace_runner_if_current(&session_id, runner, info)
            .unwrap()
    );
    (controller, provider, log, tasks, [workspace, sessions])
}

fn notify(tasks: &Tasks, text: &str) {
    tasks
        .notifications()
        .push(otto_core::agent::inbox::Notification {
            text: text.to_string(),
            ..otto_core::agent::inbox::Notification::default()
        });
}

#[tokio::test]
async fn a_wake_is_withheld_while_the_roots_tokens_are_used_up_and_delivered_with_the_next_input() {
    let (controller, provider, log, tasks, _dirs) = exhausted_controller(vec![
        assistant_text("first answer", usage(150)),
        assistant_text("second answer", usage(1)),
    ])
    .await;

    // The root turn uses 150 of 100 tokens and ends normally.
    run_with_text(&controller, "hello").await;
    assert_eq!(provider.requests().len(), 1);
    // The signals every frontend's wake loop waits on.
    let mut mailbox_updates =
        crate::runtime::workflow_notifications::updates(tasks.notifications().as_ref());
    let mut task_updates = tasks.updates();
    notify(&tasks, "[task] sub-agent finished");
    mailbox_updates.borrow_and_update();
    task_updates.borrow_and_update();

    // Every claim path refuses the wake and leaves the notification queued.
    for _ in 0..3 {
        assert!(controller.prepare_wake().unwrap().is_none());
        assert!(controller.withheld_wake().is_some());
    }
    // A refusal signals nothing, so no wake loop is woken by its own refusal.
    assert!(!mailbox_updates.has_changed().unwrap());
    assert!(!task_updates.has_changed().unwrap());
    assert_eq!(provider.requests().len(), 1, "no wake turn ran");
    assert_eq!(tasks.notifications().len(), 1, "the notification waits");
    let withheld = controller.withheld_wake().unwrap();
    assert_eq!(withheld.pending, 1);
    assert_eq!(
        withheld.exceeded.kind,
        otto_core::agent::budget::BudgetKind::Tokens
    );
    // One record for the root, however often the wake was refused.
    let skipped = log.skipped.lock().unwrap().clone();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(skipped[0].1, 1);
    assert!(!skipped[0].0.is_empty());

    // The next user input starts a new root and delivers the notification.
    run_with_text(&controller, "continue").await;
    assert_eq!(provider.requests().len(), 2);
    assert!(tasks.notifications().is_empty());
    let delivered = provider.requests()[1]
        .messages
        .iter()
        .any(|message| message.text().contains("[task] sub-agent finished"));
    assert!(delivered, "the next request carries the notification");
    // The new root has tokens left, so a later notification wakes a turn.
    notify(&tasks, "[task] another");
    assert!(controller.prepare_wake().unwrap().is_some());
}

async fn run_with_text(controller: &Controller, text: &str) {
    controller
        .run_with_approvals(
            Step::Text(text),
            &mut |_| {},
            &CancellationToken::new(),
            |_| async { ApprovalDecision::Deny },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_wake_turn_shares_the_roots_tokens_and_counts_its_own_steps() {
    // The root turn uses 10 of 100 tokens. The wake turn's response takes the
    // total past 100, so a second notification is withheld.
    let (controller, provider, _log, tasks, _dirs) = exhausted_controller(vec![
        assistant_text("answer", usage(10)),
        assistant_text("woke", usage(150)),
    ])
    .await;
    run_with_text(&controller, "hello").await;
    notify(&tasks, "[task] one");
    let wake = controller.prepare_wake().unwrap().expect("tokens remain");
    controller
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
    assert_eq!(provider.requests().len(), 2);

    notify(&tasks, "[task] two");
    assert!(controller.prepare_wake().unwrap().is_none());
    assert_eq!(tasks.notifications().len(), 1);
}

#[tokio::test]
async fn a_session_with_no_root_yet_starts_one_for_its_first_wake_turn() {
    let (controller, provider, _log, tasks, _dirs) =
        exhausted_controller(vec![assistant_text("woke", usage(150))]).await;
    notify(&tasks, "[task] before any input");
    let wake = controller
        .prepare_wake()
        .unwrap()
        .expect("a session with no root admits its first wake turn");
    controller
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
    assert_eq!(provider.requests().len(), 1);
    // That root is now the session's: its 150 tokens withhold the next wake.
    notify(&tasks, "[task] later");
    assert!(controller.prepare_wake().unwrap().is_none());
}

#[tokio::test]
async fn a_followup_is_discarded_with_a_notice_once_the_roots_tokens_are_used_up() {
    // The turn's answer after the approval takes the root past its 100 tokens.
    // The follow-up queued during the approval counts its own steps but shares
    // those tokens, so it is not started.
    let (controller, provider, _dirs) = budget_controller(
        vec![
            assistant_tool_call("call-1", "bash", APPROVAL_CALL, usage(10)),
            assistant_text("approval needed", usage(10)),
            assistant_text("approved work done", usage(150)),
            assistant_text("the follow-up must not run", usage(1)),
        ],
        policy(100, 0),
    )
    .await;
    let control = controller.runner().unwrap().task_control();

    let stop = controller
        .run_with_approvals(
            Step::Text("go"),
            &mut |_| {},
            &CancellationToken::new(),
            |_| {
                control.queue_followup("write the final report").unwrap();
                async { ApprovalDecision::Allow }
            },
        )
        .await
        .unwrap();

    assert_eq!(stop, Stop::EndTurn);
    assert_eq!(
        provider.requests().len(),
        3,
        "the follow-up made no request"
    );
    let history = controller.current_session().messages();
    let notice = history
        .iter()
        .find(|message| message.context_type == "agent_followup")
        .expect("a visible notice");
    assert!(
        notice
            .text()
            .contains("token budget of this turn is used up"),
        "{}",
        notice.text()
    );
    assert!(control.queue_followup("late").is_err(), "none stays queued");
}

#[tokio::test]
async fn a_followup_runs_when_tokens_remain_and_counts_its_own_steps() {
    // Two steps per limit: the first run uses both of the turn's steps (a
    // call, then the answer). The follow-up gets two of its own.
    let (controller, provider, _dirs) = budget_controller(
        vec![
            assistant_tool_call("call-1", "bash", APPROVAL_CALL, usage(1)),
            assistant_text("approval needed", usage(1)),
            assistant_text("approved work done", usage(1)),
            assistant_tool_call("call-2", "echo", "{}", usage(1)),
            assistant_text("follow-up done", usage(1)),
        ],
        policy(0, 3),
    )
    .await;
    let control = controller.runner().unwrap().task_control();

    controller
        .run_with_approvals(
            Step::Text("go"),
            &mut |_| {},
            &CancellationToken::new(),
            |_| {
                control.queue_followup("write the final report").unwrap();
                async { ApprovalDecision::Allow }
            },
        )
        .await
        .unwrap();

    // Three steps for the turn, two for the follow-up: five requests.
    assert_eq!(provider.requests().len(), 5);
}
