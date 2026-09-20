//! The slash-command table and completion logic. Port of
//! `internal/tui/commands.go`.
//!
//! `/memory` and `/remember` are listed here matching Go's table exactly,
//! including the `/task` singular quirk noted below. `tui::app`'s dispatcher
//! backs both with `cli::repl_commands`'s free functions, the same code the
//! line-oriented REPL uses.

/// One entry in the slash-command table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub kind: SlashCommandKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandKind {
    Help,
    Session,
    New,
    Clear,
    Model,
    Thinking,
    Resume,
    Archive,
    Rename,
    Compact,
    Image,
    Memory,
    Remember,
    Login,
    Logout,
    Exit,
    Tasks,
    Task,
    Timers,
    Skill,
    Skills,
    Sandbox,
    Approve,
    Mcp,
}

/// The command table used by completion and `/help`. The inherited commands
/// keep Go's order; `/approve` sits beside the sandbox command it controls.
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        description: "show help",
        kind: SlashCommandKind::Help,
    },
    SlashCommand {
        name: "/session",
        description: "show session details",
        kind: SlashCommandKind::Session,
    },
    SlashCommand {
        name: "/new",
        description: "start a new session",
        kind: SlashCommandKind::New,
    },
    SlashCommand {
        name: "/clear",
        description: "start a new session",
        kind: SlashCommandKind::Clear,
    },
    SlashCommand {
        name: "/model",
        description: "show current model, or switch profiles (fresh session)",
        kind: SlashCommandKind::Model,
    },
    SlashCommand {
        name: "/thinking",
        description: "show or set reasoning effort",
        kind: SlashCommandKind::Thinking,
    },
    SlashCommand {
        name: "/resume",
        description: "resume a session",
        kind: SlashCommandKind::Resume,
    },
    SlashCommand {
        name: "/archive",
        description: "archive a session",
        kind: SlashCommandKind::Archive,
    },
    SlashCommand {
        name: "/rename",
        description: "rename the current session",
        kind: SlashCommandKind::Rename,
    },
    SlashCommand {
        name: "/compact",
        description: "compact context",
        kind: SlashCommandKind::Compact,
    },
    SlashCommand {
        name: "/image",
        description: "attach an image to the next prompt",
        kind: SlashCommandKind::Image,
    },
    SlashCommand {
        name: "/sandbox",
        description: "show sandbox state, or reload the [sandbox] configuration",
        kind: SlashCommandKind::Sandbox,
    },
    SlashCommand {
        name: "/approve",
        description: "allow one exact elevated Bash command",
        kind: SlashCommandKind::Approve,
    },
    SlashCommand {
        name: "/memory",
        description: "search, forget, or review remembered records",
        kind: SlashCommandKind::Memory,
    },
    SlashCommand {
        name: "/remember",
        description: "remember a fact for later",
        kind: SlashCommandKind::Remember,
    },
    SlashCommand {
        name: "/login",
        description: "sign in to ChatGPT (add status to check)",
        kind: SlashCommandKind::Login,
    },
    SlashCommand {
        name: "/logout",
        description: "sign out of ChatGPT",
        kind: SlashCommandKind::Logout,
    },
    SlashCommand {
        name: "/mcp",
        description: "show MCP server status, or 'login <server>' to sign in",
        kind: SlashCommandKind::Mcp,
    },
    SlashCommand {
        name: "/exit",
        description: "quit",
        kind: SlashCommandKind::Exit,
    },
    SlashCommand {
        name: "/tasks",
        description: "list sub-agent tasks",
        kind: SlashCommandKind::Tasks,
    },
    SlashCommand {
        name: "/task",
        description: "show or cancel a sub-agent task",
        kind: SlashCommandKind::Task,
    },
    SlashCommand {
        name: "/timers",
        description: "list or cancel this session's timers",
        kind: SlashCommandKind::Timers,
    },
    SlashCommand {
        name: "/skill",
        description: "show a skill by name",
        kind: SlashCommandKind::Skill,
    },
    SlashCommand {
        name: "/skills",
        description: "list available skills",
        kind: SlashCommandKind::Skills,
    },
];

/// Every table entry whose name starts with `value`. Port of
/// `matchingSlashCommands`: `value` must start with `/` and contain no CR/LF
/// (a pasted multi-line value is never a command prefix).
pub fn matching_slash_commands(value: &str) -> Vec<SlashCommand> {
    if !value.starts_with('/') || value.contains(['\r', '\n']) {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|command| command.name.starts_with(value))
        .collect()
}

/// Looks up a command by its exact name (including the leading `/`).
pub fn find_slash_command(name: &str) -> Option<SlashCommand> {
    SLASH_COMMANDS
        .iter()
        .copied()
        .find(|command| command.name == name)
}

/// Splits `value` into a command name and the rest of the line, then looks
/// the name up. Port of `parseSlashCommand`.
pub fn parse_slash_command(value: &str) -> Option<(SlashCommand, String)> {
    let value = value.trim();
    let index = value.find(char::is_whitespace);
    let (name, argument) = match index {
        Some(index) => (&value[..index], value[index..].trim()),
        None => (value, ""),
    };
    find_slash_command(name).map(|command| (command, argument.to_string()))
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
        assert_eq!(names, ["/session", "/sandbox", "/skill", "/skills"]);
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

    /// Port of the completion half of Go's
    /// `TestMemoryCommandRegistryCompletionAndHelp`.
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
    fn a_prefix_matches_the_skill_commands() {
        let names: Vec<&str> = matching_slash_commands("/ski")
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["/skill", "/skills"]);
    }

    #[test]
    fn the_full_table_matches_the_bare_slash() {
        assert_eq!(matching_slash_commands("/").len(), SLASH_COMMANDS.len());
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
    fn parsing_an_unknown_command_yields_none() {
        assert!(parse_slash_command("/nope").is_none());
    }

    #[test]
    fn find_looks_up_by_exact_name() {
        assert_eq!(
            find_slash_command("/exit").map(|c| c.kind),
            Some(SlashCommandKind::Exit)
        );
        assert!(find_slash_command("/ex").is_none());
    }
}
