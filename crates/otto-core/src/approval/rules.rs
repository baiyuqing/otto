//! Pure explicit policy matching over pre-parsed commands; it never runs a shell.
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

/// Static view of a bash command, produced by the native shell parser.
/// otto-core only evaluates rules against it; it never parses shell text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShellCommand {
    /// One entry per simple command found anywhere in the program, in
    /// source order, including commands inside substitutions and compound
    /// statements. Each entry is the command's leading static words: the
    /// unquoted value of every word up to the first word that is not static.
    pub segments: Vec<Vec<String>>,
    /// True only when the whole program is a list of pipelines of simple
    /// commands whose every word is static, with no syntax outside the
    /// accepted set.
    pub complete: bool,
}

/// Match literal source/tool identities or a command prefix. An allow prefix
/// needs a complete command in which every segment starts with the prefix;
/// deny and ask prefixes match any segment. Stronger decisions win
/// regardless of configuration order: deny, then ask, then allow.
/// Invalid rules cannot grant authority, even for callers that skipped validation.
pub fn evaluate_rules(
    rules: &[ApprovalRule],
    source: &str,
    tool: &str,
    command: Option<&ShellCommand>,
) -> Option<RuleDecision> {
    let mut result = None;
    for rule in rules {
        if rule.validate().is_err() {
            continue;
        }
        if rule.decision == RuleDecision::Allow && command.is_some_and(|command| !command.complete)
        {
            continue;
        }
        let matched = rule.source.as_deref() == Some(source)
            || rule.tool.as_deref() == Some(tool)
            || rule.command_prefix.as_ref().is_some_and(|prefix| {
                command.is_some_and(|command| {
                    if rule.decision == RuleDecision::Allow {
                        !command.segments.is_empty()
                            && command
                                .segments
                                .iter()
                                .all(|words| words.starts_with(prefix))
                    } else {
                        command
                            .segments
                            .iter()
                            .any(|words| words.starts_with(prefix))
                    }
                })
            });
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

    fn command(complete: bool, segments: &[&[&str]]) -> ShellCommand {
        ShellCommand {
            segments: segments
                .iter()
                .map(|words| words.iter().map(|word| word.to_string()).collect())
                .collect(),
            complete,
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

    fn eval(rules: &[ApprovalRule], command: &ShellCommand) -> Option<RuleDecision> {
        evaluate_rules(rules, "bash", "bash", Some(command))
    }

    #[test]
    fn allow_prefix_needs_complete_command_with_prefix_in_every_segment() {
        let allow = [prefix(RuleDecision::Allow)];
        let status = ["git", "status"].as_slice();
        assert_eq!(
            eval(
                &allow,
                &command(true, &[status, &["git", "status", "--short"]])
            ),
            Some(RuleDecision::Allow)
        );
        for rejected in [
            command(true, &[status, &["rm", "x"]]),
            command(true, &[&["git", "status-other"]]),
            command(true, &[&["git", "diff"]]),
            command(true, &[]),
            command(false, &[status]),
        ] {
            assert_eq!(eval(&allow, &rejected), None, "{rejected:?}");
        }
    }

    #[test]
    fn deny_and_ask_prefix_match_any_segment_even_when_incomplete() {
        let incomplete = command(false, &[&["echo", "x"], &["git", "status", "origin"]]);
        for decision in [RuleDecision::Deny, RuleDecision::Ask] {
            assert_eq!(eval(&[prefix(decision)], &incomplete), Some(decision));
            assert_eq!(
                eval(&[prefix(decision)], &command(true, &[&["git", "diff"]])),
                None
            );
        }
    }

    #[test]
    fn precedence_is_order_independent() {
        let allow = prefix(RuleDecision::Allow);
        let status = command(true, &[&["git", "status"]]);
        for rules in [
            vec![allow.clone(), prefix(RuleDecision::Ask)],
            vec![prefix(RuleDecision::Ask), allow.clone()],
        ] {
            assert_eq!(eval(&rules, &status), Some(RuleDecision::Ask));
        }
        assert_eq!(
            eval(
                &[prefix(RuleDecision::Deny), allow, prefix(RuleDecision::Ask)],
                &status
            ),
            Some(RuleDecision::Deny)
        );
    }

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
            evaluate_rules(
                &[rule.clone()],
                "sandbox",
                "bash",
                Some(&command(false, &[&["echo"]]))
            ),
            None
        );
        assert_eq!(
            evaluate_rules(
                &[rule.clone()],
                "sandbox",
                "bash",
                Some(&command(true, &[&["echo"]]))
            ),
            Some(RuleDecision::Allow)
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
