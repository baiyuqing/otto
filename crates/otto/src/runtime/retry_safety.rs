//! Base retry safety of the built-in tools.
//!
//! The declaration belongs to the tool implementation, never to model-supplied
//! arguments or configuration. Each attempt records it, and only
//! [`RetrySafety::ReadOnly`] has a consumer today: after process loss or a
//! session takeover, a call left without a result is run again by the agent
//! when its tool is `ReadOnly` now and was when the attempt started (see
//! `docs/specs/2026-10-02-replay-readonly-on-takeover.md` and
//! `docs/specs/2026-10-06-crash-recovery-and-turn-resume.md`). Idempotent writes are not automatically replayed either. Tool implementations
//! own the metadata aggregated here for recovery before runtime construction.

use otto_core::model::RetrySafety;

/// Recovery directory assembled from tool-owned metadata; no safety policy lives here.
const BASE_SAFETY: &[(&str, RetrySafety)] = &[
    crate::tool::read::READ_RETRY,
    crate::tool::write::WRITE_RETRY,
    crate::tool::edit::EDIT_RETRY,
    crate::tool::ls::LS_RETRY,
    crate::tool::find::FIND_RETRY,
    crate::tool::grep::GREP_RETRY,
    crate::tool::bash::BASH_RETRY,
    crate::tool::skill::SKILL_RETRY,
    crate::tool::memory::MEMORY_SEARCH_RETRY,
    crate::tool::memory::REMEMBER_RETRY,
    crate::tool::memory::FORGET_RETRY,
    crate::tool::otto::OTTO_HELP_RETRY,
    crate::tool::otto::APPROVAL_QUEUE_RETRY,
    crate::tool::approval::REQUEST_APPROVAL_RETRY,
    crate::tool::models::LIST_MODELS_RETRY,
    crate::runtime::verification::SUBMIT_VERIFICATION_RETRY,
    crate::tool::plan::UPDATE_PLAN_RETRY,
    crate::schedule::remind::REMIND_RETRY,
    crate::schedule::remind::REMIND_STATUS_RETRY,
    crate::schedule::remind::REMIND_CANCEL_RETRY,
    crate::tool::web::WEB_FETCH_RETRY,
    crate::tool::web::WEB_SEARCH_RETRY,
    crate::runtime::task_control::tools::TASK_QUEUE_RETRY,
    crate::runtime::task_control::tools::TASK_REPLY_RETRY,
    crate::runtime::task_control::tools::TASK_STEER_RETRY,
    crate::schedule::tools::SCHEDULE_RETRY,
    crate::schedule::tools::SCHEDULE_LIST_RETRY,
    crate::schedule::tools::SCHEDULE_CANCEL_RETRY,
    crate::subagent::tools::AGENT_YIELD_RETRY,
    crate::subagent::tools::AGENT_RETRY,
    crate::subagent::tools::AGENT_STATUS_RETRY,
    crate::subagent::tools::AGENT_SEND_RETRY,
    crate::subagent::tools::AGENT_CANCEL_RETRY,
    crate::subagent::runner::AGENT_REPORT_RETRY,
    crate::delivery::START_DELIVERY_TASK_RETRY,
    crate::tool::mcp::MCP_SEARCH_TOOLS_RETRY,
    crate::tool::mcp::MCP_CALL_TOOL_RETRY,
    crate::tool::approval::APPROVAL_PENDING_RETRY,
    crate::tool::approval::APPROVAL_REVOKE_RETRY,
];

/// The declared base safety of the built-in tool `name`, or `None` when the
/// name is not a built-in.
pub(crate) fn declared_safety(name: &str) -> Option<RetrySafety> {
    BASE_SAFETY
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, safety)| *safety)
}

/// Whether a call to `name` may be run again after process loss or a
/// takeover: the tool is a built-in declared `ReadOnly`.
pub fn is_replayable(name: &str) -> bool {
    declared_safety(name) == Some(RetrySafety::ReadOnly)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The golden tool schemas name every built-in tool the provider sees.
    const RECORDED_DEFINITIONS: &str = include_str!("../../tests/testdata/tool_definitions.jsonl");

    #[test]
    fn every_native_tool_implementation_explicitly_declares_safety() {
        fn check(dir: &std::path::Path, count: &mut usize) {
            for entry in std::fs::read_dir(dir).expect("source directory") {
                let path = entry.expect("source entry").path();
                let name = path.file_name().unwrap().to_string_lossy();
                // Out-of-line test modules: `tests`, `tests.rs`, `*_tests`, `*_tests.rs`;
                // `fixtures` is test support compiled under `cfg(test)` or `test-support`.
                let stem = name.trim_end_matches(".rs");
                if stem == "tests" || stem.ends_with("_tests") || stem == "fixtures" {
                    continue;
                }
                if path.is_dir() {
                    check(&path, count);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let source = std::fs::read_to_string(&path).expect("Rust source");
                    // Test doubles intentionally exercise the conservative default.
                    let production = source.split("\n#[cfg(test)]\nmod tests").next().unwrap();
                    for marker in ["impl Tool for ", "impl crate::tool::Tool for "] {
                        for implementation in production.split(marker).skip(1) {
                            let body = implementation.split("\nimpl ").next().unwrap();
                            assert!(
                                body.contains("fn retry_safety("),
                                "{}: {} has no explicit retry safety",
                                path.display(),
                                implementation.lines().next().unwrap()
                            );
                            *count += 1;
                        }
                    }
                }
            }
        }
        let mut count = 0;
        check(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut count,
        );
        assert!(
            count >= 40,
            "tool implementation scan must not be empty: {count}"
        );
    }

    #[test]
    fn recovery_metadata_has_unique_names() {
        let mut names = std::collections::HashSet::new();
        for (name, _) in BASE_SAFETY {
            assert!(names.insert(name), "duplicate owner metadata for {name}");
        }
    }

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
                "built-in tool {name:?} has no declared retry safety; declare owner metadata in its implementation and aggregate it in \
                 crates/otto/src/runtime/retry_safety.rs (NonIdempotent unless it is read-only)"
            );
        }
    }

    #[test]
    fn only_read_only_tools_are_replayable() {
        for name in [
            "read",
            "ls",
            "grep",
            "find",
            "memory_search",
            "agent_status",
            "remind_status",
            "list_models",
        ] {
            assert!(is_replayable(name), "{name}");
        }
        for name in [
            "write",
            "edit",
            "bash",
            "skill",
            "remember",
            "submit_verification",
            "agent_yield",
            "approval_revoke",
            "approval_queue",
            "update_plan",
            "mcp__x__y",
            "",
        ] {
            assert!(!is_replayable(name), "{name}");
        }
    }
}
