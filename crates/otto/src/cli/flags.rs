//! Command-line parsing for the `otto` binary.
//!
//! ponytail: the flag scanner is hand written rather than delegated to a
//! parsing crate. Otto's command line accepts `-name` and `--name`
//! interchangeably, takes a value either as `-name=value` or as the next
//! argument, stops at the first non-flag argument, and records which flags were
//! actually written on the command line. Seven of Otto's behaviours
//! (`--config`, `--shell-timeout`, `--max-output-bytes`, `--prompt`,
//! `--sandbox`, `--socket`, `--listen`) branch on that "explicitly set" set,
//! and the tests pin the resulting stderr text and exit code 2 byte for byte.
//! No general-purpose parser has those exact semantics, so matching them costs
//! less here than bending one.
//!
//! Errors: a rejected command line carries the message the caller must print;
//! [`ParseFailure::Unsafe`]'s diagnostic is deliberately replaced with a fixed
//! string so a malformed argument can never echo a secret back to the terminal.

use std::collections::HashSet;
use std::io::Write;
use std::time::Duration;

use otto_core::config::duration::parse_go_duration;

/// Every command-line option, after parsing and validation. The parsed options,
/// including the `*Set` flags that record which options were written
/// explicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOptions {
    pub config_path: String,
    pub cwd: String,
    pub profile: String,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub thinking: String,
    pub prompt: String,
    pub ui: String,
    pub sandbox: String,
    pub shell_timeout: Duration,
    pub max_output_bytes: i64,
    pub no_session: bool,
    pub continue_last: bool,
    pub resume_path: String,
    pub archive_path: String,
    pub socket: String,
    pub listen: String,
    pub open: bool,

    pub explicit_config: bool,
    pub shell_time_set: bool,
    pub max_output_set: bool,
    pub prompt_set: bool,
    pub sandbox_set: bool,
    pub socket_set: bool,
    pub listen_set: bool,
    pub serve: bool,
}

/// Why a command line was refused. Both cases exit with status 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseFailure {
    /// The scanner itself refused the arguments. The caller prints a fixed
    /// message rather than the scanner's own diagnostic.
    Unsafe,
    /// A validation rule refused the combination. The string is the exact
    /// text to write to stderr, newline included.
    Rejected(String),
}

/// What a successful parse produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// `--help` was given; usage has already been written to stdout.
    Help,
    Options(Box<CliOptions>),
}

/// Parses and validates `args` (the arguments after the program name).
pub fn parse_flags(args: &[String], stdout: &mut dyn Write) -> Result<Parsed, ParseFailure> {
    let mut options = CliOptions {
        cwd: ".".to_string(),
        ..CliOptions::default()
    };
    let mut rest = args;
    if rest.first().is_some_and(|first| first == "serve") {
        options.serve = true;
        rest = &rest[1..];
    }

    let mut set = FlagSet::new();
    let scan = set.parse(rest).map_err(|()| ParseFailure::Unsafe)?;

    if set.bool_value("help") || set.bool_value("h") {
        print_usage(stdout);
        return Ok(Parsed::Help);
    }
    if !scan.is_empty() {
        return Err(reject("otto: unexpected positional arguments"));
    }

    options.config_path = set.string("config");
    options.cwd = set.string_or("cwd", ".");
    options.profile = set.string("profile");
    options.provider = set.string("provider");
    options.base_url = set.string("base-url");
    options.model = set.string("model");
    options.thinking = set.string("thinking");
    options.prompt = set.string("prompt");
    options.ui = set.string("ui");
    options.sandbox = set.string("sandbox");
    options.shell_timeout = set.duration("shell-timeout");
    options.max_output_bytes = set.int("max-output-bytes");
    options.no_session = set.bool_value("no-session");
    options.continue_last = set.bool_value("continue");
    options.resume_path = set.string("resume");
    options.archive_path = set.string("archive");
    options.socket = set.string("socket");
    options.listen = set.string("listen");
    options.open = set.bool_value("open");

    options.explicit_config = set.visited("config");
    options.shell_time_set = set.visited("shell-timeout");
    options.max_output_set = set.visited("max-output-bytes");
    options.prompt_set = set.visited("prompt");
    options.sandbox_set = set.visited("sandbox");
    options.socket_set = set.visited("socket");
    options.listen_set = set.visited("listen");

    // `--approve` used to mean what `--prompt` means now, and `/approve`
    // still grants one elevated Bash command. The retired name is declared
    // only so the rejection can name its replacement.
    if set.visited("approve") {
        return Err(reject("otto: --approve was renamed to --prompt"));
    }

    validate(&options, set.visited("ui"))?;
    Ok(Parsed::Options(Box::new(options)))
}

fn reject(line: &str) -> ParseFailure {
    ParseFailure::Rejected(format!("{line}\n"))
}

fn validate(options: &CliOptions, ui_visited: bool) -> Result<(), ParseFailure> {
    if options.sandbox_set && !matches!(options.sandbox.as_str(), "auto" | "seatbelt" | "off") {
        return Err(reject("otto: --sandbox must be one of auto, seatbelt, off"));
    }
    if options.continue_last && !options.resume_path.is_empty() {
        return Err(reject(
            "otto: --continue and --resume cannot be used together",
        ));
    }
    if options.no_session && (options.continue_last || !options.resume_path.is_empty()) {
        return Err(reject(
            "otto: --no-session cannot be used with --continue or --resume",
        ));
    }
    if !options.archive_path.is_empty() {
        let conflict = if options.continue_last {
            "--continue"
        } else if !options.resume_path.is_empty() {
            "--resume"
        } else if options.no_session {
            "--no-session"
        } else if options.prompt_set {
            "--prompt"
        } else {
            ""
        };
        if !conflict.is_empty() {
            return Err(reject(&format!(
                "otto: --archive cannot be used with {conflict}"
            )));
        }
    }
    if options.serve {
        let conflict = if ui_visited {
            "--ui"
        } else if options.prompt_set {
            "--prompt"
        } else if !options.resume_path.is_empty() {
            "--resume"
        } else if options.continue_last {
            "--continue"
        } else if !options.archive_path.is_empty() {
            "--archive"
        } else if options.no_session {
            "--no-session"
        } else {
            ""
        };
        if !conflict.is_empty() {
            return Err(reject(&format!(
                "otto: serve cannot be combined with {conflict}"
            )));
        }
    }
    if options.socket_set && !options.serve {
        return Err(reject("otto: --socket requires the serve subcommand"));
    }
    if options.listen_set && !options.serve {
        return Err(reject("otto: --listen requires the serve subcommand"));
    }
    if options.socket_set && options.listen_set {
        return Err(reject(
            "otto: --socket and --listen cannot be used together",
        ));
    }
    if options.open && !options.serve {
        return Err(reject("otto: --open requires the serve subcommand"));
    }
    if options.open && options.socket_set {
        return Err(reject("otto: --open cannot be used with --socket"));
    }
    if options.shell_time_set && options.shell_timeout.is_zero() {
        return Err(reject("otto: --shell-timeout must be greater than zero"));
    }
    if options.max_output_set && options.max_output_bytes <= 0 {
        return Err(reject("otto: --max-output-bytes must be greater than zero"));
    }
    if !matches!(
        options.thinking.as_str(),
        "" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        return Err(reject(
            "otto: --thinking must be one of low, medium, high, xhigh, max",
        ));
    }
    if options.prompt_set && options.prompt.trim().is_empty() {
        return Err(reject("otto: --prompt requires a non-empty value"));
    }
    if options.prompt_set && options.ui == "tui" {
        return Err(reject("otto: --prompt cannot be used with --ui tui"));
    }
    Ok(())
}

/// The exact usage text the binary writes.
pub fn print_usage(output: &mut dyn Write) {
    let _ = output.write_all(USAGE.as_bytes());
}

const USAGE: &str = r"Usage: otto [options]
       otto serve [options] [--socket PATH | --listen HOST:PORT [--open]]
       otto login [--status]   sign in with a ChatGPT subscription
       otto logout             remove stored ChatGPT credentials
       otto memory status|forget <id>
       otto sandbox setup [--config PATH] [--cwd PATH]

Sandbox: on macOS, auto -> Seatbelt; if it cannot be established, bash is disabled.
WARNING: off is explicitly unsafe; bash runs unsandboxed with anything accessible to your macOS user.
File tools always stay within the selected workspace.

Options:
  --help                 show help
  --config PATH          configuration file
  --cwd PATH             workspace directory
  --profile NAME         configuration profile
  --provider NAME        provider override
  --base-url URL         provider base URL override
  --model NAME           model override
  --thinking LEVEL       model thinking effort: low, medium, high, xhigh, or max
  --prompt PROMPT        run PROMPT (or @FILE) without interaction and exit
  --ui MODE              frontend mode: auto, tui, or repl
  --sandbox MODE         sandbox mode: auto, seatbelt, or off (off is unsafe)
  --shell-timeout D      shell command timeout
  --max-output-bytes N   maximum tool output bytes
  --no-session           use an in-memory session
  --continue             continue newest workspace session
  --resume PATH          resume a session file
  --archive PATH         archive an active session file
  --socket PATH          unix socket path for the serve subcommand
  --listen HOST:PORT     loopback TCP address for the serve subcommand (prints a URL with the access token)
  --open                 open the serve URL in the default browser (TCP listener only)
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Str,
    Duration,
    Int,
}

/// The declared flags and the values scanned for them.
struct FlagSet {
    declared: Vec<(&'static str, Kind)>,
    values: Vec<(String, String)>,
    visited: HashSet<String>,
}

const DECLARED: &[(&str, Kind)] = &[
    ("help", Kind::Bool),
    ("h", Kind::Bool),
    ("config", Kind::Str),
    ("cwd", Kind::Str),
    ("profile", Kind::Str),
    ("provider", Kind::Str),
    ("base-url", Kind::Str),
    ("model", Kind::Str),
    ("thinking", Kind::Str),
    ("prompt", Kind::Str),
    ("approve", Kind::Str),
    ("ui", Kind::Str),
    ("sandbox", Kind::Str),
    ("shell-timeout", Kind::Duration),
    ("max-output-bytes", Kind::Int),
    ("no-session", Kind::Bool),
    ("continue", Kind::Bool),
    ("resume", Kind::Str),
    ("archive", Kind::Str),
    ("socket", Kind::Str),
    ("listen", Kind::Str),
    ("open", Kind::Bool),
];

impl FlagSet {
    fn new() -> Self {
        Self {
            declared: DECLARED.to_vec(),
            values: Vec::new(),
            visited: HashSet::new(),
        }
    }

    fn kind(&self, name: &str) -> Option<Kind> {
        self.declared
            .iter()
            .find(|(declared, _)| *declared == name)
            .map(|(_, kind)| *kind)
    }

    /// Scans `args` the way `flag.FlagSet.Parse` does and returns the
    /// arguments left after the first non-flag argument.
    fn parse<'a>(&mut self, args: &'a [String]) -> Result<&'a [String], ()> {
        let mut rest = args;
        while let Some(argument) = rest.first() {
            let bytes = argument.as_bytes();
            if bytes.len() < 2 || bytes[0] != b'-' {
                break;
            }
            let mut body = &argument[1..];
            if body.starts_with('-') {
                body = &body[1..];
                if body.is_empty() {
                    // "--" ends the flags and is itself consumed.
                    rest = &rest[1..];
                    break;
                }
            }
            rest = &rest[1..];
            if body.starts_with('-') || body.starts_with('=') {
                return Err(());
            }
            let (name, inline) = match body.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (body, None),
            };
            let Some(kind) = self.kind(name) else {
                return Err(());
            };
            let value = match inline {
                Some(value) => value,
                None if kind == Kind::Bool => "true".to_string(),
                None => match rest.first() {
                    Some(next) => {
                        let next = next.clone();
                        rest = &rest[1..];
                        next
                    }
                    None => return Err(()),
                },
            };
            self.validate_value(kind, &value)?;
            self.values.push((name.to_string(), value));
            self.visited.insert(name.to_string());
        }
        Ok(rest)
    }

    fn validate_value(&self, kind: Kind, value: &str) -> Result<(), ()> {
        match kind {
            Kind::Bool => parse_go_bool(value).map(|_| ()),
            Kind::Duration => parse_go_duration(value).map(|_| ()).map_err(|_| ()),
            Kind::Int => parse_go_int(value).map(|_| ()),
            Kind::Str => Ok(()),
        }
    }

    /// The last value scanned for `name`: a repeated flag overwrites the
    /// earlier one.
    fn raw(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(scanned, _)| scanned == name)
            .map(|(_, value)| value.as_str())
    }

    fn visited(&self, name: &str) -> bool {
        self.visited.contains(name)
    }

    fn string(&self, name: &str) -> String {
        self.raw(name).unwrap_or_default().to_string()
    }

    fn string_or(&self, name: &str, fallback: &str) -> String {
        self.raw(name).unwrap_or(fallback).to_string()
    }

    fn bool_value(&self, name: &str) -> bool {
        self.raw(name)
            .and_then(|value| parse_go_bool(value).ok())
            .unwrap_or(false)
    }

    fn duration(&self, name: &str) -> Duration {
        let nanos = self
            .raw(name)
            .and_then(|value| parse_go_duration(value).ok())
            .unwrap_or(0);
        // Negative durations are rejected by the caller's `<= 0` check; a
        // saturating conversion keeps them at zero here.
        Duration::from_nanos(nanos.max(0) as u64)
    }

    fn int(&self, name: &str) -> i64 {
        self.raw(name)
            .and_then(|value| parse_go_int(value).ok())
            .unwrap_or(0)
    }
}

/// `strconv.ParseBool`.
fn parse_go_bool(value: &str) -> Result<bool, ()> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(false),
        _ => Err(()),
    }
}

/// `strconv.ParseInt(value, 0, 64)`: the base is taken from the prefix and
/// underscores are allowed between digits.
fn parse_go_int(value: &str) -> Result<i64, ()> {
    let (negative, digits) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let (radix, digits) = match digits.get(..2) {
        Some("0x") | Some("0X") => (16, &digits[2..]),
        Some("0o") | Some("0O") => (8, &digits[2..]),
        Some("0b") | Some("0B") => (2, &digits[2..]),
        _ => (10, digits),
    };
    let digits: String = digits.chars().filter(|c| *c != '_').collect();
    if digits.is_empty() {
        return Err(());
    }
    let magnitude = i64::from_str_radix(&digits, radix).map_err(|_| ())?;
    Ok(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Parsed, ParseFailure> {
        let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        parse_flags(&owned, &mut Vec::new())
    }

    fn options(args: &[&str]) -> CliOptions {
        match parse(args).expect("parse") {
            Parsed::Options(options) => *options,
            Parsed::Help => panic!("unexpected help"),
        }
    }

    fn rejection(args: &[&str]) -> String {
        match parse(args).expect_err("rejected") {
            ParseFailure::Rejected(message) => message,
            ParseFailure::Unsafe => panic!("unexpected unsafe parse"),
        }
    }

    #[test]
    fn defaults_the_workspace_to_the_current_directory() {
        let got = options(&[]);
        assert_eq!(got.cwd, ".");
        assert!(!got.serve);
        assert!(!got.explicit_config);
        assert_eq!(got.shell_timeout, Duration::ZERO);
        assert_eq!(got.max_output_bytes, 0);
    }

    #[test]
    fn accepts_both_dash_forms_and_both_value_forms() {
        for args in [
            ["--model", "gpt-5"].as_slice(),
            ["-model", "gpt-5"].as_slice(),
            ["--model=gpt-5"].as_slice(),
            ["-model=gpt-5"].as_slice(),
        ] {
            assert_eq!(options(args).model, "gpt-5", "{args:?}");
        }
    }

    #[test]
    fn records_which_options_were_written_explicitly() {
        let bare = options(&[]);
        assert!(!bare.prompt_set && !bare.sandbox_set && !bare.shell_time_set);
        assert!(!bare.max_output_set && !bare.socket_set && !bare.listen_set);

        let explicit = options(&[
            "--config",
            "/tmp/config.toml",
            "--prompt",
            "do it",
            "--sandbox",
            "off",
            "--shell-timeout",
            "30s",
            "--max-output-bytes",
            "4096",
        ]);
        assert!(explicit.explicit_config);
        assert!(explicit.prompt_set);
        assert!(explicit.sandbox_set);
        assert!(explicit.shell_time_set);
        assert!(explicit.max_output_set);
        assert_eq!(explicit.shell_timeout, Duration::from_secs(30));
        assert_eq!(explicit.max_output_bytes, 4096);
    }

    #[test]
    fn a_repeated_flag_keeps_the_last_value() {
        assert_eq!(options(&["--model", "a", "--model", "b"]).model, "b");
    }

    #[test]
    fn the_serve_subcommand_is_stripped_before_flag_scanning() {
        let got = options(&["serve", "--socket", "/tmp/otto.sock"]);
        assert!(got.serve);
        assert!(got.socket_set);
        assert_eq!(got.socket, "/tmp/otto.sock");
    }

    #[test]
    fn open_is_serve_only_and_needs_a_tcp_listener() {
        let got = options(&["serve", "--listen", "127.0.0.1:0", "--open"]);
        assert!(got.serve);
        assert!(got.open);
        assert!(got.listen_set);
        assert!(!options(&["serve", "--listen", "127.0.0.1:0"]).open);
    }

    #[test]
    fn help_prints_usage_and_stops() {
        for flag in ["--help", "-h"] {
            let mut stdout = Vec::new();
            let outcome = parse_flags(&[flag.to_string()], &mut stdout).expect("parse");
            assert_eq!(outcome, Parsed::Help);
            let text = String::from_utf8(stdout).expect("utf-8 usage");
            assert!(text.starts_with("Usage: otto [options]\n"), "{text}");
            assert!(text.contains(
                "  --sandbox MODE         sandbox mode: auto, seatbelt, or off (off is unsafe)\n"
            ));
            assert!(text.contains(
                "  --open                 open the serve URL in the default browser (TCP listener only)\n"
            ));
        }
    }

    #[test]
    fn an_unknown_flag_is_an_unsafe_parse() {
        assert_eq!(
            parse(&["--max-turns", "1"]).expect_err("rejected"),
            ParseFailure::Unsafe
        );
        assert_eq!(
            parse(&["--shell-timeout", "banana"]).expect_err("rejected"),
            ParseFailure::Unsafe
        );
        assert_eq!(
            parse(&["--max-output-bytes", "banana"]).expect_err("rejected"),
            ParseFailure::Unsafe
        );
        assert_eq!(
            parse(&["--model"]).expect_err("rejected"),
            ParseFailure::Unsafe
        );
        assert_eq!(
            parse(&["---x"]).expect_err("rejected"),
            ParseFailure::Unsafe
        );
    }

    #[test]
    fn the_retired_approve_flag_names_its_replacement() {
        assert_eq!(
            rejection(&["--approve", "x"]),
            "otto: --approve was renamed to --prompt\n"
        );
    }

    #[test]
    fn scanning_stops_at_the_first_positional_argument() {
        assert_eq!(
            rejection(&["extra"]),
            "otto: unexpected positional arguments\n"
        );
        assert_eq!(
            rejection(&["--", "--model", "x"]),
            "otto: unexpected positional arguments\n"
        );
        assert_eq!(rejection(&["-"]), "otto: unexpected positional arguments\n");
    }

    #[test]
    fn every_invalid_flag_reports_its_allowed_values() {
        let cases: &[(&[&str], &str)] = &[
            (
                &["--sandbox", "docker"],
                "otto: --sandbox must be one of auto, seatbelt, off\n",
            ),
            (
                &["--continue", "--resume", "s.jsonl"],
                "otto: --continue and --resume cannot be used together\n",
            ),
            (
                &["--no-session", "--continue"],
                "otto: --no-session cannot be used with --continue or --resume\n",
            ),
            (
                &["--archive", "s.jsonl", "--continue"],
                "otto: --archive cannot be used with --continue\n",
            ),
            (
                &["--archive", "s.jsonl", "--resume", "other.jsonl"],
                "otto: --archive cannot be used with --resume\n",
            ),
            (
                &["--archive", "s.jsonl", "--no-session"],
                "otto: --archive cannot be used with --no-session\n",
            ),
            (
                &["--archive", "s.jsonl", "--prompt", "x"],
                "otto: --archive cannot be used with --prompt\n",
            ),
            (
                &["serve", "--ui", "repl"],
                "otto: serve cannot be combined with --ui\n",
            ),
            (
                &["serve", "--prompt", "x"],
                "otto: serve cannot be combined with --prompt\n",
            ),
            (
                &["serve", "--resume", "anything"],
                "otto: serve cannot be combined with --resume\n",
            ),
            (
                &["serve", "--continue"],
                "otto: serve cannot be combined with --continue\n",
            ),
            (
                &["serve", "--archive", "anything"],
                "otto: serve cannot be combined with --archive\n",
            ),
            (
                &["serve", "--no-session"],
                "otto: serve cannot be combined with --no-session\n",
            ),
            (
                &["--socket", "/tmp/otto.sock"],
                "otto: --socket requires the serve subcommand\n",
            ),
            (
                &["--listen", "127.0.0.1:0"],
                "otto: --listen requires the serve subcommand\n",
            ),
            (
                &[
                    "serve",
                    "--socket",
                    "/tmp/otto.sock",
                    "--listen",
                    "127.0.0.1:0",
                ],
                "otto: --socket and --listen cannot be used together\n",
            ),
            (&["--open"], "otto: --open requires the serve subcommand\n"),
            (
                &["serve", "--open", "--socket", "/tmp/otto.sock"],
                "otto: --open cannot be used with --socket\n",
            ),
            (
                &["--shell-timeout", "0s"],
                "otto: --shell-timeout must be greater than zero\n",
            ),
            (
                &["--max-output-bytes", "0"],
                "otto: --max-output-bytes must be greater than zero\n",
            ),
            (
                &["--thinking", "banana"],
                "otto: --thinking must be one of low, medium, high, xhigh, max\n",
            ),
            (
                &["--prompt", "   "],
                "otto: --prompt requires a non-empty value\n",
            ),
            (
                &["--prompt", "x", "--ui", "tui"],
                "otto: --prompt cannot be used with --ui tui\n",
            ),
        ];
        for (args, want) in cases {
            assert_eq!(&rejection(args), want, "{args:?}");
        }
    }

    #[test]
    fn valid_thinking_levels_and_sandbox_modes_are_accepted() {
        for level in ["low", "medium", "high", "xhigh", "max"] {
            assert_eq!(options(&["--thinking", level]).thinking, level);
        }
        for mode in ["auto", "seatbelt", "off"] {
            assert_eq!(options(&["--sandbox", mode]).sandbox, mode);
        }
    }

    #[test]
    fn go_integer_and_boolean_syntax_is_accepted() {
        assert_eq!(parse_go_int("0x10"), Ok(16));
        assert_eq!(parse_go_int("1_000"), Ok(1000));
        assert_eq!(parse_go_int("-5"), Ok(-5));
        assert_eq!(parse_go_int(""), Err(()));
        assert_eq!(parse_go_bool("TRUE"), Ok(true));
        assert_eq!(parse_go_bool("F"), Ok(false));
        assert_eq!(parse_go_bool("yes"), Err(()));
        assert!(!options(&["--no-session=false"]).no_session);
    }
}
