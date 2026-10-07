//! The text of the `capability_update` context message that states a change
//! in the session's tools or Otto version, and the digest of a tool
//! definition.
//!
//! The text depends only on the session's last recorded capabilities, the
//! current capabilities, and whether the session has history; the agent
//! appends the message and writes the record.

use sha2::{Digest, Sha256};

use crate::model::ToolDefinition;
use crate::session::Capabilities;
use crate::tool::safe_prompt_tool_name;

const USE_ONLY: &str = "Use only the tools defined in the current request. Their descriptions and the policy sections of the system prompt state how to use them.";

/// The first 16 hex characters of the SHA-256 of a tool's description, a
/// 0x00 byte and its raw parameter bytes as sent.
pub(super) fn definition_digest(definition: &ToolDefinition) -> String {
    let mut hasher = Sha256::new();
    hasher.update(definition.description.as_bytes());
    hasher.update([0]);
    hasher.update(definition.parameters.as_ref().map_or("", |raw| raw.get()));
    hasher.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn safe_version(version: &str) -> bool {
    (1..=64).contains(&version.len())
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
}

fn listed<'a>(names: impl Iterator<Item = &'a String>) -> String {
    names
        .filter(|name| safe_prompt_tool_name(name))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The notice text, or an empty string when none applies.
///
/// Only tool names that [`safe_prompt_tool_name`] accepts and versions that
/// pass `safe_version` are listed. A field missing from either side of a
/// comparison is unknown and produces no line.
pub(super) fn notice(
    observed: Option<&Capabilities>,
    current: &Capabilities,
    has_history: bool,
) -> String {
    let mut lines = Vec::new();
    match observed {
        Some(observed) => {
            let added = listed(current.tools.iter().filter(|n| !observed.tools.contains(n)));
            let removed = listed(observed.tools.iter().filter(|n| !current.tools.contains(n)));
            let changed = match (&observed.definitions, &current.definitions) {
                (Some(old), Some(new)) => listed(current.tools.iter().filter(|name| {
                    observed.tools.contains(name)
                        && matches!((old.get(*name), new.get(*name)), (Some(a), Some(b)) if a != b)
                })),
                _ => String::new(),
            };
            let version_changed = match (&observed.version, &current.version) {
                (Some(a), Some(b)) => a != b && safe_version(a) && safe_version(b),
                _ => false,
            };
            let help_stale = version_changed && current.tools.iter().any(|n| n == "otto_help");
            if added.is_empty() && removed.is_empty() && changed.is_empty() && !help_stale {
                return String::new();
            }
            let (from, version) = match (&observed.version, &current.version) {
                (Some(a), Some(b)) if version_changed => (
                    a.as_str(),
                    format!("; the Otto version changed from {a} to {b}"),
                ),
                _ => ("", String::new()),
            };
            lines.push(format!(
                "The tools or documentation available in this session changed after the earlier messages were written{version}. Those messages may show tools, parameters or documentation that no longer apply."
            ));
            for (label, list) in [
                ("Added", &added),
                ("Removed", &removed),
                ("Changed definitions", &changed),
            ] {
                if !list.is_empty() {
                    lines.push(format!("{label}: {list}"));
                }
            }
            lines.push(USE_ONLY.to_owned());
            if help_stale {
                lines.push(format!(
                    "Earlier otto_help results labelled Otto {from} describe the previous version; call otto_help again before relying on them."
                ));
            }
        }
        None if has_history => {
            let names = listed(current.tools.iter());
            if names.is_empty() {
                return String::new();
            }
            lines.push(
                "The tool set in effect when the earlier messages in this session were written was not recorded. Those messages may show tools or usage that no longer apply."
                    .to_owned(),
            );
            lines.push(format!("Current tools: {names}"));
            lines.push(USE_ONLY.to_owned());
        }
        None => return String::new(),
    }
    format!(
        "<runtime_capability_update>\n{}\n</runtime_capability_update>",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::value::RawValue;

    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn map(pairs: &[(&str, &str)]) -> Option<BTreeMap<String, String>> {
        Some(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn tools(values: &[&str]) -> Capabilities {
        Capabilities {
            tools: names(values),
            ..Capabilities::default()
        }
    }

    fn definition(description: &str, parameters: Option<&str>) -> ToolDefinition {
        ToolDefinition {
            name: "t".into(),
            description: description.into(),
            parameters: parameters.map(|raw| RawValue::from_string(raw.into()).expect("json")),
        }
    }

    #[test]
    fn equal_sets_and_empty_sessions_have_no_notice() {
        let current = tools(&["bash", "read"]);
        assert_eq!(notice(Some(&current), &current, true), "");
        assert_eq!(notice(None, &current, false), "");
    }

    #[test]
    fn changed_notice_lists_added_and_removed_and_omits_empty_lists() {
        let text = notice(
            Some(&tools(&["bash", "web_search"])),
            &tools(&["agent", "agent_status", "bash"]),
            true,
        );
        assert!(text.contains("Added: agent, agent_status\nRemoved: web_search\nUse only"));
        let added_only = notice(Some(&tools(&["bash"])), &tools(&["agent", "bash"]), true);
        assert!(added_only.contains("Added: agent\nUse only"));
        assert!(!added_only.contains("Removed:"));
        assert!(!added_only.contains("otto_help"));
    }

    #[test]
    fn legacy_notice_lists_current_tools() {
        let text = notice(None, &tools(&["bash", "read"]), true);
        assert!(text.starts_with("<runtime_capability_update>\nThe tool set in effect"));
        assert!(text.contains("Current tools: bash, read\nUse only"));
        assert!(text.ends_with("</runtime_capability_update>"));
    }

    #[test]
    fn unsafe_names_are_not_listed() {
        let text = notice(Some(&tools(&["a"])), &tools(&["a", "b c", "d"]), true);
        assert!(text.contains("Added: d\n"));
        assert_eq!(
            notice(Some(&tools(&["a"])), &tools(&["a", "b c"]), true),
            ""
        );
        assert_eq!(notice(None, &tools(&["b c"]), true), "");
    }

    #[test]
    fn changed_definitions_are_listed_for_tools_present_on_both_sides() {
        let observed = Capabilities {
            definitions: map(&[("bash", "1"), ("read", "1"), ("gone", "1")]),
            ..tools(&["bash", "read", "gone"])
        };
        let current = Capabilities {
            definitions: map(&[("bash", "2"), ("read", "1"), ("new", "9")]),
            ..tools(&["bash", "read", "new"])
        };
        let text = notice(Some(&observed), &current, true);
        assert!(text.contains("\nChanged definitions: bash\n"), "{text}");
        assert!(!text.contains("Changed definitions: bash, read"));
        // A record without digests, on either side, is unknown rather than changed.
        let old_format = tools(&["bash", "read"]);
        assert!(!notice(Some(&old_format), &current, true).contains("Changed"));
        assert!(!notice(Some(&observed), &old_format, true).contains("Changed"));
    }

    #[test]
    fn version_change_is_stated_and_marks_otto_help_results_stale() {
        let with = |version: Option<&str>, tools_: &[&str]| Capabilities {
            version: version.map(str::to_owned),
            ..tools(tools_)
        };
        let text = notice(
            Some(&with(Some("0.2.0"), &["otto_help"])),
            &with(Some("0.2.1"), &["otto_help"]),
            true,
        );
        assert!(text.contains("written; the Otto version changed from 0.2.0 to 0.2.1. Those"));
        assert!(text.contains(&format!(
            "\n{USE_ONLY}\nEarlier otto_help results labelled Otto 0.2.0 describe the previous version;"
        )));
        // No otto_help: a version-only change has nothing to say.
        assert_eq!(
            notice(
                Some(&with(Some("0.2.0"), &["a"])),
                &with(Some("0.2.1"), &["a"]),
                true
            ),
            ""
        );
        // Unknown on one side, or unsafe: no phrase and no otto_help line.
        for (old, new) in [
            (None, Some("0.2.1")),
            (Some("0.2.0"), None),
            (Some("a b"), Some("0.2.1")),
        ] {
            let text = notice(
                Some(&with(old, &["otto_help"])),
                &with(new, &["otto_help", "x"]),
                true,
            );
            assert!(!text.contains("version"), "{text}");
            assert!(!text.contains("labelled"), "{text}");
        }
    }

    #[test]
    fn definition_digest_depends_on_description_and_parameters() {
        let base = definition("d", Some(r#"{"a":1}"#));
        assert_eq!(definition_digest(&base), definition_digest(&base.clone()));
        assert_eq!(definition_digest(&base).len(), 16);
        assert_ne!(
            definition_digest(&base),
            definition_digest(&definition("e", Some(r#"{"a":1}"#)))
        );
        assert_ne!(
            definition_digest(&base),
            definition_digest(&definition("d", Some(r#"{"a":2}"#)))
        );
        assert_ne!(
            definition_digest(&base),
            definition_digest(&definition("d", None))
        );
    }
}
