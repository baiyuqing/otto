//! The layer-3 items: one obsolete record, where its claim appears in a prior turn, the task
//! that would rely on it, the deterministic checks and an unaffected control task.

use serde::{Deserialize, Serialize};

use super::answer_checks::contains_term;

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
pub fn parse_items(jsonl: &str) -> Result<Vec<Item>, String> {
    let mut items: Vec<Item> = Vec::new();
    for (index, line) in jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let item: Item = serde_json::from_str(line)
            .map_err(|error| format!("items line {}: {error}", index + 1))?;
        items.push(item);
    }
    if items.is_empty() {
        return Err("items file holds no item".into());
    }
    for (index, item) in items.iter().enumerate() {
        check_item(item).map_err(|rule| format!("item {:?}: {rule}", item.name))?;
        for earlier in &items[..index] {
            if earlier.name == item.name {
                return Err(format!("item {:?}: duplicate name", item.name));
            }
            if earlier.record.marker == item.record.marker {
                return Err(format!(
                    "item {:?}: marker {:?} is already used by item {:?}",
                    item.name, item.record.marker, earlier.name
                ));
            }
        }
    }
    Ok(items)
}

/// Whether `marker` is `m-NN-xxxx`: two digits and four lowercase ASCII letters.
fn marker_is_well_formed(marker: &str) -> bool {
    let bytes = marker.as_bytes();
    bytes.len() == 9
        && bytes.starts_with(b"m-")
        && bytes[2..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..].iter().all(u8::is_ascii_lowercase)
}

/// The terms of `terms` that are empty or not lowercase.
fn check_terms(field: &str, terms: &[String]) -> Result<(), String> {
    for term in terms {
        if term.is_empty() {
            return Err(format!("{field} holds an empty term"));
        }
        if *term != term.to_lowercase() {
            return Err(format!("{field} term {term:?} is not lowercase"));
        }
    }
    Ok(())
}

fn require_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    Ok(())
}

/// The consistency rules of one item. The error is the rule that failed.
fn check_item(item: &Item) -> Result<(), String> {
    require_text("name", &item.name)?;
    let record = &item.record;
    for (field, value) in [
        ("record.id", &record.id),
        ("record.revision", &record.revision),
        ("record.kind", &record.kind),
        ("record.key", &record.key),
        ("record.scope_id", &record.scope_id),
        ("task", &item.task),
        ("control.task", &item.control.task),
        ("control.record.id", &item.control.record.id),
        ("control.record.revision", &item.control.record.revision),
        ("control.record.scope_id", &item.control.record.scope_id),
        ("control.record.text", &item.control.record.text),
        ("prior.user", &item.prior.user),
        ("prior.assistant_neutral", &item.prior.assistant_neutral),
        ("prior.tool_input", &item.prior.tool_input),
    ] {
        require_text(field, value)?;
    }
    if !marker_is_well_formed(&record.marker) {
        return Err(format!(
            "record.marker {:?} is not of the form m-NN-xxxx",
            record.marker
        ));
    }
    if !record.text.contains(&record.marker) {
        return Err("record.text does not hold record.marker".into());
    }
    check_replacement(item)?;
    check_terms("obsolete_terms", &item.obsolete_terms)?;
    check_terms("checks.must_contain_any", &item.checks.must_contain_any)?;
    check_terms("control.must_contain_any", &item.control.must_contain_any)?;
    check_terms("control.must_not_contain", &item.control.must_not_contain)?;
    if item.obsolete_terms.is_empty() {
        return Err("obsolete_terms is empty".into());
    }
    if item.control.must_contain_any.is_empty() {
        return Err("control.must_contain_any is empty".into());
    }
    if item.checks.must_contain_any.is_empty() == item.replacement.is_some() {
        return Err(if item.replacement.is_some() {
            "checks.must_contain_any is empty although a replacement exists"
        } else {
            "checks.must_contain_any is not empty although no replacement exists"
        }
        .into());
    }
    if item.control.record.id == record.id
        || item
            .replacement
            .as_ref()
            .is_some_and(|replacement| replacement.id == item.control.record.id)
    {
        return Err("control.record.id equals the record or replacement id".into());
    }
    if item.control.record.scope_id == record.scope_id {
        return Err("control.record.scope_id equals record.scope_id".into());
    }
    check_prior(item)
}

fn check_replacement(item: &Item) -> Result<(), String> {
    match (&item.retirement, &item.replacement) {
        (Retirement::Superseded, None) => Err("a superseded item needs a replacement".into()),
        (Retirement::Retracted | Retirement::Forgotten, Some(_)) => {
            Err("only a superseded item may have a replacement".into())
        }
        (_, Some(replacement)) => {
            require_text("replacement.id", &replacement.id)?;
            require_text("replacement.revision", &replacement.revision)?;
            require_text("replacement.text", &replacement.text)?;
            if replacement.id == item.record.id {
                return Err("replacement.id equals record.id".into());
            }
            if replacement.text.contains(&item.record.marker) {
                return Err("replacement.text holds record.marker".into());
            }
            if let Some(term) = holds_obsolete_term(item, &replacement.text) {
                return Err(format!("replacement.text holds the obsolete term {term:?}"));
            }
            Ok(())
        }
        (_, None) => Ok(()),
    }
}

/// The first obsolete term of `item` that `text` holds.
fn holds_obsolete_term<'a>(item: &'a Item, text: &str) -> Option<&'a str> {
    item.obsolete_terms
        .iter()
        .map(String::as_str)
        .find(|term| contains_term(text, term))
}

/// The rules on the prior turn: the neutral copy is clean, the claim copies hold an obsolete
/// term, and no copy holds the marker.
fn check_prior(item: &Item) -> Result<(), String> {
    let prior = &item.prior;
    for (field, text) in [
        ("prior.user", &prior.user),
        ("prior.assistant_neutral", &prior.assistant_neutral),
        ("prior.assistant", &prior.assistant),
        ("prior.tool_input", &prior.tool_input),
        ("prior.tool_output", &prior.tool_output),
        ("prior.summary", &prior.summary),
        ("task", &item.task),
        ("control.task", &item.control.task),
    ] {
        if text.contains(&item.record.marker) {
            return Err(format!("{field} holds record.marker"));
        }
    }
    for (field, text) in [
        ("prior.user", &prior.user),
        ("prior.assistant_neutral", &prior.assistant_neutral),
        ("task", &item.task),
    ] {
        if let Some(term) = holds_obsolete_term(item, text) {
            return Err(format!("{field} holds the obsolete term {term:?}"));
        }
    }
    for (field, text) in [
        ("prior.assistant", &prior.assistant),
        ("prior.tool_output", &prior.tool_output),
        ("prior.summary", &prior.summary),
    ] {
        if holds_obsolete_term(item, text).is_none() {
            return Err(format!("{field} holds no obsolete term"));
        }
    }
    if prior.tool_name != "bash" {
        return Err(format!(
            "prior.tool_name is {:?}, not \"bash\"",
            prior.tool_name
        ));
    }
    Ok(())
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
