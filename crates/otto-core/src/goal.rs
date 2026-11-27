//! Durable, parent-owned goal state. Execution stops do not imply attainment.

use serde::{Deserialize, Serialize};

pub const GOAL_CUSTOM_TYPE: &str = "otto.goal";
pub const DEFAULT_RETRIES: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Draft,
    Open,
    Achieved,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Execution {
    Ready,
    AwaitingUser,
    WaitingChildren,
    Paused,
    Blocked,
    Interrupted,
    RetryExhausted,
    BudgetExhausted,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    pub revision: u64,
    pub objective: String,
    pub criteria: Vec<String>,
    pub status: Status,
    pub execution: Execution,
    pub max_retries: u8,
    pub retries: u8,
    pub note: String,
    pub evidence: Vec<String>,
    #[serde(default)]
    pub waiting_children: Vec<String>,
    #[serde(default)]
    pub child_ids: Vec<String>,
}

impl Goal {
    pub fn draft(objective: String) -> Self {
        Self {
            revision: 1,
            objective,
            criteria: Vec::new(),
            status: Status::Draft,
            execution: Execution::AwaitingUser,
            max_retries: DEFAULT_RETRIES,
            retries: 0,
            note: String::new(),
            evidence: Vec::new(),
            waiting_children: Vec::new(),
            child_ids: Vec::new(),
        }
    }

    pub fn is_valid(&self) -> bool {
        let bounded = |text: &str| !text.trim().is_empty() && text.len() <= 2000;
        self.revision > 0
            && bounded(&self.objective)
            && self.note.len() <= 2000
            && self.criteria.len() <= 12
            && self.criteria.iter().all(|text| bounded(text))
            && self.evidence.len() <= self.criteria.len()
            && self.evidence.iter().all(|text| bounded(text))
            && (self.execution == Execution::WaitingChildren) != self.waiting_children.is_empty()
            && self
                .waiting_children
                .iter()
                .chain(&self.child_ids)
                .all(|id| !id.is_empty() && id.len() <= 128)
            && self.retries <= self.max_retries
            && match self.status {
                Status::Draft => self.execution == Execution::AwaitingUser,
                Status::Open => !self.criteria.is_empty() && self.execution != Execution::Ended,
                Status::Achieved => {
                    self.execution == Execution::Ended
                        && !self.criteria.is_empty()
                        && self.evidence.len() == self.criteria.len()
                }
                Status::Failed | Status::Cancelled => {
                    self.execution == Execution::Ended && bounded(&self.note)
                }
            }
    }

    pub fn needs_disposition(&self) -> bool {
        self.status == Status::Open && self.execution == Execution::Ready
    }
}
