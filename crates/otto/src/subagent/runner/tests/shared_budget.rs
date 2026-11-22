//! A child shares the token budget of the root turn that started it.

use super::*;
use crate::fixtures::subagent_doubles::assistant_tool_call;
use otto_core::agent::budget::{BudgetKind, BudgetPolicy, Limit, SessionBudgetCell};

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

/// A config whose session budget cell is `cell`.
fn budgeted(
    provider: &Arc<FakeProvider>,
    tasks: &Arc<Tasks>,
    cell: &Arc<SessionBudgetCell>,
) -> Config {
    let mut config = test_config(provider, tasks, vec![stub("echo")]);
    config.budget = Arc::clone(cell);
    // Each tool call needs an operation id of its own.
    let counter = std::sync::atomic::AtomicU64::new(0);
    config.template.new_operation_id = Arc::new(move || {
        let next = counter.fetch_add(1, Ordering::SeqCst) + 1;
        otto_core::model::OperationId::new(format!("op_{next}")).map_err(|e| e.to_string())
    });
    config
}

fn start_request() -> StartRequest {
    StartRequest {
        prompt: "work".into(),
        ..StartRequest::default()
    }
}

#[tokio::test]
async fn child_usage_exhausts_the_roots_tokens_for_the_parent_and_the_child() {
    // The child's first response uses 150 of the root's 100 tokens. Its next
    // check stops it, and the parent's next check on the same root stops too.
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call("call-1", "echo", "{}", usage(150)),
            assistant_text("never reached", usage(1)),
        ],
    );
    let tasks = Arc::new(Tasks::new());
    let cell = Arc::new(SessionBudgetCell::default());
    let parent = cell.begin_root("root-a".into(), policy(100, 0));
    parent
        .admit_step()
        .expect("the parent starts within budget");
    let (runner, _) = runner(budgeted(&provider, &tasks, &cell));

    runner.start(start_request()).await.expect("start");
    let task = wait_final(&tasks, "t1").await;

    assert_eq!(task.status, TaskStatus::Failed);
    assert!(
        task.error.contains("budget exceeded: 150 of 100 tokens"),
        "{}",
        task.error
    );
    assert_eq!(provider.requests().len(), 1);
    let from_parent = parent.admit_step().expect_err("the parent is stopped too");
    assert_eq!(from_parent.kind, BudgetKind::Tokens);
    assert_eq!(from_parent.used, 150);
}

#[tokio::test]
async fn tokens_the_parent_used_before_the_child_started_count_for_the_child() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", usage(1))]);
    let tasks = Arc::new(Tasks::new());
    let cell = Arc::new(SessionBudgetCell::default());
    let parent = cell.begin_root("root-a".into(), policy(100, 0));
    parent.charge(Some(usage(100)));
    let (runner, _) = runner(budgeted(&provider, &tasks, &cell));

    runner.start(start_request()).await.expect("start");
    let task = wait_final(&tasks, "t1").await;

    assert_eq!(task.status, TaskStatus::Failed);
    assert!(task.error.contains("budget exceeded"), "{}", task.error);
    assert!(
        provider.requests().is_empty(),
        "the child's first check already finds the root exhausted"
    );
}

#[tokio::test]
async fn a_child_counts_its_own_steps_and_not_the_parents() {
    // The parent turn used both of its two steps. The child still gets two of
    // its own: a tool call and the final answer.
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call("call-1", "echo", "{}", usage(1)),
            assistant_text("done", usage(1)),
        ],
    );
    let tasks = Arc::new(Tasks::new());
    let cell = Arc::new(SessionBudgetCell::default());
    let parent = cell.begin_root("root-a".into(), policy(0, 2));
    parent.admit_step().unwrap();
    parent.admit_step().unwrap();
    assert!(parent.admit_step().is_err());
    let (runner, _) = runner(budgeted(&provider, &tasks, &cell));

    runner.start(start_request()).await.expect("start");
    let task = wait_final(&tasks, "t1").await;

    assert_eq!(task.status, TaskStatus::Succeeded, "{}", task.error);
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn a_child_stops_at_its_own_step_limit() {
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![
            assistant_tool_call("call-1", "echo", "{}", usage(1)),
            assistant_tool_call("call-2", "echo", "{}", usage(1)),
        ],
    );
    let tasks = Arc::new(Tasks::new());
    let cell = Arc::new(SessionBudgetCell::default());
    cell.begin_root("root-a".into(), policy(0, 2));
    let (runner, _) = runner(budgeted(&provider, &tasks, &cell));

    runner.start(start_request()).await.expect("start");
    let task = wait_final(&tasks, "t1").await;

    assert_eq!(task.status, TaskStatus::Failed);
    assert!(
        task.error.contains("2 of 2 provider steps"),
        "{}",
        task.error
    );
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn a_new_root_turn_gives_later_children_fresh_tokens() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", usage(1))]);
    let tasks = Arc::new(Tasks::new());
    let cell = Arc::new(SessionBudgetCell::default());
    let spent = cell.begin_root("root-a".into(), policy(100, 0));
    spent.charge(Some(usage(100)));
    cell.begin_root("root-b".into(), policy(100, 0));
    let (runner, _) = runner(budgeted(&provider, &tasks, &cell));

    runner.start(start_request()).await.expect("start");
    let task = wait_final(&tasks, "t1").await;

    assert_eq!(task.status, TaskStatus::Succeeded, "{}", task.error);
}

#[tokio::test]
async fn a_child_started_before_any_root_turn_starts_one_from_the_template_policy() {
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", usage(1))]);
    let tasks = Arc::new(Tasks::new());
    let cell = Arc::new(SessionBudgetCell::default());
    let mut config = budgeted(&provider, &tasks, &cell);
    config.template.budget = policy(100, 0);
    let (runner, _) = runner(config);

    runner.start(start_request()).await.expect("start");
    let task = wait_final(&tasks, "t1").await;

    assert_eq!(task.status, TaskStatus::Succeeded, "{}", task.error);
    // The child's root is now the session's root: a wake turn continues it.
    assert!(matches!(
        cell.wake_gate(|| unreachable!("the root exists"), policy(100, 0)),
        otto_core::agent::budget::WakeGate::Admit(_)
    ));
}

#[tokio::test]
async fn a_resumed_child_charges_the_current_root() {
    let dir = tempfile::tempdir().expect("dir");
    let parent_path = dir.path().join("parent.jsonl");
    interrupted_fixture(
        &parent_path,
        "t1",
        "look at this",
        None,
        true,
        true,
        interrupted::INTERRUPTED_STATUS,
    );
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", usage(1))]);
    let tasks = Arc::new(Tasks::new());
    let cell = Arc::new(SessionBudgetCell::default());
    let spent = cell.begin_root("root-a".into(), policy(100, 0));
    spent.charge(Some(usage(100)));
    let mut config = config_with_children_dir(&provider, &tasks, parent_path.with_extension(""));
    config.budget = Arc::clone(&cell);
    let (runner, _) = runner(config);

    runner
        .resume("t1", "continue".into())
        .await
        .expect("resume starts");
    let task = wait_final(&tasks, "t1").await;

    assert_eq!(task.status, TaskStatus::Failed);
    assert!(task.error.contains("budget exceeded"), "{}", task.error);
    assert!(provider.requests().is_empty());
}
