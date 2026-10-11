//! Static analysis of bash command text with tree-sitter-bash.
//!
//! One parse feeds approval rules ([`analyze`]) and review facts ([`parse`]).
//! Nothing here runs a shell or expands a word: a word is static only when
//! its value follows from the quoting alone.

use std::path::Path;

use otto_core::approval::ShellCommand;
use otto_core::config::sandbox::COMMAND_RUNNERS;
use tree_sitter::{Node, Parser};

/// Commands longer than this are not parsed.
const MAX_COMMAND_BYTES: usize = 64 * 1024;

/// Node kinds that keep [`ShellCommand::complete`] true. Words are checked
/// separately by [`word_value`]; redirects by [`accepted_redirect`].
const ACCEPTED_NAMED: &[&str] = &[
    "program",
    "list",
    "pipeline",
    "command",
    "command_name",
    "redirected_statement",
    "file_redirect",
    "file_descriptor",
    "word",
    "number",
    "raw_string",
    "string",
    "string_content",
    "concatenation",
];
const ACCEPTED_TOKENS: &[&str] = &["&&", "||", ";", "|", "\"", "\n"];

const KEYWORDS: &[&str] = &[
    "!", "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "time", "coproc", "[[", "]]",
];

/// Target of an output redirect (`>`, `>>`, `>|`, `&>`, `&>>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTarget {
    /// Unquoted value when `is_static`, otherwise the source text.
    pub path: String,
    pub is_static: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub command: ShellCommand,
    pub write_targets: Vec<WriteTarget>,
    /// True when the program assigns a shell variable anywhere (`A=1 cmd`,
    /// `A=1;`, `export A=1`). Such a command can redirect `git` through
    /// `GIT_DIR` or `GIT_WORK_TREE`.
    pub assigns_variables: bool,
}

pub fn analyze(command: &str) -> ShellCommand {
    parse(command).command
}

/// Parse `command`. Input over 64 KiB or a parser failure returns the empty,
/// incomplete result.
pub fn parse(command: &str) -> Parsed {
    if command.len() > MAX_COMMAND_BYTES {
        return Parsed::default();
    }
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return Parsed::default();
    }
    let Some(tree) = parser.parse(command, None) else {
        return Parsed::default();
    };
    let root = tree.root_node();
    let source = command.as_bytes();
    let mut parsed = Parsed::default();
    let mut complete = !root.has_error();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        let accepted = if node.is_named() {
            ACCEPTED_NAMED.contains(&kind)
        } else {
            ACCEPTED_TOKENS.contains(&kind)
        };
        complete &= accepted;
        let mut descend = true;
        match kind {
            "variable_assignment" => parsed.assigns_variables = true,
            "command" => {
                let (words, all_static) = command_words(node, source);
                complete &= all_static && program_allowed(&words);
                if !words.is_empty() {
                    parsed.command.segments.push(words);
                }
            }
            "file_redirect" => {
                if matches!(
                    redirect_operator(node),
                    Some(">" | ">>" | ">|" | "&>" | "&>>")
                ) && let Some(destination) = node.child_by_field_name("destination")
                {
                    let value = word_value(destination, source, true);
                    parsed.write_targets.push(WriteTarget {
                        is_static: value.is_some(),
                        path: value.unwrap_or_else(|| node_text(destination, source).into()),
                    });
                }
                if accepted_redirect(node, source) {
                    descend = false;
                } else {
                    complete = false;
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        // `command_words` joins continued words; elsewhere bash would join
        // tokens the tree keeps apart (`>/dev/null\<newline>x`).
        if kind != "command"
            && children
                .windows(2)
                .any(|pair| continued(source, pair[0].end_byte(), pair[1].start_byte()))
        {
            complete = false;
        }
        if descend {
            stack.extend(children.into_iter().rev());
        }
    }
    parsed.command.complete = complete;
    parsed
}

/// Leading static words of a `command` node and whether every word was
/// static. Assignments and redirects are not words. Words separated only by
/// line continuations are one word, as in bash.
fn command_words(node: Node, source: &[u8]) -> (Vec<String>, bool) {
    let mut words: Vec<String> = Vec::new();
    // End byte of the previous child and whether it was a word.
    let mut previous: Option<(usize, bool)> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let is_word = !matches!(
            child.kind(),
            "variable_assignment" | "file_redirect" | "herestring_redirect" | "heredoc_redirect"
        );
        let joined = previous.is_some_and(|(end, _)| continued(source, end, child.start_byte()));
        let after_word = previous.is_some_and(|(_, word)| word);
        previous = Some((child.end_byte(), is_word));
        if joined && !(is_word && after_word) {
            // bash joins the assignment or redirect with the adjacent word.
            return (words, false);
        }
        if !is_word {
            continue;
        }
        let Some(word) = word_value(child, source, !joined) else {
            if joined {
                words.pop();
            }
            return (words, false);
        };
        match words.last_mut() {
            Some(last) if joined => last.push_str(&word),
            _ => words.push(word),
        }
    }
    (words, true)
}

/// True when only line continuations (backslash-newline) lie between two
/// nodes. tree-sitter-bash skips a continuation as whitespace, but bash
/// removes it before splitting words, so `rm -\<newline>rf` runs `rm -rf`.
fn continued(source: &[u8], end: usize, start: usize) -> bool {
    let mut gap = source.get(end..start).unwrap_or_default();
    if gap.is_empty() {
        return false;
    }
    while !gap.is_empty() {
        gap = match gap
            .strip_prefix(b"\\\n")
            .or_else(|| gap.strip_prefix(b"\\\r\n"))
        {
            Some(rest) => rest,
            None => return false,
        };
    }
    true
}

/// False for programs that run another command, assignments and keywords.
fn program_allowed(words: &[String]) -> bool {
    let Some(program) = words.first() else {
        return true;
    };
    let basename = program.rsplit('/').next().unwrap_or_default();
    !(program.is_empty()
        || (basename != "git" && COMMAND_RUNNERS.contains(&basename))
        || program.contains('=')
        || KEYWORDS.contains(&program.as_str()))
}

fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or_default()
}

fn redirect_operator<'a>(node: Node<'a>) -> Option<&'a str> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| !child.is_named())
        .map(|child| child.kind())
}

/// `2>&1`, `>/dev/null`, `2>/dev/null` and `&>/dev/null`.
fn accepted_redirect(node: Node, source: &[u8]) -> bool {
    let descriptor = node
        .child_by_field_name("descriptor")
        .map(|descriptor| node_text(descriptor, source));
    let destination = node
        .child_by_field_name("destination")
        .and_then(|destination| word_value(destination, source, true));
    matches!(
        (descriptor, redirect_operator(node), destination.as_deref()),
        (None | Some("1" | "2"), Some(">"), Some("/dev/null"))
            | (None, Some("&>"), Some("/dev/null"))
            | (Some("2"), Some(">&"), Some("1"))
    )
}

/// Unquoted value of a static word node, or `None` for anything whose value
/// depends on expansion, globbing or tilde expansion. `first` is true when
/// the node starts the shell word.
fn word_value(node: Node, source: &[u8], first: bool) -> Option<String> {
    let text = node.utf8_text(source).ok()?;
    match node.kind() {
        "command_name" => word_value(node.child(0)?, source, first),
        "word" | "number" => unquote_bare(text, first),
        "raw_string" => Some(text.get(1..text.len().checked_sub(1)?)?.to_owned()),
        "string" => {
            let mut cursor = node.walk();
            if !node
                .children(&mut cursor)
                .all(|child| matches!(child.kind(), "\"" | "string_content"))
            {
                return None;
            }
            unquote_double(text.get(1..text.len().checked_sub(1)?)?)
        }
        "concatenation" => {
            let mut cursor = node.walk();
            let mut value = String::new();
            for (index, child) in node.children(&mut cursor).enumerate() {
                value.push_str(&word_value(child, source, first && index == 0)?);
            }
            Some(value)
        }
        _ => None,
    }
}

/// Unquoted text: a backslash escapes the next character and
/// backslash-newline is removed.
fn unquote_bare(text: &str, first: bool) -> Option<String> {
    let mut value = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                '\n' => {}
                escaped => value.push(escaped),
            },
            '*' | '?' | '[' | ']' | '{' | '}' => return None,
            '~' if first && value.is_empty() => return None,
            _ => value.push(c),
        }
    }
    Some(value)
}

/// Text between double quotes: a backslash escapes only `$`, `` ` ``, `"`,
/// `\` and newline.
fn unquote_double(text: &str) -> Option<String> {
    let mut value = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            value.push(c);
            continue;
        }
        match chars.peek() {
            Some('\n') => {
                chars.next();
            }
            Some(&escaped @ ('$' | '`' | '"' | '\\')) => {
                chars.next();
                value.push(escaped);
            }
            _ => value.push('\\'),
        }
    }
    Some(value)
}

/// True when `word` names `program`, with or without a directory.
pub fn is_program(word: &str, program: &str) -> bool {
    Path::new(word)
        .file_name()
        .is_some_and(|name| name == program)
}

/// Index and name of the git subcommand in `words`, skipping the global
/// options `-C <dir>`, `-c <k=v>`, `--no-pager`, `--git-dir=<p>` and
/// `--work-tree=<p>`. `None` when `words` is not a git command, has an
/// unrecognized option before the subcommand, or has no subcommand.
pub fn git_subcommand(words: &[String]) -> Option<(usize, &str)> {
    if !is_program(words.first()?, "git") {
        return None;
    }
    let mut index = 1;
    while let Some(word) = words.get(index) {
        match word.as_str() {
            "-C" | "-c" => index += 2,
            "--no-pager" => index += 1,
            option if option.starts_with("--git-dir=") || option.starts_with("--work-tree=") => {
                index += 1
            }
            option if option.starts_with('-') => return None,
            subcommand => return Some((index, subcommand)),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(command: &str) -> Vec<Vec<String>> {
        analyze(command).segments
    }

    fn check(command: &str, complete: bool, expected: &[&[&str]]) {
        let analysis = analyze(command);
        let expected: Vec<Vec<String>> = expected
            .iter()
            .map(|words| words.iter().map(|word| word.to_string()).collect())
            .collect();
        assert_eq!(analysis.segments, expected, "{command:?}");
        assert_eq!(analysis.complete, complete, "{command:?}");
    }

    #[test]
    fn quoting_and_escapes_follow_bash() {
        check(
            "git commit -m 'two words'",
            true,
            &[&["git", "commit", "-m", "two words"]],
        );
        check(
            r#"echo "a\"b" 'c\d' e\ f"#,
            true,
            &[&["echo", "a\"b", "c\\d", "e f"]],
        );
        check(
            r#"echo "\$HOME" "\\" "a\`b" "a\nb""#,
            true,
            &[&["echo", "$HOME", "\\", "a`b", "a\\nb"]],
        );
        check(
            "echo \"literal *\" '$HOME' a\"b c\"'d' '' a~",
            true,
            &[&["echo", "literal *", "$HOME", "ab cd", "", "a~"]],
        );
        check(r#""git" status"#, true, &[&["git", "status"]]);
        check("echo a\\\nb", true, &[&["echo", "ab"]]);
        check("rm -\\\nrf x", true, &[&["rm", "-rf", "x"]]);
        check(
            "gi\\\nt push \\\n origin",
            true,
            &[&["git", "push", "origin"]],
        );
        check("git pu\\\n$X", false, &[&["git"]]);
        assert!(!analyze("echo x >/dev/null\\\ny").complete);
        assert!(!analyze("A=1\\\nrm -rf x").complete);
    }

    #[test]
    fn lists_pipelines_and_newlines_are_separate_segments() {
        check(
            "cargo fmt && cargo test || echo x; ls | wc -l",
            true,
            &[
                &["cargo", "fmt"],
                &["cargo", "test"],
                &["echo", "x"],
                &["ls"],
                &["wc", "-l"],
            ],
        );
        check("ls;", true, &[&["ls"]]);
        check("ls\npwd", true, &[&["ls"], &["pwd"]]);
    }

    #[test]
    fn only_dev_null_and_fd_duplication_redirects_are_accepted() {
        for command in [
            "ls >/dev/null",
            "ls 2>/dev/null",
            "ls &>/dev/null",
            "ls >/dev/null 2>&1",
            "cargo test 2>&1 | tail -5",
        ] {
            assert!(analyze(command).complete, "{command}");
        }
        for command in [
            "ls > out.txt",
            "ls >> out.txt",
            "ls 2> err.txt",
            "ls < in.txt",
            "ls >/dev/null.x",
            "ls 1>&2",
        ] {
            let analysis = analyze(command);
            assert!(!analysis.complete, "{command}");
            assert_eq!(analysis.segments[0][0], "ls", "{command}");
        }
    }

    #[test]
    fn dynamic_words_end_the_static_prefix_and_clear_complete() {
        check("echo $X", false, &[&["echo"]]);
        check("echo ${X}", false, &[&["echo"]]);
        check("echo \"a$X\"", false, &[&["echo"]]);
        check("echo a$X", false, &[&["echo"]]);
        check(
            r#"git push origin "$B""#,
            false,
            &[&["git", "push", "origin"]],
        );
        check("echo $((1+2))", false, &[&["echo"]]);
    }

    #[test]
    fn nested_commands_are_collected_in_source_order() {
        check("echo $(git push)", false, &[&["echo"], &["git", "push"]]);
        check("echo `id`", false, &[&["echo"], &["id"]]);
        check("cat <(ls)", false, &[&["cat"], &["ls"]]);
        check("(git status)", false, &[&["git", "status"]]);
        check("{ git status; }", false, &[&["git", "status"]]);
        check(
            "if true; then git push; fi",
            false,
            &[&["true"], &["git", "push"]],
        );
        check("for i in a b; do echo $i; done", false, &[&["echo"]]);
        check("while true; do ls; done", false, &[&["true"], &["ls"]]);
        for command in ["case x in a) ls;; esac", "f() { ls; }"] {
            let analysis = analyze(command);
            assert!(!analysis.complete, "{command}");
            assert!(analysis.segments.contains(&vec!["ls".into()]), "{command}");
        }
    }

    #[test]
    fn unsupported_syntax_clears_complete_but_keeps_static_words() {
        check("cat <<EOF\nhi\nEOF", false, &[&["cat"]]);
        check("cat <<< hi", false, &[&["cat"]]);
        check("X=1 git status", false, &[&["git", "status"]]);
        check("echo $'a'", false, &[&["echo"]]);
        check("echo $\"a\"", false, &[&["echo"]]);
        check("! git status", false, &[&["git", "status"]]);
        check("sleep 1 &", false, &[&["sleep", "1"]]);
        check("ls # note", false, &[&["ls"]]);
    }

    #[test]
    fn malformed_input_is_incomplete() {
        assert!(!analyze("git commit -m 'x").complete);
        assert!(!analyze("echo (").complete);
        assert!(!analyze("ls &&").complete);
    }

    #[test]
    fn unquoted_globs_and_tilde_are_not_static() {
        check("ls *.rs", false, &[&["ls"]]);
        check("ls a?", false, &[&["ls"]]);
        check("ls [a]", false, &[&["ls"]]);
        check("ls ~", false, &[&["ls"]]);
        check("ls ~/x", false, &[&["ls"]]);
        check("echo {a,b}", false, &[&["echo"]]);
        check("echo \"*\" '?' \\*", true, &[&["echo", "*", "?", "*"]]);
    }

    #[test]
    fn runners_assignments_and_keywords_clear_complete() {
        check("bash -c x", false, &[&["bash", "-c", "x"]]);
        check("/bin/bash -lc x", false, &[&["/bin/bash", "-lc", "x"]]);
        check("env x", false, &[&["env", "x"]]);
        check("sudo ls", false, &[&["sudo", "ls"]]);
        check("'a=b' x", false, &[&["a=b", "x"]]);
        check("'if' x", false, &[&["if", "x"]]);
        check("'' x", false, &[&["", "x"]]);
        check("git status", true, &[&["git", "status"]]);
        check("/usr/bin/git status", true, &[&["/usr/bin/git", "status"]]);
    }

    #[test]
    fn commands_over_64_kib_are_not_parsed() {
        let at_limit = format!("echo {}", "a".repeat(MAX_COMMAND_BYTES - 5));
        assert_eq!(at_limit.len(), MAX_COMMAND_BYTES);
        assert!(analyze(&at_limit).complete);
        let over = format!("echo {}", "a".repeat(MAX_COMMAND_BYTES - 4));
        assert_eq!(analyze(&over), ShellCommand::default());
        assert!(!analyze(&over).complete);
    }

    #[test]
    fn output_redirect_targets_are_reported() {
        let targets = |command: &str| parse(command).write_targets;
        assert_eq!(
            targets("echo a >> out.txt"),
            vec![WriteTarget {
                path: "out.txt".into(),
                is_static: true
            }]
        );
        assert_eq!(
            targets("echo a > \"$F\""),
            vec![WriteTarget {
                path: "\"$F\"".into(),
                is_static: false
            }]
        );
        assert!(targets("ls 2>&1 < in.txt").is_empty());
        assert!(
            segments("echo a > out.txt")
                .iter()
                .all(|words| words == &["echo", "a"])
        );
    }

    #[test]
    fn git_subcommand_skips_global_options() {
        let words =
            |command: &str| -> Vec<String> { command.split(' ').map(str::to_owned).collect() };
        let sub = |command: &str| {
            let words = words(command);
            git_subcommand(&words).map(|(index, name)| (index, name.to_owned()))
        };
        assert_eq!(sub("git push origin"), Some((1, "push".into())));
        assert_eq!(sub("git -C dir push"), Some((3, "push".into())));
        assert_eq!(
            sub("git -c a=b --no-pager --git-dir=x --work-tree=y push"),
            Some((6, "push".into()))
        );
        assert_eq!(sub("/usr/bin/git status"), Some((1, "status".into())));
        assert_eq!(sub("git --bare push"), None);
        assert_eq!(sub("git -C"), None);
        assert_eq!(sub("git"), None);
        assert_eq!(sub("echo push"), None);
        // `commit -C <rev>` is a subcommand option, not a global one.
        assert_eq!(sub("git commit -C HEAD"), Some((1, "commit".into())));
    }
}
