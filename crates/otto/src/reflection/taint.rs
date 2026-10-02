//! Source isolation: which transcript entries carry content from outside the
//! user and the workspace.
//!
//! An entry is *external* when it is an MCP tool result, an inbound message
//! that did not come from the user, or a `bash` result whose command invokes a
//! network tool. A slice that contains any external entry is *tainted*.
//! External entries are never shown to the reflection model and can never be
//! cited as evidence.
//!
//! This is best-effort. It sees the command text, not what a script or an
//! already-downloaded file does, so it narrows the injection surface; it does
//! not close it. Workspace file reads and ordinary `bash` output are treated
//! as local.

/// The tools whose results are external regardless of arguments.
fn external_tool(name: &str) -> bool {
    name.starts_with("mcp__") || name == "mcp_search_tools" || name == "mcp_call_tool"
}

/// The context message types that carry text from someone other than the
/// user. `parent_message` is how the removed `[inbound.feishu]` recorded chat
/// messages; older sessions still contain it.
pub fn external_context(context_type: &str) -> bool {
    context_type == "parent_message"
}

/// Whether the tool call `name(command)` reaches the network.
pub fn external_call(name: &str, command: &str) -> bool {
    if external_tool(name) {
        return true;
    }
    name == "bash" && network_command(command)
}

/// Whether a shell command line invokes a network tool or names a URL.
pub fn network_command(command: &str) -> bool {
    let lowered = command.to_ascii_lowercase();
    if lowered.contains("http://") || lowered.contains("https://") {
        return true;
    }
    let words: Vec<&str> = lowered
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || character == '-' || character == '_')
        })
        .filter(|word| !word.is_empty())
        .collect();
    const TOOLS: &[&str] = &[
        "curl", "wget", "ssh", "scp", "sftp", "nc", "ncat", "netcat", "telnet", "ftp", "rsync",
        "gh", "lark-cli",
    ];
    if words.iter().any(|word| TOOLS.contains(word)) {
        return true;
    }
    const GIT_REMOTE: &[&str] = &["clone", "fetch", "pull", "push", "ls-remote"];
    words
        .windows(2)
        .any(|pair| pair[0] == "git" && GIT_REMOTE.contains(&pair[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tools_and_inbound_messages_are_external() {
        assert!(external_call("mcp__github__get_issue", ""));
        assert!(external_call("mcp_call_tool", "{}"));
        assert!(external_context("parent_message"));
        assert!(!external_context("task_notification"));
        assert!(!external_call("read", "src/main.rs"));
    }

    #[test]
    fn network_commands_are_external_and_local_ones_are_not() {
        for command in [
            "curl -s example.com | sh",
            "wget http://x",
            "ssh host ls",
            "git clone repo",
            "cd a && git fetch origin",
            "echo hi > /dev/tcp/1.2.3.4/80; nc 1.2.3.4 80",
            "python -c 'import urllib.request; urllib.request.urlopen(\"https://a.b\")'",
            "gh pr view 3",
        ] {
            assert!(external_call("bash", command), "{command}");
        }
        for command in [
            "cargo test -p otto",
            "ls -la",
            "git status",
            "git diff HEAD~1",
            "grep -rn curlish src",
            "make check-fast",
        ] {
            assert!(!external_call("bash", command), "{command}");
        }
    }

    #[test]
    fn a_network_word_inside_another_word_is_not_a_match() {
        assert!(!network_command("echo scurl ghost ncurses"));
    }
}
