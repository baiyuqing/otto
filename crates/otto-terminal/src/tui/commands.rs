//! Completion and parsing use the feature entry catalog.
#[cfg(test)]
use super::features::Catalog;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Completion {
    pub replacement: String,
    pub description: String,
}
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
}
#[cfg(test)]
impl From<SlashCommand> for Completion {
    fn from(c: SlashCommand) -> Self {
        Self {
            replacement: c.name.into(),
            description: c.description.into(),
        }
    }
}
#[cfg(test)]
pub(crate) fn matching_slash_commands(value: &str) -> Vec<SlashCommand> {
    if !value.starts_with('/') || value.contains(['\r', '\n']) {
        return Vec::new();
    }
    Catalog::default()
        .entries()
        .iter()
        .filter(|c| c.name.starts_with(value))
        .map(|c| SlashCommand {
            name: c.name,
            description: c.description,
        })
        .collect()
}
#[cfg(test)]
pub(crate) fn find_slash_command(name: &str) -> Option<SlashCommand> {
    Catalog::default()
        .entries()
        .iter()
        .find(|c| c.name == name)
        .map(|c| SlashCommand {
            name: c.name,
            description: c.description,
        })
}
#[cfg(test)]
pub(crate) fn parse_slash_command(value: &str) -> Option<(SlashCommand, String)> {
    let (name, args) = value
        .trim()
        .split_once(char::is_whitespace)
        .map(|(n, a)| (n, a.trim()))
        .unwrap_or((value.trim(), ""));
    find_slash_command(name).map(|c| (c, args.into()))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_matches_only_commands_starting_with_it() {
        let names: Vec<&str> = matching_slash_commands("/s")
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["/skill", "/sandbox", "/session"]);
    }

    #[test]
    fn a_prefix_matches_the_clear_command() {
        let names: Vec<&str> = matching_slash_commands("/cl")
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["/clear"]);
    }

    #[test]
    fn a_value_without_a_leading_slash_matches_nothing() {
        assert!(matching_slash_commands("help").is_empty());
    }

    #[test]
    fn a_value_with_embedded_newlines_matches_nothing() {
        assert!(matching_slash_commands("/help\n/exit").is_empty());
    }

    #[test]
    fn an_empty_prefix_matches_nothing_since_it_has_no_leading_slash() {
        assert!(matching_slash_commands("").is_empty());
    }

    #[test]
    fn a_prefix_matches_only_the_memory_command() {
        let names: Vec<&str> = matching_slash_commands("/mem")
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["/memory"]);
    }

    #[test]
    fn a_prefix_matches_only_the_timers_command() {
        let names: Vec<&str> = matching_slash_commands("/ti")
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["/timers"]);
    }

    #[test]
    fn a_prefix_matches_only_the_mcp_command() {
        let names: Vec<&str> = matching_slash_commands("/mc")
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["/mcp"]);
    }

    #[test]
    fn a_prefix_matches_the_skill_command() {
        let names: Vec<&str> = matching_slash_commands("/ski")
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["/skill"]);
    }

    #[test]
    fn the_full_table_matches_the_bare_slash() {
        assert_eq!(
            matching_slash_commands("/").len(),
            Catalog::default().entries().len()
        );
    }

    #[test]
    fn task_is_the_only_discovered_task_command() {
        let names: Vec<&str> = matching_slash_commands("/task")
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["/task"]);
        assert!(find_slash_command("/tasks").is_none());
    }

    #[test]
    fn exit_is_the_only_discovered_exit_command() {
        assert_eq!(
            matching_slash_commands("/ex")
                .iter()
                .map(|c| c.name)
                .collect::<Vec<_>>(),
            ["/exit"]
        );
        assert!(matching_slash_commands("/qu").is_empty());
        assert!(find_slash_command("/quit").is_none());
    }

    #[test]
    fn parsing_splits_name_from_a_trimmed_argument() {
        let (command, argument) = parse_slash_command("  /task  t7  ").expect("known command");
        assert_eq!(command.name, "/task");
        assert_eq!(argument, "t7");
    }

    #[test]
    fn parsing_a_bare_command_yields_an_empty_argument() {
        let (command, argument) = parse_slash_command("/help").expect("known command");
        assert_eq!(command.name, "/help");
        assert_eq!(argument, "");
    }

    #[test]
    fn init_is_a_known_command() {
        let (command, argument) = parse_slash_command("/init").expect("known command");
        assert!(!command.name.is_empty());
        assert_eq!(argument, "");
    }

    #[test]
    fn parsing_an_unknown_command_yields_none() {
        assert!(parse_slash_command("/nope").is_none());
    }

    #[test]
    fn find_looks_up_by_exact_name() {
        assert_eq!(find_slash_command("/exit").map(|c| c.name), Some("/exit"));
        assert!(find_slash_command("/ex").is_none());
    }
}
