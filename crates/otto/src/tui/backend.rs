//! Owned asynchronous work for the terminal UI. Completions retain their source scope.
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use otto_core::agent::context_report::ContextReport;
use otto_core::model::Message;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::attach::Remote;
use crate::app::{Controller, Info, ModelSummary};
use crate::memory::{
    Candidate, CandidateListRequest, CandidateRef, CandidateState, ReviewDecision, ReviewRequest,
};
use crate::runtime::sandbox_config::SandboxChange;
use crate::subagent::record::{ListQuery, ListResult, TaskRow};

mod local;
mod local_command;
mod remote;
pub(crate) use local::*;
use local_command::*;
use remote::*;

#[derive(Clone)]
pub(crate) enum Connection {
    Local(Arc<Controller>),
    Attach(Arc<Remote>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Ticket {
    pub workspace: String,
    pub session: String,
    pub surface: u64,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum Request {
    Sessions,
    /// The current session's fork points, newest first, as session choices
    /// whose value is the prompt's entry id.
    ForkPoints,
    Models,
    Context,
    SandboxPath(String),
    Agents(ListQuery),
    ObserverSnapshot,
    AgentDetail {
        parent: String,
        id: String,
    },
    Reviews,
    ApprovalSnapshot,
    ApprovalActivity,
    Management(crate::app::management::Request),
    Command(Command),
}

#[derive(Debug, Clone)]
pub(crate) enum Command {
    NewSession,
    Resume(String),
    Archive(String),
    Delete(String),
    /// Fork the current session before the prompt with this entry id and
    /// switch to the fork.
    Fork(String),
    SwitchModel(String),
    SwitchModelThinking {
        model: String,
        thinking: String,
        save: bool,
    },
    SetThinking {
        thinking: String,
        save: bool,
    },
    Rename(String),
    Permissions(String),
    SandboxReload,
    SandboxAllow(String),
    SandboxNetwork(String),
    SandboxExclude(String),
    DecideApproval {
        id: String,
        allow: bool,
        feedback: String,
        revision: Option<u64>,
    },
    Approve(String),
    Deny(String),
    ApproveAlways(String),
    MemoryReview {
        id: String,
        accept: bool,
    },
    /// Admit ordinary input to the active turn rather than creating a queued turn.
    AdmitInput(String),
    /// The active run's guidance items; the caller repaints the queued one
    /// from this read rather than from what it sent.
    SteeringState,
    /// Withdraw one guidance item that is still queued.
    WithdrawSteer(u64),
    /// Stop the running step and deliver the queued guidance in the same turn.
    InterruptStep,
    Management(crate::app::management::Request),
    Logout,
}

#[derive(Debug)]
pub(crate) struct SessionChoice {
    pub value: String,
    pub label: String,
}

#[derive(Debug)]
pub(crate) enum Reply {
    Sessions(Vec<SessionChoice>),
    Models(Vec<ModelSummary>),
    Context(ContextReport),
    SandboxPath(String),
    Agents(ListResult),
    ObserverSnapshot(crate::app::tasks::ObserverSnapshot),
    AgentDetail {
        row: TaskRow,
        history: Vec<Message>,
        missing: bool,
    },
    Reviews(Vec<Candidate>),
    ApprovalSnapshot(otto_core::approval::Snapshot),
    ApprovalActivity(crate::approval::ApprovalActivity),
    Steering(Vec<otto_core::agent::steering::Message>),
    Management(crate::app::management::Reply),
    Operation {
        message: String,
        info: Info,
        history: Option<Vec<Message>>,
        prompt: Option<String>,
    },
    /// The switch to a fork; `draft` is the prompt the fork stops before,
    /// placed in the editor rather than sent.
    Forked {
        message: String,
        info: Info,
        history: Vec<Message>,
        draft: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) struct Completion {
    pub mutation: bool,
    pub ticket: Ticket,
    pub result: Result<Reply, String>,
}

pub(crate) struct Driver {
    connection: Connection,
    tx: mpsc::UnboundedSender<Completion>,
    rx: mpsc::UnboundedReceiver<Completion>,
    tasks: Vec<(bool, JoinHandle<()>)>,
    reads: Arc<Mutex<HashSet<Ticket>>>,
    writes: Arc<tokio::sync::Mutex<()>>,
}

impl Driver {
    pub fn new(connection: Connection) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            connection,
            tx,
            rx,
            tasks: Vec::new(),
            reads: Arc::new(Mutex::new(HashSet::new())),
            writes: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn submit(&mut self, ticket: Ticket, request: Request) {
        let read = !matches!(request, Request::Command(_));
        if read
            && !self
                .reads
                .lock()
                .expect("backend reads")
                .insert(ticket.clone())
        {
            return;
        }
        self.tasks.retain(|(_, task)| !task.is_finished());
        let connection = self.connection.clone();
        let tx = self.tx.clone();
        let reads = self.reads.clone();
        let writes = self.writes.clone();
        self.tasks.push((
            !read,
            tokio::spawn(async move {
                let _write = if read {
                    None
                } else {
                    Some(writes.lock().await)
                };
                let result = match connection {
                    Connection::Local(controller) => local(controller, &ticket, request).await,
                    Connection::Attach(remote) => remote_request(remote, &ticket, request).await,
                };
                if read {
                    reads.lock().expect("backend reads").remove(&ticket);
                }
                let _ = tx.send(Completion {
                    mutation: !read,
                    ticket,
                    result,
                });
            }),
        ));
    }

    pub async fn next(&mut self) -> Completion {
        self.rx
            .recv()
            .await
            .expect("driver keeps completion sender")
    }

    pub fn try_next(&mut self) -> Option<Completion> {
        self.rx.try_recv().ok()
    }

    /// Call during clean shutdown; effects already admitted are never replayed.
    pub async fn drain_pending_writes(&mut self) -> Vec<Completion> {
        for (mutation, task) in &mut self.tasks {
            if *mutation {
                let _ = task.await;
            }
        }
        self.tasks.retain(|(_, task)| !task.is_finished());
        let mut completions = Vec::new();
        while let Some(completion) = self.try_next() {
            completions.push(completion);
        }
        completions
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        for (mutation, task) in &self.tasks {
            if !mutation {
                task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests;
