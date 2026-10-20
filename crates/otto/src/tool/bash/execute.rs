//! Command execution, timeout handling and result formatting for the bash tool.

use super::*;

impl BashTool {
    /// Runs `request`, cancelling a private child token once the timeout
    /// elapses. Returns whether the timeout fired alongside the outcome.
    async fn run_with(
        &self,
        executor: &Arc<dyn CommandExecutor>,
        environment: &[String],
        command: &str,
        stdout: &mut (dyn std::io::Write + Send),
        stderr: &mut (dyn std::io::Write + Send),
        cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), Error>, bool) {
        let child = cancel.child_token();
        let streams = Streams { stdout, stderr };
        let execute = executor.execute_with_grace(
            self.request(command, environment),
            streams,
            &child,
            self.cancellation_grace,
        );
        let mut execute = std::pin::pin!(execute);
        let sleep = tokio::time::sleep(self.timeout);
        let mut sleep = std::pin::pin!(sleep);
        let mut timed_out = false;
        let (status, outcome) = loop {
            tokio::select! {
                outcome = &mut execute => break outcome,
                () = &mut sleep, if !timed_out => {
                    timed_out = true;
                    child.cancel();
                }
            }
        };
        (status, outcome, timed_out)
    }

    /// The path taken when no collision-safe marker exists: the command still
    /// runs, but nothing it produced may be described.
    async fn execute_suppressed(&self, command: &str, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return ToolResult::default().cancelled_not_started();
        }
        let mut stdout = std::io::sink();
        let mut stderr = std::io::sink();
        let (_, outcome, timed_out) = self
            .run_with(
                &self.executor,
                &self.environment,
                command,
                &mut stdout,
                &mut stderr,
                cancel,
            )
            .await;
        match outcome {
            Ok(()) => ToolResult::default(),
            Err(Error::Cancelled) if cancel.is_cancelled() => {
                ToolResult::default().cancelled_unknown()
            }
            Err(Error::Cancelled) if timed_out => ToolResult::default().deadline_unknown(),
            Err(Error::Cancelled) => ToolResult::default().cancelled_unknown(),
            Err(_) => ToolResult {
                is_error: true,
                ..ToolResult::default()
            }
            .transport_unknown(),
        }
    }

    pub(super) fn argument_error(&self, message: &str) -> ToolResult {
        if !self.dynamic_content {
            return ToolResult {
                is_error: true,
                ..ToolResult::default()
            }
            .not_started();
        }
        ToolResult {
            content: redact_exact_text(message, &self.redact_values, &self.redaction_marker),
            is_error: true,
            ..ToolResult::default()
        }
        .not_started()
    }

    pub(super) fn result(
        &self,
        stdout: &str,
        stderr: &str,
        status: &ExitStatus,
        summary: &str,
    ) -> ToolResult {
        let mut summary = summary.to_owned();
        if status.signaled && !status.signal.is_empty() {
            summary.push_str("; signal: ");
            summary.push_str(&status.signal);
        }
        let mut denied = String::new();
        if !status.denials.is_empty() {
            denied.push_str("sandbox_denied:\n");
            for denial in &status.denials {
                denied.push_str(&format!("{} {}\n", denial.operation, denial.target));
            }
            if status.denials_omitted > 0 {
                denied.push_str(&format!("[{} more omitted]\n", status.denials_omitted));
            }
        }
        let formatted = format!("{}\n{}\n{denied}{summary}", stdout, stderr);
        ToolResult {
            content: redact_exact_text(&formatted, &self.redact_values, &self.redaction_marker),
            ..ToolResult::default()
        }
    }
}

impl BashTool {
    /// The effect class and command words when this call needs approval for
    /// its effect: approvals are available and it changes external state or
    /// deletes content. An excluded command is gated whatever its
    /// `sandbox_permissions`, because every excluded command runs on the
    /// excluded executor.
    pub(super) fn effect_gate(
        &self,
        args: &BashArgs,
        excluded: bool,
    ) -> Option<(shell::Effect, Vec<String>)> {
        if !args.sandbox_read_path.is_empty()
            || (args.sandbox_permissions != SandboxPermissions::UseDefault && !excluded)
            || !self.dynamic_content
            || self.approvals.is_none()
        {
            return None;
        }
        let parsed = shell::parse(&args.command);
        shell::classify(&parsed).map(|(effect, segment)| (effect, segment.to_vec()))
    }

    pub(super) async fn execute_inner(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
        authorized: bool,
    ) -> ToolResult {
        let args: BashArgs = match decode_strict_json(arguments.get(), &["command"]) {
            Ok(args) => args,
            Err(message) => return self.argument_error(&message),
        };
        if args.command.trim().is_empty() {
            return self.argument_error("missing required argument: command");
        }
        if !args.sandbox_read_path.is_empty() {
            if args.sandbox_permissions != SandboxPermissions::UseDefault {
                return self.argument_error(
                    "sandbox_read_path cannot be combined with elevated execution",
                );
            }
            if args.justification.trim().is_empty() {
                return self.argument_error("justification is required for sandbox read access");
            }
            if !self.dynamic_content || cancel.is_cancelled() {
                return self.argument_error("sandbox read approval is unavailable or cancelled");
            }
            let Some((session_id, approvals)) = &self.approvals else {
                return self.argument_error("sandbox read approval is unavailable");
            };
            let path = Path::new(&args.sandbox_read_path);
            if !path.is_absolute() {
                return self.argument_error("sandbox_read_path must be an absolute existing path");
            }
            let Ok(path) = path.canonicalize() else {
                return self.argument_error("sandbox_read_path must be an absolute existing path");
            };
            let Some(path) = path.to_str() else {
                return self.argument_error("sandbox_read_path must be valid UTF-8");
            };
            if !authorized && !approvals.take_read_path(session_id, &args.command, path) {
                let id = approvals.request_read_path(session_id, &args.command, path);
                let command = serde_json::to_string(&args.command).expect("string encodes");
                let justification =
                    serde_json::to_string(&args.justification).expect("string encodes");
                let path = serde_json::to_string(path).expect("string encodes");
                return self.argument_error(&format!(
                    "approval required for persistent sandbox read access.\n\
Approve in Otto: /approve {id}\n\
Command: {command}\n\
Justification: {justification}\n\
Read path: {path}\n\
Approval saves the path to read_paths and reloads the sandbox. The command did not run. \
Only the user or their session approval mode can approve; do not run /approve in a shell. After approval retry this exact Bash call inside the sandbox."
                ));
            }
        }
        if !self.dynamic_content {
            return self.execute_suppressed(&args.command, cancel).await;
        }
        // An excluded command needs no escalation grant and never consumes
        // one; a classified excluded command still needs the effect approval.
        let excluded = self.excluded.as_ref().filter(|excluded| {
            args.sandbox_read_path.is_empty() && excluded.matches(&args.command)
        });
        if !authorized && let Some((effect, segment)) = self.effect_gate(&args, excluded.is_some())
        {
            let what = match effect {
                shell::Effect::ExternalState => "changes external state",
                shell::Effect::Destructive => "deletes workspace or repository content",
            };
            let segment = serde_json::to_string(&segment.join(" ")).expect("string encodes");
            return self.argument_error(&format!(
                "approval required: this command {what} ({segment}).\n\
The command did not run. This tool call has no approval flow; ask the user to run the command or to approve it in a session with approvals."
            ));
        }
        let (executor, environment) = match (excluded, args.sandbox_permissions) {
            (Some(excluded), _) => (&excluded.executor, excluded.environment.as_slice()),
            (None, SandboxPermissions::UseDefault) => (&self.executor, self.environment.as_slice()),
            (None, SandboxPermissions::RequireEscalated) => {
                if args.justification.trim().is_empty() {
                    return self.argument_error("justification is required for elevated execution");
                }
                let Some((session_id, approvals)) = &self.approvals else {
                    return self.argument_error("elevated execution is unavailable");
                };
                if !authorized && !approvals.take(session_id, &args.command) {
                    let id = approvals.request(session_id, &args.command);
                    let command = serde_json::to_string(&args.command).expect("string encodes");
                    let justification =
                        serde_json::to_string(&args.justification).expect("string encodes");
                    return self.argument_error(&format!(
                        "approval required for unsandboxed bash execution.\n\
Approve in Otto: /approve {id}\n\
Command: {command}\n\
Justification: {justification}\n\
The command did not run. Only the user or their session approval mode can approve it in Otto; do not run \
/approve in a shell. Ask the user to review the command above, then retry \
this exact elevated Bash command after approval."
                    ));
                }
                (&approvals.executor, approvals.environment.as_slice())
            }
        };
        if cancel.is_cancelled() {
            return self
                .result(
                    "stdout:\n",
                    "stderr:\n",
                    &ExitStatus::default(),
                    "status: cancelled",
                )
                .cancelled_not_started();
        }

        let mut stdout = RedactingCollector::with_collector(
            Output::with_logs(
                Arc::clone(&self.output_root),
                self.max_output_bytes.min(PREVIEW_BYTES),
                Arc::clone(&self.output_logs),
            ),
            &self.redact_values,
            &self.redaction_marker,
        );
        let mut stderr = RedactingCollector::with_collector(
            Output::with_logs(
                Arc::clone(&self.output_root),
                self.max_output_bytes.min(PREVIEW_BYTES),
                Arc::clone(&self.output_logs),
            ),
            &self.redact_values,
            &self.redaction_marker,
        );
        let (status, outcome, timed_out) = self
            .run_with(
                executor,
                environment,
                &args.command,
                &mut stdout,
                &mut stderr,
                cancel,
            )
            .await;
        if let Err(ref error) = outcome
            && *error != Error::Cancelled
        {
            return infrastructure_result(error);
        }
        stdout.flush();
        stderr.flush();
        stdout.collector_mut().finish();
        stderr.collector_mut().finish();

        let summary = if cancel.is_cancelled() {
            "status: cancelled".to_owned()
        } else if timed_out {
            format!("status: timed out after {}", go_duration(self.timeout))
        } else if outcome.is_err() {
            "status: cancelled".to_owned()
        } else {
            format!("exit_code: {}", status.code)
        };
        let result = self.result(
            &stdout.collector().render("stdout"),
            &stderr.collector().render("stderr"),
            &status,
            &summary,
        );
        if cancel.is_cancelled() {
            result.cancelled_unknown()
        } else if timed_out {
            result.deadline_unknown()
        } else if outcome.is_err() {
            result.cancelled_unknown()
        } else {
            result
        }
    }
}
