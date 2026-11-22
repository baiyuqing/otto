//! Run-loop tests: the per-turn token and step budget.

use super::progress_recovery::every_tool_call_has_its_result;
use super::*;
use crate::agent::budget::{BudgetKind, BudgetPolicy, Limit};

fn policy(max_tokens: u64, max_steps: u64) -> BudgetPolicy {
    BudgetPolicy {
        max_tokens: Limit::from_count(max_tokens),
        max_steps: Limit::from_count(max_steps),
    }
}

/// Tool calls with distinct arguments, so only the budget can stop the turn.
fn distinct_calls(count: usize, input: i64, output: i64) -> Vec<Turn> {
    (0..count)
        .map(|index| {
            Turn::tool_call(&format!("call-{index}"), &format!(r#"{{"n":{index}}}"#))
                .with_usage(input, output)
        })
        .collect()
}

#[tokio::test]
async fn the_token_limit_stops_the_turn_after_the_crossing_response_and_its_tool_batch() {
    // 150 tokens per response: 150, 300, 450. The third response crosses 400.
    let diagnostics = Arc::new(CapturingDiagnostics::default());
    let agent = Agent::new(
        FakeProvider::new(distinct_calls(10, 100, 50)),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            budget: policy(400, 0),
            diagnostics: Some(diagnostics.clone()),
            ..options()
        },
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let error = agent
        .run(
            "go",
            &mut |event| sink.lock().unwrap().push(event),
            &CancellationToken::new(),
        )
        .await
        .expect_err("the budget stops the turn");

    match &error {
        AgentError::BudgetExceeded(exceeded) => {
            assert_eq!(exceeded.kind, BudgetKind::Tokens);
            assert_eq!((exceeded.used, exceeded.limit.get()), (450, 400));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(
        error.to_string(),
        "budget exceeded: 450 of 400 tokens used in this turn ([agent.budget].max_tokens)"
    );
    assert_eq!(agent.provider().normal_requests().len(), 3);
    // The batch that followed the crossing response ran and has its result.
    assert!(every_tool_call_has_its_result(&agent.session().messages()));
    let results = agent
        .session()
        .messages()
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| block.block_type == BlockType::ToolResult)
        .count();
    assert_eq!(results, 3);
    assert_eq!(
        *diagnostics.budget_stops.lock().unwrap(),
        ["tokens 400 450"]
    );
    let failures: Vec<_> = collect(&events)
        .into_iter()
        .filter(|event| matches!(event, Event::AgentError { .. }))
        .collect();
    assert_eq!(failures.len(), 1, "one terminal error event");
}

#[tokio::test]
async fn the_step_limit_makes_exactly_that_many_provider_requests() {
    let diagnostics = Arc::new(CapturingDiagnostics::default());
    let agent = Agent::new(
        FakeProvider::new(distinct_calls(10, 1, 1)),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            budget: policy(0, 3),
            diagnostics: Some(diagnostics.clone()),
            ..options()
        },
    );
    let error = agent
        .run("go", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the budget stops the turn");

    assert_eq!(
        error.to_string(),
        "budget exceeded: 3 of 3 provider steps used in this turn ([agent.budget].max_steps)"
    );
    assert_eq!(agent.provider().normal_requests().len(), 3);
    assert!(every_tool_call_has_its_result(&agent.session().messages()));
    assert_eq!(*diagnostics.budget_stops.lock().unwrap(), ["steps 3 3"]);
}

#[tokio::test]
async fn one_step_runs_one_call_and_the_session_stays_usable() {
    let mut turns = distinct_calls(1, 1, 1);
    turns.push(Turn::text("second prompt answered"));
    let agent = Agent::new(
        FakeProvider::new(turns),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            budget: policy(0, 1),
            ..options()
        },
    );
    let error = agent
        .run("go", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the first call asks for a tool and a second is refused");
    assert!(matches!(error, AgentError::BudgetExceeded(_)), "{error:?}");
    assert_eq!(agent.provider().normal_requests().len(), 1);

    // The next turn gets a fresh budget.
    agent
        .run("again", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("the next prompt runs");
    assert_eq!(agent.provider().normal_requests().len(), 2);
}

#[tokio::test]
async fn disabled_limits_let_a_sixty_step_turn_finish() {
    let mut turns = distinct_calls(60, 1_000_000, 1_000_000);
    turns.push(Turn::text("done"));
    let agent = Agent::new(
        FakeProvider::new(turns),
        EchoExecutor::default(),
        MemorySession::new(),
        Options {
            budget: BudgetPolicy::unlimited(),
            ..options()
        },
    );
    agent
        .run("go", &mut |_| {}, &CancellationToken::new())
        .await
        .expect("no limit applies");
    assert_eq!(agent.provider().normal_requests().len(), 61);
}

#[tokio::test]
async fn the_default_policy_stops_a_turn_at_54_steps() {
    let agent = Agent::new(
        FakeProvider::new(distinct_calls(80, 1, 1)),
        EchoExecutor::default(),
        MemorySession::new(),
        options(),
    );
    let error = agent
        .run("go", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the default step limit applies");
    assert!(
        matches!(&error, AgentError::BudgetExceeded(exceeded) if exceeded.kind == BudgetKind::Steps && exceeded.limit.get() == 54),
        "{error:?}"
    );
    assert_eq!(agent.provider().normal_requests().len(), 54);
}

#[tokio::test]
async fn a_compaction_response_is_charged_and_blocks_the_next_provider_call() {
    // The summary response reports 5 + 7 = 12 tokens, which reaches the limit.
    let diagnostics = Arc::new(CapturingDiagnostics::default());
    let agent = Agent::new(
        FakeProvider::new(vec![Turn::summary(&structured_summary()), Turn::text("ok")]),
        EchoExecutor::default(),
        seeded_session().await,
        Options {
            compaction: tight_compaction(),
            budget: policy(12, 0),
            diagnostics: Some(diagnostics.clone()),
            ..options()
        },
    );
    let error = agent
        .run("hello", &mut |_| {}, &CancellationToken::new())
        .await
        .expect_err("the ordinary call after the summary is refused");

    assert!(
        matches!(&error, AgentError::BudgetExceeded(exceeded) if exceeded.kind == BudgetKind::Tokens && exceeded.used == 12),
        "{error:?}"
    );
    assert_eq!(agent.provider().summary_requests().len(), 1);
    assert!(agent.provider().normal_requests().is_empty());
    assert_eq!(*diagnostics.budget_stops.lock().unwrap(), ["tokens 12 12"]);
}
