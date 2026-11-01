//! `otto setup`: create an initial non-secret provider profile.
//!
//! This command intentionally runs before provider resolution: a first-time
//! user cannot ask a model how to configure the model needed to start it.
//! It creates only a missing configuration file, records no credentials, and
//! leaves shell permissions at Otto's safe built-in defaults.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use otto_core::config::{PROVIDER_CHATGPT, PROVIDER_CLAUDE, PROVIDER_OPENAI_COMPATIBLE};

use super::run::{fail, resolve_home_for};

const USAGE: &str = "usage: otto setup [--config PATH]";

struct Flags {
    config_path: String,
}

fn parse_flags(args: &[String]) -> Result<Option<Flags>, ()> {
    let mut config_path = String::new();
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        if matches!(argument, "-h" | "-help" | "--help") {
            return Ok(None);
        }
        if !argument.starts_with('-') || argument == "-" || argument == "--" {
            return Err(());
        }
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) => (name.trim_start_matches('-'), Some(value)),
            None => (argument.trim_start_matches('-'), None),
        };
        if name != "config" {
            return Err(());
        }
        let value = match inline {
            Some(value) => value.to_string(),
            None => {
                index += 1;
                args.get(index).cloned().ok_or(())?
            }
        };
        config_path = value;
        index += 1;
    }
    Ok(Some(Flags { config_path }))
}

/// Runs the initial setup wizard. It deliberately does not invoke `otto login`:
/// OAuth authentication and configuration writes remain independently reviewable.
pub fn run(
    args: &[String],
    stdin: &mut (dyn BufRead + Send),
    stdout: &mut (dyn Write + Send),
    stderr: &mut (dyn Write + Send),
    lookup: &HashMap<String, String>,
) -> i32 {
    let flags = match parse_flags(args) {
        Ok(Some(flags)) => flags,
        Ok(None) => {
            let _ = writeln!(stdout, "{USAGE}");
            return 0;
        }
        Err(()) => return fail(stderr, USAGE),
    };
    let path = match flags.config_path.is_empty() {
        true => {
            let Ok(home) = resolve_home_for(lookup) else {
                return fail(stderr, "cannot resolve home directory");
            };
            [home.as_str(), ".config", "otto", "config.toml"]
                .iter()
                .collect()
        }
        false => PathBuf::from(flags.config_path),
    };
    let Ok(path) = std::path::absolute(&path) else {
        return fail(stderr, "invalid configuration path");
    };

    let _ = writeln!(
        stdout,
        "Otto setup creates a default model without storing API keys. Shell commands remain sandboxed by default."
    );
    let Some(provider) = ask_choice(
        stdin,
        stdout,
        "Provider [chatgpt / openai-compatible / claude] (chatgpt): ",
        &[
            PROVIDER_CHATGPT,
            PROVIDER_CLAUDE,
            PROVIDER_OPENAI_COMPATIBLE,
        ],
        PROVIDER_CHATGPT,
    ) else {
        let _ = writeln!(stdout, "Cancelled; configuration unchanged.");
        return 0;
    };
    let Some(model) = ask_nonempty(
        stdin,
        stdout,
        "Model ID (required): ",
        "",
        |_| true,
        "A model ID is required. Choose one available to your account or provider.",
    ) else {
        let _ = writeln!(stdout, "Cancelled; configuration unchanged.");
        return 0;
    };

    let (base_url, api_key_env) = if matches!(
        provider.as_str(),
        PROVIDER_OPENAI_COMPATIBLE | PROVIDER_CLAUDE
    ) {
        let Some(base_url) = ask_nonempty(
            stdin,
            stdout,
            if provider == PROVIDER_CLAUDE {
                "Base URL (https://api.anthropic.com): "
            } else {
                "Base URL (required): "
            },
            if provider == PROVIDER_CLAUDE {
                "https://api.anthropic.com"
            } else {
                ""
            },
            |_| true,
            "A base URL is required.",
        ) else {
            let _ = writeln!(stdout, "Cancelled; configuration unchanged.");
            return 0;
        };
        let Some(api_key_env) = ask_nonempty(
            stdin,
            stdout,
            if provider == PROVIDER_CLAUDE {
                "API key environment variable (ANTHROPIC_API_KEY): "
            } else {
                "API key environment variable (OPENAI_API_KEY): "
            },
            if provider == PROVIDER_CLAUDE {
                "ANTHROPIC_API_KEY"
            } else {
                "OPENAI_API_KEY"
            },
            valid_environment_name,
            "Use an environment variable name such as OPENAI_API_KEY.",
        ) else {
            let _ = writeln!(stdout, "Cancelled; configuration unchanged.");
            return 0;
        };
        (base_url, api_key_env)
    } else {
        (String::new(), String::new())
    };

    let content =
        match otto::config::initial_config_content(&provider, &model, &base_url, &api_key_env) {
            Ok(content) => content,
            Err(message) => return fail(stderr, &message),
        };
    let _ = writeln!(
        stdout,
        "\nProposed configuration for {}:\n\n{content}",
        path.display()
    );
    let _ = write!(stdout, "\n[save / cancel] (cancel): ");
    if read_line(stdin).as_deref() != Some("save") {
        let _ = writeln!(stdout, "Cancelled; configuration unchanged.");
        return 0;
    }

    match otto::config::create_initial_config(&path, &provider, &model, &base_url, &api_key_env) {
        Ok(()) => {
            let _ = writeln!(stdout, "Created {}.", path.display());
            if provider == PROVIDER_CHATGPT {
                let _ = writeln!(stdout, "Next: run `otto login`, then `otto`.");
            } else {
                let _ = writeln!(
                    stdout,
                    "Next: export {api_key_env}=… in your shell, then run `otto`."
                );
            }
            0
        }
        Err(message) => fail(stderr, &message),
    }
}

fn ask_choice(
    stdin: &mut (dyn BufRead + Send),
    stdout: &mut (dyn Write + Send),
    prompt: &str,
    choices: &[&str],
    default: &str,
) -> Option<String> {
    loop {
        let _ = write!(stdout, "{prompt}");
        let answer = read_line(stdin)?;
        let answer = if answer.is_empty() { default } else { &answer };
        if choices.contains(&answer) {
            return Some(answer.to_string());
        }
        let _ = writeln!(stdout, "Choose one of: {}.", choices.join(", "));
    }
}

fn ask_nonempty(
    stdin: &mut (dyn BufRead + Send),
    stdout: &mut (dyn Write + Send),
    prompt: &str,
    default: &str,
    valid: impl Fn(&str) -> bool,
    invalid: &str,
) -> Option<String> {
    loop {
        let _ = write!(stdout, "{prompt}");
        let answer = read_line(stdin)?;
        let answer = if answer.is_empty() {
            default
        } else {
            answer.as_str()
        };
        if !answer.is_empty() && valid(answer) {
            return Some(answer.to_string());
        }
        let _ = writeln!(stdout, "{invalid}");
    }
}

fn read_line(stdin: &mut (dyn BufRead + Send)) -> Option<String> {
    let mut line = String::new();
    match stdin.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first == b'_' || first.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn lookup(home: &std::path::Path) -> HashMap<String, String> {
        HashMap::from([("HOME".to_string(), home.to_string_lossy().into_owned())])
    }

    #[test]
    fn creates_claude_with_native_defaults_without_storing_credentials() {
        let home = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(b"claude\nfixture-claude\n\n\nsave\n".to_vec());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &[],
            &mut input,
            &mut stdout,
            &mut stderr,
            &lookup(home.path()),
        );
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        let text = std::fs::read_to_string(home.path().join(".config/otto/config.toml")).unwrap();
        let file = otto_core::config::parse(&text).unwrap();
        assert_eq!(file.default_model, "claude/fixture-claude");
        assert!(
            text.contains("[providers.claude]\ntype = \"claude\"\nmodels.\"fixture-claude\" = {}"),
            "{text}"
        );
        assert!(!text.contains("api_key_env"), "{text}");
        let runtime = otto_core::config::resolve(
            &file,
            &HashMap::from([("ANTHROPIC_API_KEY".to_string(), "test-value".to_string())]),
            &otto_core::config::SessionDefaults::default(),
            &otto_core::config::Overrides::default(),
        )
        .unwrap();
        assert_eq!(runtime.provider_type, "claude");
        assert_eq!(runtime.model, "fixture-claude");
        assert_eq!(runtime.base_url, "https://api.anthropic.com/");
    }

    #[test]
    fn creates_a_chatgpt_default_model_without_a_secret() {
        let home = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(b"\nmy-model\nsave\n".to_vec());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &[],
            &mut input,
            &mut stdout,
            &mut stderr,
            &lookup(home.path()),
        );
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        let config = std::fs::read_to_string(home.path().join(".config/otto/config.toml")).unwrap();
        assert!(
            config.contains("default_model = \"chatgpt/my-model\""),
            "{config}"
        );
        assert!(
            config.contains("[providers.chatgpt]\ntype = \"chatgpt\"\nmodels.\"my-model\" = {}"),
            "{config}"
        );
        assert!(!config.contains("profile"), "{config}");
        assert!(!config.contains("api_key"), "{config}");
        assert!(!config.contains("subagent_models"), "{config}");
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("otto login"));
        assert!(output.contains("Proposed configuration"));
        assert!(output.contains("[save / cancel] (cancel):"));
    }

    #[test]
    fn scripted_save_confirms_after_preview_without_child_model_prompt() {
        for text in [
            "\nparent\nsave\n",
            "openai-compatible\nparent\nhttps://api.example/v1\nWORK_KEY\nsave\n",
        ] {
            let home = tempfile::tempdir().unwrap();
            let mut input = Cursor::new(text.as_bytes());
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            assert_eq!(
                run(
                    &[],
                    &mut input,
                    &mut stdout,
                    &mut stderr,
                    &lookup(home.path())
                ),
                0,
                "{}",
                String::from_utf8_lossy(&stderr)
            );
            let config =
                std::fs::read_to_string(home.path().join(".config/otto/config.toml")).unwrap();
            assert!(config.contains("/parent\""), "{config}");
            assert!(config.contains("default_model = "), "{config}");
            assert!(!config.contains("subagent_models"), "{config}");
            let output = String::from_utf8(stdout).unwrap();
            assert!(output.contains("Proposed configuration"));
            assert!(output.contains("Created"));
            let preview = output.find("Proposed configuration").unwrap();
            let confirmation = output.find("[save / cancel] (cancel):").unwrap();
            let created = output.find("Created").unwrap();
            assert!(preview < confirmation && confirmation < created);
            assert_eq!(output.matches("[save / cancel] (cancel):").count(), 1);
            assert!(!output.contains("Child model IDs"));
        }
    }

    #[test]
    fn creates_an_openai_compatible_provider_and_model() {
        let home = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(
            b"openai-compatible\ncheap-model\nhttps://api.example/v1\nWORK_KEY\nsave\n".to_vec(),
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(
            run(
                &[],
                &mut input,
                &mut stdout,
                &mut stderr,
                &lookup(home.path())
            ),
            0
        );
        let config = std::fs::read_to_string(home.path().join(".config/otto/config.toml")).unwrap();
        assert!(
            config.contains("default_model = \"openai-compatible/cheap-model\""),
            "{config}"
        );
        assert!(config.contains("[providers.openai-compatible]"), "{config}");
        assert!(config.contains("api_key_env = \"WORK_KEY\""), "{config}");
        assert!(!config.contains("subagent_models"), "{config}");
        assert!(
            config.contains("base_url = \"https://api.example/v1\""),
            "{config}"
        );
    }

    #[test]
    fn only_explicit_save_after_preview_writes_configuration() {
        for text in [
            "\nparent\n",
            "\nparent\ncancel\n",
            "\nparent\ncancel\nsave\n",
            "\nparent\n\n",
            "\nparent\n\ncancel\n",
            "\nparent\n\n\n",
            "\nparent\n\ninvalid\n",
            "\nparent\nchild\n",
            "\nparent\nchild\ncancel\n",
            "\nparent\nchild\n\n",
            "\nparent\nchild\ninvalid\n",
            "\nparent\ninvalid,,list\n",
        ] {
            let home = tempfile::tempdir().unwrap();
            let mut input = Cursor::new(text.as_bytes().to_vec());
            let mut output = Vec::new();
            assert_eq!(
                run(
                    &[],
                    &mut input,
                    &mut output,
                    &mut Vec::new(),
                    &lookup(home.path())
                ),
                0
            );
            assert!(
                !home.path().join(".config/otto/config.toml").exists(),
                "{text:?}"
            );
            let output = String::from_utf8(output).unwrap();
            assert!(
                output.contains("Cancelled; configuration unchanged."),
                "{text:?}"
            );
            assert!(output.contains("Proposed configuration"), "{text:?}");
            assert!(output.contains("[save / cancel] (cancel):"), "{text:?}");
            assert!(!output.contains("Created"), "{text:?}");
        }
    }

    #[test]
    fn end_of_input_leaves_no_configuration() {
        let home = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(Vec::new());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(
            run(
                &[],
                &mut input,
                &mut stdout,
                &mut stderr,
                &lookup(home.path())
            ),
            0
        );
        assert!(!home.path().join(".config/otto/config.toml").exists());
        assert!(String::from_utf8_lossy(&stdout).contains("Cancelled"));
    }
}
