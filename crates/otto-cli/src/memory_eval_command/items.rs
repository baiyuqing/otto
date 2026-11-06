//! The layer-3 items: one obsolete record, where its claim appears in a prior turn, the task
//! that would rely on it, the deterministic checks and an unaffected control task.

use serde::{Deserialize, Serialize};

/// Repository path of the item file, embedded at build time.
pub const ITEMS_PATH: &str = "testdata/memory-eval/retraction/items.jsonl";
/// The version of the item format.
pub const ITEMS_VERSION: &str = "memory-retraction-items/1";

const ITEMS_JSONL: &str = include_str!("../../../../testdata/memory-eval/retraction/items.jsonl");

/// How the record left the active state in conditions C and D.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retirement {
    /// Replaced by `Item::replacement`; the lifecycle reports `Superseded`.
    Superseded,
    /// Withdrawn with no replacement; the lifecycle reports `Retracted`.
    Retracted,
    /// Removed on request; the lifecycle reports `Forgotten` and the marker must not leak.
    Forgotten,
}

/// The obsolete record. `marker` is a unique token inside `text`; every leakage check
/// searches for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemRecord {
    pub id: String,
    pub revision: String,
    pub kind: String,
    pub key: String,
    pub scope_id: String,
    pub text: String,
    pub marker: String,
}

/// The active record that replaces the obsolete one (`Retirement::Superseded` only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub id: String,
    pub revision: String,
    pub text: String,
}

/// The prior turn each variant is built from. `assistant`, `tool_output` and `summary` hold
/// the obsolete claim; `assistant_neutral` does not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prior {
    pub user: String,
    pub assistant_neutral: String,
    pub assistant: String,
    pub tool_name: String,
    pub tool_input: String,
    pub tool_output: String,
    pub summary: String,
}

/// The deterministic checks on the task answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskChecks {
    /// Lowercase terms, at least one of which a corrected answer contains; empty when the
    /// item has no replacement.
    pub must_contain_any: Vec<String>,
    /// Whether the right answer admits that the current value is unknown.
    pub uncertainty_expected: bool,
}

/// An active record of another scope that no condition retires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRecord {
    pub id: String,
    pub revision: String,
    pub kind: String,
    pub key: String,
    pub scope_id: String,
    pub text: String,
}

/// The unaffected control task and its checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    pub record: ControlRecord,
    pub task: String,
    /// Lowercase terms, at least one of which a useful control answer contains.
    pub must_contain_any: Vec<String>,
    /// Lowercase terms whose presence shows the retirement changed another scope.
    pub must_not_contain: Vec<String>,
}

/// One layer-3 item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub name: String,
    pub retirement: Retirement,
    pub record: ItemRecord,
    #[serde(default)]
    pub replacement: Option<Replacement>,
    /// Lowercase terms that state the obsolete claim as current; one in the task answer is
    /// continued reliance.
    pub obsolete_terms: Vec<String>,
    pub prior: Prior,
    pub task: String,
    pub checks: TaskChecks,
    pub control: Control,
}

/// Parses every non-empty line, then checks the file's consistency. The error names the line
/// or the item and the rule.
pub fn parse_items(_jsonl: &str) -> Result<Vec<Item>, String> {
    Err("not implemented".into())
}

/// The embedded items.
pub fn embedded_items() -> Result<Vec<Item>, String> {
    parse_items(ITEMS_JSONL)
}

/// The embedded items named in `names`, in file order; every name must exist. `None` selects
/// all of them.
pub fn selected_items(names: Option<&[String]>) -> Result<Vec<Item>, String> {
    let items = embedded_items()?;
    let Some(names) = names else {
        return Ok(items);
    };
    for name in names {
        if !items.iter().any(|item| item.name == *name) {
            return Err(format!("unknown item {name:?}"));
        }
    }
    Ok(items
        .into_iter()
        .filter(|item| names.contains(&item.name))
        .collect())
}
