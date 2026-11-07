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
    /// Deprecated `--profile NAME`.
    pub profile: String,
    /// Deprecated `--provider NAME`, paired with a bare `--model ID`.
    pub provider: String,
    pub base_url: String,
    /// `--model <provider>/<model_id>`.
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
    /// Exact session ID or name for the top-level permanent deletion command.
    pub delete_target: Option<String>,
    pub delete_force: bool,
    pub socket: String,
    pub listen: String,
    pub acp_listen: String,
    /// Start this process's session in the YOLO approval mode.
    pub yolo: bool,

    pub explicit_config: bool,
    pub shell_time_set: bool,
    pub max_output_set: bool,
    pub prompt_set: bool,
    pub sandbox_set: bool,
    pub socket_set: bool,
    pub listen_set: bool,
    pub serve: bool,
    pub acp: bool,
    /// Use the `otto serve` on the socket instead of running the agent:
    /// `otto acp --attach` relays ACP to it, plain `otto --attach` runs the
    /// TUI against it.
    pub attach: bool,
}

impl CliOptions {
    /// One stderr line for each deprecated model flag that was written.
    pub fn deprecation_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if !self.profile.is_empty() {
            warnings.push(
                "otto: --profile is deprecated; use --model <provider>/<model_id>\n".to_string(),
            );
        }
        if !self.provider.is_empty() && !self.model.is_empty() {
            warnings.push(
                "otto: --provider is deprecated; use --model <provider>/<model_id>\n".to_string(),
            );
        }
        warnings
    }
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
    /// `--version` was given; the version line has already been written to
    /// stdout.
    Version,
    Options(Box<CliOptions>),
}

/// Parses and validates `args` (the arguments after the program name).
pub fn parse_flags(args: &[String], stdout: &mut dyn Write) -> Result<Parsed, ParseFailure> {
    let mut options = CliOptions {
        cwd: ".".to_string(),
        ..CliOptions::default()
    };
    if args.first().is_some_and(|argument| argument == "delete") {
        if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") {
            print_usage(stdout);
            return Ok(Parsed::Help);
        }
        let mut target = None;
        let mut force = false;
        for argument in &args[1..] {
            if argument == "--force" && !force {
                force = true;
            } else if !argument.starts_with('-') && !argument.is_empty() && target.is_none() {
                target = Some(argument.clone());
            } else {
                return Err(reject("otto: usage: otto delete <SESSION> [--force]"));
            }
        }
        if target.is_none() {
            return Err(reject("otto: usage: otto delete <SESSION> [--force]"));
        }
        options.delete_target = target;
        options.delete_force = force;
        return Ok(Parsed::Options(Box::new(options)));
    }
    let mut rest = args;
    if rest.first().is_some_and(|first| first == "serve") {
        options.serve = true;
        rest = &rest[1..];
    } else if rest.first().is_some_and(|first| first == "acp") {
        options.acp = true;
        rest = &rest[1..];
    }

    let mut set = FlagSet::new();
    let scan = set.parse(rest).map_err(|()| ParseFailure::Unsafe)?;

    if set.bool_value("help") || set.bool_value("h") {
        print_usage(stdout);
        return Ok(Parsed::Help);
    }
    if set.bool_value("version") || set.bool_value("V") {
        let _ = writeln!(stdout, "otto {}", otto::VERSION);
        return Ok(Parsed::Version);
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
    options.acp_listen = set.string("acp-listen");
    options.attach = set.bool_value("attach");
    options.yolo = set.bool_value("yolo");

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
    if options.serve || options.acp {
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
                "otto: {} cannot be combined with {conflict}",
                if options.acp { "acp" } else { "serve" }
            )));
        }
    }
    if options.attach && !options.acp {
        let conflict = if options.serve {
            "serve"
        } else if options.prompt_set {
            "--prompt"
        } else if options.no_session {
            "--no-session"
        } else if !options.archive_path.is_empty() {
            "--archive"
        } else if ui_visited && !matches!(options.ui.as_str(), "auto" | "tui") {
            "--ui repl"
        } else {
            ""
        };
        if !conflict.is_empty() {
            return Err(reject(&format!(
                "otto: --attach cannot be combined with {conflict}"
            )));
        }
    }
    if options.socket_set && !options.serve && !options.attach {
        return Err(reject("otto: --socket requires the serve subcommand"));
    }
    if options.yolo && options.attach {
        return Err(reject(
            "otto: --yolo applies to the session this process opens; with --attach run /permissions yolo instead",
        ));
    }
    if options.yolo && (options.serve || options.acp) {
        // A server or an agent opens its sessions on a client's request, so
        // the mode belongs to that client's call, not to this startup.
        return Err(reject(
            "otto: --yolo requires a frontend that opens one session; use /permissions yolo, _otto/approval_mode/set or PUT /v1/sessions/{id}/approval-mode instead",
        ));
    }
    if !options.acp_listen.is_empty() && !options.serve {
        return Err(reject("otto: --acp-listen requires the serve subcommand"));
    }
    if options.listen_set && !options.serve {
        return Err(reject("otto: --listen requires the serve subcommand"));
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
       otto serve [options] [--socket PATH] [--listen HOST:PORT]
       otto acp [options]      Agent Client Protocol server on stdin/stdout
       otto acp --attach [--socket PATH]   relay Agent Client Protocol to a running otto serve
       otto --attach [--socket PATH] [--resume ID | --continue]   terminal UI on a running otto serve
       otto delete <SESSION> [--force]   permanently delete by exact ID or unique exact name
                                          (active and archived; --force requires an ID)
       otto login [--status]   sign in with a ChatGPT subscription
       otto logout             remove stored ChatGPT credentials
       otto storage window-peaks <usage.db> <session-id>
       otto workspace status|fork [--cwd PATH]
       otto memory status|list|show|forget|history|diff|restore|state|verify ...
       otto mcp list|add|remove|enable|disable|login|logout ...
       otto plugin install|list|inspect|enable|disable|trust|revoke|remove ...
       otto workflow run|status|resume|fork|approve|reject|cancel ...
       otto eval view-tree build|probes|run|all ...   development evaluation of the summary-tree view
       otto eval memory-retraction [--layer 2|3|all] [--dry-run] ...   development evaluation of memory retraction
       otto sandbox setup [--config PATH] [--cwd PATH]
       otto setup [--config PATH]
       otto config explain|set ...
       otto trust <dir> [--executables | --revoke-executables] [--config PATH]

Sandbox: on macOS, auto -> Seatbelt; elsewhere there is no confined driver.
If one cannot be established, bash is disabled and the file tools remain.
WARNING: off is explicitly unsafe; bash runs unsandboxed with anything accessible to your user.
File tools always stay within the selected workspace.

Options:
  --help                 show help
  --version, -V          print version and exit
  --config PATH          user configuration source
  --cwd PATH             workspace directory
  --model PROVIDER/ID    model selector, for example claude/<model_id>
  --base-url URL         provider base URL override
  --profile NAME         deprecated: legacy profile name; use --model
  --provider NAME        deprecated: pairs with a bare --model ID; use --model PROVIDER/ID
  --thinking LEVEL       model thinking effort: low, medium, high, xhigh, or max
  --prompt PROMPT        run PROMPT (or @FILE) without interaction and exit
  --ui MODE              frontend mode: auto, tui, or repl
  --sandbox MODE         sandbox mode: auto, seatbelt, or off (off is unsafe)
  --shell-timeout D      shell command timeout
  --max-output-bytes N   maximum tool output bytes
  --no-session           use an in-memory session
  --continue             continue newest workspace session
  --resume PATH          resume a session file (with --attach: a session id)
  --archive PATH         archive an active session file
  --socket PATH          unix socket path for the serve subcommand and --attach
  --attach               use the otto serve on the socket instead of running the agent (terminal UI, or acp: relay)
  --yolo                 start this process's session in the YOLO approval mode
                         (every request of the session is approved without asking)
  --listen HOST:PORT     TCP address for the serve subcommand (non-loopback needs [server].tls_cert, tls_key and OTTO_SERVER_TOKEN)
  --acp-listen HOST:PORT authenticated loopback ACP TCP (requires OTTO_ACP_TOKEN)
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
    ("version", Kind::Bool),
    ("V", Kind::Bool),
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
    ("attach", Kind::Bool),
    ("listen", Kind::Str),
    ("acp-listen", Kind::Str),
    ("yolo", Kind::Bool),
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
mod tests;
