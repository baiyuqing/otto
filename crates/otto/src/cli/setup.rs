//! `otto setup`: create an initial non-secret provider profile.
//!
//! This command intentionally runs before provider resolution: a first-time
//! user cannot ask a model how to configure the model needed to start it.
//! It creates only a missing configuration file, records no credentials, and
//! leaves shell permissions at Otto's safe built-in defaults.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use otto_core::config::{PROVIDER_CHATGPT, PROVIDER_OPENAI_COMPATIBLE};

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
        "Otto setup creates a default profile without storing API keys. Shell commands remain sandboxed by default."
    );
    let Some(provider) = ask_choice(
        stdin,
        stdout,
        "Provider [chatgpt / openai-compatible] (chatgpt): ",
        &[PROVIDER_CHATGPT, PROVIDER_OPENAI_COMPATIBLE],
        PROVIDER_CHATGPT,
    ) else {
        let _ = writeln!(stdout, "Cancelled; configuration unchanged.");
        return 0;
    };
    let default_profile = if provider == PROVIDER_CHATGPT {
        "chatgpt"
    } else {
        "default"
    };
    let Some(profile) = ask_nonempty(
        stdin,
        stdout,
        &format!("Profile name ({default_profile}): "),
        default_profile,
        valid_profile_name,
        "Profile names use letters, digits, hyphens, and underscores.",
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

    let (base_url, api_key_env) = if provider == PROVIDER_OPENAI_COMPATIBLE {
        let Some(base_url) = ask_nonempty(
            stdin,
            stdout,
            "Base URL (required): ",
            "",
            |_| true,
            "A base URL is required.",
        ) else {
            let _ = writeln!(stdout, "Cancelled; configuration unchanged.");
            return 0;
        };
        let Some(api_key_env) = ask_nonempty(
            stdin,
            stdout,
            "API key environment variable (OPENAI_API_KEY): ",
            "OPENAI_API_KEY",
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

    let Some(child_model_choice) = ask_subagent_models(stdin, stdout) else {
        let _ = writeln!(stdout, "Cancelled; configuration unchanged.");
        return 0;
    };
    let (subagent_models, save_confirmed) = match child_model_choice {
        ChildModelChoice::Preview(models) => (models, false),
        ChildModelChoice::Save => (Vec::new(), true),
    };
    let content = match crate::config::initial_profile_content(
        &profile,
        &provider,
        &model,
        &base_url,
        &api_key_env,
        &subagent_models,
    ) {
        Ok(content) => content,
        Err(message) => return fail(stderr, &message),
    };
    let _ = writeln!(
        stdout,
        "\nProposed configuration for {}:\n\n{content}",
        path.display()
    );
    if !save_confirmed {
        let _ = write!(stdout, "\n[save / cancel] (cancel): ");
        if read_line(stdin).as_deref() != Some("save") {
            let _ = writeln!(stdout, "Cancelled; configuration unchanged.");
            return 0;
        }
    }

    match crate::config::create_initial_profile(
        &path,
        &profile,
        &provider,
        &model,
        &base_url,
        &api_key_env,
        &subagent_models,
    ) {
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

#[derive(Debug, PartialEq)]
enum ChildModelChoice {
    Preview(Vec<String>),
    Save,
}

fn ask_subagent_models(
    stdin: &mut (dyn BufRead + Send),
    stdout: &mut (dyn Write + Send),
) -> Option<ChildModelChoice> {
    let _ = writeln!(
        stdout,
        "Optional child model IDs for this provider connection, in preferred order. Check your provider's model list and pricing; Otto does not infer availability or cost. Blank inherits the parent model; this is not an automatic retry list. Enter a list or leave blank to preview before confirming. Enter save to save now with the parent model, or cancel to exit without writing. EOF always cancels."
    );
    loop {
        let _ = write!(
            stdout,
            "Child model IDs (comma-separated, optional) or save / cancel: "
        );
        let answer = read_line(stdin)?;
        match answer.as_str() {
            "save" => return Some(ChildModelChoice::Save),
            "cancel" => return None,
            "" => return Some(ChildModelChoice::Preview(Vec::new())),
            _ => {}
        }
        let models: Vec<String> = answer.split(',').map(|id| id.trim().to_string()).collect();
        let mut seen = std::collections::HashSet::new();
        if models.len() <= 16
            && models.iter().all(|id| {
                !id.is_empty()
                    && id.len() <= 256
                    && !id.chars().any(char::is_control)
                    && seen.insert(id)
            })
        {
            return Some(ChildModelChoice::Preview(models));
        }
        let _ = writeln!(
            stdout,
            "Enter at most 16 distinct, nonempty model IDs (at most 256 bytes each, no control characters), leave blank to preview, or enter save / cancel."
        );
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

fn valid_profile_name(name: &str) -> bool {
    name.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
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
    fn creates_a_chatgpt_profile_without_a_secret() {
        let home = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(b"\n\nmy-model\n\nsave\n".to_vec());
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
        assert!(config.contains("default_profile = \"chatgpt\""), "{config}");
        assert!(config.contains("provider = \"chatgpt\""), "{config}");
        assert!(config.contains("model = \"my-model\""), "{config}");
        assert!(!config.contains("api_key"), "{config}");
        assert!(!config.contains("subagent_models"), "{config}");
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("otto login"));
        assert!(output.contains("Proposed configuration"));
        assert!(output.contains("[save / cancel] (cancel):"));
    }

    #[test]
    fn legacy_scripted_save_inherits_parent_without_another_confirmation() {
        for text in [
            "\n\nparent\nsave\n",
            "openai-compatible\nwork\nparent\nhttps://api.example/v1\nWORK_KEY\nsave\n",
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
            assert!(config.contains("model = \"parent\""), "{config}");
            assert!(!config.contains("subagent_models"), "{config}");
            let output = String::from_utf8(stdout).unwrap();
            assert!(output.contains("Proposed configuration"));
            assert!(output.contains("Created"));
            assert!(!output.contains("[save / cancel] (cancel):"));
        }
    }

    #[test]
    fn creates_an_openai_compatible_profile() {
        let home = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(
            b"openai-compatible\nwork\ncheap-model\nhttps://api.example/v1\nWORK_KEY\nfirst-child, second-child\nsave\n"
                .to_vec(),
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
        assert!(config.contains("api_key_env = \"WORK_KEY\""), "{config}");
        let parsed = otto_core::config::parse(&config).unwrap();
        assert_eq!(
            parsed.profiles["work"].subagent_models,
            ["first-child", "second-child"]
        );
        assert!(
            config.contains("base_url = \"https://api.example/v1\""),
            "{config}"
        );
    }

    #[test]
    fn child_model_input_reprompts_without_echoing_invalid_values() {
        let invalid = [
            "duplicate,duplicate".to_string(),
            "first,,last".into(),
            "bad\u{001b}id".into(),
            "x".repeat(257),
            (0..17)
                .map(|n| format!("id-{n}"))
                .collect::<Vec<_>>()
                .join(","),
        ];
        for answer in invalid {
            let mut input = Cursor::new(format!("{answer}\n first , second \n").into_bytes());
            let mut output = Vec::new();
            assert_eq!(
                ask_subagent_models(&mut input, &mut output).unwrap(),
                ChildModelChoice::Preview(vec!["first".into(), "second".into()])
            );
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("at most 16 distinct"));
            assert!(!output.contains(&answer));
        }
    }

    #[test]
    fn chatgpt_child_models_are_persisted_in_order() {
        let home = tempfile::tempdir().unwrap();
        let mut input = Cursor::new(b"\n\nparent\nchild-b,child-a\nsave\n".to_vec());
        assert_eq!(
            run(
                &[],
                &mut input,
                &mut Vec::new(),
                &mut Vec::new(),
                &lookup(home.path())
            ),
            0
        );
        let text = std::fs::read_to_string(home.path().join(".config/otto/config.toml")).unwrap();
        assert_eq!(
            otto_core::config::parse(&text).unwrap().profiles["chatgpt"].subagent_models,
            ["child-b", "child-a"]
        );
    }

    #[test]
    fn eof_or_cancel_at_child_model_setup_does_not_write() {
        for text in [
            "\n\nparent\n",
            "\n\nparent\ncancel\n",
            "\n\nparent\ncancel\nsave\n",
            "\n\nparent\n\n",
            "\n\nparent\n\ncancel\n",
            "\n\nparent\n\n\n",
            "\n\nparent\n\ninvalid\n",
            "\n\nparent\nchild\n",
            "\n\nparent\nchild\ncancel\n",
            "\n\nparent\nchild\n\n",
            "\n\nparent\nchild\ninvalid\n",
            "\n\nparent\ninvalid,,list\n",
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
