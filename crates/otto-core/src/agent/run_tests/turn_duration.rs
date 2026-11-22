//! Run-loop tests: the active-time limit, with an injected clock.

use super::progress_recovery::every_tool_call_has_its_result;
use super::*;
use crate::agent::budget::{BudgetKind, BudgetPolicy, BudgetScope, Limit, ManualClock, RootBudget};
use std::time::Duration;

fn policy(max_seconds: u64) -> BudgetPolicy {
    BudgetPolicy {
        max_duration: Limit::from_count(max_seconds),
        ..BudgetPolicy::unlimited()
    }
}

/// A tool that takes `seconds` on the injected clock. With `waits_for_user`
/// it holds a pause for that time, as a tool blocked on an approval does.
struct SlowTool {
    clock: ManualClock,
    seconds: u64,
    waits_for_user: bool,
}

#[async_trait::async_trait]
impl ToolExecutor for SlowTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        EchoExecutor::default().definitions()
    }

    async fn execute(
        &self,
        call: crate::tool::ToolCall<'_>,
        control: &dyn OperationControl,
    ) -> crate::tool::ToolExecution {
        let _pause = self
            .waits_for_user
            .then(|| control.holder().map(|holder| holder.pause_for_user()))
            .flatten();
        self.clock.advance(Duration::from_secs(self.seconds));
        EchoExecutor::default().execute(call, control).await
    }
}

fn distinct_calls(count: usize) -> Vec<Turn> {
    (0..count)
        .map(|index| {
            Turn::tool_call(&format!("call-{index}"), &format!(r#"{{"n":{index}}}"#))
                .with_usage(1, 1)
        })
        .collect()
}

fn agent(
    tool: SlowTool,
    turns: Vec<Turn>,
    diagnostics: &Arc<CapturingDiagnostics>,
) -> Agent<FakeProvider, SlowTool, MemorySession> {
    Agent::new(
        FakeProvider::new(turns),
        tool,
        MemorySession::new(),
        Options {
            diagnostics: Some(diagnostics.clone()),
            ..options()
        },
    )
}

#[tokio::test]
async fn a_turn_stops_with_the_duration_error_once_the_injected_time_passes_the_limit() {
    let clock = ManualClock::new();
    let diagnostics = Arc::new(CapturingDiagnostics::default());
    let agent = agent(
        SlowTool {
            clock: clock.clone(),
            seconds: 400,
            waits_for_user: false,
        },
        distinct_calls(10),
        &diagnostics,
    );
    let root = RootBudget::new("root-1", policy(1_000), clock.shared());
    let error = agent
        .run_with_budget(
            BudgetScope::Shared(root.new_turn()),
            "go",
            None,
            "",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect_err("the active time runs out");

    match &error {
        AgentError::BudgetExceeded(exceeded) => {
            assert_eq!(exceeded.kind, BudgetKind::Duration);
            assert_eq!((exceeded.used, exceeded.limit.get()), (1_200, 1_000));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(
        error.to_string(),
        "budget exceeded: 1200 of 1000 seconds of active time used by this root turn ([agent.budget].max_duration)"
    );
    // Checked before each provider call: the call that follows the third tool
    // batch is refused, and the batch that crossed the limit has its result.
    assert_eq!(agent.provider().normal_requests().len(), 3);
    assert!(every_tool_call_has_its_result(&agent.session().messages()));
    assert_eq!(
        *diagnostics.budget_stops.lock().unwrap(),
        ["duration 1000 1200"]
    );
    // The run ended: its holder left the clock.
    clock.advance(Duration::from_secs(9_999));
    assert_eq!(root.active_time().as_secs(), 1_200);
}

#[tokio::test]
async fn time_a_tool_spends_waiting_for_the_user_does_not_count() {
    let clock = ManualClock::new();
    let diagnostics = Arc::new(CapturingDiagnostics::default());
    let agent = agent(
        SlowTool {
            clock: clock.clone(),
            seconds: 3_600,
            waits_for_user: true,
        },
        vec![
            Turn::tool_call("call-1", r#"{"n":1}"#).with_usage(1, 1),
            Turn::text("done").with_usage(1, 1),
        ],
        &diagnostics,
    );
    let root = RootBudget::new("root-1", policy(60), clock.shared());
    agent
        .run_with_budget(
            BudgetScope::Shared(root.new_turn()),
            "go",
            None,
            "",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect("an hour of waiting adds no active time");
    assert_eq!(root.active_time(), Duration::ZERO);
}

#[tokio::test]
async fn a_second_run_on_an_exhausted_root_is_refused_at_its_first_step() {
    let clock = ManualClock::new();
    let diagnostics = Arc::new(CapturingDiagnostics::default());
    let agent = agent(
        SlowTool {
            clock: clock.clone(),
            seconds: 100,
            waits_for_user: false,
        },
        distinct_calls(10),
        &diagnostics,
    );
    let root = RootBudget::new("root-1", policy(150), clock.shared());
    agent
        .run_with_budget(
            BudgetScope::Shared(root.new_turn()),
            "first",
            None,
            "",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect_err("two tool batches pass 150 seconds");
    let requests = agent.provider().normal_requests().len();
    let error = agent
        .run_with_budget(
            BudgetScope::Shared(root.new_turn()),
            "second",
            None,
            "",
            &mut |_| {},
            &CancellationToken::new(),
        )
        .await
        .expect_err("the root stays exhausted");
    assert!(matches!(
        error,
        AgentError::BudgetExceeded(exceeded) if exceeded.kind == BudgetKind::Duration
    ));
    assert_eq!(agent.provider().normal_requests().len(), requests);
}
