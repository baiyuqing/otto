//! The root budget of a run: where an attempt gets it and what a budget stop
//! does to the run.

use super::*;
use crate::fixtures::subagent_doubles::{FakeProvider, assistant_text, match_any, test_config};
use otto_core::agent::budget::{BudgetPolicy, Limit, RootBudget};
use otto_core::model::Usage;

const SEQUENTIAL: &[u8] = br#"
version = 1
[[steps]]
id = "first"
agent = "worker"
prompt = "first"
[[steps]]
id = "second"
agent = "worker"
prompt = "second"
needs = ["first"]
"#;

fn policy(max_tokens: u64) -> BudgetPolicy {
    BudgetPolicy {
        max_tokens: Limit::from_count(max_tokens),
        max_steps: Limit::Unlimited,
    }
}

fn controller_with(
    executor: Arc<dyn Executor>,
    budget: BudgetPolicy,
) -> (Arc<Controller>, tempfile::TempDir) {
    let transcripts = tempfile::tempdir().expect("transcripts");
    let controller = Controller::new_with_deadlines(
        Arc::new(Store::open_in_memory()),
        Catalog::from_definitions(vec![definition(SEQUENTIAL)]),
        executor,
        workspace().into(),
        transcripts.path().into(),
        RuntimeIdentity::default(),
        1,
        DeadlinePolicy::default(),
        Arc::new(Logger::discard()),
        budget,
    );
    (controller, transcripts)
}

/// Records the budget of every attempt and the number of attempts.
#[derive(Default)]
struct Recording {
    budgets: Mutex<Vec<RootBudget>>,
}

#[async_trait::async_trait]
impl Executor for Recording {
    async fn execute(&self, attempt: Attempt, _control: &Control) -> Result<String, String> {
        self.budgets.lock().unwrap().push(attempt.budget);
        Ok("done".into())
    }
}

#[tokio::test]
async fn every_attempt_of_a_parent_started_run_charges_the_parents_root() {
    let executor = Arc::new(Recording::default());
    let (controller, _transcripts) = controller_with(executor.clone(), policy(100));
    let parent = RootBudget::new("parent-root", policy(100));

    let run = controller
        .start_parent("flow", "input", "session", "operation", parent.clone())
        .await
        .expect("start");
    let finished = controller.wait(&run.id).await.expect("wait");

    assert_eq!(finished.status, RunStatus::Succeeded);
    let budgets = executor.budgets.lock().unwrap();
    assert_eq!(budgets.len(), 2);
    assert!(budgets.iter().all(|budget| *budget == parent));
}

#[tokio::test]
async fn a_run_started_without_a_parent_turn_gets_one_root_of_its_own() {
    let executor = Arc::new(Recording::default());
    let (controller, _transcripts) = controller_with(executor.clone(), policy(100));

    let run = controller.start("flow", "input").await.expect("start");
    controller.wait(&run.id).await.expect("wait");

    let budgets = executor.budgets.lock().unwrap();
    assert_eq!(budgets.len(), 2);
    assert_eq!(budgets[0], budgets[1], "the steps of a run share one root");
    assert_eq!(budgets[0].policy(), policy(100));
}

/// Fails the first step with a budget error and counts every attempt.
struct BudgetFailure {
    attempts: AtomicUsize,
}

#[async_trait::async_trait]
impl Executor for BudgetFailure {
    async fn execute(&self, _attempt: Attempt, _control: &Control) -> Result<String, String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(
            "budget exceeded: 150 of 100 tokens used in this turn ([agent.budget].max_tokens)"
                .to_string(),
        )
    }
}

#[tokio::test]
async fn a_budget_stop_fails_the_step_and_the_run_without_a_retry() {
    let executor = Arc::new(BudgetFailure {
        attempts: AtomicUsize::new(0),
    });
    let (controller, _transcripts) = controller_with(executor.clone(), policy(100));

    let run = controller.start("flow", "input").await.expect("start");
    let finished = controller.wait(&run.id).await.expect("wait");

    assert_eq!(executor.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(finished.status, RunStatus::Failed);
    let first = &finished.steps[0];
    assert_eq!(first.status, StepStatus::Failed);
    assert_eq!(first.attempt, 1);
    assert!(
        first
            .error
            .contains("budget exceeded: 150 of 100 tokens used in this turn"),
        "{}",
        first.error
    );
    assert_ne!(finished.steps[1].status, StepStatus::Succeeded);
}

/// Runs each attempt as a real child on a sub-agent runner, as the runtime's
/// executor does, and maps a failed task to the attempt's error.
struct ChildExecutor {
    runner: Arc<crate::subagent::runner::Runner>,
}

#[async_trait::async_trait]
impl Executor for ChildExecutor {
    async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
        let definition = crate::subagent::Definition {
            name: attempt.agent_definition.name,
            write_policy: attempt.agent_definition.write_policy.as_subagent(),
            ..crate::subagent::Definition::default()
        };
        let task = self
            .runner
            .run_with_definition_control(
                crate::subagent::runner::StartRequest {
                    prompt: attempt.prompt,
                    description: attempt.step_id,
                    agent: attempt.agent,
                    context: "fresh".into(),
                    ..Default::default()
                },
                Arc::new(otto_core::session::MemorySession::new()),
                definition,
                control,
                attempt.budget,
            )
            .await?;
        match task.status {
            crate::subagent::tasks::TaskStatus::Succeeded => Ok(task.result),
            _ => Err(task.error),
        }
    }
}

#[tokio::test]
async fn a_step_that_finds_the_shared_tokens_used_up_fails_the_run_and_is_not_retried() {
    // The first step's one response uses 150 of 100 tokens and ends normally.
    // The second step's child stops at its first check, before any request.
    let provider = FakeProvider::new();
    provider.add_route(
        match_any,
        vec![assistant_text(
            "first answer",
            Usage {
                input_tokens: 150,
                ..Usage::default()
            },
        )],
    );
    let tasks = Arc::new(crate::subagent::tasks::Tasks::new());
    let (runner, _) =
        crate::subagent::runner::Runner::new(test_config(&provider, &tasks, Vec::new()))
            .expect("runner");
    let (controller, _transcripts) = controller_with(
        Arc::new(ChildExecutor {
            runner: Arc::new(runner),
        }),
        policy(100),
    );

    let run = controller.start("flow", "input").await.expect("start");
    let finished = controller.wait(&run.id).await.expect("wait");

    assert_eq!(finished.status, RunStatus::Failed);
    assert_eq!(finished.steps[0].status, StepStatus::Succeeded);
    let second = &finished.steps[1];
    assert_eq!(second.status, StepStatus::Failed);
    assert_eq!(second.attempt, 1, "a budget stop is not retried");
    assert!(
        second
            .error
            .contains("budget exceeded: 150 of 100 tokens used in this turn"),
        "{}",
        second.error
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "the second step made no request"
    );
}

#[tokio::test]
async fn an_explicit_retry_starts_a_new_root() {
    // The first attempt fails; the retry's attempt must not hold the root of
    // the failed attempt, which may be the one that ran out.
    struct FailOnce {
        budgets: Mutex<Vec<RootBudget>>,
    }
    #[async_trait::async_trait]
    impl Executor for FailOnce {
        async fn execute(&self, attempt: Attempt, _control: &Control) -> Result<String, String> {
            let mut budgets = self.budgets.lock().unwrap();
            budgets.push(attempt.budget);
            if budgets.len() == 1 {
                Err("budget exceeded".into())
            } else {
                Ok("done".into())
            }
        }
    }
    let executor = Arc::new(FailOnce {
        budgets: Mutex::new(Vec::new()),
    });
    let (controller, _transcripts) = controller_with(executor.clone(), policy(100));
    let run = controller.start("flow", "input").await.expect("start");
    let failed = controller.wait(&run.id).await.expect("wait");
    assert_eq!(failed.status, RunStatus::Failed);

    controller
        .resume(&run.id, Some("first"))
        .await
        .expect("retry");
    controller.wait(&run.id).await.expect("wait");

    let budgets = executor.budgets.lock().unwrap();
    assert!(budgets.len() >= 2);
    assert_ne!(budgets[0], budgets[1]);
}
