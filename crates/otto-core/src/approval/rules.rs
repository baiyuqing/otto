//! Pure explicit policy matching. This parses argv; it never runs a shell.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_prefix: Option<Vec<String>>,
    pub decision: RuleDecision,
}

impl ApprovalRule {
    pub fn validate(&self) -> Result<(), &'static str> {
        if usize::from(self.source.is_some())
            + usize::from(self.tool.is_some())
            + usize::from(self.command_prefix.is_some())
            != 1
        {
            return Err("approval rule requires exactly one selector");
        }
        if self
            .source
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.chars().any(char::is_control))
            || self
                .tool
                .as_ref()
                .is_some_and(|value| value.trim().is_empty() || value.chars().any(char::is_control))
            || self.command_prefix.as_ref().is_some_and(|argv| {
                argv.is_empty() || argv.iter().any(|arg| arg.is_empty() || arg.contains('\0'))
            })
        {
            return Err("approval rule selector must not be empty");
        }
        Ok(())
    }
}

/// Extract a single command's literal argv. Shell expansions, assignments,
/// control syntax, redirections and substitutions fail closed.
pub fn command_argv(command: &str) -> Option<Vec<String>> {
    if !crate::config::sandbox::is_simple_command(command) || command.contains(['\n', '\r', '\0']) {
        return None;
    }
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for c in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if single {
            single = c != '\'';
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => double = !double,
            '\'' if !double => single = true,
            '*' | '?' | '[' | ']' | '{' | '}' | '~' if !double => return None,
            _ => {}
        }
    }
    let argv = shlex::split(command)?;
    let program = argv.first()?;
    let basename = program.rsplit('/').next()?;
    if program.is_empty()
        || (basename != "git" && crate::config::sandbox::COMMAND_RUNNERS.contains(&basename))
        || program.contains('=')
        || matches!(
            program.as_str(),
            "!" | "if"
                | "then"
                | "else"
                | "elif"
                | "fi"
                | "for"
                | "while"
                | "until"
                | "do"
                | "done"
                | "case"
                | "esac"
                | "function"
                | "select"
                | "time"
                | "coproc"
                | "[["
                | "]]"
        )
    {
        return None;
    }
    Some(argv)
}

/// Match literal source/tool identities or an argv prefix. Stronger decisions
/// win regardless of configuration order: deny, then ask, then allow.
/// Invalid rules cannot grant authority, even for callers that skipped validation.
pub fn evaluate_rules(
    rules: &[ApprovalRule],
    source: &str,
    tool: &str,
    command: Option<&str>,
) -> Option<RuleDecision> {
    let argv = command.and_then(command_argv);
    let mut result = None;
    for rule in rules {
        if rule.validate().is_err() {
            continue;
        }
        if rule.decision == RuleDecision::Allow && command.is_some() && argv.is_none() {
            continue;
        }
        let matched = rule.source.as_deref() == Some(source)
            || rule.tool.as_deref() == Some(tool)
            || rule
                .command_prefix
                .as_ref()
                .is_some_and(|prefix| argv.as_ref().is_some_and(|argv| argv.starts_with(prefix)));
        if matched {
            result = match (result, rule.decision) {
                (_, RuleDecision::Deny) | (Some(RuleDecision::Deny), _) => Some(RuleDecision::Deny),
                (_, RuleDecision::Ask) | (Some(RuleDecision::Ask), _) => Some(RuleDecision::Ask),
                _ => Some(RuleDecision::Allow),
            };
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[test]
    fn argv_preserves_quotes_and_rejects_shell_effects() {
        assert_eq!(
            command_argv("git commit -m 'two words'"),
            Some(vec![
                "git".into(),
                "commit".into(),
                "-m".into(),
                "two words".into()
            ])
        );
        assert_eq!(
            command_argv("echo \"literal *\" '$HOME'"),
            Some(vec!["echo".into(), "literal *".into(), "$HOME".into()])
        );
        for command in [
            "git status && rm x",
            "git status; rm x",
            "git status | cat",
            "echo $(id)",
            "echo `id`",
            "echo $HOME",
            "echo \"$HOME\"",
            "echo *",
            "echo ~",
            "echo {a,b}",
            "echo a > b",
            "X=y git status",
            "git 'status",
            "git status\necho x",
            "if true",
            "bash -lc 'git status && rm x'",
            "sh -c 'git status; rm x'",
            "env bash -lc 'git status && rm x'",
            "/bin/bash -lc 'git status && rm x'",
            "",
        ] {
            assert_eq!(command_argv(command), None, "{command}");
        }
    }

    fn prefix(decision: RuleDecision) -> ApprovalRule {
        ApprovalRule {
            source: None,
            tool: None,
            command_prefix: Some(vec!["git".into(), "status".into()]),
            decision,
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[test]
    fn prefixes_match_argv_and_precedence_is_order_independent() {
        let allow = prefix(RuleDecision::Allow);
        assert_eq!(
            evaluate_rules(
                std::slice::from_ref(&allow),
                "bash",
                "bash",
                Some("git 'status' --short")
            ),
            Some(RuleDecision::Allow)
        );
        for command in [
            "git status-other",
            "git status && echo x",
            "git status $X",
            "git diff",
        ] {
            assert_eq!(
                evaluate_rules(std::slice::from_ref(&allow), "bash", "bash", Some(command)),
                None
            );
        }
        for rules in [
            vec![allow.clone(), prefix(RuleDecision::Ask)],
            vec![prefix(RuleDecision::Ask), allow.clone()],
        ] {
            assert_eq!(
                evaluate_rules(&rules, "bash", "bash", Some("git status")),
                Some(RuleDecision::Ask)
            );
        }
        assert_eq!(
            evaluate_rules(
                &[prefix(RuleDecision::Deny), allow, prefix(RuleDecision::Ask)],
                "bash",
                "bash",
                Some("git status")
            ),
            Some(RuleDecision::Deny)
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[test]
    fn identities_are_exact_and_invalid_rules_cannot_allow() {
        let mut rule = ApprovalRule {
            source: Some("sandbox".into()),
            tool: None,
            command_prefix: None,
            decision: RuleDecision::Allow,
        };
        assert_eq!(
            evaluate_rules(&[rule.clone()], "sandbox-other", "sandbox", None),
            None
        );
        assert_eq!(
            evaluate_rules(&[rule.clone()], "sandbox", "bash", None),
            Some(RuleDecision::Allow)
        );
        assert_eq!(
            evaluate_rules(&[rule.clone()], "sandbox", "bash", Some("echo $HOME")),
            None
        );
        rule.tool = Some("bash".into());
        assert!(rule.validate().is_err());
        assert_eq!(evaluate_rules(&[rule], "sandbox", "bash", None), None);
        assert!(
            ApprovalRule {
                source: None,
                tool: None,
                command_prefix: Some(vec![]),
                decision: RuleDecision::Allow
            }
            .validate()
            .is_err()
        );
        for value in ["sandbox\0", "sandbox\n", "bash\t"] {
            assert!(
                ApprovalRule {
                    source: Some(value.into()),
                    tool: None,
                    command_prefix: None,
                    decision: RuleDecision::Allow
                }
                .validate()
                .is_err()
            );
            assert!(
                ApprovalRule {
                    source: None,
                    tool: Some(value.into()),
                    command_prefix: None,
                    decision: RuleDecision::Allow
                }
                .validate()
                .is_err()
            );
        }
    }
}
