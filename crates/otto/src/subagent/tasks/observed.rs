//! Read-only projection types of the current parent's child tasks, shared by
//! the sub-agent observer, the application controller and the frontends.

use serde::{Deserialize, Serialize};

/// A bounded read-only view of the current parent's children.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObserverSnapshot {
    pub tasks: Vec<ObserverTask>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObserverTask {
    pub row: super::super::record::TaskRow,
    pub entries: Vec<ObserverEntry>,
    /// True when the source transcript/capture is unavailable, not merely truncated.
    pub missing: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObserverEntry {
    pub kind: ObserverEntryKind,
    pub text: String,
    pub tool_name: String,
    pub tool_args: String,
    pub tool_output: String,
    pub tool_done: bool,
    pub tool_error: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverEntryKind {
    Assistant,
    Tool,
    Error,
    #[default]
    System,
}
