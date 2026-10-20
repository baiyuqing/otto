//! The `Tool` trait implementation for the bash tool.

use super::*;

#[async_trait::async_trait]
impl Tool for BashTool {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        BASH_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        if self.approvals.is_some() {
            bash_definition_with_approvals(self.private_home)
        } else {
            bash_definition(self.private_home)
        }
    }

    fn approval_spec(
        &self,
        arguments: &RawValue,
    ) -> Result<Option<super::super::ApprovalSpec>, ToolResult> {
        let args: BashArgs = decode_strict_json(arguments.get(), &["command"])
            .map_err(|e| self.argument_error(&e))?;
        let excluded = self
            .excluded
            .as_ref()
            .is_some_and(|excluded| excluded.matches(&args.command));
        let gate = self.effect_gate(&args, excluded);
        if args.sandbox_read_path.is_empty()
            && (args.sandbox_permissions == SandboxPermissions::UseDefault || excluded)
            && gate.is_none()
        {
            return Ok(None);
        }
        if !self.dynamic_content
            || self.approvals.is_none()
            || args.command.trim().is_empty()
            || (gate.is_none() && args.justification.trim().is_empty())
        {
            return Err(
                self.argument_error("approval is unavailable or command/justification is missing")
            );
        }
        let read_path = if args.sandbox_read_path.is_empty() {
            String::new()
        } else {
            if args.sandbox_permissions != SandboxPermissions::UseDefault
                || self.grant_control.is_none()
            {
                return Err(self.argument_error("persistent sandbox read approval is unavailable"));
            }
            let path = Path::new(&args.sandbox_read_path);
            if !path.is_absolute() {
                return Err(self.argument_error("read path must be absolute"));
            }
            path.canonicalize()
                .map_err(|_| self.argument_error("read path is unavailable"))?
                .to_str()
                .ok_or_else(|| self.argument_error("read path must be UTF-8"))?
                .to_string()
        };
        let config_version = if let Some((config, _)) = &self.grant_control {
            let bytes = match std::fs::read(config) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
                Err(_) => return Err(self.argument_error("configuration is unavailable")),
            };
            format!("{:x}", sha2::Sha256::digest(&bytes))
        } else {
            String::new()
        };
        let facts = self.review_facts(&args.command);
        let version=serde_json::json!({"arguments":crate::approval::digest(arguments.get()),"config":config_version,"path":read_path,"facts":crate::approval::digest(&facts.to_string())}).to_string();
        // One escalation leaves the sandbox for a single command; a read path
        // is a persistent grant outside the workspace; a gated command runs
        // in the sandbox after one approval, and a gated excluded command
        // runs outside the sandbox after one approval.
        let (authority, title) = match &gate {
            Some((shell::Effect::ExternalState, _)) => (
                otto_core::approval::Authority::Boundary,
                "Run a command that changes external state?",
            ),
            Some((shell::Effect::Destructive, _)) => (
                otto_core::approval::Authority::Bounded,
                "Run a destructive workspace command?",
            ),
            None if read_path.is_empty() => (
                otto_core::approval::Authority::Boundary,
                "Run an unsandboxed command?",
            ),
            None => (
                otto_core::approval::Authority::Consent,
                "Save persistent sandbox read access?",
            ),
        };
        let mut details = serde_json::json!({"command":args.command,"workspace":self.workspace_root,"read_path":read_path,"native_facts":facts});
        let mut reason = args.justification;
        if let Some((effect, segment)) = &gate {
            details["sandboxed"] = (!excluded).into();
            details["effect"] = serde_json::json!({"class":effect.as_str(),"segment":segment});
            if reason.trim().is_empty() {
                reason = match effect {
                    shell::Effect::ExternalState => {
                        format!("Changes external state: {}", segment.join(" "))
                    }
                    shell::Effect::Destructive => {
                        format!(
                            "Deletes workspace or repository content: {}",
                            segment.join(" ")
                        )
                    }
                };
            }
        }
        Ok(Some(super::super::ApprovalSpec {
            version,
            source: "sandbox".into(),
            authority,
            title: title.into(),
            reason,
            persistent: !read_path.is_empty(),
            details: details.to_string(),
        }))
    }

    async fn approval_spec_async(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
    ) -> Result<Option<super::super::ApprovalSpec>, ToolResult> {
        let Some(mut spec) = self.approval_spec(arguments)? else {
            return Ok(None);
        };
        let args: BashArgs = decode_strict_json(arguments.get(), &["command"])
            .map_err(|error| self.argument_error(&error))?;
        let analysis = shell::analyze(&args.command);
        if analysis
            .segments
            .iter()
            .flatten()
            .any(|word| shell::is_program(word, "git"))
        {
            let target = match self.git_review_target(&args.command) {
                Ok(Some(target)) => target,
                _ => {
                    let mut details: serde_json::Value = serde_json::from_str(&spec.details)
                        .map_err(|_| self.argument_error("invalid prepared Bash details"))?;
                    details["native_facts"]["git"]["target"] = json!({"unavailable":true});
                    spec.details = details.to_string();
                    spec.version =
                        crate::approval::digest(&format!("{}:{}", spec.version, spec.details));
                    return Ok(Some(spec));
                }
            };
            let child = cancel.child_token();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            let commands: [(&str, &[&str]); 4] = [
                ("branch", &["rev-parse", "--abbrev-ref", "HEAD"]),
                ("remotes", &["remote", "-v"]),
                (
                    "working_diff",
                    &[
                        "--no-pager",
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                        "HEAD",
                    ],
                ),
                (
                    "outgoing_diff",
                    &[
                        "--no-pager",
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                        "@{upstream}",
                        "HEAD",
                    ],
                ),
            ];
            let mut git = serde_json::Map::new();
            git.insert("cwd".into(), json!(target));
            for (name, argv) in commands {
                let body = crate::runtime::workspace_context::run_git(
                    &self.executor,
                    &target.to_string_lossy(),
                    &self.environment,
                    argv,
                    deadline,
                    &child,
                )
                .await;
                let body = body
                    .filter(|body| body.len() <= 12 * 1024)
                    .and_then(|body| self.redact_review_fact(&body));
                git.insert(
                    name.into(),
                    body.map_or(
                        json!({"unavailable":true}),
                        |body| json!({"body":body,"sha256":crate::approval::digest(&body)}),
                    ),
                );
            }
            if git["outgoing_diff"]["unavailable"] == true
                && let Some((body, base)) = self
                    .first_push_diff(&target, &args.command, deadline, &child)
                    .await
            {
                git.insert(
                    "outgoing_diff".into(),
                    json!({"sha256":crate::approval::digest(&body),"body":body}),
                );
                git.insert("chosen_base".into(), base);
            }
            let mut details: serde_json::Value = serde_json::from_str(&spec.details)
                .map_err(|_| self.argument_error("invalid prepared Bash details"))?;
            // Worktree gitfiles make .git/HEAD and .git/config unavailable;
            // the native sandboxed git queries supply those canonical facts.
            if git["branch"]["unavailable"] != true
                && git["remotes"]["unavailable"] != true
                && let Some(files) = details
                    .pointer_mut("/native_facts/files")
                    .and_then(|files| files.as_array_mut())
            {
                files.retain(|file| {
                    !file["path"]
                        .as_str()
                        .is_some_and(|path| path.starts_with(".git/"))
                });
            }
            details["native_facts"]["git"] = serde_json::Value::Object(git);
            spec.details = details.to_string();
            spec.version = crate::approval::digest(&format!("{}:{}", spec.version, spec.details));
        }
        Ok(Some(spec))
    }

    async fn apply_approval(
        &self,
        arguments: &RawValue,
        cancel: &CancellationToken,
        request: &otto_core::approval::Request,
    ) -> Result<(), String> {
        let args: BashArgs =
            serde_json::from_str(arguments.get()).map_err(|_| "invalid Bash arguments")?;
        self.approvals.as_ref().ok_or("approval unavailable")?;
        if cancel.is_cancelled() {
            return Err("approval operation cancelled".into());
        }
        if !args.sandbox_read_path.is_empty() {
            let path = Path::new(&args.sandbox_read_path)
                .canonicalize()
                .map_err(|_| "read path unavailable")?;
            let path = path.to_str().ok_or("read path is not UTF-8")?;
            let (config, control) = self
                .grant_control
                .as_ref()
                .ok_or("sandbox reload unavailable")?;
            let expected: serde_json::Value = serde_json::from_str(&request.subject.version)
                .map_err(|_| "invalid authorization revision")?;
            if expected["path"].as_str() != Some(path) {
                return Err("read path changed after approval".into());
            }
            let amendment = crate::runtime::sandbox_config::amend_sandbox_config_checked(
                config,
                &crate::runtime::sandbox_config::SandboxChange::AllowReadPath(path.into()),
                expected["config"].as_str(),
            )?;
            if let Err(error) = control.reload().await {
                let rollback =
                    crate::runtime::sandbox_config::revert_sandbox_config(config, &amendment);
                return Err(format!("{error}; rollback: {rollback:?}"));
            }
        }
        Ok(())
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        self.execute_inner(arguments, cancel, false).await
    }
    async fn execute_approved(
        &self,
        call: otto_core::tool::ToolCall<'_>,
        cancel: &CancellationToken,
        _request: &otto_core::approval::Request,
    ) -> ToolResult {
        self.execute_inner(call.arguments, cancel, true).await
    }
}
