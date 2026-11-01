//! `/model`, `/thinking` and `/sandbox` commands.

use super::*;

impl<'a> Repl<'a> {
    pub(super) async fn model(&mut self, args: &str) -> Result<(), Error> {
        let unavailable = || Error::Command {
            command: "/model".to_string(),
            message: MODEL_SWITCH_UNAVAILABLE.to_string(),
        };
        if !self.controller.dynamic_content() {
            return Err(unavailable());
        }
        let parsed = parse_model_args(args).map_err(|message| Error::Command {
            command: "/model".to_string(),
            message,
        })?;
        if parsed.model.is_empty() {
            let info = self.controller.info();
            let _ = writeln!(
                self.stdout,
                "Model {} (type {}, thinking {})",
                info.model_selector,
                info.provider_type,
                display_thinking(&info.thinking)
            );
            let models = self.controller.model_summaries();
            if models.is_empty() {
                let _ = writeln!(self.stdout, "No models configured.");
            } else {
                let list = models
                    .iter()
                    .map(|model| {
                        format!(
                            "{} (thinking {})",
                            model.selector,
                            display_thinking(&model.thinking)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(self.stdout, "Configured models: {list}");
            }
            return Ok(());
        }
        self.controller
            .switch_model(&parsed.model)
            .await
            .map_err(|message| Error::Command {
                command: "/model".to_string(),
                message,
            })?;
        if !parsed.thinking.is_empty() {
            self.controller
                .set_thinking(&parsed.thinking)
                .await
                .map_err(|message| Error::Command {
                    command: "/model".to_string(),
                    message,
                })?;
        }
        let saved = self.controller.set_default_model(&parsed.model);
        if parsed.save && !parsed.thinking.is_empty() {
            self.controller
                .save_model_thinking(&parsed.thinking)
                .map_err(|message| Error::Command {
                    command: "/model".to_string(),
                    message,
                })?;
        }
        let info = self.controller.info();
        match saved {
            Ok(()) => {
                let _ = writeln!(
                    self.stdout,
                    "Switched to model {} (type {}, thinking {}). Set as default model.",
                    info.model_selector,
                    info.provider_type,
                    display_thinking(&info.thinking)
                );
            }
            Err(message) => {
                let _ = writeln!(
                    self.stdout,
                    "Switched to model {} (type {}, thinking {}), but the default model was not saved: {}",
                    info.model_selector,
                    info.provider_type,
                    display_thinking(&info.thinking),
                    message
                );
            }
        }
        if parsed.save && !parsed.thinking.is_empty() {
            let _ = writeln!(self.stdout, "Saved thinking to model.");
        }
        if !info.session_id.is_empty() {
            let _ = writeln!(self.stdout, "Session: {}", info.session_id);
        }
        Ok(())
    }

    pub(super) async fn thinking(&mut self, args: &str) -> Result<(), Error> {
        let parsed = parse_thinking_args(args).map_err(|message| Error::Command {
            command: "/thinking".to_string(),
            message,
        })?;
        if parsed.thinking.is_empty() {
            let info = self.controller.info();
            let _ = writeln!(
                self.stdout,
                "Thinking: {}",
                display_thinking(&info.thinking)
            );
            return Ok(());
        }
        self.controller
            .set_thinking(&parsed.thinking)
            .await
            .map_err(|message| Error::Command {
                command: "/thinking".to_string(),
                message,
            })?;
        if parsed.save {
            self.controller
                .save_model_thinking(&parsed.thinking)
                .map_err(|message| Error::Command {
                    command: "/thinking".to_string(),
                    message,
                })?;
        }
        let info = self.controller.info();
        let _ = writeln!(
            self.stdout,
            "Thinking: {}",
            display_thinking(&info.thinking)
        );
        if parsed.save {
            let _ = writeln!(self.stdout, "Saved thinking to model.");
        }
        Ok(())
    }

    /// False means "unknown command".
    ///
    /// `allow`, `network` and `exclude` write the `[sandbox]` table and reload it, the
    /// same amendment the TUI confirms through a picker; here the typed
    /// command is the confirmation.
    pub(super) async fn sandbox(&mut self, args: &str) -> Result<bool, Error> {
        let (subcommand, rest) = match args.split_once(char::is_whitespace) {
            Some((subcommand, rest)) => (subcommand, rest.trim()),
            None => (args, ""),
        };
        match (subcommand, rest) {
            ("", _) => {
                self.print_sandbox(self.controller.sandbox_info());
                Ok(true)
            }
            ("reload", "") => {
                let info = self
                    .controller
                    .reload_sandbox()
                    .await
                    .map_err(sandbox_error)?;
                self.print_sandbox(info);
                Ok(true)
            }
            ("allow", path) => {
                let resolved = self
                    .controller
                    .resolve_sandbox_read_path(path)
                    .map_err(sandbox_error)?;
                let info = self
                    .controller
                    .amend_sandbox(SandboxChange::AllowReadPath(resolved))
                    .await
                    .map_err(sandbox_error)?;
                self.print_sandbox(info);
                Ok(true)
            }
            ("network", mode @ ("allow" | "deny")) => {
                let info = self
                    .controller
                    .amend_sandbox(SandboxChange::Network(mode.to_string()))
                    .await
                    .map_err(sandbox_error)?;
                self.print_sandbox(info);
                Ok(true)
            }
            ("exclude", entry) => {
                let entry = parse_exclude_entry(entry).map_err(sandbox_error)?;
                let info = self
                    .controller
                    .amend_sandbox(SandboxChange::ExcludeCommand(entry))
                    .await
                    .map_err(sandbox_error)?;
                self.print_sandbox(info);
                Ok(true)
            }
            _ => {
                let _ = writeln!(self.stderr, "unknown command: /sandbox {args}");
                Ok(true)
            }
        }
    }

    fn print_sandbox(&mut self, info: crate::runtime::info::SandboxInfo) {
        let _ = writeln!(self.stdout, "Sandbox: {}", info.summary());
        let reason = info.reason_code();
        if !reason.is_empty() {
            let _ = writeln!(self.stdout, "Sandbox reason: {reason}");
        }
    }
}

pub(super) fn display_thinking(thinking: &str) -> &str {
    if thinking.is_empty() {
        "default"
    } else {
        thinking
    }
}

#[derive(Default)]
struct ParsedThinkingArgs {
    thinking: String,
    save: bool,
}

#[derive(Default)]
struct ParsedModelArgs {
    model: String,
    thinking: String,
    save: bool,
}

fn parse_thinking_args(args: &str) -> Result<ParsedThinkingArgs, String> {
    let mut parsed = ParsedThinkingArgs::default();
    for part in args.split_whitespace() {
        if part == "--save" {
            parsed.save = true;
        } else if parsed.thinking.is_empty() {
            parsed.thinking = part.to_string();
        } else {
            return Err("usage: /thinking [LEVEL] [--save]".to_string());
        }
    }
    Ok(parsed)
}

fn parse_model_args(args: &str) -> Result<ParsedModelArgs, String> {
    let mut parsed = ParsedModelArgs::default();
    let mut parts = args.split_whitespace();
    while let Some(part) = parts.next() {
        match part {
            "--save" => parsed.save = true,
            "--thinking" => {
                let Some(level) = parts.next() else {
                    return Err("--thinking requires a level".to_string());
                };
                parsed.thinking = level.to_string();
            }
            value if parsed.model.is_empty() => parsed.model = value.to_string(),
            _ => {
                return Err(
                    "usage: /model [provider/model_id] [--thinking LEVEL] [--save]".to_string(),
                );
            }
        }
    }
    Ok(parsed)
}

/// Whether the error is the store's fatal-persistence failure.
///
/// ponytail: the kind does not survive `SessionError::Persist(String)`, so this
/// matches the sentinel text the store prefixes onto the message. A typed flag
/// on `SessionError` would be the upgrade, in `otto-core`.
/// A `/sandbox` failure in the shape the REPL reports command failures.
pub(super) fn sandbox_error(message: String) -> Error {
    Error::Command {
        command: "/sandbox".to_string(),
        message,
    }
}
