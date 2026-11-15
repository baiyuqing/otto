use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use otto_core::operation::OperationControl;
use tokio::sync::Notify;

use crate::fixtures::shared_workspace as workspace;

fn definition(source: &[u8]) -> Definition {
    let mut definition = parse_definition("flow", source, &|_| true).expect("definition");
    let names: HashSet<String> = definition
        .steps
        .iter()
        .filter(|step| matches!(step.kind, StepKind::Agent | StepKind::Handoff))
        .map(|step| step.agent.clone())
        .collect();
    definition.agents = names
        .into_iter()
        .map(|name| AgentSnapshot {
            name,
            write_policy: SnapshotWritePolicy::ReadOnly,
            ..AgentSnapshot::default()
        })
        .collect();
    definition
}

fn agent_snapshot(
    name: &str,
    tools: Option<&[&str]>,
    write_policy: SnapshotWritePolicy,
    write_paths: &[&str],
) -> AgentSnapshot {
    AgentSnapshot {
        name: name.to_string(),
        tools: tools.map(|tools| tools.iter().map(|tool| (*tool).to_string()).collect()),
        write_policy,
        write_paths: write_paths.iter().map(|path| (*path).to_string()).collect(),
        ..AgentSnapshot::default()
    }
}

struct FakeExecutor;

#[async_trait::async_trait]
impl Executor for FakeExecutor {
    async fn execute(&self, attempt: Attempt, _control: &Control) -> Result<String, String> {
        Ok(format!("{} complete", attempt.step_id))
    }
}

struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FailAndCancelExecutor {
    sibling_stopped: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Executor for FailAndCancelExecutor {
    async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
        if attempt.step_id == "fail" {
            return Err("failed".to_string());
        }
        control.cancellation_token().cancelled().await;
        self.sibling_stopped.store(true, Ordering::SeqCst);
        Err("context canceled".to_string())
    }
}

struct CountingExecutor(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Executor for CountingExecutor {
    async fn execute(&self, attempt: Attempt, _control: &Control) -> Result<String, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(format!("{} done", attempt.step_id))
    }
}

struct BoundaryExecutor(Arc<Notify>);

#[async_trait::async_trait]
impl Executor for BoundaryExecutor {
    async fn execute(&self, _attempt: Attempt, _control: &Control) -> Result<String, String> {
        self.0.notify_one();
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok("boundary".into())
    }
}

struct TimeoutBatchExecutor {
    sibling_started: Arc<Notify>,
    sibling_release: CancellationToken,
}

#[async_trait::async_trait]
impl Executor for TimeoutBatchExecutor {
    async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
        if attempt.step_id == "timeout" {
            control.cancellation_token().cancelled().await;
            return Err("stopped".into());
        }
        self.sibling_started.notify_one();
        self.sibling_release.cancelled().await;
        Ok("settled".into())
    }
}

struct BlockingExecutor {
    started: Arc<Notify>,
}

#[async_trait::async_trait]
impl Executor for BlockingExecutor {
    async fn execute(&self, _attempt: Attempt, control: &Control) -> Result<String, String> {
        self.started.notify_one();
        control.cancellation_token().cancelled().await;
        Err("context canceled".to_string())
    }
}

const TWO_STEP: &[u8] = br#"
version = 1
[[steps]]
id = "a"
agent = "worker"
prompt = "a"
[[steps]]
id = "b"
agent = "worker"
prompt = "b"
"#;

struct GateExecutor {
    started: Arc<AtomicBool>,
    canceled: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Executor for GateExecutor {
    async fn execute(&self, attempt: Attempt, control: &Control) -> Result<String, String> {
        if attempt.step_id == "work" {
            self.started.store(true, Ordering::SeqCst);
            control.cancellation_token().cancelled().await;
            self.canceled.store(true, Ordering::SeqCst);
            return Err("context canceled".to_string());
        }
        Ok(format!("{} complete", attempt.step_id))
    }
}

mod controller;
mod definitions;
mod recovery;
mod store;
