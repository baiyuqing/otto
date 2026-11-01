//! Runs the Claude-format hooks of trusted `claude` packages through the shared sandboxed
//! process runner: Otto event data becomes a Claude payload, the exit status and stdout
//! become an Otto hook response.
use super::binding::{Bound, check_response_fields};
use super::*;
use crate::plugin::claude_hooks::{
    ClaudeHook,
    matcher::Matcher,
    payload::{self, ClaudePayload, SessionFields},
    response::{self, ResponseContext},
};

/// Replaces or appends `NAME=VALUE` so the given value wins over an inherited one.
fn set_environment_entry(environment: &mut Vec<String>, name: &str, value: &str) {
    let prefix = format!("{name}=");
    environment.retain(|entry| !entry.starts_with(&prefix));
    environment.push(format!("{prefix}{value}"));
}

/// The argv of a Claude hook handler. With `args` the command runs directly; otherwise the
/// command text runs through `/bin/sh -c` or `/bin/bash -c`. `${CLAUDE_PLUGIN_ROOT}`,
/// `${CLAUDE_PLUGIN_DATA}` and `${CLAUDE_PROJECT_DIR}` are substituted in every element.
fn claude_argv(hook: &ClaudeHook, package: &Package, workspace: &Path) -> Vec<String> {
    let workspace_text = workspace.to_string_lossy();
    let expand = |text: &str| {
        super::super::runtime::expand(text, &package.root, &package.data, true)
            .replace("${CLAUDE_PROJECT_DIR}", &workspace_text)
    };
    match &hook.args {
        Some(args) => std::iter::once(hook.command.as_str())
            .chain(args.iter().map(String::as_str))
            .map(expand)
            .collect(),
        None => {
            let shell = if hook.shell == "bash" {
                "/bin/bash"
            } else {
                "/bin/sh"
            };
            vec![shell.into(), "-c".into(), expand(&hook.command)]
        }
    }
}

impl Bound {
    /// Runs one Claude hook for an Otto event. `None` when the event produces no Claude event
    /// for this session, produces a different Claude event than the hook is configured for
    /// (`SessionStart` against a subagent session, `Stop` against a cancelled turn), or the
    /// matcher rejects the payload.
    pub(super) async fn run_claude(
        &self,
        event: &str,
        data: &Value,
        hook: &ClaudeHook,
        matcher: &Matcher,
        package: &Package,
        control: &dyn OperationControl,
    ) -> Option<Result<Response, String>> {
        let history_path = (self.binding.history_path)();
        let session = SessionFields {
            session_id: &self.binding.session_id,
            parent_session_id: self.binding.parent_session_id.as_deref(),
            task_id: &self.binding.task_id,
            mode: &self.binding.mode,
            history_path: &history_path,
        };
        let payload = payload::translate(event, data, &session, &self.runtime.execution.workspace)?;
        if payload.event != hook.event || !matcher.matches(&payload.matcher_value) {
            return None;
        }
        Some(
            self.execute_claude(event, data, hook, package, payload, control)
                .await,
        )
    }

    async fn execute_claude(
        &self,
        event: &str,
        data: &Value,
        hook: &ClaudeHook,
        package: &Package,
        payload: ClaudePayload,
        control: &dyn OperationControl,
    ) -> Result<Response, String> {
        let workspace = &self.runtime.execution.workspace;
        let input = self
            .runtime
            .redacted_input(&payload.body)
            .map_err(|error| error.message)?;
        let mut environment = self.runtime.command_environment(Some(package));
        set_environment_entry(
            &mut environment,
            "CLAUDE_PLUGIN_ROOT",
            &package.root.to_string_lossy(),
        );
        set_environment_entry(
            &mut environment,
            "CLAUDE_PLUGIN_DATA",
            &package.data.to_string_lossy(),
        );
        set_environment_entry(
            &mut environment,
            "CLAUDE_PROJECT_DIR",
            &workspace.to_string_lossy(),
        );
        set_environment_entry(
            &mut environment,
            "CLAUDE_CODE_SESSION_ATTENDED",
            if self.binding.headless { "0" } else { "1" },
        );
        let invocation = Invocation {
            argv: claude_argv(hook, package, workspace),
            environment,
            timeout: Duration::from_secs(hook.timeout_seconds),
            package: Some(package),
        };
        let output = self
            .runtime
            .run_process(invocation, input, event, control, self.guard.as_ref())
            .await
            .map_err(|error| error.message)?;
        let context = ResponseContext {
            workspace,
            otto_tool: data.get("tool").and_then(Value::as_str),
            original_arguments: data.get("arguments"),
            original_content: data.get("content").and_then(Value::as_str),
        };
        let response = response::translate(
            payload.event,
            output.exit_code,
            &output.stdout,
            &output.stderr,
            &context,
        )?;
        check_response_fields(event, &response)?;
        Ok(response)
    }
}
