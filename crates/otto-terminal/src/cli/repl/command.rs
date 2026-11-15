//! Slash-command dispatch.

use super::*;

impl<'a> Repl<'a> {
    /// Dispatches one `/command`. Returns true when the loop should stop.
    pub(super) async fn command(
        &mut self,
        command: &str,
        cancel: &CancellationToken,
    ) -> Result<bool, Error> {
        let outcome: Option<bool> = 'dispatch: {
            let Some((name, args)) = split_command(command) else {
                break 'dispatch None;
            };
            let delete_confirmation = self.delete_confirmation.take();
            match name {
                "help" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    let _ = write!(self.stdout, "{HELP}");
                    Some(false)
                }
                "init" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    self.prompt(otto_core::agent::INIT_PROMPT, cancel).await?;
                    Some(false)
                }
                "exit" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    Some(true)
                }
                "new" | "clear" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    self.controller
                        .new_session()
                        .await
                        .map_err(|message| Error::Command {
                            command: command.to_string(),
                            message,
                        })?;
                    self.print_session_id();
                    Some(false)
                }
                "delete" => {
                    if args.is_empty() {
                        self.delete_confirmation = Some(self.controller.info().session_id);
                        let _ = writeln!(
                            self.stdout,
                            "Permanently delete session {}? This cannot be undone. Default: Cancel. Use /delete confirm to delete; any other input cancels.",
                            self.controller.info().session_id
                        );
                    } else if args == "confirm" {
                        if delete_confirmation.as_deref()
                            != Some(self.controller.info().session_id.as_str())
                        {
                            return Err(Error::Command {
                                command: command.into(),
                                message:
                                    "Run /delete first to review the irreversible deletion warning."
                                        .into(),
                            });
                        }
                        self.controller
                            .delete_current_session()
                            .await
                            .map_err(|message| Error::Command {
                                command: command.to_string(),
                                message,
                            })?;
                        let _ = writeln!(self.stdout, "Session permanently deleted.");
                        self.print_session_id();
                    } else if args != "cancel" {
                        break 'dispatch None;
                    }
                    Some(false)
                }
                "archive" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    let result =
                        self.controller
                            .archive_current_session()
                            .await
                            .map_err(|message| Error::Command {
                                command: command.to_string(),
                                message,
                            })?;
                    let _ = writeln!(self.stdout, "Archived: {}", result.path);
                    self.print_session_id();
                    Some(false)
                }
                "session" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    let info = self.controller.info();
                    let _ = writeln!(
                        self.stdout,
                        "ID: {}\nPath: {}\nProvider: {}\nModel: {}\nThinking: {}\nSandbox: {}",
                        info.session_id,
                        info.session_path,
                        info.provider,
                        info.model,
                        display_thinking(&info.thinking),
                        info.sandbox.summary()
                    );
                    if !info.session_name.is_empty() {
                        let _ = writeln!(self.stdout, "Name: {}", info.session_name);
                    }
                    let reason = info.sandbox.reason_code();
                    if !reason.is_empty() {
                        let _ = writeln!(self.stdout, "Sandbox reason: {reason}");
                    }
                    Some(false)
                }
                "rename" => {
                    if args.is_empty() {
                        break 'dispatch None;
                    }
                    self.controller
                        .rename_session(args)
                        .map_err(|message| Error::Command {
                            command: command.to_string(),
                            message,
                        })?;
                    let _ = writeln!(self.stdout, "Renamed session: {args}");
                    Some(false)
                }
                "compact" => {
                    self.compact(args, cancel).await?;
                    Some(false)
                }
                "model" => {
                    self.model(args).await?;
                    Some(false)
                }
                "thinking" => {
                    self.thinking(args).await?;
                    Some(false)
                }
                "auto" | "permissions" => {
                    let line = self
                        .controller
                        .permissions_command(args)
                        .map_err(|message| Error::Command {
                            command: "/permissions".into(),
                            message,
                        })?;
                    let _ = writeln!(self.stdout, "{line}");
                    Some(false)
                }
                "sandbox" => self.sandbox(args).await?.then_some(false),
                "approve" | "deny" | "withdraw" => {
                    let mut parts = args.trim().splitn(2, char::is_whitespace);
                    let shared_id = parts.next().unwrap_or("");
                    let feedback = parts.next().unwrap_or("").trim();
                    let scope = self.controller.approval_scope();
                    let service = self.controller.approval_service();
                    if let Ok(request) =
                        service.get(shared_id, &scope.workspace, Some(&scope.session_id))
                    {
                        if name == "approve" && feedback == "always" && !request.persistent {
                            return Err(Error::Command { command: "/approve".into(), message: "This request grants only one operation; persistent changes need their own review.".into() });
                        }
                        let decided = if name == "withdraw" {
                            service.withdraw(shared_id, &request.scope, request.revision)
                        } else {
                            service.decide(
                                shared_id,
                                &request.scope,
                                request.revision,
                                name == "approve",
                                feedback,
                                &otto::approval::Actor::local("repl"),
                            )
                        }
                        .map_err(|error| Error::Command {
                            command: format!("/{name}"),
                            message: error.to_string(),
                        })?;
                        let decided = service.finish_decision(decided).await.map_err(|error| {
                            Error::Command {
                                command: format!("/{name}"),
                                message: error.to_string(),
                            }
                        })?;
                        let _ = writeln!(
                            self.stdout,
                            "Approval {shared_id}: {:?}; application: {:?}. {}",
                            decided.state, decided.application, decided.application_error
                        );
                        break 'dispatch Some(false);
                    }
                    if name != "approve" {
                        return Err(Error::Command {
                            command: format!("/{name}"),
                            message: "approval not found".into(),
                        });
                    }
                    let (id, always) = match args.split_whitespace().collect::<Vec<_>>()[..] {
                        [id] => (id, false),
                        [id, "always"] => (id, true),
                        _ => break 'dispatch None,
                    };
                    let approved = if always {
                        self.controller.approve_bash_always(id).await
                    } else {
                        self.controller.approve_tool(id).await
                    };
                    let retry = approved.map_err(|message| Error::Command {
                        command: "/approve".to_string(),
                        message,
                    })?;
                    let _ = writeln!(self.stdout, "Approved {id}.");
                    self.prompt(&retry, cancel).await?;
                    Some(false)
                }
                "login" => {
                    crate::cli::login::repl_login(
                        self.controller,
                        &mut *self.stdout,
                        &mut *self.stderr,
                        args,
                        cancel,
                    )
                    .await?;
                    Some(false)
                }
                "logout" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    crate::cli::login::repl_logout(self.controller, &mut *self.stdout, cancel)
                        .await?;
                    Some(false)
                }
                "mcp" => {
                    crate::cli::repl_commands::repl_mcp_command(
                        self.controller,
                        args,
                        &mut *self.stdout,
                        &mut *self.stderr,
                        cancel,
                    )
                    .await?;
                    Some(false)
                }
                "memory" => {
                    self.memory_command(args)?;
                    Some(false)
                }
                "remember" => {
                    self.remember_command(args)?;
                    Some(false)
                }
                "skill" => {
                    self.skill_command(args);
                    Some(false)
                }
                "tasks" => {
                    self.tasks_command();
                    Some(false)
                }
                "task" => {
                    self.task_command(args);
                    Some(false)
                }
                "agents" => {
                    if !args.is_empty() {
                        break 'dispatch None;
                    }
                    self.agents_command();
                    Some(false)
                }
                "timers" => {
                    self.timers_command(args);
                    Some(false)
                }
                _ => None,
            }
        };
        match outcome {
            Some(exit) => Ok(exit),
            None => {
                let _ = writeln!(self.stderr, "unknown command: {command}");
                Ok(false)
            }
        }
    }
}
