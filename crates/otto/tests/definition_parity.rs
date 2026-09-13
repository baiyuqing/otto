//! Tool schema parity with the Go implementation.
//!
//! `testdata/go_tool_definitions.jsonl` holds one JSON object per tool,
//! produced by marshalling `Definition()` for every tool in `internal/tool`
//! through `encoding/json`. Go marshals maps with sorted keys and
//! `serde_json::Value` is a `BTreeMap`, so the two encodings compare directly
//! once both are parsed. Regenerate the golden file with a scratch program
//! that prints `json.Marshal(tool.Definition())` for each tool.

use std::collections::BTreeMap;

use otto::tool::bash::bash_definition;
use otto::tool::edit::edit_definition;
use otto::tool::find::find_definition;
use otto::tool::grep::grep_definition;
use otto::tool::ls::ls_definition;
use otto::tool::read::read_definition;
use otto::tool::skill::skill_definition;
use otto::tool::write::write_definition;
use otto_core::model::ToolDefinition;

const GO_DEFINITIONS: &str = include_str!("testdata/go_tool_definitions.jsonl");

/// Parses the golden file into a name-keyed map of Go schemas.
fn go_definitions() -> BTreeMap<String, serde_json::Value> {
    GO_DEFINITIONS
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
fn every_rust_tool_schema_matches_the_go_schema() {
    let go = go_definitions();
    let rust = [
        read_definition(),
        write_definition(),
        edit_definition(),
        ls_definition(),
        find_definition(),
        grep_definition(),
        skill_definition(),
        bash_definition(),
    ];
    for definition in &rust {
        let expected = go
            .get(&definition.name)
            .unwrap_or_else(|| panic!("no Go schema recorded for {}", definition.name));
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
    for name in go.keys() {
        assert!(
            covered.contains_key(name.as_str()),
            "no Rust tool advertises the recorded Go schema for {name}"
        );
    }
}
