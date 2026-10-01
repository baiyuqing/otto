//! Resolution of the `[sandbox]` table into validated, sorted settings.
//!
//! ponytail: these types mirror `otto::sandbox`'s own `DriverMode`,
//! `NetworkMode` and `Settings` field for field rather than reusing them,
//! because otto-core cannot depend on the native `otto` crate that owns the
//! sandbox executor. The native layer converts a [`SandboxSettings`] into its
//! own `sandbox::Settings` at the call site.

use super::{ConfigError, SandboxConfig};

const MAX_SANDBOX_READ_PATH_BYTES: usize = 32 * 1024;

/// Mirrors `otto::sandbox`'s three driver modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxDriverMode {
    Auto,
    Seatbelt,
    Off,
}

/// Mirrors `otto::sandbox`'s two network modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxNetworkMode {
    Deny,
    Allow,
}

/// The resolved, validated `[sandbox]` settings, ready to convert into a
/// native `sandbox::Settings`. `read_paths` and `allow_env` are sorted and
/// own their storage independently of the input `SandboxConfig`.
/// `excluded_commands` keeps the configured order and is not part of the
/// native `sandbox::Settings`: the bash tool applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSettings {
    pub driver: SandboxDriverMode,
    pub network: SandboxNetworkMode,
    pub read_paths: Vec<String>,
    pub allow_env: Vec<String>,
    pub excluded_commands: Vec<String>,
}

/// Resolves `raw` (the `[sandbox]` table) and an optional `--sandbox` CLI
/// flag value into [`SandboxSettings`]. `cli_driver` wins over
/// `raw.driver`, which wins over the `auto` default. An explicit empty
/// string for either mode is rejected; only an absent key falls back to a
/// default.
pub fn resolve_sandbox(
    raw: &SandboxConfig,
    cli_driver: Option<&str>,
) -> Result<SandboxSettings, ConfigError> {
    let mut driver_value = "auto";
    if let Some(value) = raw.driver.as_deref() {
        driver_value = value;
    }
    if let Some(value) = cli_driver {
        driver_value = value;
    }
    let driver = sandbox_driver_mode(driver_value)
        .ok_or_else(|| ConfigError::new("invalid sandbox driver"))?;

    let network_value = raw.network.as_deref().unwrap_or("allow");
    let network = sandbox_network_mode(network_value)
        .ok_or_else(|| ConfigError::new("invalid sandbox network"))?;

    let mut read_paths = raw.read_paths.clone();
    if !read_paths.iter().all(|path| valid_sandbox_read_path(path)) {
        return Err(ConfigError::new("invalid sandbox read_paths"));
    }
    read_paths.sort();

    let mut allow_env = raw.allow_env.clone();
    let mut seen: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(allow_env.len());
    for name in &allow_env {
        if !valid_sandbox_environment_name(name) || !seen.insert(name.as_str()) {
            return Err(ConfigError::new("invalid sandbox allow_env"));
        }
    }
    allow_env.sort();

    let mut seen: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(raw.excluded_commands.len());
    for entry in &raw.excluded_commands {
        if !valid_excluded_command(entry) || !seen.insert(entry.as_str()) {
            return Err(ConfigError::new("invalid sandbox excluded_commands"));
        }
    }

    Ok(SandboxSettings {
        driver,
        network,
        read_paths,
        allow_env,
        excluded_commands: raw.excluded_commands.clone(),
    })
}

/// Whether `command` matches one of `entries` and is a simple command, so the
/// bash tool may run it outside the sandbox.
///
/// An entry `prefix *` matches `prefix` alone or `prefix` followed by a space
/// or tab and anything else; any other entry matches only a command equal to
/// it. Leading and trailing whitespace of `command` is ignored. A command that
/// is not [simple](is_simple_command) never matches, so `lark-cli *` does not
/// cover `lark-cli x && rm -rf y`.
pub fn excluded_command_matches(entries: &[String], command: &str) -> bool {
    let command = command.trim();
    if !is_simple_command(command) {
        return false;
    }
    entries.iter().any(|entry| match entry.strip_suffix(" *") {
        Some(prefix) => command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t'])),
        None => command == entry,
    })
}

/// Whether `command` is one simple shell command: every quote is closed, and
/// it has no unquoted `;`, `&`, `|`, `<`, `>`, `(`, `)`, `#` or line feed, and
/// no `$` or backtick outside single quotes. Anything that could chain,
/// redirect, or substitute another command fails. `#` fails because quotes
/// after a comment start are not quotes to the shell, so `x #'` followed by a
/// line feed and `y #'` would scan as one quoted word but run `y`.
pub fn is_simple_command(command: &str) -> bool {
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
            '$' | '`' => return false,
            '"' => double = !double,
            '\'' if !double => single = true,
            ';' | '&' | '|' | '<' | '>' | '(' | ')' | '#' | '\n' if !double => return false,
            _ => {}
        }
    }
    !single && !double && !escaped
}

/// The entry `/approve <id> always` adds for `command`: its first word
/// followed by ` *`. `None` when the command is not simple, its first word
/// has a quote, a backslash or `=` (an environment assignment), or the first
/// word is one of [`COMMAND_RUNNERS`], because the entry would then not
/// describe one program.
pub fn excluded_command_entry(command: &str) -> Option<String> {
    let command = command.trim();
    if !is_simple_command(command) {
        return None;
    }
    let program = command.split([' ', '\t']).next()?;
    if program.is_empty()
        || program.contains(['\'', '"', '\\', '=', '*'])
        || COMMAND_RUNNERS.contains(&program.rsplit('/').next().unwrap_or(program))
    {
        return None;
    }
    Some(format!("{program} *"))
}

/// Programs that run another command given as an argument. `bash *` or
/// `env *` would take every command out of the sandbox, so
/// [`excluded_command_entry`] does not derive an entry from them; a user can
/// still write one in the configuration file.
const COMMAND_RUNNERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "env",
    "eval",
    "exec",
    "command",
    "builtin",
    "source",
    ".",
    "sudo",
    "doas",
    "su",
    "xargs",
    "nohup",
    "nice",
    "time",
    "timeout",
    "watch",
    "osascript",
    "python",
    "python3",
    "perl",
    "ruby",
    "node",
    "git",
];

fn valid_excluded_command(entry: &str) -> bool {
    let body = entry.strip_suffix(" *").unwrap_or(entry);
    !body.is_empty()
        && body.trim() == body
        && !body.contains('*')
        && !entry.chars().any(char::is_control)
        && is_simple_command(body)
}

fn sandbox_driver_mode(value: &str) -> Option<SandboxDriverMode> {
    match value {
        "auto" => Some(SandboxDriverMode::Auto),
        "seatbelt" => Some(SandboxDriverMode::Seatbelt),
        "off" => Some(SandboxDriverMode::Off),
        _ => None,
    }
}

fn sandbox_network_mode(value: &str) -> Option<SandboxNetworkMode> {
    match value {
        "allow" => Some(SandboxNetworkMode::Allow),
        "deny" => Some(SandboxNetworkMode::Deny),
        _ => None,
    }
}

fn valid_sandbox_read_path(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_SANDBOX_READ_PATH_BYTES || path.contains('\0') {
        return false;
    }
    super::paths::is_abs(path) || path.starts_with("~/")
}

fn valid_sandbox_environment_name(name: &str) -> bool {
    let mut chars = name.bytes();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(
        driver: Option<&str>,
        network: Option<&str>,
        read_paths: &[&str],
        allow_env: &[&str],
    ) -> SandboxConfig {
        SandboxConfig {
            driver: driver.map(String::from),
            network: network.map(String::from),
            read_paths: read_paths.iter().map(|s| s.to_string()).collect(),
            allow_env: allow_env.iter().map(|s| s.to_string()).collect(),
            excluded_commands: Vec::new(),
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn defaults_to_auto_allow_and_empty_lists() {
        let got = resolve_sandbox(&SandboxConfig::default(), None).expect("resolve");
        assert_eq!(
            got,
            SandboxSettings {
                driver: SandboxDriverMode::Auto,
                network: SandboxNetworkMode::Allow,
                read_paths: vec![],
                allow_env: vec![],
                excluded_commands: vec![],
            }
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn accepts_and_sorts_valid_settings() {
        let raw = config(
            Some("seatbelt"),
            Some("deny"),
            &["~/zeta", "/opt/zeta", "/opt/alpha"],
            &["ZETA_TOKEN", "ALPHA_TOKEN"],
        );
        let got = resolve_sandbox(&raw, None).expect("resolve");
        assert_eq!(got.driver, SandboxDriverMode::Seatbelt);
        assert_eq!(got.network, SandboxNetworkMode::Deny);
        assert_eq!(got.read_paths, vec!["/opt/alpha", "/opt/zeta", "~/zeta"]);
        assert_eq!(got.allow_env, vec!["ALPHA_TOKEN", "ZETA_TOKEN"]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_explicit_empty_modes() {
        assert!(resolve_sandbox(&config(Some(""), None, &[], &[]), None).is_err());
        assert!(resolve_sandbox(&config(None, Some(""), &[], &[]), None).is_err());
        assert!(resolve_sandbox(&SandboxConfig::default(), Some("")).is_err());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_driver_values() {
        for value in ["docker", "apple-container", "podman", "AUTO", "unknown"] {
            let err = resolve_sandbox(&config(Some(value), None, &[], &[]), None).unwrap_err();
            assert!(err.to_string().contains("driver"), "{value}: {err}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_network_values() {
        for value in ["block", "ALLOW", "off", "unknown"] {
            let err = resolve_sandbox(&config(None, Some(value), &[], &[]), None).unwrap_err();
            assert!(err.to_string().contains("network"), "{value}: {err}");
        }
    }

    fn entries(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn excluded_commands_match_a_prefix_or_the_exact_command() {
        let list = entries(&["lark-cli *", "gh auth status"]);
        for command in [
            "lark-cli",
            "lark-cli auth status",
            "  lark-cli\tim +messages-send --text 'a; b | c > d $(e) `f` # g'\n",
            "lark-cli --text \"a; b & c\"",
            "lark-cli --text \"two\nlines\"",
            "lark-cli --text a\\;b",
            "gh auth status",
        ] {
            assert!(excluded_command_matches(&list, command), "{command:?}");
        }
        for command in [
            "lark-clix",
            "lark-cli-other auth",
            "./lark-cli auth",
            "gh auth status --show-token",
            "gh auth",
            "FOO=1 lark-cli auth",
        ] {
            assert!(!excluded_command_matches(&list, command), "{command:?}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn excluded_commands_never_match_a_command_that_runs_something_else() {
        let list = entries(&["lark-cli *"]);
        for command in [
            "lark-cli auth; rm -rf x",
            "lark-cli auth && rm -rf x",
            "lark-cli auth || rm -rf x",
            "lark-cli auth | sh",
            "lark-cli auth & rm -rf x",
            "lark-cli auth > ~/.zshrc",
            "lark-cli auth < /etc/passwd",
            "lark-cli auth\nrm -rf x",
            "lark-cli $(rm -rf x)",
            "lark-cli ${HOME}",
            "lark-cli \"$(rm -rf x)\"",
            "lark-cli `rm -rf x`",
            "lark-cli \"`rm -rf x`\"",
            "lark-cli (x)",
            "lark-cli #'\nrm -rf x #'",
            "lark-cli 'unterminated",
            "lark-cli \"unterminated",
            "lark-cli trailing\\",
        ] {
            assert!(!excluded_command_matches(&list, command), "{command:?}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn excluded_command_entries_are_validated() {
        let mut raw = SandboxConfig {
            excluded_commands: entries(&[
                "lark-cli *",
                "gh auth status",
                "/opt/bin/tool *",
                "git *",
            ]),
            ..SandboxConfig::default()
        };
        let got = resolve_sandbox(&raw, None).expect("resolve");
        assert_eq!(got.excluded_commands, raw.excluded_commands);

        for invalid in [
            "",
            " *",
            "*",
            "lark-cli*",
            "lark-* *",
            " lark-cli *",
            "lark-cli  *",
            "a; b *",
            "a $(b)",
            "a\tb\n",
            "lark-cli * x",
        ] {
            raw.excluded_commands = entries(&[invalid]);
            let error = resolve_sandbox(&raw, None).expect_err(invalid);
            assert!(
                error.to_string().contains("excluded_commands"),
                "{invalid:?}"
            );
        }
        raw.excluded_commands = entries(&["lark-cli *", "lark-cli *"]);
        assert!(resolve_sandbox(&raw, None).is_err());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn approve_always_derives_one_program_entry() {
        assert_eq!(
            excluded_command_entry("  lark-cli auth status").as_deref(),
            Some("lark-cli *")
        );
        assert_eq!(
            excluded_command_entry("/opt/bin/tool --flag").as_deref(),
            Some("/opt/bin/tool *")
        );
        for command in [
            "lark-cli auth && rm x",
            "FOO=1 lark-cli auth",
            "'lark-cli' auth",
            "bash -c 'lark-cli auth'",
            "/usr/bin/env lark-cli",
            "sudo lark-cli",
            "python3 script.py",
            "git status",
            "/usr/bin/git log",
            "",
        ] {
            assert_eq!(excluded_command_entry(command), None, "{command:?}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_read_paths() {
        let too_long = format!("/{}", "x".repeat(32 * 1024));
        for path in [
            "relative/path",
            ".",
            "~",
            "~someone/source",
            "$HOME/source",
            "/safe\0unsafe",
            too_long.as_str(),
        ] {
            let err = resolve_sandbox(&config(None, None, &[path], &[]), None).unwrap_err();
            assert!(err.to_string().contains("read_paths"), "{path:?}: {err}");
        }

        let boundary = format!("/{}", "x".repeat(32 * 1024 - 1));
        let got = resolve_sandbox(&config(None, None, &[boundary.as_str()], &[]), None)
            .expect("boundary path accepted");
        assert_eq!(got.read_paths, vec![boundary]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_or_duplicate_allow_env() {
        let cases: &[&[&str]] = &[
            &[""],
            &["*_TOKEN"],
            &["PROJECT_*"],
            &["1PROJECT"],
            &["PROJECT-TOKEN"],
            &["PROJECT_TOKEN", "PROJECT_TOKEN"],
        ];
        for names in cases {
            let err = resolve_sandbox(&config(None, None, &[], names), None).unwrap_err();
            assert!(err.to_string().contains("allow_env"), "{names:?}: {err}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn clones_inputs_without_aliasing() {
        let raw = config(
            None,
            None,
            &["~/zeta", "/opt/alpha"],
            &["ZETA_TOKEN", "ALPHA_TOKEN"],
        );
        let got = resolve_sandbox(&raw, None).expect("resolve");
        // `raw` is untouched (Rust borrows immutably; this also checks the
        // resolved result copied rather than aliased the source Vecs).
        assert_eq!(raw.read_paths, vec!["~/zeta", "/opt/alpha"]);
        assert_eq!(raw.allow_env, vec!["ZETA_TOKEN", "ALPHA_TOKEN"]);
        assert_eq!(got.read_paths, vec!["/opt/alpha", "~/zeta"]);
        assert_eq!(got.allow_env, vec!["ALPHA_TOKEN", "ZETA_TOKEN"]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn cli_driver_takes_precedence_without_changing_other_settings() {
        let raw = config(
            Some("seatbelt"),
            Some("deny"),
            &["/opt/sdk"],
            &["PROJECT_TOKEN"],
        );
        let got = resolve_sandbox(&raw, Some("off")).expect("resolve");
        assert_eq!(got.driver, SandboxDriverMode::Off);
        assert_eq!(got.network, SandboxNetworkMode::Deny);
        assert_eq!(got.read_paths, vec!["/opt/sdk"]);
        assert_eq!(got.allow_env, vec!["PROJECT_TOKEN"]);
    }
}
