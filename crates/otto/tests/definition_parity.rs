//! Tool schema regression pins.
//!
//! `testdata/tool_definitions.jsonl` holds one JSON object per tool, in the
//! encoding the provider wire format produces. Both sides are parsed before
//! they are compared, so only the decoded schema has to match, not the bytes.
//! A schema change is deliberate: edit the matching line in the same commit.

use std::collections::BTreeMap;

use otto::subagent::tools::tool_definitions as agent_tool_definitions;
use otto::tool::bash::bash_definition;
use otto::tool::edit::edit_definition;
use otto::tool::find::find_definition;
use otto::tool::grep::grep_definition;
use otto::tool::ls::ls_definition;
use otto::tool::memory::{forget_definition, memory_search_definition, remember_definition};
use otto::tool::read::read_definition;
use otto::tool::skill::skill_definition;
use otto::tool::write::write_definition;
use otto_core::model::ToolDefinition;

const RECORDED_DEFINITIONS: &str = include_str!("testdata/tool_definitions.jsonl");

/// Parses the golden file into a name-keyed map of recorded schemas.
fn recorded_definitions() -> BTreeMap<String, serde_json::Value> {
    RECORDED_DEFINITIONS
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("the golden file holds one JSON object per line");
            let name = value["name"]
                .as_str()
                .expect("every definition carries a name")
                .to_string();
            (name, value)
        })
        .collect()
}

/// Re-encodes a Rust definition the way the provider wire format does.
fn rust_definition(definition: &ToolDefinition) -> serde_json::Value {
    serde_json::to_value(definition).expect("a definition serializes")
}

#[test]
fn every_tool_schema_matches_the_recorded_schema() {
    let recorded = recorded_definitions();
    let mut rust = vec![
        read_definition(),
        write_definition(),
        edit_definition(),
        ls_definition(),
        find_definition(),
        grep_definition(),
        skill_definition(),
        bash_definition(),
        memory_search_definition(),
        remember_definition(),
        forget_definition(),
    ];
    rust.extend(agent_tool_definitions());
    for definition in &rust {
        let expected = recorded
            .get(&definition.name)
            .unwrap_or_else(|| panic!("no schema recorded for {}", definition.name));
        assert_eq!(
            &rust_definition(definition),
            expected,
            "schema mismatch for {}",
            definition.name
        );
    }
    let covered: BTreeMap<&str, ()> = rust
        .iter()
        .map(|definition| (definition.name.as_str(), ()))
        .collect();
    for name in recorded.keys() {
        assert!(
            covered.contains_key(name.as_str()),
            "no tool advertises the recorded schema for {name}"
        );
    }
}
