//! The text of the `capability_update` context message that states a change
//! in the session's tool set.
//!
//! The text depends only on the session's last recorded tool names, the
//! current tool names, and whether the session has history; the agent appends
//! the message and writes the record.

use crate::tool::safe_prompt_tool_name;

const USE_ONLY: &str = "Use only the tools defined in the current request. Their descriptions and the policy sections of the system prompt state how to use them.";

/// The notice text, or an empty string when none applies.
///
/// `current` holds the sorted, deduplicated names of the tool definitions
/// sent. Only names that [`safe_prompt_tool_name`] accepts are listed.
pub(super) fn notice(observed: Option<&[String]>, current: &[String], has_history: bool) -> String {
    let listed = |names: Vec<&String>| {
        names
            .into_iter()
            .filter(|name| safe_prompt_tool_name(name))
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = Vec::new();
    match observed {
        Some(observed) if observed == current => return String::new(),
        Some(observed) => {
            lines.push(
                "The tools available in this session changed after the earlier messages were written. Those messages may show tools or usage that no longer apply."
                    .to_owned(),
            );
            let added = listed(current.iter().filter(|n| !observed.contains(n)).collect());
            let removed = listed(observed.iter().filter(|n| !current.contains(n)).collect());
            if !added.is_empty() {
                lines.push(format!("Added: {added}"));
            }
            if !removed.is_empty() {
                lines.push(format!("Removed: {removed}"));
            }
            if added.is_empty() && removed.is_empty() {
                return String::new();
            }
        }
        None if has_history => {
            let names = listed(current.iter().collect());
            if names.is_empty() {
                return String::new();
            }
            lines.push(
                "The tool set in effect when the earlier messages in this session were written was not recorded. Those messages may show tools or usage that no longer apply."
                    .to_owned(),
            );
            lines.push(format!("Current tools: {names}"));
        }
        None => return String::new(),
    }
    lines.push(USE_ONLY.to_owned());
    format!(
        "<runtime_capability_update>\n{}\n</runtime_capability_update>",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn equal_sets_and_empty_sessions_have_no_notice() {
        let current = names(&["bash", "read"]);
        assert_eq!(notice(Some(&current), &current, true), "");
        assert_eq!(notice(None, &current, false), "");
    }

    #[test]
    fn changed_notice_lists_added_and_removed_and_omits_empty_lists() {
        let text = notice(
            Some(&names(&["bash", "web_search"])),
            &names(&["agent", "agent_wait", "bash"]),
            true,
        );
        assert!(text.contains("Added: agent, agent_wait\nRemoved: web_search\nUse only"));
        let added_only = notice(Some(&names(&["bash"])), &names(&["agent", "bash"]), true);
        assert!(added_only.contains("Added: agent\nUse only"));
        assert!(!added_only.contains("Removed:"));
    }

    #[test]
    fn legacy_notice_lists_current_tools() {
        let text = notice(None, &names(&["bash", "read"]), true);
        assert!(text.starts_with("<runtime_capability_update>\nThe tool set in effect"));
        assert!(text.contains("Current tools: bash, read\nUse only"));
        assert!(text.ends_with("</runtime_capability_update>"));
    }

    #[test]
    fn unsafe_names_are_not_listed() {
        let text = notice(Some(&names(&["a"])), &names(&["a", "b c", "d"]), true);
        assert!(text.contains("Added: d\n"));
        assert_eq!(
            notice(Some(&names(&["a"])), &names(&["a", "b c"]), true),
            ""
        );
        assert_eq!(notice(None, &names(&["b c"]), true), "");
    }
}
