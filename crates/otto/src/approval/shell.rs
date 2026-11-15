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
    /// Every word of each `command` node that has one, in the order of
    /// `command.segments`. A static word is its unquoted value; any other
    /// word is its source text. Assignments and redirects are not words.
    pub commands: Vec<Vec<String>>,
    pub write_targets: Vec<WriteTarget>,
    /// True when the program assigns a shell variable anywhere (`A=1 cmd`,
    /// `A=1;`, `export A=1`). Such a command can redirect `git` through
    /// `GIT_DIR` or `GIT_WORK_TREE`.
    pub assigns_variables: bool,
    /// Same as `command.complete`, except that a `NAME=value` prefix
    /// assignment with a static value on a simple command whose program is not
    /// `cd`, `pushd` or `popd` is accepted; such a prefix sets only that
    /// command's environment and does not change the shell's working directory.
    pub complete_with_command_env: bool,
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
    let mut complete_with_command_env = complete;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        let accepted = if node.is_named() {
            ACCEPTED_NAMED.contains(&kind)
        } else {
            ACCEPTED_TOKENS.contains(&kind)
        };
        complete &= accepted;
        let command_env_assignment =
            kind == "variable_assignment" && command_env_assignment(node, source);
        complete_with_command_env &= accepted || command_env_assignment;
        let mut descend = !command_env_assignment;
        match kind {
            "variable_assignment" => parsed.assigns_variables = true,
            "command" => {
                let (words, separated) = command_words(node, source);
                let leading: Vec<String> = words
                    .iter()
                    .take_while(|(_, is_static)| *is_static)
                    .map(|(word, _)| word.clone())
                    .collect();
                let words_accepted =
                    separated && leading.len() == words.len() && program_allowed(&leading);
                complete &= words_accepted;
                complete_with_command_env &= words_accepted;
                if !leading.is_empty() {
                    parsed.command.segments.push(leading);
                }
                if !words.is_empty() {
                    parsed
                        .commands
                        .push(words.into_iter().map(|(word, _)| word).collect());
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
                    complete_with_command_env = false;
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
            complete_with_command_env = false;
        }
        if descend {
            stack.extend(children.into_iter().rev());
        }
    }
    parsed.command.complete = complete;
    parsed.complete_with_command_env = complete_with_command_env;
    parsed
}

/// True for a `NAME=value` prefix of a simple command that is not `cd`,
/// `pushd` or `popd`, whose name is a plain variable name (no subscript) and
/// whose value is static (a missing value is empty).
fn command_env_assignment(node: Node, source: &[u8]) -> bool {
    let Some(parent) = node.parent().filter(|parent| parent.kind() == "command") else {
        return false;
    };
    let name_is_plain = node
        .child_by_field_name("name")
        .is_some_and(|name| name.kind() == "variable_name");
    let value_is_static = node
        .child_by_field_name("value")
        .is_none_or(|value| word_value(value, source, true).is_some());
    let (words, _) = command_words(parent, source);
    name_is_plain
        && value_is_static
        && words.first().is_some_and(|(program, _)| {
            ["cd", "pushd", "popd"]
                .iter()
                .all(|name| !is_program(program, name))
        })
}

/// Words of a `command` node, each with whether it is static: a static word
/// is its unquoted value, any other word its source text. Assignments and
/// redirects are not words. Words separated only by line continuations are
/// one word, as in bash. The flag is false when a continuation joins a word
/// with an assignment or redirect.
fn command_words(node: Node, source: &[u8]) -> (Vec<(String, bool)>, bool) {
    let mut words: Vec<(String, bool)> = Vec::new();
    let mut separated = true;
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
        if !is_word {
            // bash joins the assignment or redirect with the adjacent word.
            separated &= !joined;
            continue;
        }
        let value = word_value(child, source, !joined);
        let is_static = value.is_some();
        let text = value.unwrap_or_else(|| node_text(child, source).to_owned());
        match words.last_mut() {
            Some((last, last_static)) if joined && after_word => {
                last.push_str(&text);
                *last_static &= is_static;
            }
            _ => {
                separated &= !joined;
                words.push((text, is_static));
            }
        }
    }
    (words, separated)
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
pub(crate) fn is_program(word: &str, program: &str) -> bool {
    Path::new(word)
        .file_name()
        .is_some_and(|name| name == program)
}

/// Index and name of the git subcommand in `words`, skipping the global
/// options `-C <dir>`, `-c <k=v>`, `--no-pager`, `--git-dir=<p>` and
/// `--work-tree=<p>`. `None` when `words` is not a git command, has an
/// unrecognized option before the subcommand, or has no subcommand.
pub(crate) fn git_subcommand(words: &[String]) -> Option<(usize, &str)> {
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

/// What a command changes beyond the sandboxed workspace session. The
/// variant order is the ranking: `ExternalState` is the highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effect {
    Destructive,
    ExternalState,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Destructive => "destructive",
            Effect::ExternalState => "external_state",
        }
    }
}

/// Programs that run the command given in their later words.
const WRAPPERS: &[&str] = &[
    "env", "command", "exec", "nohup", "nice", "timeout", "time", "sudo", "xargs",
];

/// Programs [`effect`] has rules for.
const CLASSIFIED: &[&str] = &[
    "git",
    "gh",
    "curl",
    "wget",
    "npm",
    "pnpm",
    "yarn",
    "cargo",
    "docker",
    "podman",
    "twine",
    "gem",
    "kubectl",
    "helm",
    "terraform",
    "tofu",
    "rm",
    "find",
];

/// Highest effect among `parsed.commands` and the words of the first command
/// that has it.
pub fn classify(parsed: &Parsed) -> Option<(Effect, &[String])> {
    let mut best: Option<(Effect, &[String])> = None;
    for words in &parsed.commands {
        if let Some(found) = effect(words)
            && best.is_none_or(|(current, _)| found > current)
        {
            best = Some((found, words));
        }
    }
    best
}

fn program_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or_default()
}

/// Letters of a short-flag cluster such as `-rf`: one leading `-`, then
/// anything that does not start with another `-`.
fn cluster(word: &str) -> Option<&str> {
    word.strip_prefix('-')
        .filter(|letters| !letters.is_empty() && !letters.starts_with('-'))
}

fn has_letter(args: &[String], letter: char) -> bool {
    args.iter()
        .filter_map(|arg| cluster(arg))
        .any(|letters| letters.contains(letter))
}

fn has_arg(args: &[String], names: &[&str]) -> bool {
    args.iter().any(|arg| names.contains(&arg.as_str()))
}

/// If `args[index]` sets an HTTP method through `long` (`--long M`,
/// `--long=M`) or the `X` letter of a short cluster (`-XM`, `-X M`), the
/// method; the inner `None` means the value is missing.
fn method_flag<'a>(
    args: &'a [String],
    index: usize,
    long: &str,
    short: bool,
) -> Option<Option<&'a str>> {
    let arg = args[index].as_str();
    let next = || args.get(index + 1).map(String::as_str);
    if arg == long {
        return Some(next());
    }
    if let Some(value) = arg
        .strip_prefix(long)
        .and_then(|rest| rest.strip_prefix('='))
    {
        return Some(Some(value));
    }
    if short && let Some((_, rest)) = cluster(arg)?.split_once('X') {
        return Some(if rest.is_empty() { next() } else { Some(rest) });
    }
    None
}

fn is_read_method(method: Option<&str>) -> bool {
    method.is_some_and(|m| m.eq_ignore_ascii_case("GET") || m.eq_ignore_ascii_case("HEAD"))
}

fn gh_api_read_only(args: &[String]) -> bool {
    (0..args.len()).all(|index| {
        let arg = args[index].as_str();
        let writes_field = matches!(arg, "-f" | "-F" | "--field" | "--raw-field" | "--input")
            || ["--field=", "--raw-field=", "--input="]
                .iter()
                .any(|prefix| arg.starts_with(prefix))
            || cluster(arg).is_some_and(|l| l.starts_with(['f', 'F']));
        let method_ok = method_flag(args, index, "--method", true)
            .is_none_or(|m| m.is_some_and(|m| m.eq_ignore_ascii_case("GET")));
        !writes_field && method_ok
    })
}

fn curl_writes(args: &[String]) -> bool {
    (0..args.len()).any(|index| {
        let arg = args[index].as_str();
        if matches!(
            arg,
            "-d" | "-F" | "-T" | "--json" | "--form" | "--upload-file"
        ) || arg.starts_with("--data")
            || arg.starts_with("--form")
        {
            return true;
        }
        // Letters after `X` are the method, not flags (`-XGET` has a `T`).
        if let Some(letters) = cluster(arg)
            && letters
                .split('X')
                .next()
                .is_some_and(|flags| flags.contains(['d', 'F', 'T']))
        {
            return true;
        }
        method_flag(args, index, "--request", true).is_some_and(|m| !is_read_method(m))
    })
}

fn wget_writes(args: &[String]) -> bool {
    (0..args.len()).any(|index| {
        let arg = args[index].as_str();
        ["--post-data", "--post-file", "--body-data", "--body-file"]
            .iter()
            .any(|prefix| arg.starts_with(prefix))
            || method_flag(args, index, "--method", false).is_some_and(|m| !is_read_method(m))
    })
}

fn git_effect(words: &[String]) -> Option<Effect> {
    // An unrecognized global option (`git -P push`) hides the subcommand.
    let Some((index, subcommand)) = git_subcommand(words) else {
        return has_arg(words, &["push"]).then_some(Effect::ExternalState);
    };
    let args = &words[index + 1..];
    let first = args.first().map(String::as_str);
    match subcommand {
        "push" => (!has_arg(args, &["-n", "--dry-run"])).then_some(Effect::ExternalState),
        "reset" => has_arg(args, &["--hard"]).then_some(Effect::Destructive),
        "clean" => {
            let force = has_arg(args, &["--force"]) || has_letter(args, 'f');
            let dry = has_arg(args, &["--dry-run"]) || has_letter(args, 'n');
            (force && !dry).then_some(Effect::Destructive)
        }
        "checkout" => has_arg(args, &["-f", "--force", "--", "."]).then_some(Effect::Destructive),
        "restore" => {
            let staged_only =
                has_arg(args, &["--staged", "-S"]) && !has_arg(args, &["--worktree", "-W"]);
            (!staged_only).then_some(Effect::Destructive)
        }
        "switch" => {
            has_arg(args, &["-f", "--force", "--discard-changes"]).then_some(Effect::Destructive)
        }
        "stash" => matches!(first, Some("drop" | "clear")).then_some(Effect::Destructive),
        "branch" => {
            let delete = has_arg(args, &["--delete"]) || has_letter(args, 'd');
            let force = has_arg(args, &["--force"]) || has_letter(args, 'f');
            (has_letter(args, 'D') || (delete && force)).then_some(Effect::Destructive)
        }
        "reflog" => matches!(first, Some("expire" | "delete")).then_some(Effect::Destructive),
        "filter-branch" | "filter-repo" => Some(Effect::Destructive),
        _ => None,
    }
}

fn gh_effect(args: &[String]) -> Option<Effect> {
    let group = args.first().map_or("", String::as_str);
    let action = args.get(1).map_or("", String::as_str);
    let read_only = match group {
        "pr" | "issue" => matches!(action, "list" | "view" | "status" | "diff" | "checks"),
        "repo" => matches!(action, "view" | "list" | "clone"),
        "run" => matches!(action, "list" | "view" | "watch" | "download"),
        "release" => matches!(action, "list" | "view" | "download"),
        "search" | "status" | "browse" | "help" | "version" | "--help" | "--version" | "-h" => true,
        "auth" => action == "status",
        "api" => gh_api_read_only(&args[1..]),
        _ => false,
    };
    (!read_only).then_some(Effect::ExternalState)
}

/// Effect of one command given as its full word list.
fn effect(words: &[String]) -> Option<Effect> {
    let program = program_name(words.first()?);
    let args = &words[1..];
    if WRAPPERS.contains(&program) {
        let index = args
            .iter()
            .position(|word| CLASSIFIED.contains(&program_name(word)))?;
        return effect(&args[index..]);
    }
    match program {
        "git" => git_effect(words),
        "gh" => gh_effect(args),
        "curl" => curl_writes(args).then_some(Effect::ExternalState),
        "wget" => wget_writes(args).then_some(Effect::ExternalState),
        "npm" | "pnpm" | "yarn" => {
            has_arg(args, &["publish", "unpublish", "deprecate"]).then_some(Effect::ExternalState)
        }
        "cargo" => has_arg(args, &["publish", "yank", "owner"]).then_some(Effect::ExternalState),
        "docker" | "podman" | "gem" => has_arg(args, &["push"]).then_some(Effect::ExternalState),
        "twine" => has_arg(args, &["upload"]).then_some(Effect::ExternalState),
        "kubectl" => has_arg(
            args,
            &[
                "apply", "create", "delete", "patch", "replace", "edit", "scale", "rollout",
                "label", "annotate", "set", "drain", "cordon", "uncordon", "taint",
            ],
        )
        .then_some(Effect::ExternalState),
        "helm" => has_arg(
            args,
            &["install", "upgrade", "uninstall", "rollback", "push"],
        )
        .then_some(Effect::ExternalState),
        "terraform" | "tofu" => {
            has_arg(args, &["apply", "destroy", "import"]).then_some(Effect::ExternalState)
        }
        "rm" => (has_arg(args, &["--recursive"]) || has_letter(args, 'r') || has_letter(args, 'R'))
            .then_some(Effect::Destructive),
        "find" => {
            let delete = has_arg(args, &["-delete"]).then_some(Effect::Destructive);
            let executed = (0..args.len())
                .filter(|&i| matches!(args[i].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
                .filter_map(|i| effect(&args[i + 1..]))
                .max();
            delete.max(executed)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
