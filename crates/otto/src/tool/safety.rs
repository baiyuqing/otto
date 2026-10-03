//! Base retry safety of the built-in tools.
//!
//! The declaration belongs to the tool implementation, never to model-supplied
//! arguments or configuration. Only [`RetrySafety::ReadOnly`] has a consumer
//! today: after a session takeover, a call left without a result is run again
//! by the agent when its tool is `ReadOnly` (see
//! `docs/specs/2026-10-02-replay-readonly-on-takeover.md`). Every other
//! built-in is listed as `NonIdempotent` on purpose, so adding a tool without
//! classifying it fails `every_builtin_tool_is_classified`.

use otto_core::model::RetrySafety;

/// The declared base safety of each built-in tool, by provider-visible name.
/// A tool absent from this table, including every MCP tool, is treated as
/// `NonIdempotent`.
const BASE_SAFETY: &[(&str, RetrySafety)] = &[
    ("otto_help", RetrySafety::ReadOnly),
    ("approval_pending", RetrySafety::ReadOnly),
    ("approval_revoke", RetrySafety::NonIdempotent),
    ("approval_queue", RetrySafety::NonIdempotent),
    ("read", RetrySafety::ReadOnly),
    ("ls", RetrySafety::ReadOnly),
    ("grep", RetrySafety::ReadOnly),
    ("find", RetrySafety::ReadOnly),
    ("memory_search", RetrySafety::ReadOnly),
    ("write", RetrySafety::NonIdempotent),
    ("edit", RetrySafety::NonIdempotent),
    ("bash", RetrySafety::NonIdempotent),
    ("skill", RetrySafety::NonIdempotent),
    ("remember", RetrySafety::NonIdempotent),
    ("forget", RetrySafety::NonIdempotent),
    ("remind", RetrySafety::NonIdempotent),
    ("remind_status", RetrySafety::NonIdempotent),
    ("remind_cancel", RetrySafety::NonIdempotent),
    ("list_models", RetrySafety::NonIdempotent),
    ("agent", RetrySafety::NonIdempotent),
    ("agent_wait", RetrySafety::NonIdempotent),
    ("agent_status", RetrySafety::NonIdempotent),
    ("agent_send", RetrySafety::NonIdempotent),
    ("agent_report", RetrySafety::NonIdempotent),
];

/// The declared base safety of the built-in tool `name`, or `None` when the
/// name is not a built-in.
pub fn declared_safety(name: &str) -> Option<RetrySafety> {
    BASE_SAFETY
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, safety)| *safety)
}

/// Whether a call to `name` may be run again after a takeover: the tool is a
/// built-in declared `ReadOnly`.
pub fn is_replayable(name: &str) -> bool {
    declared_safety(name) == Some(RetrySafety::ReadOnly)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The golden tool schemas name every built-in tool the provider sees.
    const RECORDED_DEFINITIONS: &str = include_str!("../../tests/testdata/tool_definitions.jsonl");

    #[test]
    fn every_builtin_tool_is_classified() {
        for line in RECORDED_DEFINITIONS
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            let value: serde_json::Value = serde_json::from_str(line).expect("golden line");
            let name = value["name"].as_str().expect("golden name");
            assert!(
                declared_safety(name).is_some(),
                "built-in tool {name:?} has no declared retry safety; add it to BASE_SAFETY in \
                 crates/otto/src/tool/safety.rs (NonIdempotent unless it is read-only)"
            );
        }
    }

    #[test]
    fn only_read_only_tools_are_replayable() {
        for name in ["read", "ls", "grep", "find", "memory_search"] {
            assert!(is_replayable(name), "{name}");
        }
        for name in [
            "write",
            "edit",
            "bash",
            "skill",
            "remember",
            "mcp__x__y",
            "",
        ] {
            assert!(!is_replayable(name), "{name}");
        }
    }
}
